use std::{
    collections::{HashMap, HashSet},
    env,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use rig::{
    OneOrMany,
    agent::{Agent, FinalResponse, MultiTurnStreamItem},
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
        RawStreamingChoice, RawStreamingToolCall, StreamedAssistantContent, StreamedUserContent,
        StreamingChat, StreamingCompletionResponse as RigStreamingCompletionResponse,
        ToolCallDeltaContent,
    },
};
use serde::Serialize;
use tokio::sync::RwLock;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message as TungsteniteMessage, client::IntoClientRequest},
};
use tools::{
    AskUserClient, AskUserQuestionTool, DeleteTool, EditTool, ExitPlanModeTool, PlanWriteTool,
    ReadTool, RunCommandTool, SpawnAgentTool, WriteTool,
};

use crate::agent::{
    AgentControl, PLAN_MODE_INSTRUCTIONS,
    role::{RoleOverride, render_spawn_agent_tool_description},
};
use crate::environment::EnvironmentContext;

const DEFAULT_SYSTEM_PROMPT: &str = include_str!("../../../docs/system_prompt.md");

/// Injectable builder for session-scoped models.
pub trait SessionModelFactory: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    fn build(
        &self,
        cwd: PathBuf,
        thread_id: smooth_protocol::ThreadId,
        ask_user_client: Option<AskUserClient>,
        current_turn_id: Arc<RwLock<Option<String>>>,
        role_override: RoleOverride,
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
        role_override: RoleOverride,
        agent_control: AgentControl,
        plan_mode: bool,
    ) -> Result<SessionModel> {
        SessionModel::from_env(
            cwd,
            thread_id,
            ask_user_client,
            current_turn_id,
            role_override,
            agent_control,
            plan_mode,
        )
    }
}

/// Test seam for custom streaming behavior.
pub trait SessionModelDriver: Send + Sync {
    fn stream_turn(&self, prompt: Message, history: Vec<Message>) -> Result<SessionStream>;

    fn supports_manual_tool_loop(&self) -> bool {
        false
    }

    fn stream_completion_turn(
        &self,
        _prompt: Message,
        _history: Vec<Message>,
    ) -> Result<SessionCompletionStream> {
        Err(anyhow!("manual completion streaming is not supported"))
    }

    fn call_tool(&self, _tool_name: &str, _args: &str) -> Result<String> {
        Err(anyhow!("manual tool execution is not supported"))
    }
}

