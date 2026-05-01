use std::{env, path::PathBuf, pin::Pin, sync::Arc};

use anyhow::{Result, bail};
use futures_util::StreamExt;
use rig::{
    agent::{Agent, FinalResponse, MultiTurnStreamItem},
    client::CompletionClient,
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
use tokio::sync::watch;

use crate::tools::{DynamicTool, DynamicToolClient, ListDirTool, ReadFileTool, RunCommandTool};

#[derive(Debug)]
pub(crate) enum SessionStreamEvent {
    StreamAssistantItem(SessionAssistantContent),
    StreamUserItem(StreamedUserContent),
    FinalResponse(FinalResponse),
}

#[derive(Debug)]
pub(crate) enum SessionAssistantContent {
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

type SessionStream = Pin<Box<dyn futures_util::Stream<Item = Result<SessionStreamEvent>> + Send>>;

pub(crate) enum SessionModel {
    OpenAi(Arc<Agent<openai::responses_api::ResponsesCompletionModel>>),
    OpenRouter(Arc<Agent<openrouter::CompletionModel>>),
    Anthropic(Arc<Agent<anthropic::completion::CompletionModel>>),
    Gemini(Arc<Agent<gemini::completion::CompletionModel>>),
}

impl SessionModel {
    pub(crate) fn from_env(
        cwd: PathBuf,
        thread_id: smooth_protocol::ThreadId,
        dynamic_tool_client: Option<Arc<dyn DynamicToolClient>>,
        current_turn_id: Arc<watch::Sender<Option<String>>>,
    ) -> Result<Self> {
        let provider = env::var("SMOOTH_CODE_LLM_PROVIDER")
            .unwrap_or_else(|_| "openai".to_string())
            .to_ascii_lowercase();
        let model = env::var("SMOOTH_CODE_LLM_MODEL").unwrap_or_else(|_| "gpt-5.4".to_string());
        let preamble = env::var("SMOOTH_CODE_LLM_PREAMBLE")
            .unwrap_or_else(|_| "You are smooth-code, a code agent.".to_string());

        match provider.as_str() {
            "openai" => {
                let mut builder = openai::Client::builder().api_key("cazean");
                builder = builder.base_url("http://localhost:8317/v1");
                let client = builder.build()?;
                let additional_params = AdditionalParameters {
                    reasoning: Some(OpenAiReasoning::new().with_effort(ReasoningEffort::High)),
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
        }
    }
}

fn build_agent<M>(
    builder: rig::agent::AgentBuilder<M, (), rig::agent::NoToolConfig>,
    cwd: PathBuf,
    thread_id: smooth_protocol::ThreadId,
    dynamic_tool_client: Option<Arc<dyn DynamicToolClient>>,
    current_turn_id: Arc<watch::Sender<Option<String>>>,
) -> Agent<M>
where
    M: rig::completion::CompletionModel,
{
    let builder = builder
        .tool(ListDirTool::new(cwd.clone()))
        .tool(ReadFileTool::new(cwd.clone()))
        .tool(RunCommandTool::new(cwd));
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
