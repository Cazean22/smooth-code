use std::{collections::HashMap, env, path::PathBuf, pin::Pin, sync::Arc};

use anyhow::{Result, anyhow, bail};
use futures_util::StreamExt;
use rig::{
    agent::{Agent, FinalResponse, MultiTurnStreamItem},
    client::CompletionClient,
    completion::Completion,
    message::{Message, Reasoning as MessageReasoning, Text, ToolCall},
    providers::{
        anthropic, gemini,
        openai::{
            self,
            responses_api::{AdditionalParameters, Reasoning as OpenAiReasoning, ReasoningEffort},
        },
        openrouter,
    },
    streaming::{
        StreamedAssistantContent, StreamedUserContent, StreamingChat, ToolCallDeltaContent,
    },
};
use tokio::sync::RwLock;
use tools::{
    DynamicTool, DynamicToolClient, EditTool, ListDirTool, ReadTool, RunCommandTool,
    SpawnAgentTool, WriteTool,
};

use crate::agent::{
    AgentControl,
    role::{RoleOverride, render_spawn_agent_tool_description},
};

/// Injectable builder for session-scoped models.
pub trait SessionModelFactory: Send + Sync {
    fn build(
        &self,
        cwd: PathBuf,
        thread_id: smooth_protocol::ThreadId,
        dynamic_tool_client: Option<Arc<dyn DynamicToolClient>>,
        current_turn_id: Arc<RwLock<Option<String>>>,
        role_override: RoleOverride,
        agent_control: AgentControl,
    ) -> Result<SessionModel>;
}

/// Default environment-backed `SessionModelFactory`.
pub struct EnvSessionModelFactory;

