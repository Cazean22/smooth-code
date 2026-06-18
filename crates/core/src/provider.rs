use std::{
    collections::{HashMap, HashSet},
    env,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use cazean_config::{Config, OpenAiConfig, ReasoningEffortConfig, ReasoningSummaryConfig};
use futures_util::{SinkExt, StreamExt};
use rig::{
    OneOrMany,
    agent::Agent,
    client::CompletionClient,
    completion::{Completion, CompletionError, CompletionRequest},
    message::{Message, Reasoning as MessageReasoning, ReasoningContent, Text, ToolCall},
    providers::{
        anthropic, gemini,
        openai::{
            self,
            responses_api::{
                AdditionalParameters, CompletionRequest as OpenAiResponsesCompletionRequest,
                InputItem as OpenAiResponsesInputItem, Output, Reasoning as OpenAiReasoning,
                ReasoningEffort, ReasoningSummary, ResponseStatus, ResponsesToolDefinition,
                ResponsesUsage,
                streaming::{
                    ContentPartChunkPart, ItemChunk, ItemChunkKind, ResponseChunkKind,
                    StreamingCompletionResponse as OpenAiStreamingCompletionResponse,
                },
            },
        },
        openrouter,
    },
    streaming::{
        RawStreamingChoice, RawStreamingToolCall, StreamedAssistantContent,
        StreamingCompletionResponse as RigStreamingCompletionResponse, ToolCallDeltaContent,
    },
};
use serde::Serialize;
use tokio::sync::{Mutex, RwLock};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message as TungsteniteMessage, client::IntoClientRequest},
};
use tools::{
    AskUserClient, AskUserQuestionTool, DeleteTool, EditTool, ExitPlanModeTool, PlanWriteTool,
    ReadTool, RunCommandTool, SkillTool, SpawnAgentTool, TodoWriteTool, WriteTool,
};

use crate::agent::{
    AgentControl, SystemPromptKind, plan_mode_instructions,
    prompt::{render_spawn_agent_tool_description, system_prompt_for_kind},
};
use crate::environment::EnvironmentContext;

/// Injectable builder for session-scoped models.
pub trait SessionModelFactory: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    fn build(
        &self,
        cwd: PathBuf,
        thread_id: cazean_protocol::ThreadId,
        ask_user_client: Option<AskUserClient>,
        current_turn_id: Arc<RwLock<Option<String>>>,
        system_prompt_kind: SystemPromptKind,
        agent_control: AgentControl,
        plan_mode: bool,
    ) -> Result<SessionModel>;
}

/// Default `SessionModelFactory` backed by a resolved [`Config`].
pub struct ConfigSessionModelFactory {
    config: Arc<Config>,
}

impl ConfigSessionModelFactory {
    pub fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

impl SessionModelFactory for ConfigSessionModelFactory {
    fn build(
        &self,
        cwd: PathBuf,
        thread_id: cazean_protocol::ThreadId,
        ask_user_client: Option<AskUserClient>,
        current_turn_id: Arc<RwLock<Option<String>>>,
        system_prompt_kind: SystemPromptKind,
        agent_control: AgentControl,
        plan_mode: bool,
    ) -> Result<SessionModel> {
        SessionModel::from_config(
            &self.config,
            cwd,
            thread_id,
            ask_user_client,
            current_turn_id,
            system_prompt_kind,
            agent_control,
            plan_mode,
        )
    }
}

/// Test seam for custom streaming behavior.
pub trait SessionModelDriver: Send + Sync {
    fn stream_completion_turn(
        &self,
        prompt: Message,
        history: Vec<Message>,
    ) -> Result<SessionCompletionStream>;

    fn call_tool(
        &self,
        _tool_name: &str,
        _args: &str,
    ) -> futures_util::future::BoxFuture<'static, Result<String>> {
        Box::pin(async { Err(anyhow!("manual tool execution is not supported")) })
    }
}

#[derive(Debug, Clone)]
pub struct SessionTurnSummary {
    pub assistant_message_id: Option<String>,
    pub response: String,
}

#[derive(Debug)]
pub enum SessionCompletionEvent {
    AssistantItem(SessionAssistantContent),
    Completed(SessionTurnSummary),
}

#[derive(Debug)]
pub enum SessionAssistantContent {
    Text(Text),
    ToolCall {
        tool_call: ToolCall,
        internal_call_id: String,
    },
    ToolCallDelta {
        id: String,
        internal_call_id: String,
        content: ToolCallDeltaContent,
    },
    Reasoning(MessageReasoning),
    ReasoningDelta {
        id: Option<String>,
        reasoning: String,
    },
    Final,
}

pub type SessionCompletionStream =
    Pin<Box<dyn futures_util::Stream<Item = Result<SessionCompletionEvent>> + Send>>;

#[derive(Clone)]
pub enum SessionModel {
    OpenAi(Arc<OpenAiSessionModel>),
    OpenRouter(Arc<Agent<openrouter::CompletionModel>>),
    Anthropic(Arc<Agent<anthropic::completion::CompletionModel>>),
    Gemini(Arc<Agent<gemini::completion::CompletionModel>>),
    Stub(Arc<dyn SessionModelDriver>),
}

pub struct OpenAiSessionModel {
    agent: Arc<Agent<openai::responses_api::ResponsesCompletionModel>>,
    client: openai::Client,
    model: String,
    websocket: cazean_config::WebSocketConfig,
    session_ws: Mutex<Option<OpenAiParkedWebSocket>>,
}

impl SessionModel {
    pub(crate) fn requires_provider_reasoning_ids(&self) -> bool {
        matches!(self, Self::OpenAi(_))
    }

    /// Whether a cancelled turn must keep polling the stream briefly: the
    /// OpenAI WebSocket sends `response.cancel` and parks its socket from
    /// inside the stream, which only makes progress while polled. The other
    /// providers cancel by drop.
    pub(crate) fn requires_stream_cancel_drain(&self) -> bool {
        matches!(self, Self::OpenAi(_))
    }

    /// How long the turn loop should keep polling a cancelled stream so the
    /// provider's own cancel can finish (it parks the WebSocket from inside the
    /// stream). For OpenAI this is the configured `cancel_drain_ms`; other
    /// models use the default. Only consulted when `requires_stream_cancel_drain`
    /// is true.
    pub(crate) fn stream_cancel_drain(&self) -> Duration {
        let ms = match self {
            Self::OpenAi(openai) => openai.websocket.cancel_drain_ms,
            _ => cazean_config::WebSocketConfig::default().cancel_drain_ms,
        };
        Duration::from_millis(ms)
    }

    /// OpenAI WebSocket pre-output retry budget. For OpenAI this is the
    /// configured value; other models (incl. the test stub) use the default,
    /// because actual retries are gated by the transient-marker check, not the
    /// budget — preserving the historical global-budget behavior.
    pub(crate) fn websocket_retry_budget(&self) -> usize {
        match self {
            Self::OpenAi(openai) => openai.websocket.retry_budget,
            _ => cazean_config::WebSocketConfig::default().retry_budget,
        }
    }

