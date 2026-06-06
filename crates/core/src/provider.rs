use std::{
    collections::{HashMap, HashSet},
    env,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
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
                InputItem as OpenAiResponsesInputItem, Output, OutputTokensDetails,
                Reasoning as OpenAiReasoning, ReasoningEffort, ReasoningSummary, ResponseStatus,
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
    ReadTool, RunCommandTool, SpawnAgentTool, WriteTool,
};

use crate::agent::{
    AgentControl, PLAN_MODE_INSTRUCTIONS, SystemPromptKind,
    prompt::{render_spawn_agent_tool_description, system_prompt_for_kind},
};
use crate::environment::EnvironmentContext;

/// Injectable builder for session-scoped models.
pub trait SessionModelFactory: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    fn build(
        &self,
        cwd: PathBuf,
        thread_id: smooth_protocol::ThreadId,
        ask_user_client: Option<AskUserClient>,
        current_turn_id: Arc<RwLock<Option<String>>>,
        system_prompt_kind: SystemPromptKind,
        agent_control: AgentControl,
        plan_mode: bool,
    ) -> Result<SessionModel>;
}

/// Default environment-backed `SessionModelFactory`.
pub struct EnvSessionModelFactory;

impl SessionModelFactory for EnvSessionModelFactory {
    fn build(
        &self,
        cwd: PathBuf,
        thread_id: smooth_protocol::ThreadId,
        ask_user_client: Option<AskUserClient>,
        current_turn_id: Arc<RwLock<Option<String>>>,
        system_prompt_kind: SystemPromptKind,
        agent_control: AgentControl,
        plan_mode: bool,
    ) -> Result<SessionModel> {
        SessionModel::from_env(
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

    fn call_tool(&self, _tool_name: &str, _args: &str) -> Result<String> {
        Err(anyhow!("manual tool execution is not supported"))
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
    session_ws: Mutex<Option<OpenAiParkedWebSocket>>,
}

impl SessionModel {
    pub(crate) fn requires_provider_reasoning_ids(&self) -> bool {
        matches!(self, Self::OpenAi(_))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_env(
        cwd: PathBuf,
        thread_id: smooth_protocol::ThreadId,
        ask_user_client: Option<AskUserClient>,
        current_turn_id: Arc<RwLock<Option<String>>>,
        system_prompt_kind: SystemPromptKind,
        agent_control: AgentControl,
        plan_mode: bool,
    ) -> Result<Self> {
        let provider = env::var("SMOOTH_CODE_LLM_PROVIDER")
            .unwrap_or_else(|_| "openai".to_string())
            .to_ascii_lowercase();
        let model = env::var("SMOOTH_CODE_LLM_MODEL")
            .ok()
            .unwrap_or_else(|| "gpt-5.5".to_string());
        let environment_context = EnvironmentContext::gather(&cwd);
        let preamble = compose_session_preamble(
            system_prompt_kind,
            env::var("SMOOTH_CODE_LLM_PREAMBLE").ok(),
            &environment_context,
            plan_mode,
        );

        match provider.as_str() {
            "openai" => {
                let mut builder = openai::Client::builder().api_key("cazean");
                builder = builder.base_url("http://localhost:8317/v1");
                let client = builder.build()?;
                let additional_params = AdditionalParameters {
                    reasoning: Some(
                        OpenAiReasoning::new()
                            .with_effort(ReasoningEffort::High)
                            .with_summary_level(openai::responses_api::ReasoningSummaryLevel::Auto),
                    ),
                    ..Default::default()
                };
                let agent = build_agent(
                    client
                        .agent(&model)
                        .preamble(&preamble)
                        .additional_params(additional_params.to_json()),
                    cwd,
                    thread_id,
                    ask_user_client.clone(),
                    Arc::clone(&current_turn_id),
                    system_prompt_kind,
                    agent_control.clone(),
                    plan_mode,
                );
                Ok(Self::OpenAi(Arc::new(OpenAiSessionModel {
                    agent: Arc::new(agent),
                    client,
                    model,
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
                ))))
            }
            other => bail!("unsupported SMOOTH_CODE_LLM_PROVIDER `{other}`"),
        }
    }

    pub(crate) async fn stream_completion_turn(
        &self,
        prompt: Message,
        history: &[Message],
    ) -> Result<SessionCompletionStream> {
        match self {
            Self::OpenAi(openai) => stream_openai_agent_completion(openai, prompt, history).await,
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
            Self::Stub(driver) => driver.call_tool(tool_name, args),
        }
    }
}

pub(crate) fn default_session_model_factory() -> Arc<dyn SessionModelFactory> {
    Arc::new(EnvSessionModelFactory)
}

pub(crate) fn stub_session_model_factory(
    models: HashMap<smooth_protocol::ThreadId, SessionModel>,
) -> Arc<dyn SessionModelFactory> {
    Arc::new(StubSessionModelFactory {
        models: std::sync::Mutex::new(models),
    })
}

struct StubSessionModelFactory {
    models: std::sync::Mutex<HashMap<smooth_protocol::ThreadId, SessionModel>>,
}

impl SessionModelFactory for StubSessionModelFactory {
    fn build(
        &self,
        _cwd: PathBuf,
        thread_id: smooth_protocol::ThreadId,
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
        preamble = format!("{}\n\n{PLAN_MODE_INSTRUCTIONS}", preamble.trim_end());
    }
    preamble
}

#[allow(clippy::too_many_arguments)]
fn build_agent<M>(
    builder: rig::agent::AgentBuilder<M, (), rig::agent::NoToolConfig>,
    cwd: PathBuf,
    thread_id: smooth_protocol::ThreadId,
    ask_user_client: Option<AskUserClient>,
    current_turn_id: Arc<RwLock<Option<String>>>,
    system_prompt_kind: SystemPromptKind,
    _agent_control: AgentControl,
    plan_mode: bool,
) -> Agent<M>
where
    M: rig::completion::CompletionModel,
{
    // File reads, shell inspection, and sub-agent spawning are always present.
    let builder = builder
        .tool(ReadTool::new(cwd.clone()))
        .tool(RunCommandTool::new(cwd.clone()));
    if matches!(system_prompt_kind, SystemPromptKind::Explore) {
        return builder.default_max_turns(99999).build();
    }
    // File-mutating tools are only registered outside plan mode;
    // plan-mode-specific tools (`plan_write`, `exit_plan_mode`) are only
    // registered inside plan mode.
    let builder = if plan_mode {
        builder
            .tool(PlanWriteTool::new(cwd.clone(), thread_id))
            .tool(ExitPlanModeTool::new())
    } else {
        builder
            .tool(DeleteTool::new(cwd.clone()))
            .tool(EditTool::new(cwd.clone()))
            .tool(WriteTool::new(cwd))
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
    builder.default_max_turns(99999).build()
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
// A connection reset before output is how the local proxy (CLIProxyAPI) surfaces an upstream
// codex account running out of usage: it drops the socket, and on the next attempt the proxy can
// rotate to a different account. Budget enough attempts to roll through several exhausted accounts
// before giving up, and cap the exponential backoff so the retry tail stays interactive.
pub(crate) const OPENAI_WEBSOCKET_RETRY_BUDGET: usize = 8;
const OPENAI_WEBSOCKET_RETRY_BASE_DELAY: Duration = Duration::from_millis(250);
const OPENAI_WEBSOCKET_RETRY_MAX_DELAY: Duration = Duration::from_secs(3);

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
            let mut stream = match openai_websocket_completion_stream(&openai, completion_request.clone()).await {
                Ok(stream) => stream,
                Err(error)
                    if retry_count < OPENAI_WEBSOCKET_RETRY_BUDGET
                        && should_retry_openai_websocket_error(&error, false) =>
                {
                    retry_count += 1;
                    tracing::debug!(
                        retry_count,
                        error = %error,
                        "OpenAI WebSocket transient failure before the turn stream started; retrying"
                    );
                    tokio::time::sleep(openai_websocket_retry_delay(retry_count)).await;
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
                        if retry_count < OPENAI_WEBSOCKET_RETRY_BUDGET
                            && should_retry_openai_websocket_error(&error, yielded_assistant_item) =>
                    {
                        retry_count += 1;
                        tracing::debug!(
                            retry_count,
                            error = %error,
                            "OpenAI WebSocket transient failure before any assistant item; retrying"
                        );
                        tokio::time::sleep(openai_websocket_retry_delay(retry_count)).await;
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
        if let Err(error) = socket.send(TungsteniteMessage::Text(payload.clone())).await {
            let error = openai_websocket_provider_error(error);
            tracing::debug!(
                error = %error,
                "OpenAI WebSocket send failed on a reused socket; reconnecting before retrying request"
            );
            socket = openai_websocket_connect(&openai.client).await?;
            trailing_done_response_id = None;
            socket
                .send(TungsteniteMessage::Text(payload))
                .await
                .map_err(openai_websocket_provider_error)?;
        }
    } else {
        socket
            .send(TungsteniteMessage::Text(payload))
            .await
            .map_err(openai_websocket_provider_error)?;
    }
    Ok(openai_websocket_stream(
        Arc::clone(openai),
        socket,
        trailing_done_response_id,
    ))
}

fn openai_websocket_stream(
    openai: Arc<OpenAiSessionModel>,
    socket: OpenAiWebSocket,
    mut stale_done_response_id: Option<String>,
) -> RigStreamingCompletionResponse<OpenAiStreamingCompletionResponse> {
    let raw_stream = async_stream::try_stream! {
        let mut socket = Some(socket);
        let mut accumulator = OpenAiWebSocketAccumulator::new();
        let mut terminal_error = None;
        let mut terminal_response_id = None;
        let mut clean_terminal = false;

        while let Some(socket) = socket.as_mut() {
            let message = socket.next().await;
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
    serde_json::to_string(&OpenAiWebSocketCreateEvent {
        kind: "response.create",
        request,
    })
    .map_err(CompletionError::from)
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
        TungsteniteMessage::Text(text) => Ok(Some(text)),
        TungsteniteMessage::Binary(bytes) => String::from_utf8(bytes)
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
        "error" => Err(CompletionError::ProviderError(
            openai_websocket_error_event_message(&value),
        )),
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

fn openai_websocket_provider_error(
    error: tokio_tungstenite::tungstenite::Error,
) -> CompletionError {
    CompletionError::ProviderError(error.to_string())
}

fn openai_websocket_stream_error(error: CompletionError) -> CompletionError {
    if is_openai_websocket_connection_reset(&error) {
        CompletionError::ProviderError(
            "OpenAI WebSocket connection reset before response.completed".to_string(),
        )
    } else {
        error
    }
}

fn is_openai_websocket_connection_reset(error: &CompletionError) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("connection reset without closing handshake")
        || message.contains("connection reset by peer")
        || message.contains("openai websocket connection reset before response.completed")
}

/// Structured proxy error codes/types (`error.code` / `error.type` / top-level `code`) that
/// mark a transient condition worth retrying. `openai_websocket_error_event_message` folds
/// these into the rendered error string, so matching them here is effectively a match on the
/// structured code rather than on free-form prose — robust to the proxy's message wording.
const OPENAI_WEBSOCKET_RETRYABLE_PROXY_CODES: &[&str] = &["websocket_connection_limit_reached"];

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
];

pub(crate) fn is_openai_websocket_transient_start_error(error: &CompletionError) -> bool {
    // Classification is text-based because `CompletionError` carries only a rendered message at
    // this boundary; the marker lists are the single source of truth for what counts as
    // transient. The provider uses this before output, and the manual turn loop reuses it after
    // partial output so both retry paths agree on what a transient WebSocket disconnect means.
    let message = error.to_string();
    is_openai_websocket_connection_reset(error)
        || OPENAI_WEBSOCKET_RETRYABLE_PROXY_CODES
            .iter()
            .chain(OPENAI_WEBSOCKET_RETRYABLE_TRANSIENT_MARKERS)
            .any(|marker| message.contains(marker))
}

fn should_retry_openai_websocket_error(
    error: &CompletionError,
    yielded_assistant_item: bool,
) -> bool {
    !yielded_assistant_item && is_openai_websocket_transient_start_error(error)
}

pub(crate) fn openai_websocket_retry_delay(retry_count: usize) -> Duration {
    let factor = 1_u32
        .checked_shl(retry_count.saturating_sub(1) as u32)
        .unwrap_or(u32::MAX);
    OPENAI_WEBSOCKET_RETRY_BASE_DELAY
        .saturating_mul(factor)
        .min(OPENAI_WEBSOCKET_RETRY_MAX_DELAY)
}

struct OpenAiWebSocketAccumulator {
    final_usage: ResponsesUsage,
    message_id: Option<String>,
    tool_calls: Vec<OpenAiWebSocketRawChoice>,
    tool_call_internal_ids: HashMap<String, String>,
    pending_tool_calls: HashMap<String, PendingOpenAiToolCall>,
    pending_tool_call_order: Vec<String>,
    completed_tool_call_ids: HashSet<String>,
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
            tool_calls: Vec::new(),
            tool_call_internal_ids: HashMap::new(),
            pending_tool_calls: HashMap::new(),
            pending_tool_call_order: Vec::new(),
            completed_tool_call_ids: HashSet::new(),
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
                let internal_call_id = self.internal_call_id_for(&delta.item_id);
                choices.push(RawStreamingChoice::ToolCallDelta {
                    id: delta.item_id,
                    internal_call_id,
                    content: ToolCallDeltaContent::Delta(delta.delta),
                });
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
        if self.message_id.is_none() {
            self.message_id = response
                .get("output")
                .and_then(serde_json::Value::as_array)
                .and_then(|output| {
                    output.iter().find_map(|item| {
                        if item
                            .get("type")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|kind| kind == "message")
                        {
                            item.get("id")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_string)
                        } else {
                            None
                        }
                    })
                });
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
                for summary in summary {
                    let ReasoningSummary::SummaryText { text } = summary;
                    immediate.push(RawStreamingChoice::Reasoning {
                        id: Some(id.clone()),
                        content: ReasoningContent::Summary(text),
                    });
                }
                if let Some(encrypted_content) = encrypted_content {
                    immediate.push(RawStreamingChoice::Reasoning {
                        id: Some(id),
                        content: ReasoningContent::Encrypted(encrypted_content),
                    });
                }
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

fn empty_openai_usage() -> ResponsesUsage {
    ResponsesUsage {
        input_tokens: 0,
        input_tokens_details: None,
        output_tokens: 0,
        output_tokens_details: OutputTokensDetails {
            reasoning_tokens: 0,
        },
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

    use super::{build_agent, compose_session_preamble};
    use crate::{
        agent::{AgentControl, PLAN_MODE_INSTRUCTIONS, SystemPromptKind},
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
            working_directory: "/workspace/smooth-code".to_string(),
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
                            .send(TestWebSocketMessage::Text(frame))
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
                                    .send(TestWebSocketMessage::Text(telemetry_frame))
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
                                .send(TestWebSocketMessage::Text(done_frame))
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
                    .send(TestWebSocketMessage::Text(frame.clone()))
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

        assert!(preamble.starts_with("# Smooth Code System Prompt"));
        assert!(preamble.contains("You are Smooth Code"));
        assert!(preamble.contains("Working directory: /workspace/smooth-code"));
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

        assert!(preamble.starts_with("# Smooth Code Default Subagent Prompt"));
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

        assert!(preamble.starts_with("# Smooth Code Explorer Subagent Prompt"));
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
            format!("Base prompt.\n\n{PLAN_MODE_INSTRUCTIONS}")
        );
    }

    #[tokio::test]
    async fn build_agent_registers_root_tools() -> Result<()> {
        let workspace = tempfile::TempDir::new()?;
        let agent = build_agent(
            AgentBuilder::new(DummyModel),
            workspace.path().to_path_buf(),
            smooth_protocol::ThreadId::new(),
            None,
            Arc::new(RwLock::new(None)),
            SystemPromptKind::Root,
            AgentControl::new(),
            false,
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
            smooth_protocol::ThreadId::new(),
            None,
            Arc::new(RwLock::new(None)),
            SystemPromptKind::DefaultSubagent,
            AgentControl::new(),
            false,
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
            smooth_protocol::ThreadId::new(),
            None,
            Arc::new(RwLock::new(None)),
            SystemPromptKind::Root,
            AgentControl::new(),
            true,
        );

        let tool_names = agent
            .tool_server_handle
            .get_tool_defs(None)
            .await?
            .into_iter()
            .map(|definition| definition.name)
            .collect::<HashSet<_>>();

        // File reads, shell inspection, and sub-agent tools remain available.
        assert!(tool_names.contains("read"));
        assert!(tool_names.contains("run_command"));
        assert!(tool_names.contains("spawn_agent"));
        assert!(!tool_names.contains("explore"));
        assert!(!tool_names.contains("list_dir"));
        // Plan-mode planning tools are registered.
        assert!(tool_names.contains("plan_write"));
        assert!(tool_names.contains("exit_plan_mode"));
        // Mutating tools must be stripped.
        assert!(!tool_names.contains("edit"));
        assert!(!tool_names.contains("delete"));
        assert!(!tool_names.contains("write"));
        Ok(())
    }

    #[tokio::test]
    async fn build_agent_for_explore_child_is_read_only() -> Result<()> {
        let workspace = tempfile::TempDir::new()?;
        let agent = build_agent(
            AgentBuilder::new(DummyModel),
            workspace.path().to_path_buf(),
            smooth_protocol::ThreadId::new(),
            Some(stub_ask_user_client()),
            Arc::new(RwLock::new(None)),
            SystemPromptKind::Explore,
            AgentControl::new(),
            false,
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
                item_id: "fc_1".to_string(),
                content_index: 0,
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
                        content_index: 0,
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
            assert_eq!(
                super::openai_websocket_stream_error(error).to_string(),
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
        ];

        for message in retryable {
            let error = CompletionError::ProviderError(message.to_string());
            assert!(
                super::is_openai_websocket_transient_start_error(&error),
                "{message} should be retryable"
            );
        }

        let invalid_request =
            CompletionError::ProviderError("invalid_request_error: Bad input".to_string());
        assert!(!super::is_openai_websocket_transient_start_error(
            &invalid_request
        ));
    }

    #[test]
    fn openai_websocket_retry_classification_stops_after_assistant_output() {
        let error =
            CompletionError::ProviderError("stream closed before response.completed".to_string());

        assert!(super::should_retry_openai_websocket_error(&error, false));
        assert!(!super::should_retry_openai_websocket_error(&error, true));
    }

    #[test]
    fn openai_websocket_retry_delay_grows_then_caps() {
        // Early retries back off exponentially from the base delay.
        assert_eq!(
            super::openai_websocket_retry_delay(1),
            super::OPENAI_WEBSOCKET_RETRY_BASE_DELAY
        );
        assert_eq!(
            super::openai_websocket_retry_delay(2),
            super::OPENAI_WEBSOCKET_RETRY_BASE_DELAY * 2
        );

        // The later attempts in the budget are clamped so the retry tail stays interactive, and a
        // huge retry count saturates to the cap instead of panicking on Duration overflow.
        assert!(
            super::openai_websocket_retry_delay(super::OPENAI_WEBSOCKET_RETRY_BUDGET)
                <= super::OPENAI_WEBSOCKET_RETRY_MAX_DELAY
        );
        assert_eq!(
            super::openai_websocket_retry_delay(64),
            super::OPENAI_WEBSOCKET_RETRY_MAX_DELAY
        );
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
        )
        .await?;

        drain_openai_websocket_stream(stream).await?;

        assert_eq!(handshakes.load(Ordering::SeqCst), 1);
        assert!(model.session_ws.lock().await.is_some());
        server.abort();
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

        let first =
            super::stream_openai_agent_completion(&model, Message::user("first"), &[]).await?;
        drain_session_completion_stream(first).await?;
        let second =
            super::stream_openai_agent_completion(&model, Message::user("second"), &[]).await?;
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
        )
        .await?;
        assert_eq!(openai_websocket_final_total_tokens(first).await?, 100);

        let second = super::openai_websocket_completion_stream(
            &model,
            openai_websocket_test_request("second")?,
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
        )
        .await?;
        assert_eq!(openai_websocket_final_total_tokens(first).await?, 100);

        let second = super::openai_websocket_completion_stream(
            &model,
            openai_websocket_test_request("second")?,
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
        )
        .await?;
        assert_eq!(openai_websocket_final_total_tokens(first).await?, 100);

        tokio::time::sleep(Duration::from_millis(50)).await;

        let second = super::openai_websocket_completion_stream(
            &model,
            openai_websocket_test_request("second")?,
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

        let first =
            super::stream_openai_agent_completion(&model, Message::user("first"), &[]).await?;
        drain_session_completion_stream(first).await?;
        let second =
            super::stream_openai_agent_completion(&model, Message::user("second"), &[]).await?;
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