impl SessionModelFactory for EnvSessionModelFactory {
    fn build(
        &self,
        cwd: PathBuf,
        thread_id: smooth_protocol::ThreadId,
        dynamic_tool_client: Option<Arc<dyn DynamicToolClient>>,
        current_turn_id: Arc<RwLock<Option<String>>>,
        role_override: RoleOverride,
        agent_control: AgentControl,
    ) -> Result<SessionModel> {
        SessionModel::from_env(
            cwd,
            thread_id,
            dynamic_tool_client,
            current_turn_id,
            role_override,
            agent_control,
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
    OpenAi(Arc<Agent<openai::responses_api::ResponsesCompletionModel>>),
    OpenRouter(Arc<Agent<openrouter::CompletionModel>>),
    Anthropic(Arc<Agent<anthropic::completion::CompletionModel>>),
    Gemini(Arc<Agent<gemini::completion::CompletionModel>>),
    Stub(Arc<dyn SessionModelDriver>),
}

impl SessionModel {
    pub(crate) fn requires_provider_reasoning_ids(&self) -> bool {
        matches!(self, Self::OpenAi(_))
    }

    pub fn from_env(
        cwd: PathBuf,
        thread_id: smooth_protocol::ThreadId,
        dynamic_tool_client: Option<Arc<dyn DynamicToolClient>>,
        current_turn_id: Arc<RwLock<Option<String>>>,
        role_override: RoleOverride,
        agent_control: AgentControl,
    ) -> Result<Self> {
        let provider = env::var("SMOOTH_CODE_LLM_PROVIDER")
            .unwrap_or_else(|_| "openai".to_string())
            .to_ascii_lowercase();
        let model = role_override
            .model
            .clone()
            .or_else(|| env::var("SMOOTH_CODE_LLM_MODEL").ok())
            .unwrap_or_else(|| "gpt-5.4".to_string());
        let preamble = role_override
            .preamble
            .clone()
            .or_else(|| env::var("SMOOTH_CODE_LLM_PREAMBLE").ok())
            .unwrap_or_else(|| "You are smooth-code, a code agent.".to_string());

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
                Ok(Self::OpenAi(Arc::new(build_agent(
                    client
                        .agent(&model)
                        .preamble(&preamble)
                        .additional_params(additional_params.to_json()),
                    cwd,
                    thread_id,
                    dynamic_tool_client.clone(),
                    Arc::clone(&current_turn_id),
                    agent_control.clone(),
                ))))
            }
            "openrouter" => {
                let client = openrouter::Client::new(&env::var("OPENROUTER_API_KEY")?)?;
                Ok(Self::OpenRouter(Arc::new(build_agent(
                    client.agent(&model).preamble(&preamble),
                    cwd,
                    thread_id,
                    dynamic_tool_client.clone(),
                    Arc::clone(&current_turn_id),
                    agent_control.clone(),
                ))))
            }
            "anthropic" => {
                let client = anthropic::Client::new(env::var("ANTHROPIC_API_KEY")?)?;
                Ok(Self::Anthropic(Arc::new(build_agent(
                    client.agent(&model).preamble(&preamble),
                    cwd,
                    thread_id,
                    dynamic_tool_client.clone(),
                    Arc::clone(&current_turn_id),
                    agent_control.clone(),
                ))))
            }
            "gemini" => {
                let client = gemini::Client::new(env::var("GEMINI_API_KEY")?)?;
                Ok(Self::Gemini(Arc::new(build_agent(
                    client.agent(&model).preamble(&preamble),
                    cwd,
                    thread_id,
                    dynamic_tool_client,
                    current_turn_id,
                    agent_control,
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
            Self::OpenAi(agent) => stream_agent(agent, prompt, history).await,
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
            Self::OpenAi(agent) => stream_agent_completion(agent, prompt, history).await,
            Self::OpenRouter(agent) => stream_agent_completion(agent, prompt, history).await,
            Self::Anthropic(agent) => stream_agent_completion(agent, prompt, history).await,
            Self::Gemini(agent) => stream_agent_completion(agent, prompt, history).await,
            Self::Stub(driver) => driver.stream_completion_turn(prompt, history.to_vec()),
        }
    }

    pub(crate) async fn call_tool(&self, tool_name: &str, args: &str) -> Result<String> {
        match self {
            Self::OpenAi(agent) => call_agent_tool(agent, tool_name, args).await,
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
        _dynamic_tool_client: Option<Arc<dyn DynamicToolClient>>,
        _current_turn_id: Arc<RwLock<Option<String>>>,
        _role_override: RoleOverride,
        _agent_control: AgentControl,
    ) -> Result<SessionModel> {
        self.models
            .lock()
            .expect("stub session model factory mutex should lock")
            .get(&thread_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing stub session model for thread {thread_id}"))
    }
}

fn build_agent<M>(
    builder: rig::agent::AgentBuilder<M, (), rig::agent::NoToolConfig>,
    cwd: PathBuf,
    thread_id: smooth_protocol::ThreadId,
    dynamic_tool_client: Option<Arc<dyn DynamicToolClient>>,
    current_turn_id: Arc<RwLock<Option<String>>>,
    _agent_control: AgentControl,
) -> Agent<M>
where
    M: rig::completion::CompletionModel,
{
    let builder = builder
        .tool(EditTool::new(cwd.clone()))
        .tool(ListDirTool::new(cwd.clone()))
        .tool(ReadTool::new(cwd.clone()))
        .tool(RunCommandTool::new(cwd.clone()))
        .tool(WriteTool::new(cwd));
    let builder = if let Some(dynamic_tool_client) = dynamic_tool_client {
        builder.tool(DynamicTool::new(
            "dynamic_echo",
            thread_id,
            dynamic_tool_client,
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
            let assistant_item = match item? {
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

    use rig::{
        agent::AgentBuilder,
        completion::{
            CompletionError, CompletionModel, CompletionRequest, CompletionResponse, Usage,
        },
        streaming::StreamingCompletionResponse,
    };
    use serde::{Deserialize, Serialize};
    use tokio::sync::RwLock;

    use super::build_agent;
    use crate::agent::AgentControl;

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

    #[tokio::test]
    async fn build_agent_registers_only_spawn_agent_agent_tool() {
        let workspace = tempfile::TempDir::new().expect("tempdir");
        let agent = build_agent(
            AgentBuilder::new(DummyModel),
            workspace.path().to_path_buf(),
            smooth_protocol::ThreadId::new(),
            None,
            Arc::new(RwLock::new(None)),
            AgentControl::new(),
        );

        let tool_names = agent
            .tool_server_handle
            .get_tool_defs(None)
            .await
            .expect("tool definitions")
            .into_iter()
            .map(|definition| definition.name)
            .collect::<HashSet<_>>();

        assert!(tool_names.contains("spawn_agent"));
        assert!(!tool_names.contains("send_message"));
        assert!(!tool_names.contains("list_agents"));
        assert!(!tool_names.contains("close_agent"));
    }
}