    /// Backoff delay for the `retry_count`-th OpenAI WebSocket retry, using the
    /// configured base/max for OpenAI and the defaults for other models.
    pub(crate) fn websocket_retry_delay(&self, retry_count: usize) -> Duration {
        let (base_ms, max_ms) = match self {
            Self::OpenAi(openai) => (
                openai.websocket.retry_base_ms,
                openai.websocket.retry_max_ms,
            ),
            _ => {
                let ws = cazean_config::WebSocketConfig::default();
                (ws.retry_base_ms, ws.retry_max_ms)
            }
        };
        openai_websocket_retry_delay(
            retry_count,
            Duration::from_millis(base_ms),
            Duration::from_millis(max_ms),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_config(
        config: &Config,
        cwd: PathBuf,
        thread_id: cazean_protocol::ThreadId,
        ask_user_client: Option<AskUserClient>,
        current_turn_id: Arc<RwLock<Option<String>>>,
        system_prompt_kind: SystemPromptKind,
        agent_control: AgentControl,
        plan_mode: bool,
    ) -> Result<Self> {
        // Normalize identically to config validation (shared helper) so a
        // value like " openai " that passed validation also matches an arm.
        let provider = cazean_config::normalize_provider(&config.provider.provider);
        let model = config.provider.model.clone();
        let environment_context = EnvironmentContext::gather(&cwd);
        let preamble = compose_session_preamble(
            system_prompt_kind,
            config.provider.preamble.clone(),
            &environment_context,
            plan_mode,
        );

        match provider.as_str() {
            "openai" => {
                let openai = &config.provider.openai;
                let builder = openai::Client::builder()
                    .api_key(&openai.api_key)
                    .base_url(&openai.base_url);
                let client = builder.build()?;
                // Hosted `web_search` is OpenAI-only and is suppressed in plan
                // mode, whose prompt enumerates an allowed-tool contract that an
                // un-listed hosted tool would contradict.
                let additional_params_json = openai_additional_params_json(
                    openai,
                    config.tools.web_search.enabled && !plan_mode,
                );
                let agent = build_agent(
                    client
                        .agent(&model)
                        .preamble(&preamble)
                        .additional_params(additional_params_json),
                    cwd,
                    thread_id,
                    ask_user_client.clone(),
                    Arc::clone(&current_turn_id),
                    system_prompt_kind,
                    agent_control.clone(),
                    plan_mode,
                    config,
                );
                Ok(Self::OpenAi(Arc::new(OpenAiSessionModel {
                    agent: Arc::new(agent),
                    client,
                    model,
                    websocket: config.provider.websocket.clone(),
                    session_ws: Mutex::new(None),
                })))
            }
            "openrouter" => {
                let client = openrouter::Client::new(&env::var("OPENROUTER_API_KEY")?)?;
                Ok(Self::OpenRouter(Arc::new(build_agent(
                    client.agent(&model).preamble(&preamble),
                    cwd,
                    thread_id,
                    ask_user_client.clone(),
                    Arc::clone(&current_turn_id),
                    system_prompt_kind,
                    agent_control.clone(),
                    plan_mode,
                    config,
                ))))
            }
            "anthropic" => {
                let client = anthropic::Client::new(env::var("ANTHROPIC_API_KEY")?)?;
                Ok(Self::Anthropic(Arc::new(build_agent(
                    client.agent(&model).preamble(&preamble),
                    cwd,
                    thread_id,
                    ask_user_client.clone(),
                    Arc::clone(&current_turn_id),
                    system_prompt_kind,
                    agent_control.clone(),
                    plan_mode,
                    config,
                ))))
            }
            "gemini" => {
                let client = gemini::Client::new(env::var("GEMINI_API_KEY")?)?;
                Ok(Self::Gemini(Arc::new(build_agent(
                    client.agent(&model).preamble(&preamble),
                    cwd,
                    thread_id,
                    ask_user_client,
                    current_turn_id,
                    system_prompt_kind,
                    agent_control,
                    plan_mode,
                    config,
                ))))
            }
            other => bail!("unsupported LLM provider `{other}`"),
        }
    }

    pub(crate) async fn stream_completion_turn(
        &self,
        prompt: Message,
        history: &[Message],
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<SessionCompletionStream> {
        match self {
            Self::OpenAi(openai) => {
                stream_openai_agent_completion(openai, prompt, history, cancel).await
            }
            // The HTTP providers take no explicit cancel: dropping the
            // response body aborts the underlying request at the transport
            // layer, which is the cleanest cancel reqwest offers.
            Self::OpenRouter(agent) => stream_agent_completion(agent, prompt, history).await,
            Self::Anthropic(agent) => stream_agent_completion(agent, prompt, history).await,
            Self::Gemini(agent) => stream_agent_completion(agent, prompt, history).await,
            Self::Stub(driver) => driver.stream_completion_turn(prompt, history.to_vec()),
        }
    }

    pub(crate) async fn call_tool(&self, tool_name: &str, args: &str) -> Result<String> {
        match self {
            Self::OpenAi(openai) => call_agent_tool(&openai.agent, tool_name, args).await,
            Self::OpenRouter(agent) => call_agent_tool(agent, tool_name, args).await,
            Self::Anthropic(agent) => call_agent_tool(agent, tool_name, args).await,
            Self::Gemini(agent) => call_agent_tool(agent, tool_name, args).await,
            Self::Stub(driver) => driver.call_tool(tool_name, args).await,
        }
    }
}

/// Whether `err` (as surfaced by [`SessionModel::call_tool`]) carries an
/// interrupted tool error. Rig wraps the tool's error as
/// `ToolServerError::ToolsetError(ToolCallError(ToolCallError(Box<tools::ToolError>)))`,
/// each layer exposing the next via `source()`, so the concrete `ToolError`
/// is reachable through the anyhow chain.
pub(crate) fn tool_error_is_interrupted(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<tools::ToolError>()
            .is_some_and(tools::ToolError::is_interrupted)
    })
}

/// Fallback factory for callers that pass no factory. In production the
/// resolved factory is always threaded down from `ThreadManagerState`, so this
/// is only hit by direct `CoreThread` callers; it uses built-in defaults
/// (equivalent to today's behavior).
pub(crate) fn default_session_model_factory() -> Arc<dyn SessionModelFactory> {
    Arc::new(ConfigSessionModelFactory::new(Arc::new(Config::default())))
}

pub(crate) fn stub_session_model_factory(
    models: HashMap<cazean_protocol::ThreadId, SessionModel>,
) -> Arc<dyn SessionModelFactory> {
    Arc::new(StubSessionModelFactory {
        models: std::sync::Mutex::new(models),
    })
}

struct StubSessionModelFactory {
    models: std::sync::Mutex<HashMap<cazean_protocol::ThreadId, SessionModel>>,
}

impl SessionModelFactory for StubSessionModelFactory {
    fn build(
        &self,
        _cwd: PathBuf,
        thread_id: cazean_protocol::ThreadId,
        _ask_user_client: Option<AskUserClient>,
        _current_turn_id: Arc<RwLock<Option<String>>>,
        _system_prompt_kind: SystemPromptKind,
        _agent_control: AgentControl,
        _plan_mode: bool,
    ) -> Result<SessionModel> {
        self.models
            .lock()
            .map_err(|_| anyhow::anyhow!("stub session model factory mutex was poisoned"))?
            .get(&thread_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing stub session model for thread {thread_id}"))
    }
}

fn compose_session_preamble(
    system_prompt_kind: SystemPromptKind,
    env_preamble: Option<String>,
    environment_context: &EnvironmentContext,
    plan_mode: bool,
) -> String {
    let base_preamble = if matches!(system_prompt_kind, SystemPromptKind::Root) {
        env_preamble.unwrap_or_else(|| system_prompt_for_kind(system_prompt_kind).to_string())
    } else {
        system_prompt_for_kind(system_prompt_kind).to_string()
    };
    let mut preamble = environment_context.apply(&base_preamble);
    if plan_mode {
        preamble = format!("{}\n\n{}", preamble.trim_end(), plan_mode_instructions());
    }
    preamble
}

#[allow(clippy::too_many_arguments)]
fn build_agent<M>(
    builder: rig::agent::AgentBuilder<M, (), rig::agent::NoToolConfig>,
    cwd: PathBuf,
    thread_id: cazean_protocol::ThreadId,
    ask_user_client: Option<AskUserClient>,
    current_turn_id: Arc<RwLock<Option<String>>>,
    system_prompt_kind: SystemPromptKind,
    _agent_control: AgentControl,
    plan_mode: bool,
    config: &Config,
) -> Agent<M>
where
    M: rig::completion::CompletionModel,
{
    let tools = &config.tools;
    let rc = &tools.run_command;
    let max_turns = config.provider.max_turns as usize;
    // File reads, shell inspection, and sub-agent spawning are always present.
    let builder = builder
        .tool(ReadTool::new(cwd.clone()).with_default_limit(tools.read_default_limit))
        .tool(RunCommandTool::new(cwd.clone()).with_limits(
            rc.default_timeout_secs,
            rc.max_timeout_secs,
            rc.term_grace_ms,
            tools.max_tool_output_bytes,
        ));
    if matches!(system_prompt_kind, SystemPromptKind::Explore) {
        return builder.default_max_turns(max_turns).build();
    }
    // Progress tracking is available everywhere except read-only Explore agents.
    let builder = builder
        .tool(TodoWriteTool::new().with_max_todos(tools.max_todos))
        .tool(SkillTool::new(cwd.clone()).with_max_skill_bytes(tools.max_skill_bytes));
    // File-mutating tools are only registered outside plan mode;
    // plan-mode-specific tools (`plan_write`, `exit_plan_mode`) are only
    // registered inside plan mode.
    let builder = if plan_mode {
        builder
            .tool(PlanWriteTool::new(cwd.clone(), thread_id))
            .tool(ExitPlanModeTool::new())
    } else {
        builder
            .tool(
                DeleteTool::new(cwd.clone())
                    .with_max_file_change_bytes(tools.max_file_change_bytes),
            )
            .tool(
                EditTool::new(cwd.clone()).with_max_file_change_bytes(tools.max_file_change_bytes),
            )
            .tool(WriteTool::new(cwd).with_max_file_change_bytes(tools.max_file_change_bytes))
    };
    let builder = if let Some(ask_user_client) = ask_user_client {
        builder.tool(AskUserQuestionTool::new(
            thread_id,
            ask_user_client,
            current_turn_id,
        ))
    } else {
        builder
    };
    let builder = builder.tool(SpawnAgentTool::new(render_spawn_agent_tool_description()));
    builder.default_max_turns(max_turns).build()
}

/// Convert the config reasoning-effort enum to Rig's `ReasoningEffort`.
fn reasoning_effort(effort: ReasoningEffortConfig) -> ReasoningEffort {
    match effort {
        ReasoningEffortConfig::None => ReasoningEffort::None,
        ReasoningEffortConfig::Minimal => ReasoningEffort::Minimal,
        ReasoningEffortConfig::Low => ReasoningEffort::Low,
        ReasoningEffortConfig::Medium => ReasoningEffort::Medium,
        ReasoningEffortConfig::High => ReasoningEffort::High,
        ReasoningEffortConfig::Xhigh => ReasoningEffort::Xhigh,
    }
}

/// Convert the config reasoning-summary enum to Rig's `ReasoningSummaryLevel`.
fn reasoning_summary(
    summary: ReasoningSummaryConfig,
) -> openai::responses_api::ReasoningSummaryLevel {
    use openai::responses_api::ReasoningSummaryLevel;
    match summary {
        ReasoningSummaryConfig::Auto => ReasoningSummaryLevel::Auto,
        ReasoningSummaryConfig::Concise => ReasoningSummaryLevel::Concise,
        ReasoningSummaryConfig::Detailed => ReasoningSummaryLevel::Detailed,
    }
}

/// Wire value for OpenAI's hosted `web_search` tool. `external_web_access: true`
/// requests live results and mirrors codex's working request shape against the
/// shared backend; the serialization of this small Rig struct is infallible in
/// practice, so the fallback is only a defensive equivalent.
fn openai_web_search_tool_value() -> serde_json::Value {
    serde_json::to_value(
        ResponsesToolDefinition::web_search()
            .with_config("external_web_access", serde_json::Value::Bool(true)),
    )
    .unwrap_or_else(|_| serde_json::json!({ "type": "web_search", "external_web_access": true }))
}

/// Build the OpenAI `additional_params` JSON: reasoning configuration plus, when
/// `include_web_search`, the hosted `web_search` tool. This is the sole writer of
/// the `"tools"` key, so there is no pre-existing array to merge with — Rig's
/// request builder lifts `additional_params["tools"]` out and appends it to the
/// request's tool list.
fn openai_additional_params_json(
    openai: &OpenAiConfig,
    include_web_search: bool,
) -> serde_json::Value {
    let additional_params = AdditionalParameters {
        reasoning: Some(
            OpenAiReasoning::new()
                .with_effort(reasoning_effort(openai.reasoning_effort))
                .with_summary_level(reasoning_summary(openai.reasoning_summary)),
        ),
        ..Default::default()
    };
    let mut params = additional_params.to_json();
    if include_web_search && let Some(object) = params.as_object_mut() {
        object.insert(
            "tools".to_string(),
            serde_json::Value::Array(vec![openai_web_search_tool_value()]),
        );
    }
    params
}

async fn stream_agent_completion<M>(
    agent: &Arc<Agent<M>>,
    prompt: Message,
    history: &[Message],
) -> Result<SessionCompletionStream>
where
    M: rig::completion::CompletionModel + 'static,
    M::StreamingResponse: Clone + Unpin + rig::completion::GetTokenUsage + Send,
{
    let mut stream = agent
        .completion(prompt, history.iter().cloned())
        .await?
        .stream()
        .await?;
    Ok(Box::pin(async_stream::try_stream! {
        while let Some(item) = stream.next().await {
            let assistant_item = session_assistant_content_from_streamed(item?);
            yield SessionCompletionEvent::AssistantItem(assistant_item);
        }

        let response = stream
            .choice
            .iter()
            .filter_map(|content| match content {
                rig::message::AssistantContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<String>();
        yield SessionCompletionEvent::Completed(SessionTurnSummary {
            assistant_message_id: stream.message_id.clone(),
            response,
        });
    }))
}

fn session_assistant_content_from_streamed<R>(
    item: StreamedAssistantContent<R>,
) -> SessionAssistantContent {
    match item {
        StreamedAssistantContent::Text(text) => SessionAssistantContent::Text(text),
        StreamedAssistantContent::ToolCall {
            tool_call,
            internal_call_id,
        } => SessionAssistantContent::ToolCall {
            tool_call,
            internal_call_id,
        },
        StreamedAssistantContent::ToolCallDelta {
            id,
            internal_call_id,
            content,
        } => SessionAssistantContent::ToolCallDelta {
            id,
            internal_call_id,
            content,
        },
        StreamedAssistantContent::Reasoning(reasoning) => {
            SessionAssistantContent::Reasoning(reasoning)
        }
        StreamedAssistantContent::ReasoningDelta { id, reasoning } => {
            SessionAssistantContent::ReasoningDelta { id, reasoning }
        }
        StreamedAssistantContent::Final(_) => SessionAssistantContent::Final,
    }
}

type OpenAiWebSocketRawChoice = RawStreamingChoice<OpenAiStreamingCompletionResponse>;
type OpenAiWebSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
// Retry/backoff/cancel-drain tuning is configurable via `[provider.websocket]`
// and carried on `OpenAiSessionModel.websocket`. Defaults (budget 8, base 250ms,
// max 3s, drain 1500ms) live in `cazean-config`. A connection reset before
// output is how the local proxy (CLIProxyAPI) surfaces an upstream codex account
// running out of usage; the budget rolls through several exhausted accounts and
// the capped exponential backoff keeps the retry tail interactive. The
// cancel-drain bounds how long the read loop keeps consuming after sending
// `response.cancel`, hoping for a clean terminal so the socket can be parked.

struct OpenAiParkedWebSocket {
    socket: OpenAiWebSocket,
    trailing_done_response_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct OpenAiWebSocketCreateEvent {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(flatten)]
    request: OpenAiResponsesCompletionRequest,
}

#[derive(Debug, Serialize)]
struct OpenAiWebSocketCancelEvent<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_id: Option<&'a str>,
}

#[derive(Default)]
struct OpenAiWebSocketPayloadOutcome {
    choices: Vec<OpenAiWebSocketRawChoice>,
    terminal: bool,
    terminal_response_id: Option<String>,
    terminal_error: Option<CompletionError>,
}

async fn stream_openai_agent_completion(
    openai: &Arc<OpenAiSessionModel>,
    prompt: Message,
    history: &[Message],
    cancel: tokio_util::sync::CancellationToken,
) -> Result<SessionCompletionStream> {
    let completion_request = openai
        .agent
        .completion(prompt, history.iter().cloned())
        .await
        .context("failed to build OpenAI WebSocket completion request")?
        .build();
    let openai = Arc::clone(openai);
    Ok(Box::pin(async_stream::try_stream! {
        let mut retry_count = 0;
        loop {
            let mut stream = match openai_websocket_completion_stream(&openai, completion_request.clone(), cancel.clone()).await {
                Ok(stream) => stream,
                Err(error)
                    if retry_count < openai.websocket.retry_budget
                        && should_retry_openai_websocket_error(&error, false) =>
                {
                    retry_count += 1;
                    tracing::debug!(
                        retry_count,
                        error = %error,
                        "OpenAI WebSocket transient failure before the turn stream started; retrying"
                    );
                    tokio::time::sleep(openai_websocket_retry_delay(retry_count, Duration::from_millis(openai.websocket.retry_base_ms), Duration::from_millis(openai.websocket.retry_max_ms))).await;
                    continue;
                }
                Err(error) => Err(error).context("failed to start OpenAI WebSocket completion stream")?,
            };

            let mut yielded_assistant_item = false;
            let mut retry_after_start_error = false;
            while let Some(item) = stream.next().await {
                match item {
                    Ok(item) => {
                        yielded_assistant_item = true;
                        let assistant_item = session_assistant_content_from_streamed(item);
                        yield SessionCompletionEvent::AssistantItem(assistant_item);
                    }
                    Err(error)
                        if retry_count < openai.websocket.retry_budget
                            && should_retry_openai_websocket_error(&error, yielded_assistant_item) =>
                    {
                        retry_count += 1;
                        tracing::debug!(
                            retry_count,
                            error = %error,
                            "OpenAI WebSocket transient failure before any assistant item; retrying"
                        );
                        tokio::time::sleep(openai_websocket_retry_delay(retry_count, Duration::from_millis(openai.websocket.retry_base_ms), Duration::from_millis(openai.websocket.retry_max_ms))).await;
                        retry_after_start_error = true;
                        break;
                    }
                    Err(error) => Err(openai_websocket_stream_error(error))?,
                }
            }
            if retry_after_start_error {
                continue;
            }

            let response = stream
                .choice
                .iter()
                .filter_map(|content| match content {
                    rig::message::AssistantContent::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect::<String>();
            yield SessionCompletionEvent::Completed(SessionTurnSummary {
                assistant_message_id: stream.message_id.clone(),
                response,
            });
            break;
        }
    }))
}

async fn openai_websocket_completion_stream(
    openai: &Arc<OpenAiSessionModel>,
    completion_request: CompletionRequest,
    cancel: tokio_util::sync::CancellationToken,
) -> std::result::Result<
    RigStreamingCompletionResponse<OpenAiStreamingCompletionResponse>,
    CompletionError,
> {
    let payload = openai_websocket_create_payload(openai.model.as_str(), completion_request)?;
    let parked_socket = openai.session_ws.lock().await.take();
    let (mut socket, mut trailing_done_response_id, reused_socket) = match parked_socket {
        Some(parked) => (parked.socket, parked.trailing_done_response_id, true),
        None => (openai_websocket_connect(&openai.client).await?, None, false),
    };
    if reused_socket {
        // A parked socket may have gone stale since the previous turn, so keep the payload
        // around to resend after reconnecting if the first send fails. A fresh connection
        // can't be stale, so the `else` branch sends by move and pays no clone.
        if let Err(error) = socket.send(TungsteniteMessage::text(payload.clone())).await {
            let error = openai_websocket_provider_error(error);
            tracing::debug!(
                error = %error,
                "OpenAI WebSocket send failed on a reused socket; reconnecting before retrying request"
            );
            socket = openai_websocket_connect(&openai.client).await?;
            trailing_done_response_id = None;
            socket
                .send(TungsteniteMessage::text(payload))
                .await
                .map_err(openai_websocket_provider_error)?;
        }
    } else {
        socket
            .send(TungsteniteMessage::text(payload))
            .await
            .map_err(openai_websocket_provider_error)?;
    }
    Ok(openai_websocket_stream(
        Arc::clone(openai),
        socket,
        trailing_done_response_id,
        cancel,
    ))
}

fn openai_websocket_stream(
    openai: Arc<OpenAiSessionModel>,
    socket: OpenAiWebSocket,
    mut stale_done_response_id: Option<String>,
    cancel: tokio_util::sync::CancellationToken,
) -> RigStreamingCompletionResponse<OpenAiStreamingCompletionResponse> {
    let raw_stream = async_stream::try_stream! {
        let mut socket = Some(socket);
        let mut accumulator = OpenAiWebSocketAccumulator::new();
        let mut terminal_error = None;
        let mut terminal_response_id = None;
        let mut clean_terminal = false;
        let mut cancel_sent = false;
        let mut drain_deadline = tokio::time::Instant::now();

        while let Some(socket) = socket.as_mut() {
            let message = if cancel_sent {
                // Cancel already sent: keep consuming, hoping the provider
                // answers with a clean terminal so the socket can be parked,
                // but never wait past the drain deadline.
                tokio::select! {
                    message = socket.next() => message,
                    _ = tokio::time::sleep_until(drain_deadline) => {
                        tracing::debug!(
                            "OpenAI WebSocket cancel drain deadline expired; dropping socket"
                        );
                        terminal_error = Some(CompletionError::ProviderError(
                            "turn cancelled".to_string(),
                        ));
                        break;
                    }
                }
            } else {
                tokio::select! {
                    message = socket.next() => message,
                    _ = cancel.cancelled() => {
                        // Tell the provider to stop generating instead of
                        // silently dropping the connection. Frame per the
                        // Responses-over-WebSocket dialect; a proxy that does
                        // not understand it just runs into the drain deadline.
                        let cancel_payload = openai_websocket_cancel_payload(
                            accumulator.active_response_id.as_deref(),
                        );
                        let _ = socket
                            .send(TungsteniteMessage::text(cancel_payload))
                            .await;
                        cancel_sent = true;
                        drain_deadline =
                            tokio::time::Instant::now() + Duration::from_millis(openai.websocket.cancel_drain_ms);
                        continue;
                    }
                }
            };
            match message {
                Some(Ok(message)) => {
                    let payload = match openai_websocket_message_to_text(message) {
                        Ok(Some(payload)) => payload,
                        Ok(None) => continue,
                        Err(error) => {
                            if accumulator.can_finish_after_disconnect() {
                                tracing::debug!(
                                    error = %error,
                                    "OpenAI WebSocket closed after a complete output item; finishing turn"
                                );
                            } else {
                                terminal_error = Some(error);
                            }
                            break;
                        }
                    };
                    if stale_done_response_id
                        .as_deref()
                        .is_some_and(|response_id| {
                            openai_websocket_is_response_done_for(&payload, response_id)
                        })
                    {
                        // A reused socket can still carry the previous turn's trailing
                        // `response.done`. Other previous-turn frames (e.g. telemetry) may be
                        // buffered ahead of it, so the guard stays armed until the matching
                        // done is actually seen — disarming earlier would let the stale done
                        // through and prematurely terminate this turn with old metadata.
                        stale_done_response_id = None;
                        tracing::debug!(
                            "skipping stale trailing OpenAI WebSocket response.done from previous turn"
                        );
                        continue;
                    }
                    match parse_openai_websocket_payload(&payload, &mut accumulator) {
                        Ok(outcome) => {
                            for choice in outcome.choices {
                                yield choice;
                            }
                            if outcome.terminal {
                                clean_terminal = true;
                                terminal_response_id = outcome.terminal_response_id;
                                terminal_error = outcome.terminal_error;
                                break;
                            }
                        }
                        Err(error) => {
                            terminal_error = Some(error);
                            break;
                        }
                    }
                }
                Some(Err(error)) => {
                    let error = openai_websocket_provider_error(error);
                    if accumulator.can_finish_after_disconnect()
                        && is_openai_websocket_connection_reset(&error)
                    {
                        tracing::debug!(
                            error = %error,
                            "OpenAI WebSocket reset after a complete output item; finishing turn"
                        );
                    } else {
                        terminal_error = Some(error);
                    }
                    break;
                }
                None => {
                    if accumulator.can_finish_after_disconnect() {
                        tracing::debug!(
                            "OpenAI WebSocket ended after a complete output item; finishing turn"
                        );
                    } else {
                        terminal_error = Some(CompletionError::ProviderError(
                            "The OpenAI WebSocket connection closed before the turn finished"
                                .to_string(),
                        ));
                    }
                    break;
                }
            }
        }

        // Park the socket for the next turn whenever the turn ended on a clean protocol
        // terminal (success, failure, or incomplete) — the connection itself is still
        // healthy. A mid-turn transport drop leaves `clean_terminal` false, so the socket is
        // dropped instead of reused. Parking before yielding `accumulator.finish()` is safe:
        // `finish()` only drains in-memory accumulator state and never touches the socket.
        if clean_terminal && let Some(socket) = socket.take() {
            *openai.session_ws.lock().await = Some(OpenAiParkedWebSocket {
                socket,
                trailing_done_response_id: terminal_response_id.take(),
            });
        }

        if let Some(error) = terminal_error {
            Err(openai_websocket_stream_error(error))?;
        }

        for choice in accumulator.finish() {
            yield choice;
        }
    };

    RigStreamingCompletionResponse::stream(Box::pin(raw_stream))
}

async fn openai_websocket_connect(
    client: &openai::Client,
) -> std::result::Result<OpenAiWebSocket, CompletionError> {
    let url = openai_websocket_url(client.base_url())?;
    let mut request = url.into_client_request().map_err(|error| {
        CompletionError::ProviderError(format!("Failed to build OpenAI WebSocket request: {error}"))
    })?;
    for (name, value) in client.headers() {
        request.headers_mut().insert(name, value.clone());
    }

    connect_async(request)
        .await
        .map(|(socket, _)| socket)
        .map_err(openai_websocket_provider_error)
}

fn openai_websocket_url(base_url: &str) -> std::result::Result<String, CompletionError> {
    let mut url = url::Url::parse(base_url).map_err(|error| {
        CompletionError::ProviderError(format!("Invalid OpenAI WebSocket base URL: {error}"))
    })?;
    match url.scheme() {
        "https" => {
            url.set_scheme("wss").map_err(|_| {
                CompletionError::ProviderError("Failed to convert https URL to wss".to_string())
            })?;
        }
        "http" => {
            url.set_scheme("ws").map_err(|_| {
                CompletionError::ProviderError("Failed to convert http URL to ws".to_string())
            })?;
        }
        scheme => {
            return Err(CompletionError::ProviderError(format!(
                "Unsupported base URL scheme for OpenAI WebSocket mode: {scheme}"
            )));
        }
    }

    let path = format!("{}/responses", url.path().trim_end_matches('/'));
    url.set_path(&path);
    Ok(url.to_string())
}

fn openai_websocket_create_payload(
    model: &str,
    completion_request: CompletionRequest,
) -> std::result::Result<String, CompletionError> {
    let mut request =
        OpenAiResponsesCompletionRequest::try_from((model.to_string(), completion_request))?;
    normalize_openai_websocket_response_request(&mut request)?;
    request.stream = None;
    request.additional_parameters.background = None;

    dump_openai_turn_request_body(&request);

    serde_json::to_string(&OpenAiWebSocketCreateEvent {
        kind: "response.create",
        request,
    })
    .map_err(CompletionError::from)
}

fn openai_websocket_cancel_payload(response_id: Option<&str>) -> String {
    serde_json::to_string(&OpenAiWebSocketCancelEvent {
        kind: "response.cancel",
        response_id,
    })
    .unwrap_or_else(|error| {
        tracing::warn!(
            error = %error,
            "failed to serialize OpenAI WebSocket cancel payload; falling back to bare cancel"
        );
        r#"{"type":"response.cancel"}"#.to_string()
    })
}

fn dump_openai_turn_request_body(request: &OpenAiResponsesCompletionRequest) {
    let Ok(path) = env::var("CAZEAN_OPENAI_TURN_REQUEST_JSON") else {
        return;
    };
    if path.trim().is_empty() {
        tracing::warn!("CAZEAN_OPENAI_TURN_REQUEST_JSON is empty; skipping turn request dump");
        return;
    }

    let path = PathBuf::from(path);
    let payload = match serde_json::to_vec_pretty(request) {
        Ok(payload) => payload,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "failed to serialize OpenAI turn request body for debug dump"
            );
            return;
        }
    };

    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(
            path = %parent.display(),
            error = %error,
            "failed to create OpenAI turn request dump directory"
        );
        return;
    }

    if let Err(error) = std::fs::write(&path, payload) {
        tracing::warn!(
            path = %path.display(),
            error = %error,
            "failed to write OpenAI turn request body debug dump"
        );
    }
}

fn normalize_openai_websocket_response_request(
    request: &mut OpenAiResponsesCompletionRequest,
) -> std::result::Result<(), CompletionError> {
    let mut saw_system_input = false;
    let mut system_instructions = Vec::new();
    let mut input = Vec::new();

    for item in request.input.iter().cloned() {
        match openai_websocket_system_input_text(&item)? {
            Some(instructions) => {
                saw_system_input = true;
                let instructions = instructions.trim();
                if !instructions.is_empty() {
                    system_instructions.push(instructions.to_string());
                }
            }
            None => input.push(item),
        }
    }

    if !saw_system_input {
        return Ok(());
    }

    request.input = OneOrMany::many(input).map_err(|_| {
        CompletionError::RequestError(
            "OpenAI WebSocket request must contain at least one non-system input item".into(),
        )
    })?;

    if !system_instructions.is_empty() {
        request.instructions = Some(openai_websocket_merge_instructions(
            &system_instructions.join("\n\n"),
            request.instructions.as_deref(),
        ));
    }

    Ok(())
}

fn openai_websocket_system_input_text(
    item: &OpenAiResponsesInputItem,
) -> std::result::Result<Option<String>, CompletionError> {
    let value = serde_json::to_value(item)?;
    if !value
        .get("role")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|role| role == "system" || role == "developer")
    {
        return Ok(None);
    }