#[derive(Debug)]
pub enum SessionStreamEvent {
    StreamAssistantItem(SessionAssistantContent),
    StreamUserItem(StreamedUserContent),
    FinalResponse(FinalResponse),
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

pub type SessionStream =
    Pin<Box<dyn futures_util::Stream<Item = Result<SessionStreamEvent>> + Send>>;

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

#[derive(Clone)]
pub struct OpenAiSessionModel {
    agent: Arc<Agent<openai::responses_api::ResponsesCompletionModel>>,
    client: openai::Client,
    model: String,
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
        role_override: RoleOverride,
        agent_control: AgentControl,
        plan_mode: bool,
    ) -> Result<Self> {
        let provider = env::var("SMOOTH_CODE_LLM_PROVIDER")
            .unwrap_or_else(|_| "openai".to_string())
            .to_ascii_lowercase();
        let model = role_override
            .model
            .clone()
            .or_else(|| env::var("SMOOTH_CODE_LLM_MODEL").ok())
            .unwrap_or_else(|| "gpt-5.5".to_string());
        let environment_context = EnvironmentContext::gather(&cwd);
        let preamble = compose_session_preamble(
            &role_override,
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
                    agent_control.clone(),
                    plan_mode,
                );
                Ok(Self::OpenAi(Arc::new(OpenAiSessionModel {
                    agent: Arc::new(agent),
                    client,
                    model,
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
                    agent_control,
                    plan_mode,
                ))))
            }
            other => bail!("unsupported SMOOTH_CODE_LLM_PROVIDER `{other}`"),
        }
    }

    pub(crate) async fn stream_turn(
        &self,
        prompt: Message,
        history: &[Message],
    ) -> Result<SessionStream> {
        match self {
            Self::OpenAi(_) => Err(anyhow!(
                "OpenAI opaque streaming is not supported; OpenAI turns use the manual WebSocket tool loop"
            )),
            Self::OpenRouter(agent) => stream_agent(agent, prompt, history).await,
            Self::Anthropic(agent) => stream_agent(agent, prompt, history).await,
            Self::Gemini(agent) => stream_agent(agent, prompt, history).await,
            Self::Stub(driver) => driver.stream_turn(prompt, history.to_vec()),
        }
    }

    pub(crate) fn supports_manual_tool_loop(&self) -> bool {
        match self {
            Self::Stub(driver) => driver.supports_manual_tool_loop(),
            _ => true,
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
        _role_override: RoleOverride,
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
    role_override: &RoleOverride,
    env_preamble: Option<String>,
    environment_context: &EnvironmentContext,
    plan_mode: bool,
) -> String {
    let base_preamble = env_preamble.unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string());
    let mut preamble = environment_context.apply(&base_preamble);
    if let Some(role_preamble) = role_override
        .preamble
        .as_deref()
        .map(str::trim)
        .filter(|preamble| !preamble.is_empty())
    {
        preamble = format!("{}\n\n{role_preamble}", preamble.trim_end());
    }
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

async fn stream_agent<M>(
    agent: &Arc<Agent<M>>,
    prompt: Message,
    history: &[Message],
) -> Result<SessionStream>
where
    M: rig::completion::CompletionModel + 'static,
    M::StreamingResponse: Clone + Unpin + rig::completion::GetTokenUsage,
{
    let stream = agent.stream_chat(prompt, history.iter().cloned()).await;
    Ok(Box::pin(stream_to_events(stream)))
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
const OPENAI_WEBSOCKET_START_ATTEMPTS: usize = 2;

#[derive(Debug, Serialize)]
struct OpenAiWebSocketCreateEvent {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(flatten)]
    request: OpenAiResponsesCompletionRequest,
}

struct OpenAiWebSocketPayloadOutcome {
    choices: Vec<OpenAiWebSocketRawChoice>,
    terminal: bool,
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
        for attempt in 1..=OPENAI_WEBSOCKET_START_ATTEMPTS {
            let mut stream = match openai_websocket_completion_stream(&openai, completion_request.clone()).await {
                Ok(stream) => stream,
                Err(error)
                    if attempt < OPENAI_WEBSOCKET_START_ATTEMPTS
                        && is_openai_websocket_transient_start_error(&error) =>
                {
                    tracing::debug!(
                        attempt,
                        error = %error,
                        "OpenAI WebSocket transient failure before the turn stream started; retrying"
                    );
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
                        if !yielded_assistant_item
                            && attempt < OPENAI_WEBSOCKET_START_ATTEMPTS
                            && is_openai_websocket_transient_start_error(&error) =>
                    {
                        tracing::debug!(
                            attempt,
                            error = %error,
                            "OpenAI WebSocket transient failure before any assistant item; retrying"
                        );
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
    openai: &OpenAiSessionModel,
    completion_request: CompletionRequest,
) -> std::result::Result<
    RigStreamingCompletionResponse<OpenAiStreamingCompletionResponse>,
    CompletionError,
> {
    let mut socket = openai_websocket_connect(&openai.client).await?;
    let payload = openai_websocket_create_payload(openai.model.as_str(), completion_request)?;
    socket
        .send(TungsteniteMessage::Text(payload))
        .await
        .map_err(openai_websocket_provider_error)?;
    Ok(openai_websocket_stream(socket))
}

fn openai_websocket_stream(
    mut socket: OpenAiWebSocket,
) -> RigStreamingCompletionResponse<OpenAiStreamingCompletionResponse> {
    let raw_stream = async_stream::try_stream! {
        let mut accumulator = OpenAiWebSocketAccumulator::new();
        let mut terminal_error = None;

        loop {
            match socket.next().await {
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
                    match parse_openai_websocket_payload(&payload, &mut accumulator) {
                        Ok(outcome) => {
                            for choice in outcome.choices {
                                yield choice;
                            }
                            if outcome.terminal {
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

        if let Err(error) = socket.close(None).await {
            tracing::debug!(
                error = %error,
                "failed to close OpenAI WebSocket session cleanly"
            );
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
        return Ok(OpenAiWebSocketPayloadOutcome {
            choices: Vec::new(),
            terminal: false,
        });
    };

    match kind.as_str() {
        "error" => {
            let message = value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("OpenAI WebSocket error");
            Err(CompletionError::ProviderError(message.to_string()))
        }
        "response.done" => {
            let response_value = value.get("response").ok_or_else(|| {
                CompletionError::ProviderError(
                    "OpenAI WebSocket response.done was missing response".to_string(),
                )
            })?;
            accumulator.record_done_response_value(response_value)?;
            Ok(OpenAiWebSocketPayloadOutcome {
                choices: Vec::new(),
                terminal: true,
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
            accumulator.record_response_value(response_kind, response_value)?;
            Ok(OpenAiWebSocketPayloadOutcome {
                choices: Vec::new(),
                terminal,
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
                terminal: false,
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
                terminal: false,
            })
        }
        "response.output_text.done" | "response.refusal.done" => {
            accumulator.mark_completed_message_item(openai_websocket_item_id(&value));
            Ok(OpenAiWebSocketPayloadOutcome {
                choices: Vec::new(),
                terminal: false,
            })
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
            Ok(OpenAiWebSocketPayloadOutcome {
                choices: Vec::new(),
                terminal: false,
            })
        }
        "response.function_call_arguments.delta" => {
            let Some(item_id) = value
                .get("item_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
            else {
                return Ok(OpenAiWebSocketPayloadOutcome {
                    choices: Vec::new(),
                    terminal: false,
                });
            };
            let Some(delta) = value.get("delta").and_then(serde_json::Value::as_str) else {
                return Ok(OpenAiWebSocketPayloadOutcome {
                    choices: Vec::new(),
                    terminal: false,
                });
            };
            let internal_call_id = accumulator.internal_call_id_for(&item_id);
            Ok(OpenAiWebSocketPayloadOutcome {
                choices: vec![RawStreamingChoice::ToolCallDelta {
                    id: item_id,
                    internal_call_id,
                    content: ToolCallDeltaContent::Delta(delta.to_string()),
                }],
                terminal: false,
            })
        }
        "response.function_call_arguments.done" => {
            if let Some(item_id) = value.get("item_id").and_then(serde_json::Value::as_str) {
                let arguments = openai_websocket_arguments_value(&value);
                accumulator.record_tool_call_args_done(item_id, arguments);
            }
            Ok(OpenAiWebSocketPayloadOutcome {
                choices: Vec::new(),
                terminal: false,
            })
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
                terminal: false,
            })
        }
        _ => Ok(OpenAiWebSocketPayloadOutcome {
            choices: Vec::new(),
            terminal: false,
        }),
    }
}

fn openai_websocket_item_id(value: &serde_json::Value) -> Option<String> {
    value
        .get("item_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
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
    let message = error.to_string();
    message.contains("Connection reset without closing handshake")
        || message.contains("connection reset without closing handshake")
        || message.contains("OpenAI WebSocket connection reset before response.completed")
}

fn is_openai_websocket_transient_start_error(error: &CompletionError) -> bool {
    let message = error.to_string();
    is_openai_websocket_connection_reset(error)
        || message.contains("The OpenAI WebSocket connection closed before the turn finished")
        || message.contains("The OpenAI WebSocket connection closed without a close reason")
        || message.contains("An error occurred while processing the request.")
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
    ) -> std::result::Result<(), CompletionError> {
        match kind {
            ResponseChunkKind::ResponseCompleted => {
                self.record_completed_response_value(response);
                Ok(())
            }
            ResponseChunkKind::ResponseFailed => Err(CompletionError::ProviderError(
                openai_response_error_message_value(
                    response,
                    "OpenAI WebSocket returned a failed response",
                ),
            )),
            ResponseChunkKind::ResponseIncomplete => Err(CompletionError::ProviderError(
                openai_incomplete_response_message_value(response),
            )),
            ResponseChunkKind::ResponseCreated | ResponseChunkKind::ResponseInProgress => Ok(()),
        }
    }

    fn record_done_response_value(
        &mut self,
        response: &serde_json::Value,
    ) -> std::result::Result<(), CompletionError> {
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
                Ok(())
            }
            ResponseStatus::Failed => Err(CompletionError::ProviderError(
                openai_response_error_message_value(
                    response,
                    "OpenAI WebSocket returned a failed response",
                ),
            )),
            ResponseStatus::Incomplete => Err(CompletionError::ProviderError(
                openai_incomplete_response_message_value(response),
            )),
            status => Err(CompletionError::ProviderError(format!(
                "OpenAI WebSocket response ended with status {status:?}"
            ))),
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

fn stream_to_events<R>(
    mut stream: Pin<
        Box<
            dyn futures_util::Stream<
                    Item = Result<MultiTurnStreamItem<R>, rig::agent::StreamingError>,
                > + Send,
        >,
    >,
) -> impl futures_util::Stream<Item = Result<SessionStreamEvent>> + Send
where
    R: Clone + Unpin + rig::completion::GetTokenUsage + Send,
{
    async_stream::try_stream! {
        while let Some(item) = stream.next().await {
            match item? {
                MultiTurnStreamItem::StreamAssistantItem(assistant_item) => {
                    let assistant_item = match assistant_item {
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
                    };
                    yield SessionStreamEvent::StreamAssistantItem(assistant_item);
                }
                MultiTurnStreamItem::StreamUserItem(user_item) => {
                    yield SessionStreamEvent::StreamUserItem(user_item);
                }
                MultiTurnStreamItem::FinalResponse(final_response) => {
                    yield SessionStreamEvent::FinalResponse(final_response);
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, sync::Arc};

    use anyhow::{Context, Result};
    use rig::{
        OneOrMany,
        agent::AgentBuilder,
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
    use tokio::sync::RwLock;

    use super::{build_agent, compose_session_preamble};
    use crate::{
        agent::{AgentControl, PLAN_MODE_INSTRUCTIONS, role::RoleOverride},
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

    #[test]
    fn default_session_preamble_uses_markdown_system_prompt() {
        let preamble = compose_session_preamble(
            &RoleOverride::default(),
            None,
            &environment_context(),
            false,
        );

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
            &RoleOverride::default(),
            Some("Custom prompt for ${shell}.".to_string()),
            &environment_context(),
            false,
        );

        assert_eq!(preamble, "Custom prompt for /bin/zsh.");
    }

    #[test]
    fn role_and_plan_mode_preambles_layer_on_base_prompt() {
        let role_override = RoleOverride {
            preamble: Some("Role-specific instructions for ${shell}.".to_string()),
            model: None,
        };

        let preamble = compose_session_preamble(
            &role_override,
            Some("Base prompt.\n".to_string()),
            &environment_context(),
            true,
        );

        assert_eq!(
            preamble,
            format!(
                "Base prompt.\n\nRole-specific instructions for ${{shell}}.\n\n{PLAN_MODE_INSTRUCTIONS}"
            )
        );
    }

    #[tokio::test]
    async fn build_agent_registers_only_spawn_agent_agent_tool() -> Result<()> {
        let workspace = tempfile::TempDir::new()?;
        let agent = build_agent(
            AgentBuilder::new(DummyModel),
            workspace.path().to_path_buf(),
            smooth_protocol::ThreadId::new(),
            None,
            Arc::new(RwLock::new(None)),
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
    async fn build_agent_in_plan_mode_swaps_mutating_for_planning_tools() -> Result<()> {
        let workspace = tempfile::TempDir::new()?;
        let agent = build_agent(
            AgentBuilder::new(DummyModel),
            workspace.path().to_path_buf(),
            smooth_protocol::ThreadId::new(),
            None,
            Arc::new(RwLock::new(None)),
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
        let error = CompletionError::ProviderError(
            "WebSocket protocol error: Connection reset without closing handshake".to_string(),
        );

        assert!(super::is_openai_websocket_connection_reset(&error));
        assert!(super::is_openai_websocket_transient_start_error(&error));
        assert_eq!(
            super::openai_websocket_stream_error(error).to_string(),
            "ProviderError: OpenAI WebSocket connection reset before response.completed"
        );
    }

    #[test]
    fn openai_websocket_transient_start_errors_are_retryable() {
        let retryable = [
            "OpenAI WebSocket connection reset before response.completed",
            "The OpenAI WebSocket connection closed before the turn finished",
            "The OpenAI WebSocket connection closed without a close reason",
            "An error occurred while processing the request.",
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