    let Some(content) = value.get("content") else {
        return Ok(Some(String::new()));
    };

    let mut parts = Vec::new();
    openai_websocket_collect_text_content(content, &mut parts)?;
    Ok(Some(parts.join("\n")))
}

fn openai_websocket_collect_text_content(
    value: &serde_json::Value,
    parts: &mut Vec<String>,
) -> std::result::Result<(), CompletionError> {
    match value {
        serde_json::Value::String(text) => parts.push(text.clone()),
        serde_json::Value::Array(items) => {
            for item in items {
                openai_websocket_collect_text_content(item, parts)?;
            }
        }
        serde_json::Value::Object(map) => {
            let text = map
                .get("text")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    CompletionError::RequestError(
                        "OpenAI WebSocket system input contained non-text content".into(),
                    )
                })?;
            parts.push(text.to_string());
        }
        serde_json::Value::Null => {}
        _ => {
            return Err(CompletionError::RequestError(
                "OpenAI WebSocket system input contained non-text content".into(),
            ));
        }
    }

    Ok(())
}

fn openai_websocket_merge_instructions(
    system_instructions: &str,
    existing_instructions: Option<&str>,
) -> String {
    match existing_instructions
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(existing) => format!("{system_instructions}\n\n{existing}"),
        None => system_instructions.to_string(),
    }
}

fn openai_websocket_message_to_text(
    message: TungsteniteMessage,
) -> std::result::Result<Option<String>, CompletionError> {
    match message {
        TungsteniteMessage::Text(text) => Ok(Some(text.to_string())),
        TungsteniteMessage::Binary(bytes) => String::from_utf8(bytes.to_vec())
            .map(Some)
            .map_err(|error| CompletionError::ResponseError(error.to_string())),
        TungsteniteMessage::Ping(_)
        | TungsteniteMessage::Pong(_)
        | TungsteniteMessage::Frame(_) => Ok(None),
        TungsteniteMessage::Close(frame) => {
            let reason = frame
                .map(|frame| frame.reason.to_string())
                .filter(|reason| !reason.is_empty())
                .unwrap_or_else(|| "without a close reason".to_string());
            Err(CompletionError::ProviderError(format!(
                "The OpenAI WebSocket connection closed {reason}"
            )))
        }
    }
}

fn parse_openai_websocket_payload(
    payload: &str,
    accumulator: &mut OpenAiWebSocketAccumulator,
) -> std::result::Result<OpenAiWebSocketPayloadOutcome, CompletionError> {
    let value = serde_json::from_str::<serde_json::Value>(payload)?;
    let Some(kind) = value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
    else {
        return Ok(OpenAiWebSocketPayloadOutcome::default());
    };

    match kind.as_str() {
        "error" => {
            let message = openai_websocket_error_event_message(&value);
            Err(provider_error_with_optional_tag(
                message,
                openai_websocket_error_value_is_transient(&value),
            ))
        }
        "response.done" => {
            let response_value = value.get("response").ok_or_else(|| {
                CompletionError::ProviderError(
                    "OpenAI WebSocket response.done was missing response".to_string(),
                )
            })?;
            let terminal_error = accumulator.record_done_response_value(response_value)?;
            Ok(OpenAiWebSocketPayloadOutcome {
                terminal: true,
                terminal_response_id: openai_websocket_response_id(response_value),
                terminal_error,
                ..Default::default()
            })
        }
        "response.created"
        | "response.in_progress"
        | "response.completed"
        | "response.failed"
        | "response.incomplete" => {
            let response_value = value.get("response").ok_or_else(|| {
                CompletionError::ProviderError(format!(
                    "OpenAI WebSocket {kind} was missing response"
                ))
            })?;
            let response_kind = match kind.as_str() {
                "response.created" => ResponseChunkKind::ResponseCreated,
                "response.in_progress" => ResponseChunkKind::ResponseInProgress,
                "response.completed" => ResponseChunkKind::ResponseCompleted,
                "response.failed" => ResponseChunkKind::ResponseFailed,
                "response.incomplete" => ResponseChunkKind::ResponseIncomplete,
                _ => unreachable!("response kind matched above"),
            };
            let terminal = matches!(
                response_kind,
                ResponseChunkKind::ResponseCompleted
                    | ResponseChunkKind::ResponseFailed
                    | ResponseChunkKind::ResponseIncomplete
            );
            let terminal_error =
                accumulator.record_response_value(response_kind, response_value)?;
            Ok(OpenAiWebSocketPayloadOutcome {
                terminal,
                terminal_response_id: terminal
                    .then(|| openai_websocket_response_id(response_value))
                    .flatten(),
                terminal_error,
                ..Default::default()
            })
        }
        "response.output_item.added" | "response.output_item.done" => {
            let choices = match serde_json::from_value::<ItemChunk>(value) {
                Ok(item) => accumulator.decode_item_chunk(item),
                Err(error) => {
                    tracing::debug!(
                        error = %error,
                        event_type = kind.as_str(),
                        "skipping OpenAI WebSocket item event with unsupported payload shape"
                    );
                    Vec::new()
                }
            };
            Ok(OpenAiWebSocketPayloadOutcome {
                choices,
                ..Default::default()
            })
        }
        "response.output_text.delta" | "response.refusal.delta" => {
            let choices = value
                .get("delta")
                .and_then(serde_json::Value::as_str)
                .map(|delta| vec![RawStreamingChoice::Message(delta.to_string())])
                .unwrap_or_default();
            Ok(OpenAiWebSocketPayloadOutcome {
                choices,
                ..Default::default()
            })
        }
        "response.output_text.done" | "response.refusal.done" => {
            accumulator.mark_completed_message_item(openai_websocket_item_id(&value));
            Ok(OpenAiWebSocketPayloadOutcome::default())
        }
        "response.content_part.done" => {
            if value
                .get("part")
                .and_then(|part| part.get("type"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|part_type| part_type == "output_text")
            {
                accumulator.mark_completed_message_item(openai_websocket_item_id(&value));
            }
            Ok(OpenAiWebSocketPayloadOutcome::default())
        }
        "response.function_call_arguments.delta" => {
            let Some(item_id) = value
                .get("item_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
            else {
                return Ok(OpenAiWebSocketPayloadOutcome::default());
            };
            let Some(delta) = value.get("delta").and_then(serde_json::Value::as_str) else {
                return Ok(OpenAiWebSocketPayloadOutcome::default());
            };
            let internal_call_id = accumulator.internal_call_id_for(&item_id);
            Ok(OpenAiWebSocketPayloadOutcome {
                choices: vec![RawStreamingChoice::ToolCallDelta {
                    id: item_id,
                    internal_call_id,
                    content: ToolCallDeltaContent::Delta(delta.to_string()),
                }],
                ..Default::default()
            })
        }
        "response.function_call_arguments.done" => {
            if let Some(item_id) = value.get("item_id").and_then(serde_json::Value::as_str) {
                let arguments = openai_websocket_arguments_value(&value);
                accumulator.record_tool_call_args_done(item_id, arguments);
            }
            Ok(OpenAiWebSocketPayloadOutcome::default())
        }
        "response.reasoning_summary_text.delta" => {
            let choices = value
                .get("delta")
                .or_else(|| value.get("text"))
                .and_then(serde_json::Value::as_str)
                .map(|reasoning| {
                    vec![RawStreamingChoice::ReasoningDelta {
                        id: openai_websocket_item_id(&value),
                        reasoning: reasoning.to_string(),
                    }]
                })
                .unwrap_or_default();
            Ok(OpenAiWebSocketPayloadOutcome {
                choices,
                ..Default::default()
            })
        }
        _ => Ok(OpenAiWebSocketPayloadOutcome::default()),
    }
}

fn openai_websocket_item_id(value: &serde_json::Value) -> Option<String> {
    value
        .get("item_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn openai_websocket_response_id(value: &serde_json::Value) -> Option<String> {
    value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn openai_websocket_is_response_done_for(payload: &str, response_id: &str) -> bool {
    // Reuse skips only a late `response.done`: the primary terminal frame is
    // expected to be `response.completed`/`failed`/`incomplete` for that ID.
    let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
        return false;
    };
    value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|kind| kind == "response.done")
        && value
            .get("response")
            .and_then(openai_websocket_response_id)
            .is_some_and(|observed_id| observed_id == response_id)
}

fn openai_websocket_arguments_value(value: &serde_json::Value) -> serde_json::Value {
    let arguments = value
        .get("arguments")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    if let serde_json::Value::String(raw) = &arguments {
        serde_json::from_str(raw).unwrap_or(arguments)
    } else {
        arguments
    }
}

fn openai_websocket_error_event_message(value: &serde_json::Value) -> String {
    let error = value.get("error");
    let message = error
        .and_then(|error| error.get("message"))
        .or_else(|| value.get("message"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("OpenAI WebSocket error");
    let mut labels = Vec::new();
    for label in [
        error
            .and_then(|error| error.get("code"))
            .and_then(serde_json::Value::as_str),
        error
            .and_then(|error| error.get("type"))
            .and_then(serde_json::Value::as_str),
        value.get("code").and_then(serde_json::Value::as_str),
    ]
    .into_iter()
    .flatten()
    {
        if !label.is_empty() && !labels.contains(&label) {
            labels.push(label);
        }
    }

    let missing_labels = labels
        .into_iter()
        .filter(|label| !message.contains(label))
        .collect::<Vec<_>>();

    if missing_labels.is_empty() {
        message.to_string()
    } else {
        format!("{}: {message}", missing_labels.join(": "))
    }
}

/// Owned marker prepended to a surfaced WebSocket error message once it has been
/// classified as a transient, pre-output disconnect worth retrying. Classification
/// happens once, at the structured/typed source (the proxy `error` event, the
/// tungstenite variant, or the chokepoint prose fallback); the retry decision
/// points then key off this owned tag instead of re-matching foreign upstream
/// prose, which a proxy reword could otherwise silently flip.
const WS_TRANSIENT_TAG: &str = "[cazean:transient-ws]";

/// Build a `ProviderError`, prepending [`WS_TRANSIENT_TAG`] when `transient` so
/// the verdict travels with the error to the retry decision points.
fn provider_error_with_optional_tag(message: String, transient: bool) -> CompletionError {
    if transient {
        CompletionError::ProviderError(format!("{WS_TRANSIENT_TAG} {message}"))
    } else {
        CompletionError::ProviderError(message)
    }
}

fn openai_error_message_is_tagged_transient(error: &CompletionError) -> bool {
    error.to_string().contains(WS_TRANSIENT_TAG)
}

/// The raw human message of a `CompletionError`, without the variant's `Display`
/// prefix. Used when re-tagging an error so a `ProviderError`'s message is not
/// nested inside another `ProviderError` (which would render as
/// `ProviderError: ProviderError: …`).
fn raw_completion_message(error: &CompletionError) -> String {
    match error {
        CompletionError::ProviderError(message) | CompletionError::ResponseError(message) => {
            message.clone()
        }
        other => other.to_string(),
    }
}

/// Remove the internal [`WS_TRANSIENT_TAG`] from a rendered error message before
/// it is shown to a person (e.g. once retries are exhausted and the error
/// surfaces). The tag is an internal retry-classification marker, not user copy.
pub(crate) fn strip_ws_transient_tag(message: &str) -> String {
    message.replace(&format!("{WS_TRANSIENT_TAG} "), "")
}

fn openai_websocket_provider_error(
    error: tokio_tungstenite::tungstenite::Error,
) -> CompletionError {
    // Classify from the typed tungstenite variant (robust to Display wording),
    // falling back to the shared prose markers, and tag once here: the
    // connect-retry path classifies this error directly without passing through
    // the stream chokepoint, so the tag has to be set at this source.
    let candidate = CompletionError::ProviderError(error.to_string());
    let transient = tungstenite_error_is_transient(&error) || legacy_prose_is_transient(&candidate);
    provider_error_with_optional_tag(error.to_string(), transient)
}

/// Typed transient classification for a tungstenite transport error: a reset
/// without a closing handshake, or an `ECONNRESET` I/O error. These are the
/// variants whose `Display` the legacy prose list matched, expressed structurally.
fn tungstenite_error_is_transient(error: &tokio_tungstenite::tungstenite::Error) -> bool {
    use tokio_tungstenite::tungstenite::Error;
    use tokio_tungstenite::tungstenite::error::ProtocolError;
    match error {
        Error::Protocol(ProtocolError::ResetWithoutClosingHandshake) => true,
        Error::Io(io) => io.kind() == std::io::ErrorKind::ConnectionReset,
        _ => false,
    }
}

fn openai_websocket_stream_error(error: CompletionError) -> CompletionError {
    // Canonicalize a bare connection reset (the parked-socket reuse path and the
    // user-facing message both expect this phrasing) and tag it transient.
    if is_openai_websocket_connection_reset(&error) {
        return provider_error_with_optional_tag(
            "OpenAI WebSocket connection reset before response.completed".to_string(),
            true,
        );
    }
    // Already classified at a structured/typed source — keep the verdict and the
    // message (idempotent if the chokepoint runs twice on the same error).
    if openai_error_message_is_tagged_transient(&error) {
        return error;
    }
    // Fallback for errors that reach the chokepoint untagged (e.g. our own
    // synthesized "closed before the turn finished" strings). Re-tag the raw
    // message so a `ProviderError` is not nested inside another `ProviderError`.
    if legacy_prose_is_transient(&error) {
        return provider_error_with_optional_tag(raw_completion_message(&error), true);
    }
    error
}

fn is_openai_websocket_connection_reset(error: &CompletionError) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("connection reset without closing handshake")
        || message.contains("connection reset by peer")
        || message.contains("openai websocket connection reset before response.completed")
}

/// Structured proxy error codes/types (`error.code` / `error.type` / top-level `code`) that
/// mark a transient condition worth retrying. [`openai_websocket_error_value_is_transient`]
/// matches these against the structured fields of a proxy `error` event, so the verdict
/// keys on the code rather than on free-form prose. `internal_server_error` is the upstream
/// 5xx the proxy reports when the codex backend returns a server error (often alongside a
/// `": EOF` transport tail; see the markers below).
const OPENAI_WEBSOCKET_RETRYABLE_PROXY_CODES: &[&str] = &[
    "websocket_connection_limit_reached",
    "internal_server_error",
];

/// Free-text transport/stream phrases that indicate the connection dropped before the turn
/// finished. Unlike the codes above, these conditions are only ever surfaced as human-readable
/// messages — by tungstenite, by our own synthesized errors, or by CLIProxyAPI when the
/// upstream Responses stream ends early — so substring matching is the only signal available.
const OPENAI_WEBSOCKET_RETRYABLE_TRANSIENT_MARKERS: &[&str] = &[
    "The OpenAI WebSocket connection closed before the turn finished",
    "The OpenAI WebSocket connection closed without a close reason",
    "An error occurred while processing the request.",
    "stream closed before response.completed",
    "disconnected before completion",
    // CLIProxyAPI surfaces an upstream POST that closed without a response as a Go `net/http`
    // error ending in `": EOF` (e.g. `Post "…/codex/responses": EOF`). The quote+colon+space
    // prefix keeps this from matching unrelated phrases such as `unexpected EOF`.
    "\": EOF",
];

/// Whether a proxy `error` event is a transient, retryable disconnect, judged from its
/// *structured* fields (`error.code` / `error.type` / top-level `code`) with a fallback to
/// the shared message markers for proxy drops that are only ever expressed as prose (e.g. the
/// Go `": EOF` tail).
fn openai_websocket_error_value_is_transient(value: &serde_json::Value) -> bool {
    let error = value.get("error");
    let codes = [
        error
            .and_then(|error| error.get("code"))
            .and_then(serde_json::Value::as_str),
        error
            .and_then(|error| error.get("type"))
            .and_then(serde_json::Value::as_str),
        value.get("code").and_then(serde_json::Value::as_str),
    ];
    if codes
        .into_iter()
        .flatten()
        .any(|code| OPENAI_WEBSOCKET_RETRYABLE_PROXY_CODES.contains(&code))
    {
        return true;
    }
    let message = openai_websocket_error_event_message(value);
    OPENAI_WEBSOCKET_RETRYABLE_TRANSIENT_MARKERS
        .iter()
        .any(|marker| message.contains(marker))
}

/// Legacy prose-based transient classification, retained as the fallback applied at the stream
/// chokepoint ([`openai_websocket_stream_error`]) for errors that arrive without a structured
/// tag, and by [`openai_websocket_provider_error`] alongside the typed variant check. The
/// retry *decision* points no longer call this — they read the owned tag.
fn legacy_prose_is_transient(error: &CompletionError) -> bool {
    let message = error.to_string();
    is_openai_websocket_connection_reset(error)
        || OPENAI_WEBSOCKET_RETRYABLE_PROXY_CODES
            .iter()
            .chain(OPENAI_WEBSOCKET_RETRYABLE_TRANSIENT_MARKERS)
            .any(|marker| message.contains(marker))
}

pub(crate) fn is_openai_websocket_transient_start_error(error: &CompletionError) -> bool {
    // Prefer the owned tag baked in at the error's structured/typed source (the
    // proxy `error` arm classifies from `error.code`; `openai_websocket_provider_error`
    // from the tungstenite variant), so a proxy rewording its prose cannot flip the
    // verdict. Fall back to the shared prose classifier for errors that reach a
    // decision point untagged — non-OpenAI providers, test doubles, or any path that
    // did not pass through the tagging sites — so classification never regresses.
    openai_error_message_is_tagged_transient(error) || legacy_prose_is_transient(error)
}

fn should_retry_openai_websocket_error(
    error: &CompletionError,
    yielded_assistant_item: bool,
) -> bool {
    !yielded_assistant_item && is_openai_websocket_transient_start_error(error)
}

pub(crate) fn openai_websocket_retry_delay(
    retry_count: usize,
    base: Duration,
    max: Duration,
) -> Duration {
    let factor = 1_u32
        .checked_shl(retry_count.saturating_sub(1) as u32)
        .unwrap_or(u32::MAX);
    base.saturating_mul(factor).min(max)
}

struct OpenAiWebSocketAccumulator {
    final_usage: ResponsesUsage,
    message_id: Option<String>,
    fallback_reasoning: Vec<OpenAiWebSocketRawChoice>,
    completed_reasoning_ids: HashSet<String>,
    tool_calls: Vec<OpenAiWebSocketRawChoice>,
    tool_call_internal_ids: HashMap<String, String>,
    pending_tool_calls: HashMap<String, PendingOpenAiToolCall>,
    pending_tool_call_order: Vec<String>,
    completed_tool_call_ids: HashSet<String>,
    active_response_id: Option<String>,
    complete_output_seen: bool,
    completed_response_seen: bool,
}

#[derive(Debug, Clone, Default)]
struct PendingOpenAiToolCall {
    name: Option<String>,
    call_id: Option<String>,
    arguments: Option<serde_json::Value>,
}

impl PendingOpenAiToolCall {
    fn is_complete(&self) -> bool {
        self.name.is_some() && self.call_id.is_some() && self.arguments.is_some()
    }
}

impl OpenAiWebSocketAccumulator {
    fn new() -> Self {
        Self {
            final_usage: empty_openai_usage(),
            message_id: None,
            fallback_reasoning: Vec::new(),
            completed_reasoning_ids: HashSet::new(),
            tool_calls: Vec::new(),
            tool_call_internal_ids: HashMap::new(),
            pending_tool_calls: HashMap::new(),
            pending_tool_call_order: Vec::new(),
            completed_tool_call_ids: HashSet::new(),
            active_response_id: None,
            complete_output_seen: false,
            completed_response_seen: false,
        }
    }

    fn decode_item_chunk(&mut self, item: ItemChunk) -> Vec<OpenAiWebSocketRawChoice> {
        let item_id = item.item_id;
        let mut choices = Vec::new();

        match item.data {
            ItemChunkKind::OutputItemAdded(output) => {
                if let Output::FunctionCall(func) = output.item {
                    self.record_tool_call_started(&func.id, &func.name, &func.call_id);
                    let internal_call_id = self.internal_call_id_for(&func.id);
                    choices.push(RawStreamingChoice::ToolCallDelta {
                        id: func.id,
                        internal_call_id,
                        content: ToolCallDeltaContent::Name(func.name),
                    });
                }
            }
            ItemChunkKind::OutputItemDone(output) => {
                self.push_output_item_done(output.item, &mut choices);
            }
            ItemChunkKind::ContentPartDone(part) => {
                if matches!(part.part, ContentPartChunkPart::OutputText { .. }) {
                    self.mark_completed_message_item(item_id);
                }
            }
            ItemChunkKind::OutputTextDelta(delta) | ItemChunkKind::RefusalDelta(delta) => {
                choices.push(RawStreamingChoice::Message(delta.delta));
            }
            ItemChunkKind::OutputTextDone(_) | ItemChunkKind::RefusalDone(_) => {
                self.mark_completed_message_item(item_id);
            }
            ItemChunkKind::FunctionCallArgsDelta(delta) => {
                if let Some(item_id) = item_id.as_deref() {
                    let internal_call_id = self.internal_call_id_for(item_id);
                    choices.push(RawStreamingChoice::ToolCallDelta {
                        id: item_id.to_string(),
                        internal_call_id,
                        content: ToolCallDeltaContent::Delta(delta.delta),
                    });
                }
            }
            ItemChunkKind::FunctionCallArgsDone(done) => {
                if let Some(item_id) = item_id.as_deref() {
                    self.record_tool_call_args_done(item_id, done.arguments);
                }
            }
            ItemChunkKind::ReasoningSummaryTextDelta(delta) => {
                choices.push(RawStreamingChoice::ReasoningDelta {
                    id: item_id,
                    reasoning: delta.delta,
                });
            }
            _ => {}
        }

        choices
    }

    fn mark_completed_message_item(&mut self, item_id: Option<String>) {
        self.complete_output_seen = true;
        if self.message_id.is_none() {
            self.message_id = item_id;
        }
    }

    fn record_response_value(
        &mut self,
        kind: ResponseChunkKind,
        response: &serde_json::Value,
    ) -> std::result::Result<Option<CompletionError>, CompletionError> {
        if let Some(response_id) = openai_websocket_response_id(response) {
            self.active_response_id = Some(response_id);
        }
        match kind {
            ResponseChunkKind::ResponseCompleted => {
                self.record_completed_response_value(response);
                Ok(None)
            }
            ResponseChunkKind::ResponseFailed => Ok(Some(CompletionError::ProviderError(
                openai_response_error_message_value(
                    response,
                    "OpenAI WebSocket returned a failed response",
                ),
            ))),
            ResponseChunkKind::ResponseIncomplete => Ok(Some(CompletionError::ProviderError(
                openai_incomplete_response_message_value(response),
            ))),
            ResponseChunkKind::ResponseCreated | ResponseChunkKind::ResponseInProgress => Ok(None),
        }
    }

    fn record_done_response_value(
        &mut self,
        response: &serde_json::Value,
    ) -> std::result::Result<Option<CompletionError>, CompletionError> {
        if let Some(response_id) = openai_websocket_response_id(response) {
            self.active_response_id = Some(response_id);
        }
        // The local proxy sometimes emits `response.done` without a `status`
        // field. Treat a missing status as a completed turn rather than failing
        // the whole response — genuine failures surface earlier via
        // `response.failed` / `response.incomplete`, which carry an explicit
        // status. This matches the leniency applied to the other lifecycle
        // events the proxy emits.
        let status = match response.get("status").cloned() {
            Some(status_value) => serde_json::from_value::<ResponseStatus>(status_value)?,
            None => ResponseStatus::Completed,
        };
        match status {
            ResponseStatus::Completed => {
                self.record_completed_response_value(response);
                Ok(None)
            }
            ResponseStatus::Failed => Ok(Some(CompletionError::ProviderError(
                openai_response_error_message_value(
                    response,
                    "OpenAI WebSocket returned a failed response",
                ),
            ))),
            ResponseStatus::Incomplete => Ok(Some(CompletionError::ProviderError(
                openai_incomplete_response_message_value(response),
            ))),
            status => Ok(Some(CompletionError::ProviderError(format!(
                "OpenAI WebSocket response ended with status {status:?}"
            )))),
        }
    }

    fn record_completed_response_value(&mut self, response: &serde_json::Value) {
        self.completed_response_seen = true;
        if let Some(usage) = response
            .get("usage")
            .cloned()
            .and_then(|usage| serde_json::from_value::<ResponsesUsage>(usage).ok())
        {
            self.final_usage = usage;
        }
        self.record_terminal_output_items(response);
    }

    fn record_terminal_output_items(&mut self, response: &serde_json::Value) {
        let Some(output) = response.get("output").and_then(serde_json::Value::as_array) else {
            return;
        };

        for item in output {
            match serde_json::from_value::<Output>(item.clone()) {
                Ok(output) => self.record_terminal_output_item(output),
                Err(error) => {
                    tracing::debug!(
                        error = %error,
                        "skipping OpenAI WebSocket terminal output item with unsupported payload shape"
                    );
                }
            }
        }
    }

    fn record_terminal_output_item(&mut self, item: Output) {
        match item {
            Output::Reasoning {
                id,
                summary,
                encrypted_content,
                ..
            } => self.push_fallback_reasoning_output(id, summary, encrypted_content),
            Output::FunctionCall(func) => {
                if !self.completed_tool_call_ids.contains(&func.id) {
                    self.complete_output_seen = true;
                    self.completed_tool_call_ids.insert(func.id.clone());
                    let internal_call_id = self.internal_call_id_for(&func.id);
                    let tool_call = RawStreamingToolCall::new(func.id, func.name, func.arguments)
                        .with_internal_call_id(internal_call_id)
                        .with_call_id(func.call_id);
                    self.tool_calls
                        .push(RawStreamingChoice::ToolCall(tool_call));
                }
            }
            Output::Message(message) => {
                self.complete_output_seen = true;
                if self.message_id.is_none() {
                    self.message_id = Some(message.id);
                }
            }
            Output::Unknown => {}
        }
    }

    fn push_fallback_reasoning_output(
        &mut self,
        id: String,
        summary: Vec<ReasoningSummary>,
        encrypted_content: Option<String>,
    ) {
        if self.completed_reasoning_ids.insert(id.clone()) {
            push_reasoning_choices(&mut self.fallback_reasoning, id, summary, encrypted_content);
        }
    }

    fn push_reasoning_output_immediate(
        &mut self,
        id: String,
        summary: Vec<ReasoningSummary>,
        encrypted_content: Option<String>,
        immediate: &mut Vec<OpenAiWebSocketRawChoice>,
    ) {
        if self.completed_reasoning_ids.insert(id.clone()) {
            push_reasoning_choices(immediate, id, summary, encrypted_content);
        }
    }

    fn push_output_item_done(
        &mut self,
        item: Output,
        immediate: &mut Vec<OpenAiWebSocketRawChoice>,
    ) {
        match item {
            Output::FunctionCall(func) => {
                self.complete_output_seen = true;
                self.completed_tool_call_ids.insert(func.id.clone());
                let internal_call_id = self.internal_call_id_for(&func.id);
                let tool_call = RawStreamingToolCall::new(func.id, func.name, func.arguments)
                    .with_internal_call_id(internal_call_id)
                    .with_call_id(func.call_id);
                self.tool_calls
                    .push(RawStreamingChoice::ToolCall(tool_call));
            }
            Output::Reasoning {
                id,
                summary,
                encrypted_content,
                ..
            } => {
                self.push_reasoning_output_immediate(id, summary, encrypted_content, immediate);
            }
            Output::Message(message) => {
                self.complete_output_seen = true;
                self.message_id = Some(message.id);
            }
            Output::Unknown => {}
        }
    }

    fn can_finish_after_disconnect(&self) -> bool {
        if self.completed_response_seen {
            return true;
        }
        // A handshake-less reset is only graceful when nothing is mid-flight.
        // A completed text/output item does not mean the model is done — it may
        // have started emitting a tool call when the connection dropped.
        // Finishing here would silently drop that tool call and end the turn
        // with only the (often preamble) text, so hold out for a terminal
        // response in that case rather than truncating.
        if self.has_incomplete_pending_tool_call() {
            return false;
        }
        self.complete_output_seen || self.has_completed_fallback_tool_call()
    }

    fn internal_call_id_for(&mut self, tool_call_id: &str) -> String {
        self.tool_call_internal_ids
            .entry(tool_call_id.to_owned())
            .or_insert_with(|| uuid::Uuid::new_v4().to_string())
            .clone()
    }

    fn record_tool_call_started(&mut self, id: &str, name: &str, call_id: &str) {
        if !self.pending_tool_calls.contains_key(id) {
            self.pending_tool_call_order.push(id.to_string());
        }
        let pending = self.pending_tool_calls.entry(id.to_string()).or_default();
        pending.name = Some(name.to_string());
        pending.call_id = Some(call_id.to_string());
    }

    fn record_tool_call_args_done(&mut self, id: &str, arguments: serde_json::Value) {
        if !self.pending_tool_calls.contains_key(id) {
            self.pending_tool_call_order.push(id.to_string());
        }
        let pending = self.pending_tool_calls.entry(id.to_string()).or_default();
        pending.arguments = Some(arguments);
        if pending.is_complete() {
            self.complete_output_seen = true;
        }
    }

    fn has_completed_fallback_tool_call(&self) -> bool {
        self.pending_tool_calls.iter().any(|(id, tool_call)| {
            !self.completed_tool_call_ids.contains(id) && tool_call.is_complete()
        })
    }

    fn has_incomplete_pending_tool_call(&self) -> bool {
        self.pending_tool_calls.iter().any(|(id, tool_call)| {
            !self.completed_tool_call_ids.contains(id) && !tool_call.is_complete()
        })
    }

    fn fallback_tool_call(&mut self, id: &str) -> Option<RawStreamingToolCall> {
        let pending = self.pending_tool_calls.get(id)?;
        let name = pending.name.clone()?;
        let call_id = pending.call_id.clone()?;
        let arguments = pending.arguments.clone()?;
        let internal_call_id = self.internal_call_id_for(id);
        Some(
            RawStreamingToolCall::new(id.to_string(), name, arguments)
                .with_internal_call_id(internal_call_id)
                .with_call_id(call_id),
        )
    }

    fn finish(mut self) -> Vec<OpenAiWebSocketRawChoice> {
        let mut choices = Vec::new();
        for id in self.pending_tool_call_order.clone() {
            if self.completed_tool_call_ids.contains(&id) {
                continue;
            }
            if let Some(tool_call) = self.fallback_tool_call(&id) {
                self.completed_tool_call_ids.insert(id);
                self.tool_calls
                    .push(RawStreamingChoice::ToolCall(tool_call));
            }
        }
        choices.append(&mut self.fallback_reasoning);
        if let Some(message_id) = self.message_id.take() {
            choices.push(RawStreamingChoice::MessageId(message_id));
        }
        choices.append(&mut self.tool_calls);
        choices.push(RawStreamingChoice::FinalResponse(
            OpenAiStreamingCompletionResponse {
                usage: self.final_usage,
            },
        ));
        choices
    }
}

fn push_reasoning_choices(
    choices: &mut Vec<OpenAiWebSocketRawChoice>,
    id: String,
    summary: Vec<ReasoningSummary>,
    encrypted_content: Option<String>,
) {
    for summary in summary {
        let ReasoningSummary::SummaryText { text } = summary;
        choices.push(RawStreamingChoice::Reasoning {
            id: Some(id.clone()),
            content: ReasoningContent::Summary(text),
        });
    }
    if let Some(encrypted_content) = encrypted_content {
        choices.push(RawStreamingChoice::Reasoning {
            id: Some(id),
            content: ReasoningContent::Encrypted(encrypted_content),
        });
    }
}

fn empty_openai_usage() -> ResponsesUsage {
    ResponsesUsage {
        input_tokens: 0,
        input_tokens_details: None,
        output_tokens: 0,
        output_tokens_details: None,
        total_tokens: 0,
    }
}

fn openai_response_error_message_value(response: &serde_json::Value, fallback: &str) -> String {
    let error = response.get("error");
    let code = error
        .and_then(|error| error.get("code"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(serde_json::Value::as_str);
    match (code.is_empty(), message) {
        (_, None) => fallback.to_string(),
        (true, Some(message)) => message.to_string(),
        (false, Some(message)) => format!("{code}: {message}"),
    }
}

fn openai_incomplete_response_message_value(response: &serde_json::Value) -> String {
    let reason = response
        .get("incomplete_details")
        .and_then(|details| details.get("reason"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown reason");
    format!("OpenAI WebSocket response was incomplete: {reason}")
}

async fn call_agent_tool<M>(agent: &Arc<Agent<M>>, tool_name: &str, args: &str) -> Result<String>
where
    M: rig::completion::CompletionModel,
{
    Ok(agent.tool_server_handle.call_tool(tool_name, args).await?)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use anyhow::{Context, Result};
    use futures_util::{SinkExt, StreamExt};
    use rig::{
        OneOrMany,
        agent::AgentBuilder,
        client::CompletionClient,
        completion::{
            CompletionError, CompletionModel, CompletionRequest, CompletionResponse, Usage,
        },
        message::Message,
        providers::openai::responses_api::{
            Output, OutputFunctionCall, ToolStatus,
            streaming::{
                ArgsTextChunk, DeltaTextChunk, DeltaTextChunkWithItemId, ItemChunk, ItemChunkKind,
                OutputTextChunk, StreamingItemDoneOutput, SummaryTextChunk,
            },
        },
        streaming::{RawStreamingChoice, StreamingCompletionResponse, ToolCallDeltaContent},
    };
    use serde::{Deserialize, Serialize};
    use tokio::{
        net::TcpListener,
        sync::{Mutex, RwLock},
    };
    use tokio_tungstenite::{accept_async, tungstenite::Message as TestWebSocketMessage};

    use cazean_config::{Config, OpenAiConfig};

    use super::{
        build_agent, compose_session_preamble, openai_additional_params_json,
        openai_web_search_tool_value,
    };
    use crate::{
        agent::{
            AgentControl, SystemPromptKind, plan_mode::plan_mode_tool_names, plan_mode_instructions,
        },
        environment::EnvironmentContext,
    };

    #[derive(Clone, Debug)]
    struct DummyModel;

    #[derive(Clone, Debug, Deserialize, Serialize)]
    struct DummyStreamingResponse;

    impl rig::completion::GetTokenUsage for DummyStreamingResponse {
        fn token_usage(&self) -> Option<Usage> {
            None
        }
    }

    #[allow(refining_impl_trait)]
    impl CompletionModel for DummyModel {
        type Response = serde_json::Value;
        type StreamingResponse = DummyStreamingResponse;
        type Client = ();

        fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
            Self
        }

        async fn completion(
            &self,
            _request: CompletionRequest,
        ) -> std::result::Result<CompletionResponse<Self::Response>, CompletionError> {
            Err(CompletionError::ProviderError(
                "dummy completion model".to_string(),
            ))
        }

        async fn stream(
            &self,
            _request: CompletionRequest,
        ) -> std::result::Result<
            StreamingCompletionResponse<Self::StreamingResponse>,
            CompletionError,
        > {
            Err(CompletionError::ProviderError(
                "dummy completion model".to_string(),
            ))
        }
    }

    fn environment_context() -> EnvironmentContext {
        EnvironmentContext {
            working_directory: "/workspace/cazean".to_string(),
            is_git_repo: "yes".to_string(),
            platform: "macos".to_string(),
            os_version: "25.0.0".to_string(),
            shell: "/bin/zsh".to_string(),
            rg_available: "yes".to_string(),
            fd_available: "yes".to_string(),
            eza_available: "yes".to_string(),
        }
    }

    fn stub_ask_user_client() -> tools::AskUserClient {
        tools::AskUserClient::new(
            |_params| async {
                Ok(app_server_protocol::AskUserQuestionResponse {
                    answers: Vec::new(),
                })
            },
            |_thread_id| async {},
        )
    }

    #[derive(Clone, Copy)]
    enum OpenAiWebSocketTestServerMode {
        KeepAlive,
        KeepAliveWithTrailingDone,
        KeepAliveWithTelemetryThenTrailingDone,
        CloseAfterResponse,
        AbortAfterResponse,
    }

    async fn openai_websocket_test_model(base_url: &str) -> Result<Arc<super::OpenAiSessionModel>> {
        let client = rig::providers::openai::Client::builder()
            .api_key("test")
            .base_url(base_url)
            .build()?;
        let agent = client.agent("gpt-test").build();
        Ok(Arc::new(super::OpenAiSessionModel {
            agent: Arc::new(agent),
            client,
            model: "gpt-test".to_string(),
            websocket: cazean_config::WebSocketConfig::default(),
            session_ws: Mutex::new(None),
        }))
    }

    fn openai_websocket_test_request(input: &str) -> Result<CompletionRequest> {
        Ok(CompletionRequest {
            model: None,
            preamble: None,
            chat_history: OneOrMany::many(vec![Message::user(input)]).context("chat history")?,
            documents: Vec::new(),
            tools: Vec::new(),
            temperature: None,
            max_tokens: None,
            tool_choice: None,
            additional_params: None,
            output_schema: None,
        })
    }

    fn openai_websocket_completed_frame(response_id: &str) -> String {
        openai_websocket_completed_frame_with_total_tokens(response_id, 2)
    }

    fn openai_websocket_created_frame(response_id: &str) -> String {
        serde_json::json!({
            "type": "response.created",
            "response": {
                "id": response_id,
                "status": "in_progress"
            }
        })
        .to_string()
    }

    fn openai_websocket_completed_frame_with_total_tokens(
        response_id: &str,
        total_tokens: u64,
    ) -> String {
        serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": response_id,
                "status": "completed",
                "usage": {
                    "input_tokens": 1,
                    "output_tokens": 1,
                    "output_tokens_details": {"reasoning_tokens": 0},
                    "total_tokens": total_tokens
                }
            }
        })
        .to_string()
    }

    fn openai_websocket_done_frame_with_total_tokens(
        response_id: &str,
        total_tokens: u64,
    ) -> String {
        serde_json::json!({
            "type": "response.done",
            "response": {
                "id": response_id,
                "status": "completed",
                "usage": {
                    "input_tokens": 1,
                    "output_tokens": 1,
                    "output_tokens_details": {"reasoning_tokens": 0},
                    "total_tokens": total_tokens
                }
            }
        })
        .to_string()
    }

    fn openai_websocket_failed_frame() -> String {
        serde_json::json!({
            "type": "response.failed",
            "response": {
                "id": "resp_failed",
                "status": "failed",
                "error": {
                    "code": "server_error",
                    "message": "test failure"
                }
            }
        })
        .to_string()
    }

    fn openai_websocket_incomplete_frame() -> String {
        serde_json::json!({
            "type": "response.incomplete",
            "response": {
                "id": "resp_incomplete",
                "status": "incomplete",
                "incomplete_details": {"reason": "max_output_tokens"}
            }
        })
        .to_string()
    }

    fn openai_websocket_text_delta_frame() -> String {
        serde_json::json!({
            "type": "response.output_text.delta",
            "item_id": "msg_1",
            "output_index": 0,
            "content_index": 0,
            "delta": "hello"
        })
        .to_string()
    }

    async fn start_openai_websocket_sequence_server(
        mode: OpenAiWebSocketTestServerMode,
    ) -> Result<(String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let handshakes = Arc::new(AtomicUsize::new(0));
        let server_handshakes = Arc::clone(&handshakes);
        let turns = Arc::new(AtomicUsize::new(0));
        let handle = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                server_handshakes.fetch_add(1, Ordering::SeqCst);
                let server_turns = Arc::clone(&turns);
                let connection_mode = mode;
                let _connection_task = tokio::spawn(async move {
                    let Ok(mut socket) = accept_async(stream).await else {
                        return;
                    };
                    while let Some(message) = socket.next().await {
                        let Ok(message) = message else {
                            break;
                        };
                        if !(message.is_text() || message.is_binary()) {
                            continue;
                        }
                        let turn_index = server_turns.fetch_add(1, Ordering::SeqCst) + 1;
                        let response_id = format!("resp_{turn_index}");
                        let total_tokens = u64::try_from(turn_index)
                            .map(|index| index * 100)
                            .unwrap_or(u64::MAX);
                        let frame = openai_websocket_completed_frame_with_total_tokens(
                            &response_id,
                            total_tokens,
                        );
                        if socket
                            .send(TestWebSocketMessage::text(frame))
                            .await
                            .is_err()
                        {
                            break;
                        }
                        if matches!(
                            connection_mode,
                            OpenAiWebSocketTestServerMode::KeepAliveWithTrailingDone
                                | OpenAiWebSocketTestServerMode::KeepAliveWithTelemetryThenTrailingDone
                        ) {
                            if matches!(
                                connection_mode,
                                OpenAiWebSocketTestServerMode::KeepAliveWithTelemetryThenTrailingDone
                            ) {
                                // Buffer a non-`response.done` telemetry frame ahead of the
                                // trailing done so a reused turn must skip past it without
                                // disarming the stale-done guard early.
                                let telemetry_frame =
                                    r#"{"type":"codex.rate_limits","rate_limits":{"allowed":true}}"#
                                        .to_string();
                                if socket
                                    .send(TestWebSocketMessage::text(telemetry_frame))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            let done_frame = openai_websocket_done_frame_with_total_tokens(
                                &response_id,
                                total_tokens + 1,
                            );
                            if socket
                                .send(TestWebSocketMessage::text(done_frame))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        if matches!(
                            connection_mode,
                            OpenAiWebSocketTestServerMode::CloseAfterResponse
                        ) {
                            let _ = socket.close(None).await;
                            break;
                        }
                        if matches!(
                            connection_mode,
                            OpenAiWebSocketTestServerMode::AbortAfterResponse
                        ) {
                            #[allow(deprecated)]
                            if let Err(error) = socket.get_ref().set_linger(Some(Duration::ZERO)) {
                                tracing::debug!(
                                    error = %error,
                                    "failed to configure abortive close for test socket"
                                );
                            }
                            break;
                        }
                    }
                });
            }
        });

        Ok((format!("http://{address}/v1"), handshakes, handle))
    }

    async fn start_openai_websocket_single_frame_server(
        frame: String,
        close_after_frame: bool,
    ) -> Result<(String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let handshakes = Arc::new(AtomicUsize::new(0));
        let server_handshakes = Arc::clone(&handshakes);
        let handle = tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            server_handshakes.fetch_add(1, Ordering::SeqCst);
            let Ok(mut socket) = accept_async(stream).await else {
                return;
            };
            while let Some(message) = socket.next().await {
                let Ok(message) = message else {
                    break;
                };
                if !(message.is_text() || message.is_binary()) {
                    continue;
                }
                if socket
                    .send(TestWebSocketMessage::text(frame.clone()))
                    .await
                    .is_err()
                {
                    break;
                }
                if close_after_frame {
                    let _ = socket.close(None).await;
                } else {
                    std::future::pending::<()>().await;
                }
                break;
            }
        });

        Ok((format!("http://{address}/v1"), handshakes, handle))
    }

    async fn drain_openai_websocket_stream(
        mut stream: StreamingCompletionResponse<super::OpenAiStreamingCompletionResponse>,
    ) -> Result<()> {
        loop {
            let item = tokio::time::timeout(Duration::from_secs(5), stream.next())
                .await
                .context("timed out waiting for OpenAI WebSocket stream item")?;
            let Some(item) = item else {
                break;
            };
            item?;
        }
        Ok(())
    }

    async fn openai_websocket_final_total_tokens(
        mut stream: StreamingCompletionResponse<super::OpenAiStreamingCompletionResponse>,
    ) -> Result<u64> {
        loop {
            let item = tokio::time::timeout(Duration::from_secs(5), stream.next())
                .await
                .context("timed out waiting for OpenAI WebSocket stream item")?;
            let Some(item) = item else {
                break;
            };
            item?;
        }
        stream
            .response
            .as_ref()
            .map(|response| response.usage.total_tokens)
            .context("OpenAI WebSocket stream did not emit a final response")
    }

    async fn drain_session_completion_stream(
        mut stream: super::SessionCompletionStream,
    ) -> Result<()> {
        loop {
            let item = tokio::time::timeout(Duration::from_secs(5), stream.next())
                .await
                .context("timed out waiting for session completion stream item")?;
            let Some(item) = item else {
                break;
            };
            item?;
        }
        Ok(())
    }

    #[test]
    fn default_session_preamble_uses_markdown_system_prompt() {
        let preamble =
            compose_session_preamble(SystemPromptKind::Root, None, &environment_context(), false);

        assert!(preamble.starts_with("# Cazean System Prompt"));
        assert!(preamble.contains("You are Cazean"));
        assert!(preamble.contains("Working directory: /workspace/cazean"));
        assert!(preamble.contains("Shell: /bin/zsh"));
        assert!(preamble.contains("rg available: yes"));
        assert!(preamble.contains("fd available: yes"));
        assert!(preamble.contains("eza available: yes"));
        assert!(!preamble.contains("${"));
        assert!(!preamble.contains("You are in PLAN MODE."));
    }

    #[test]
    fn configured_preamble_replaces_markdown_system_prompt() {
        let preamble = compose_session_preamble(
            SystemPromptKind::Root,
            Some("Custom prompt for ${shell}.".to_string()),
            &environment_context(),
            false,
        );

        assert_eq!(preamble, "Custom prompt for /bin/zsh.");
    }

    #[test]
    fn default_subagent_preamble_uses_builtin_prompt_and_ignores_env_override() {
        let preamble = compose_session_preamble(
            SystemPromptKind::DefaultSubagent,
            Some("Root override.".to_string()),
            &environment_context(),
            false,
        );

        assert!(preamble.starts_with("# Cazean Default Subagent Prompt"));
        assert!(preamble.contains("delegated task from a parent agent"));
        assert!(!preamble.contains("Root override."));
    }

    #[test]
    fn explore_preamble_uses_documented_explorer_prompt() {
        let preamble = compose_session_preamble(
            SystemPromptKind::Explore,
            Some("Root override.".to_string()),
            &environment_context(),
            false,
        );

        assert!(preamble.starts_with("# Cazean Explorer Subagent Prompt"));
        assert!(preamble.contains("Do not create, edit, delete"));
        assert!(preamble.contains("run_command"));
        assert!(!preamble.contains("Root override."));
    }

    #[test]
    fn plan_mode_preamble_layers_on_selected_prompt() {
        let preamble = compose_session_preamble(
            SystemPromptKind::Root,
            Some("Base prompt.\n".to_string()),
            &environment_context(),
            true,
        );

        assert_eq!(
            preamble,
            format!("Base prompt.\n\n{}", plan_mode_instructions())
        );
    }

    #[test]
    fn web_search_tool_value_has_expected_wire_shape() {
        let value = openai_web_search_tool_value();
        assert_eq!(value["type"], "web_search");
        assert_eq!(value["external_web_access"], true);
    }

    #[test]
    fn additional_params_includes_web_search_when_enabled() {
        let params = openai_additional_params_json(&OpenAiConfig::default(), true);
        let Some(tools) = params.get("tools").and_then(|tools| tools.as_array()) else {
            panic!("tools array should be present when web search is enabled: {params}");
        };
        assert!(
            tools
                .iter()
                .any(|tool| tool.get("type").and_then(|kind| kind.as_str()) == Some("web_search")),
            "web_search tool should be present: {params}"
        );
        // Reasoning config is preserved alongside the injected tool.
        assert!(params.get("reasoning").is_some());
    }

    #[test]
    fn additional_params_omits_web_search_when_disabled() {
        let params = openai_additional_params_json(&OpenAiConfig::default(), false);
        assert!(
            params.get("tools").is_none(),
            "no tools key when web search is disabled: {params}"
        );
        assert!(params.get("reasoning").is_some());
    }

    #[tokio::test]
    async fn build_agent_registers_root_tools() -> Result<()> {
        let workspace = tempfile::TempDir::new()?;
        let agent = build_agent(
            AgentBuilder::new(DummyModel),
            workspace.path().to_path_buf(),
            cazean_protocol::ThreadId::new(),
            None,
            Arc::new(RwLock::new(None)),
            SystemPromptKind::Root,
            AgentControl::new(),
            false,
            &Config::default(),
        );

        let tool_names = agent
            .tool_server_handle
            .get_tool_defs(None)
            .await?
            .into_iter()
            .map(|definition| definition.name)
            .collect::<HashSet<_>>();

        assert!(tool_names.contains("spawn_agent"));
        assert!(!tool_names.contains("explore"));
        assert!(tool_names.contains("delete"));
        assert!(tool_names.contains("todo_write"));
        assert!(!tool_names.contains("list_dir"));
        assert!(!tool_names.contains("send_message"));
        assert!(!tool_names.contains("list_agents"));
        assert!(!tool_names.contains("close_agent"));
        // Plan-mode-only tools must not be present in the default agent.
        assert!(!tool_names.contains("plan_write"));
        assert!(!tool_names.contains("exit_plan_mode"));
        Ok(())
    }

    #[tokio::test]
    async fn build_agent_for_default_child_keeps_normal_tools() -> Result<()> {
        let workspace = tempfile::TempDir::new()?;
        let agent = build_agent(
            AgentBuilder::new(DummyModel),
            workspace.path().to_path_buf(),
            cazean_protocol::ThreadId::new(),
            None,
            Arc::new(RwLock::new(None)),
            SystemPromptKind::DefaultSubagent,
            AgentControl::new(),
            false,
            &Config::default(),
        );

        let tool_names = agent
            .tool_server_handle
            .get_tool_defs(None)
            .await?
            .into_iter()
            .map(|definition| definition.name)
            .collect::<HashSet<_>>();

        assert!(tool_names.contains("read"));
        assert!(tool_names.contains("run_command"));
        assert!(tool_names.contains("spawn_agent"));
        assert!(tool_names.contains("delete"));
        assert!(tool_names.contains("edit"));
        assert!(tool_names.contains("write"));
        assert!(tool_names.contains("todo_write"));
        assert!(!tool_names.contains("apply_patch"));
        assert!(!tool_names.contains("explore"));
        Ok(())
    }

    #[tokio::test]
    async fn build_agent_in_plan_mode_swaps_mutating_for_planning_tools() -> Result<()> {
        let workspace = tempfile::TempDir::new()?;
        let agent = build_agent(
            AgentBuilder::new(DummyModel),
            workspace.path().to_path_buf(),
            cazean_protocol::ThreadId::new(),
            Some(stub_ask_user_client()),
            Arc::new(RwLock::new(None)),
            SystemPromptKind::Root,
            AgentControl::new(),
            true,
            &Config::default(),
        );

        let tool_names = agent
            .tool_server_handle
            .get_tool_defs(None)
            .await?
            .into_iter()
            .map(|definition| definition.name)
            .collect::<HashSet<_>>();

        // The registered tool set must match the advertised plan-mode list
        // exactly — it is the single source of truth for the instructions.
        let expected = plan_mode_tool_names()
            .map(str::to_string)
            .collect::<HashSet<_>>();
        assert_eq!(tool_names, expected);
        Ok(())
    }

    #[tokio::test]
    async fn build_agent_for_explore_child_is_read_only() -> Result<()> {
        let workspace = tempfile::TempDir::new()?;
        let agent = build_agent(
            AgentBuilder::new(DummyModel),
            workspace.path().to_path_buf(),
            cazean_protocol::ThreadId::new(),
            Some(stub_ask_user_client()),
            Arc::new(RwLock::new(None)),
            SystemPromptKind::Explore,
            AgentControl::new(),
            false,
            &Config::default(),
        );

        let tool_names = agent
            .tool_server_handle
            .get_tool_defs(None)
            .await?
            .into_iter()
            .map(|definition| definition.name)
            .collect::<HashSet<_>>();

        assert_eq!(
            tool_names,
            HashSet::from(["read".to_string(), "run_command".to_string()])
        );
        Ok(())
    }

    #[test]
    fn openai_websocket_payload_moves_system_inputs_to_instructions() -> Result<()> {
        let request = CompletionRequest {
            model: None,
            preamble: Some("Use concise answers.".to_string()),
            chat_history: OneOrMany::many(vec![
                Message::system("Follow repo instructions."),
                Message::user("Inspect the workspace."),
            ])
            .context("chat history")?,
            documents: Vec::new(),
            tools: Vec::new(),
            temperature: None,
            max_tokens: None,
            tool_choice: None,
            additional_params: None,
            output_schema: None,
        };

        let payload = super::openai_websocket_create_payload("gpt-test", request)?;
        let value: serde_json::Value = serde_json::from_str(&payload)?;

        assert_eq!(
            value
                .get("instructions")
                .and_then(serde_json::Value::as_str),
            Some("Use concise answers.\n\nFollow repo instructions.")
        );
        let input = value
            .get("input")
            .and_then(serde_json::Value::as_array)
            .context("input array")?;
        assert!(input.iter().all(|item| {
            !item
                .get("role")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|role| role == "system" || role == "developer")
        }));
        assert!(input.iter().any(|item| {
            item.get("role")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|role| role == "user")
        }));
        assert!(!payload.contains("System instructions:"));
        Ok(())
    }

    #[test]
    fn openai_websocket_accumulator_preserves_tool_delta_identity() -> Result<()> {
        let mut accumulator = super::OpenAiWebSocketAccumulator::new();

        let name_delta = accumulator.decode_item_chunk(ItemChunk {
            item_id: Some("fc_1".to_string()),
            output_index: 0,
            data: ItemChunkKind::OutputItemAdded(StreamingItemDoneOutput {
                sequence_number: 1,
                item: Output::FunctionCall(OutputFunctionCall {
                    id: "fc_1".to_string(),
                    arguments: serde_json::json!({}),
                    call_id: "call_1".to_string(),
                    name: "read".to_string(),
                    status: ToolStatus::InProgress,
                }),
            }),
        });

        let internal_call_id = match name_delta.as_slice() {
            [
                RawStreamingChoice::ToolCallDelta {
                    id,
                    internal_call_id,
                    content: ToolCallDeltaContent::Name(name),
                },
            ] => {
                assert_eq!(id, "fc_1");
                assert_eq!(name, "read");
                internal_call_id.clone()
            }
            other => panic!("unexpected name delta: {other:?}"),
        };

        let args_delta = accumulator.decode_item_chunk(ItemChunk {
            item_id: Some("fc_1".to_string()),
            output_index: 0,
            data: ItemChunkKind::FunctionCallArgsDelta(DeltaTextChunkWithItemId {
                content_index: Some(0),
                sequence_number: 2,
                delta: "{\"path\":\"src/lib.rs\"}".to_string(),
            }),
        });

        match args_delta.as_slice() {
            [
                RawStreamingChoice::ToolCallDelta {
                    id,
                    internal_call_id: observed,
                    content: ToolCallDeltaContent::Delta(delta),
                },
            ] => {
                assert_eq!(id, "fc_1");
                assert_eq!(observed, &internal_call_id);
                assert_eq!(delta, "{\"path\":\"src/lib.rs\"}");
            }
            other => panic!("unexpected args delta: {other:?}"),
        }

        assert!(
            accumulator
                .decode_item_chunk(ItemChunk {
                    item_id: Some("fc_1".to_string()),
                    output_index: 0,
                    data: ItemChunkKind::OutputItemDone(StreamingItemDoneOutput {
                        sequence_number: 3,
                        item: Output::FunctionCall(OutputFunctionCall {
                            id: "fc_1".to_string(),
                            arguments: serde_json::json!({"path": "src/lib.rs"}),
                            call_id: "call_1".to_string(),
                            name: "read".to_string(),
                            status: ToolStatus::Completed,
                        }),
                    }),
                })
                .is_empty()
        );
        assert!(accumulator.can_finish_after_disconnect());

        let finished = accumulator.finish();
        let tool_call = finished.iter().find_map(|choice| match choice {
            RawStreamingChoice::ToolCall(tool_call) => Some(tool_call),
            _ => None,
        });
        let tool_call = tool_call.context("completed tool call should be emitted at finish")?;
        assert_eq!(tool_call.id, "fc_1");
        assert_eq!(tool_call.internal_call_id, internal_call_id);
        assert_eq!(tool_call.call_id.as_deref(), Some("call_1"));
        assert_eq!(tool_call.name, "read");
        assert_eq!(
            tool_call.arguments,
            serde_json::json!({"path": "src/lib.rs"})
        );
        Ok(())
    }

    #[test]
    fn openai_websocket_reasoning_delta_uses_provider_item_id() {
        let mut accumulator = super::OpenAiWebSocketAccumulator::new();

        let choices = accumulator.decode_item_chunk(ItemChunk {
            item_id: Some("rs_1".to_string()),
            output_index: 0,
            data: ItemChunkKind::ReasoningSummaryTextDelta(SummaryTextChunk {
                summary_index: 0,
                sequence_number: 1,
                delta: "thinking".to_string(),
            }),
        });

        match choices.as_slice() {
            [RawStreamingChoice::ReasoningDelta { id, reasoning }] => {
                assert_eq!(id.as_deref(), Some("rs_1"));
                assert_eq!(reasoning, "thinking");
            }
            other => panic!("unexpected reasoning delta: {other:?}"),
        }
    }

    #[test]
    fn openai_websocket_parser_skips_provider_telemetry_events() -> Result<()> {
        let mut accumulator = super::OpenAiWebSocketAccumulator::new();

        let outcome = super::parse_openai_websocket_payload(
            r#"{"type":"codex.rate_limits","rate_limits":{"allowed":true}}"#,
            &mut accumulator,
        )?;

        assert!(outcome.choices.is_empty());
        assert!(!outcome.terminal);
        assert!(!accumulator.can_finish_after_disconnect());
        Ok(())
    }

    #[test]
    fn openai_websocket_parser_tolerates_reasoning_text_done_shape() -> Result<()> {
        let mut accumulator = super::OpenAiWebSocketAccumulator::new();

        let outcome = super::parse_openai_websocket_payload(
            r#"{"type":"response.reasoning_summary_text.done","item_id":"rs_1","output_index":0,"summary_index":0,"sequence_number":1,"text":"done"}"#,
            &mut accumulator,
        )?;

        assert!(outcome.choices.is_empty());
        assert!(!outcome.terminal);
        assert!(!accumulator.can_finish_after_disconnect());
        Ok(())
    }

    #[test]
    fn openai_websocket_parser_tolerates_completed_response_without_output() -> Result<()> {
        let mut accumulator = super::OpenAiWebSocketAccumulator::new();

        let outcome = super::parse_openai_websocket_payload(
            r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":1,"output_tokens":2,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":3}}}"#,
            &mut accumulator,
        )?;

        assert!(outcome.choices.is_empty());
        assert!(outcome.terminal);
        assert!(accumulator.can_finish_after_disconnect());
        let finished = accumulator.finish();
        assert!(matches!(
            finished.last(),
            Some(RawStreamingChoice::FinalResponse(response)) if response.usage.total_tokens == 3
        ));
        Ok(())
    }

    #[test]
    fn openai_websocket_completed_response_output_preserves_reasoning_before_message_id()
    -> Result<()> {
        let mut accumulator = super::OpenAiWebSocketAccumulator::new();
        let payload = serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1",
                "status": "completed",
                "output": [
                    {
                        "type": "reasoning",
                        "id": "rs_1",
                        "summary": [{
                            "type": "summary_text",
                            "text": "thinking"
                        }],
                        "encrypted_content": "opaque-cot"
                    },
                    {
                        "type": "message",
                        "id": "msg_1",
                        "role": "assistant",
                        "status": "completed",
                        "content": [{
                            "type": "output_text",
                            "text": "answer"
                        }]
                    }
                ],
                "usage": {
                    "input_tokens": 1,
                    "output_tokens": 2,
                    "output_tokens_details": {"reasoning_tokens": 1},
                    "total_tokens": 3
                }
            }
        })
        .to_string();

        let outcome = super::parse_openai_websocket_payload(&payload, &mut accumulator)?;

        assert!(outcome.choices.is_empty());
        assert!(outcome.terminal);
        let finished = accumulator.finish();
        let summary_index = finished
            .iter()
            .position(|choice| {
                matches!(
                    choice,
                    RawStreamingChoice::Reasoning {
                        id,
                        content: rig::message::ReasoningContent::Summary(text)
                    } if id.as_deref() == Some("rs_1") && text == "thinking"
                )
            })
            .context("terminal response reasoning summary should be emitted")?;
        let encrypted_index = finished
            .iter()
            .position(|choice| {
                matches!(
                    choice,
                    RawStreamingChoice::Reasoning {
                        id,
                        content: rig::message::ReasoningContent::Encrypted(data)
                    } if id.as_deref() == Some("rs_1") && data == "opaque-cot"
                )
            })
            .context("terminal response encrypted reasoning should be emitted")?;
        let message_index = finished
            .iter()
            .position(|choice| matches!(choice, RawStreamingChoice::MessageId(id) if id == "msg_1"))
            .context("terminal response message id should be emitted")?;

        assert!(summary_index < message_index);
        assert!(encrypted_index < message_index);
        Ok(())
    }

    #[test]
    fn openai_websocket_output_text_done_allows_disconnect_finish() {
        let mut accumulator = super::OpenAiWebSocketAccumulator::new();

        let delta = accumulator.decode_item_chunk(ItemChunk {
            item_id: Some("msg_1".to_string()),
            output_index: 0,
            data: ItemChunkKind::OutputTextDelta(DeltaTextChunk {
                content_index: 0,
                sequence_number: 1,
                delta: "hello".to_string(),
            }),
        });

        assert!(matches!(
            delta.as_slice(),
            [RawStreamingChoice::Message(message)] if message == "hello"
        ));
        assert!(!accumulator.can_finish_after_disconnect());

        assert!(
            accumulator
                .decode_item_chunk(ItemChunk {
                    item_id: Some("msg_1".to_string()),
                    output_index: 0,
                    data: ItemChunkKind::OutputTextDone(OutputTextChunk {
                        content_index: 0,
                        sequence_number: 2,
                        text: "hello".to_string(),
                    }),
                })
                .is_empty()
        );
        assert!(accumulator.can_finish_after_disconnect());

        let finished = accumulator.finish();
        assert!(finished.iter().any(|choice| {
            matches!(choice, RawStreamingChoice::MessageId(message_id) if message_id == "msg_1")
        }));
    }

    #[test]
    fn openai_websocket_function_args_done_allows_tool_call_finish() -> Result<()> {
        let mut accumulator = super::OpenAiWebSocketAccumulator::new();

        let name_delta = accumulator.decode_item_chunk(ItemChunk {
            item_id: Some("fc_1".to_string()),
            output_index: 0,
            data: ItemChunkKind::OutputItemAdded(StreamingItemDoneOutput {
                sequence_number: 1,
                item: Output::FunctionCall(OutputFunctionCall {
                    id: "fc_1".to_string(),
                    arguments: serde_json::json!({}),
                    call_id: "call_1".to_string(),
                    name: "read".to_string(),
                    status: ToolStatus::InProgress,
                }),
            }),
        });

        let internal_call_id = match name_delta.as_slice() {
            [
                RawStreamingChoice::ToolCallDelta {
                    id,
                    internal_call_id,
                    content: ToolCallDeltaContent::Name(name),
                },
            ] => {
                assert_eq!(id, "fc_1");
                assert_eq!(name, "read");
                internal_call_id.clone()
            }
            other => panic!("unexpected name delta: {other:?}"),
        };
        assert!(!accumulator.can_finish_after_disconnect());

        assert!(
            accumulator
                .decode_item_chunk(ItemChunk {
                    item_id: Some("fc_1".to_string()),
                    output_index: 0,
                    data: ItemChunkKind::FunctionCallArgsDone(ArgsTextChunk {
                        content_index: Some(0),
                        sequence_number: 2,
                        arguments: serde_json::json!({"path": "Cargo.toml"}),
                    }),
                })
                .is_empty()
        );
        assert!(accumulator.can_finish_after_disconnect());

        let finished = accumulator.finish();
        let tool_call = finished.iter().find_map(|choice| match choice {
            RawStreamingChoice::ToolCall(tool_call) => Some(tool_call),
            _ => None,
        });
        let tool_call = tool_call.context("tool call should be emitted from args.done fallback")?;
        assert_eq!(tool_call.id, "fc_1");
        assert_eq!(tool_call.internal_call_id, internal_call_id);
        assert_eq!(tool_call.call_id.as_deref(), Some("call_1"));
        assert_eq!(tool_call.name, "read");
        assert_eq!(
            tool_call.arguments,
            serde_json::json!({"path": "Cargo.toml"})
        );
        Ok(())
    }

    #[test]
    fn openai_websocket_reset_error_is_rewritten() {
        for message in [
            "WebSocket protocol error: Connection reset without closing handshake",
            "IO error: Connection reset by peer (os error 54)",
        ] {
            let error = CompletionError::ProviderError(message.to_string());

            assert!(super::is_openai_websocket_connection_reset(&error));
            assert!(super::is_openai_websocket_transient_start_error(&error));

            // The chokepoint canonicalizes the message and tags it transient, so the
            // retry verdict is stable regardless of the original prose; the tag is an
            // internal marker, stripped before the message is shown to a user.
            let rewritten = super::openai_websocket_stream_error(error);
            assert!(super::is_openai_websocket_transient_start_error(&rewritten));
            assert_eq!(
                super::strip_ws_transient_tag(&rewritten.to_string()),
                "ProviderError: OpenAI WebSocket connection reset before response.completed"
            );
        }
    }

    #[test]
    fn openai_websocket_transient_start_errors_are_retryable() {
        let retryable = [
            "websocket_connection_limit_reached",
            "IO error: Connection reset by peer (os error 54)",
            "OpenAI WebSocket connection reset before response.completed",
            "The OpenAI WebSocket connection closed before the turn finished",
            "The OpenAI WebSocket connection closed without a close reason",
            "An error occurred while processing the request.",
            "stream closed before response.completed",
            "disconnected before completion",
            "stream disconnected before completion",
            // Upstream-drop 5xx the proxy reports when its POST to the codex backend
            // closes without a response (structured code plus the Go `": EOF` tail).
            "internal_server_error: server_error: Post \"https://chatgpt.com/backend-api/codex/responses\": EOF",
            "internal_server_error: upstream unavailable",
        ];

        for message in retryable {
            let error = CompletionError::ProviderError(message.to_string());
            assert!(
                super::is_openai_websocket_transient_start_error(&error),
                "{message} should be retryable"
            );
        }

        // Boundary: a non-transient request error and the clean terminal failures must stay
        // fatal. In particular a bare `server_error` is deliberately not matched, so a genuine
        // `response.failed: server_error` keeps surfacing instead of being retried.
        let non_retryable = [
            "invalid_request_error: Bad input",
            "server_error: test failure",
            "OpenAI WebSocket response was incomplete: max_output_tokens",
        ];
        for message in non_retryable {
            let error = CompletionError::ProviderError(message.to_string());
            assert!(
                !super::is_openai_websocket_transient_start_error(&error),
                "{message} should not be retryable"
            );
        }
    }

    #[test]
    fn openai_websocket_retry_classification_stops_after_assistant_output() {
        let error =
            CompletionError::ProviderError("stream closed before response.completed".to_string());

        assert!(super::should_retry_openai_websocket_error(&error, false));
        assert!(!super::should_retry_openai_websocket_error(&error, true));
    }

    #[test]
    fn proxy_error_event_is_classified_from_structured_code_not_prose() {
        // A retryable proxy code stays retryable even when the human-readable
        // message is reworded to something the prose markers would never match.
        let reworded = serde_json::json!({
            "type": "error",
            "error": { "code": "internal_server_error", "message": "totally new wording" },
        });
        assert!(super::openai_websocket_error_value_is_transient(&reworded));

        // The `websocket_connection_limit_reached` code likewise classifies on the
        // structured field.
        let limit = serde_json::json!({
            "type": "error",
            "error": { "type": "websocket_connection_limit_reached", "message": "busy" },
        });
        assert!(super::openai_websocket_error_value_is_transient(&limit));

        // A genuine request error has no transient code and a non-marker message,
        // so it stays fatal.
        let fatal = serde_json::json!({
            "type": "error",
            "error": { "code": "invalid_request_error", "message": "bad input" },
        });
        assert!(!super::openai_websocket_error_value_is_transient(&fatal));

        // A proxy drop expressed only as the Go `": EOF` message tail (no code) is
        // still caught by the marker fallback.
        let eof = serde_json::json!({
            "type": "error",
            "error": { "message": "Post \"https://chatgpt.com/backend-api/codex/responses\": EOF" },
        });
        assert!(super::openai_websocket_error_value_is_transient(&eof));
    }

    #[test]
    fn tungstenite_reset_variants_are_tagged_transient_by_type() {
        use tokio_tungstenite::tungstenite::Error;
        use tokio_tungstenite::tungstenite::error::ProtocolError;

        // A reset without a closing handshake is classified by the typed variant.
        let reset = super::openai_websocket_provider_error(Error::Protocol(
            ProtocolError::ResetWithoutClosingHandshake,
        ));
        assert!(super::is_openai_websocket_transient_start_error(&reset));

        // An `ECONNRESET` I/O error is matched by `io.kind()` even though its
        // `Display` ("connection reset") is not one of the prose markers — the
        // typed check adds coverage the string match would miss.
        let io_reset = super::openai_websocket_provider_error(Error::Io(std::io::Error::from(
            std::io::ErrorKind::ConnectionReset,
        )));
        assert!(super::is_openai_websocket_transient_start_error(&io_reset));

        // An unrelated I/O error stays fatal.
        let denied = super::openai_websocket_provider_error(Error::Io(std::io::Error::from(
            std::io::ErrorKind::PermissionDenied,
        )));
        assert!(!super::is_openai_websocket_transient_start_error(&denied));
    }

    #[test]
    fn transient_tag_round_trips_and_is_stripped_for_display() {
        let tagged = super::provider_error_with_optional_tag("upstream blip".to_string(), true);
        assert!(super::is_openai_websocket_transient_start_error(&tagged));
        // The internal marker is never shown to a user.
        assert_eq!(
            super::strip_ws_transient_tag(&tagged.to_string()),
            "ProviderError: upstream blip"
        );

        let untagged = super::provider_error_with_optional_tag("upstream blip".to_string(), false);
        // No tag, and "upstream blip" matches no prose marker, so it stays fatal.
        assert!(!super::is_openai_websocket_transient_start_error(&untagged));
    }

    #[test]
    fn openai_websocket_stream_error_does_not_nest_provider_error_prefix() {
        // A non-reset transient error reaches the prose fallback; re-tagging must
        // re-use the raw message, not the `ProviderError:`-prefixed `Display`, so
        // the user never sees `ProviderError: ProviderError: …`.
        let error = CompletionError::ProviderError(
            "The OpenAI WebSocket connection closed before the turn finished".to_string(),
        );
        let rewritten = super::openai_websocket_stream_error(error);
        assert!(super::is_openai_websocket_transient_start_error(&rewritten));
        assert_eq!(
            super::strip_ws_transient_tag(&rewritten.to_string()),
            "ProviderError: The OpenAI WebSocket connection closed before the turn finished"
        );
    }

    #[test]
    fn openai_websocket_retry_delay_grows_then_caps() {
        // Use the built-in WebSocket defaults (base 250ms, max 3s).
        let ws = cazean_config::WebSocketConfig::default();
        let base = std::time::Duration::from_millis(ws.retry_base_ms);
        let max = std::time::Duration::from_millis(ws.retry_max_ms);
        // Early retries back off exponentially from the base delay.
        assert_eq!(super::openai_websocket_retry_delay(1, base, max), base);
        assert_eq!(super::openai_websocket_retry_delay(2, base, max), base * 2);

        // The later attempts in the budget are clamped so the retry tail stays interactive, and a
        // huge retry count saturates to the cap instead of panicking on Duration overflow.
        assert!(super::openai_websocket_retry_delay(ws.retry_budget, base, max) <= max);
        assert_eq!(super::openai_websocket_retry_delay(64, base, max), max);
    }

    #[test]
    fn openai_websocket_error_event_connection_limit_is_retryable() {
        let mut accumulator = super::OpenAiWebSocketAccumulator::new();
        let payload = r#"{"type":"error","error":{"code":"websocket_connection_limit_reached","message":"too many websocket connections"}}"#;
        let Err(error) = super::parse_openai_websocket_payload(payload, &mut accumulator) else {
            panic!("proxy error event should surface as a CompletionError");
        };

        assert!(
            error
                .to_string()
                .contains("websocket_connection_limit_reached")
        );
        assert!(super::is_openai_websocket_transient_start_error(&error));
    }

    #[test]
    fn openai_websocket_error_event_type_and_top_level_code_are_retryable() {
        let mut accumulator = super::OpenAiWebSocketAccumulator::new();
        let payload = r#"{"type":"error","code":"websocket_connection_limit_reached","error":{"type":"websocket_connection_limit_reached","message":"too many websocket connections"}}"#;
        let Err(error) = super::parse_openai_websocket_payload(payload, &mut accumulator) else {
            panic!("proxy error event should surface as a CompletionError");
        };

        assert!(
            error
                .to_string()
                .contains("websocket_connection_limit_reached")
        );
        assert!(super::is_openai_websocket_transient_start_error(&error));
    }

    #[test]
    fn openai_websocket_error_event_early_close_is_retryable() {
        let mut accumulator = super::OpenAiWebSocketAccumulator::new();
        let payload =
            r#"{"type":"error","error":{"message":"stream closed before response.completed"}}"#;
        let Err(error) = super::parse_openai_websocket_payload(payload, &mut accumulator) else {
            panic!("proxy error event should surface as a CompletionError");
        };
        assert!(
            super::is_openai_websocket_transient_start_error(&error),
            "early-close error events must be retryable before output"
        );
    }

    #[test]
    fn openai_websocket_error_event_disconnected_before_completion_is_retryable() {
        let mut accumulator = super::OpenAiWebSocketAccumulator::new();
        let payload = r#"{"type":"error","error":{"message":"disconnected before completion"}}"#;
        let Err(error) = super::parse_openai_websocket_payload(payload, &mut accumulator) else {
            panic!("proxy error event should surface as a CompletionError");
        };
        assert!(
            super::is_openai_websocket_transient_start_error(&error),
            "disconnect error events must be retryable before output"
        );
    }

    #[test]
    fn openai_websocket_error_event_upstream_eof_is_retryable() {
        let mut accumulator = super::OpenAiWebSocketAccumulator::new();
        // CLIProxyAPI relays an upstream POST that closed without a response: a structured 5xx
        // code/type folded into the message plus the Go `net/http` `": EOF` transport tail.
        let payload = serde_json::json!({
            "type": "error",
            "error": {
                "code": "internal_server_error",
                "type": "server_error",
                "message": "Post \"https://chatgpt.com/backend-api/codex/responses\": EOF"
            }
        })
        .to_string();
        let Err(error) = super::parse_openai_websocket_payload(&payload, &mut accumulator) else {
            panic!("proxy error event should surface as a CompletionError");
        };

        let rendered = error.to_string();
        assert!(rendered.contains("internal_server_error"), "{rendered}");
        assert!(rendered.contains("EOF"), "{rendered}");
        assert!(
            super::is_openai_websocket_transient_start_error(&error),
            "upstream-drop EOF error events must be retryable before output"
        );
    }

    #[tokio::test]
    async fn openai_websocket_clean_terminal_completion_parks_socket() -> Result<()> {
        let (base_url, handshakes, server) = start_openai_websocket_single_frame_server(
            openai_websocket_completed_frame("resp_park"),
            false,
        )
        .await?;
        let model = openai_websocket_test_model(&base_url).await?;
        let stream = super::openai_websocket_completion_stream(
            &model,
            openai_websocket_test_request("first")?,
            tokio_util::sync::CancellationToken::new(),
        )
        .await?;

        drain_openai_websocket_stream(stream).await?;

        assert_eq!(handshakes.load(Ordering::SeqCst), 1);
        assert!(model.session_ws.lock().await.is_some());
        server.abort();
        Ok(())
    }

    /// Server for the cancellation flow: answers `response.create` with one
    /// text delta and then waits; when (and only when) it receives a
    /// `response.cancel` frame, it answers with the clean `response.completed`
    /// terminal. Records whether the cancel frame arrived.
    async fn start_openai_websocket_cancel_server() -> Result<(
        String,
        Arc<std::sync::atomic::AtomicBool>,
        Arc<Mutex<Option<String>>>,
        tokio::task::JoinHandle<()>,
    )> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let cancel_received = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancel_payload = Arc::new(Mutex::new(None));
        let server_cancel_received = Arc::clone(&cancel_received);
        let server_cancel_payload = Arc::clone(&cancel_payload);
        let handle = tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(mut socket) = accept_async(stream).await else {
                return;
            };
            let mut saw_create = false;
            while let Some(message) = socket.next().await {
                let Ok(message) = message else {
                    break;
                };
                if !(message.is_text() || message.is_binary()) {
                    continue;
                }
                let text = message.into_text().unwrap_or_default();
                if !saw_create {
                    saw_create = true;
                    if socket
                        .send(TestWebSocketMessage::text(openai_websocket_created_frame(
                            "resp_cancel_target",
                        )))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    if socket
                        .send(TestWebSocketMessage::text(
                            openai_websocket_text_delta_frame(),
                        ))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    continue;
                }
                if text.contains("response.cancel") {
                    server_cancel_received.store(true, Ordering::SeqCst);
                    *server_cancel_payload.lock().await = Some(text.to_string());
                    let _ = socket
                        .send(TestWebSocketMessage::text(
                            openai_websocket_completed_frame("resp_cancel_target"),
                        ))
                        .await;
                    // Keep the connection open so the client can park it.
                    std::future::pending::<()>().await;
                }
            }
        });
        Ok((
            format!("http://{address}/v1"),
            cancel_received,
            cancel_payload,
            handle,
        ))
    }

    #[tokio::test]
    async fn openai_websocket_cancel_sends_cancel_frame_and_parks_socket() -> Result<()> {
        let (base_url, cancel_received, cancel_payload, server) =
            start_openai_websocket_cancel_server().await?;
        let model = openai_websocket_test_model(&base_url).await?;
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut stream = super::openai_websocket_completion_stream(
            &model,
            openai_websocket_test_request("to-be-cancelled")?,
            cancel.clone(),
        )
        .await?;

        // Wait for the first streamed delta, then cancel mid-response.
        let first = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .context("timed out waiting for first delta")?;
        assert!(matches!(first, Some(Ok(_))), "expected a streamed delta");
        cancel.cancel();

        // Keep polling: the stream sends `response.cancel`, receives the
        // clean terminal, and parks the socket.
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(item) = stream.next().await {
                let _ = item;
            }
        })
        .await;

        assert!(
            cancel_received.load(Ordering::SeqCst),
            "server should have received a response.cancel frame"
        );
        let cancel_payload = cancel_payload.lock().await.clone();
        let Some(cancel_payload) = cancel_payload else {
            panic!("server should have captured the response.cancel payload");
        };
        let cancel_json: serde_json::Value = serde_json::from_str(&cancel_payload)?;
        assert_eq!(
            cancel_json.get("type").and_then(serde_json::Value::as_str),
            Some("response.cancel")
        );
        assert_eq!(
            cancel_json
                .get("response_id")
                .and_then(serde_json::Value::as_str),
            Some("resp_cancel_target")
        );
        assert!(
            model.session_ws.lock().await.is_some(),
            "socket should be parked after a clean post-cancel terminal"
        );
        server.abort();
        Ok(())
    }

    #[test]
    fn openai_websocket_cancel_payload_uses_optional_response_id() -> Result<()> {
        let targeted: serde_json::Value =
            serde_json::from_str(&super::openai_websocket_cancel_payload(Some("resp_123")))?;
        assert_eq!(
            targeted.get("type").and_then(serde_json::Value::as_str),
            Some("response.cancel")
        );
        assert_eq!(
            targeted
                .get("response_id")
                .and_then(serde_json::Value::as_str),
            Some("resp_123")
        );

        let bare: serde_json::Value =
            serde_json::from_str(&super::openai_websocket_cancel_payload(None))?;
        assert_eq!(
            bare.get("type").and_then(serde_json::Value::as_str),
            Some("response.cancel")
        );
        assert!(bare.get("response_id").is_none());
        Ok(())
    }

    #[tokio::test]
    async fn openai_websocket_clean_terminal_failed_frame_parks_socket_and_errors() -> Result<()> {
        let (base_url, _handshakes, server) =
            start_openai_websocket_single_frame_server(openai_websocket_failed_frame(), false)
                .await?;
        let model = openai_websocket_test_model(&base_url).await?;
        let stream = super::openai_websocket_completion_stream(
            &model,
            openai_websocket_test_request("failed")?,
            tokio_util::sync::CancellationToken::new(),
        )
        .await?;

        let error = drain_openai_websocket_stream(stream)
            .await
            .err()
            .context("failed response should surface provider error")?;

        assert!(error.to_string().contains("server_error: test failure"));
        assert!(model.session_ws.lock().await.is_some());
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn openai_websocket_clean_terminal_incomplete_frame_parks_socket_and_errors() -> Result<()>
    {
        let (base_url, _handshakes, server) =
            start_openai_websocket_single_frame_server(openai_websocket_incomplete_frame(), false)
                .await?;
        let model = openai_websocket_test_model(&base_url).await?;
        let stream = super::openai_websocket_completion_stream(
            &model,
            openai_websocket_test_request("incomplete")?,
            tokio_util::sync::CancellationToken::new(),
        )
        .await?;

        let error = drain_openai_websocket_stream(stream)
            .await
            .err()
            .context("incomplete response should surface provider error")?;

        assert!(
            error
                .to_string()
                .contains("OpenAI WebSocket response was incomplete: max_output_tokens")
        );
        assert!(model.session_ws.lock().await.is_some());
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn openai_websocket_mid_turn_transport_error_does_not_park_socket() -> Result<()> {
        let (base_url, _handshakes, server) =
            start_openai_websocket_single_frame_server(openai_websocket_text_delta_frame(), true)
                .await?;
        let model = openai_websocket_test_model(&base_url).await?;
        let stream = super::openai_websocket_completion_stream(
            &model,
            openai_websocket_test_request("transport error")?,
            tokio_util::sync::CancellationToken::new(),
        )
        .await?;

        let error = drain_openai_websocket_stream(stream)
            .await
            .err()
            .context("mid-turn close should surface provider error")?;

        assert!(
            error
                .to_string()
                .contains("OpenAI WebSocket connection closed")
        );
        assert!(model.session_ws.lock().await.is_none());
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn openai_websocket_dropped_stream_does_not_park_socket() -> Result<()> {
        let (base_url, _handshakes, server) =
            start_openai_websocket_single_frame_server(openai_websocket_text_delta_frame(), false)
                .await?;
        let model = openai_websocket_test_model(&base_url).await?;
        let mut stream = super::openai_websocket_completion_stream(
            &model,
            openai_websocket_test_request("drop")?,
            tokio_util::sync::CancellationToken::new(),
        )
        .await?;

        let item = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .context("timed out waiting for first stream item")?
            .context("stream ended before first item")?;
        item?;
        drop(stream);

        assert!(model.session_ws.lock().await.is_none());
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn openai_websocket_sequential_turns_reuse_one_handshake() -> Result<()> {
        let (base_url, handshakes, server) =
            start_openai_websocket_sequence_server(OpenAiWebSocketTestServerMode::KeepAlive)
                .await?;
        let model = openai_websocket_test_model(&base_url).await?;

        let first = super::stream_openai_agent_completion(
            &model,
            Message::user("first"),
            &[],
            tokio_util::sync::CancellationToken::new(),
        )
        .await?;
        drain_session_completion_stream(first).await?;
        let second = super::stream_openai_agent_completion(
            &model,
            Message::user("second"),
            &[],
            tokio_util::sync::CancellationToken::new(),
        )
        .await?;
        drain_session_completion_stream(second).await?;

        assert_eq!(handshakes.load(Ordering::SeqCst), 1);
        assert!(model.session_ws.lock().await.is_some());
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn openai_websocket_reused_turn_ignores_stale_trailing_response_done() -> Result<()> {
        let (base_url, handshakes, server) = start_openai_websocket_sequence_server(
            OpenAiWebSocketTestServerMode::KeepAliveWithTrailingDone,
        )
        .await?;
        let model = openai_websocket_test_model(&base_url).await?;

        let first = super::openai_websocket_completion_stream(
            &model,
            openai_websocket_test_request("first")?,
            tokio_util::sync::CancellationToken::new(),
        )
        .await?;
        assert_eq!(openai_websocket_final_total_tokens(first).await?, 100);

        let second = super::openai_websocket_completion_stream(
            &model,
            openai_websocket_test_request("second")?,
            tokio_util::sync::CancellationToken::new(),
        )
        .await?;
        assert_eq!(openai_websocket_final_total_tokens(second).await?, 200);

        assert_eq!(handshakes.load(Ordering::SeqCst), 1);
        assert!(model.session_ws.lock().await.is_some());
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn openai_websocket_reused_turn_skips_stale_done_behind_buffered_telemetry() -> Result<()>
    {
        // A previous-turn telemetry frame is buffered ahead of the trailing `response.done`,
        // so the stale-done guard must stay armed across it. If it disarms on the first frame,
        // the stale done (total_tokens 101) terminates the second turn with the previous
        // turn's metadata instead of the second turn's own `response.completed` (200).
        let (base_url, handshakes, server) = start_openai_websocket_sequence_server(
            OpenAiWebSocketTestServerMode::KeepAliveWithTelemetryThenTrailingDone,
        )
        .await?;
        let model = openai_websocket_test_model(&base_url).await?;

        let first = super::openai_websocket_completion_stream(
            &model,
            openai_websocket_test_request("first")?,
            tokio_util::sync::CancellationToken::new(),
        )
        .await?;
        assert_eq!(openai_websocket_final_total_tokens(first).await?, 100);

        let second = super::openai_websocket_completion_stream(
            &model,
            openai_websocket_test_request("second")?,
            tokio_util::sync::CancellationToken::new(),
        )
        .await?;
        assert_eq!(openai_websocket_final_total_tokens(second).await?, 200);

        assert_eq!(handshakes.load(Ordering::SeqCst), 1);
        assert!(model.session_ws.lock().await.is_some());
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn openai_websocket_reused_socket_send_failure_reconnects_and_resends() -> Result<()> {
        let (base_url, handshakes, server) = start_openai_websocket_sequence_server(
            OpenAiWebSocketTestServerMode::AbortAfterResponse,
        )
        .await?;
        let model = openai_websocket_test_model(&base_url).await?;

        let first = super::openai_websocket_completion_stream(
            &model,
            openai_websocket_test_request("first")?,
            tokio_util::sync::CancellationToken::new(),
        )
        .await?;
        assert_eq!(openai_websocket_final_total_tokens(first).await?, 100);

        tokio::time::sleep(Duration::from_millis(50)).await;

        let second = super::openai_websocket_completion_stream(
            &model,
            openai_websocket_test_request("second")?,
            tokio_util::sync::CancellationToken::new(),
        )
        .await?;
        assert_eq!(openai_websocket_final_total_tokens(second).await?, 200);

        assert_eq!(handshakes.load(Ordering::SeqCst), 2);
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn openai_websocket_server_close_between_turns_forces_reconnect() -> Result<()> {
        let (base_url, handshakes, server) = start_openai_websocket_sequence_server(
            OpenAiWebSocketTestServerMode::CloseAfterResponse,
        )
        .await?;
        let model = openai_websocket_test_model(&base_url).await?;

        let first = super::stream_openai_agent_completion(
            &model,
            Message::user("first"),
            &[],
            tokio_util::sync::CancellationToken::new(),
        )
        .await?;
        drain_session_completion_stream(first).await?;
        let second = super::stream_openai_agent_completion(
            &model,
            Message::user("second"),
            &[],
            tokio_util::sync::CancellationToken::new(),
        )
        .await?;
        drain_session_completion_stream(second).await?;

        assert_eq!(handshakes.load(Ordering::SeqCst), 2);
        assert!(model.session_ws.lock().await.is_some());
        server.abort();
        Ok(())
    }

    #[test]
    fn openai_websocket_incomplete_tool_call_blocks_disconnect_finish() {
        let mut accumulator = super::OpenAiWebSocketAccumulator::new();

        // A text item streams and completes.
        accumulator.decode_item_chunk(ItemChunk {
            item_id: Some("msg_1".to_string()),
            output_index: 0,
            data: ItemChunkKind::OutputTextDelta(DeltaTextChunk {
                content_index: 0,
                sequence_number: 1,
                delta: "Let me check".to_string(),
            }),
        });
        accumulator.decode_item_chunk(ItemChunk {
            item_id: Some("msg_1".to_string()),
            output_index: 0,
            data: ItemChunkKind::OutputTextDone(OutputTextChunk {
                content_index: 0,
                sequence_number: 2,
                text: "Let me check".to_string(),
            }),
        });

        // A tool call starts but never finishes (no args.done, no
        // output_item.done).
        accumulator.decode_item_chunk(ItemChunk {
            item_id: Some("fc_1".to_string()),
            output_index: 1,
            data: ItemChunkKind::OutputItemAdded(StreamingItemDoneOutput {
                sequence_number: 3,
                item: Output::FunctionCall(OutputFunctionCall {
                    id: "fc_1".to_string(),
                    arguments: serde_json::json!({}),
                    call_id: "call_1".to_string(),
                    name: "read".to_string(),
                    status: ToolStatus::InProgress,
                }),
            }),
        });

        // A reset here must NOT be treated as a graceful finish: doing so would
        // drop the in-flight tool call and end the turn with only the text.
        assert!(!accumulator.can_finish_after_disconnect());
    }

    #[test]
    fn openai_websocket_response_done_without_status_completes() -> Result<()> {
        let mut accumulator = super::OpenAiWebSocketAccumulator::new();

        let outcome = super::parse_openai_websocket_payload(
            r#"{"type":"response.done","response":{"id":"resp_1","usage":{"input_tokens":1,"output_tokens":2,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":3}}}"#,
            &mut accumulator,
        )?;

        assert!(outcome.choices.is_empty());
        assert!(outcome.terminal);
        assert!(accumulator.can_finish_after_disconnect());
        let finished = accumulator.finish();
        assert!(matches!(
            finished.last(),
            Some(RawStreamingChoice::FinalResponse(response)) if response.usage.total_tokens == 3
        ));
        Ok(())
    }
}
