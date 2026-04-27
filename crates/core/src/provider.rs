use std::{env, path::PathBuf, pin::Pin, sync::Arc};

use anyhow::{Result, bail};
use futures_util::StreamExt;
use rig::{
    agent::{Agent, MultiTurnStreamItem},
    client::CompletionClient,
    message::{Message, ToolCall, ToolResult},
    providers::{
        anthropic, gemini,
        openai::{
            self,
            responses_api::{AdditionalParameters, Reasoning, ReasoningEffort},
        },
        openrouter,
    },
    streaming::{StreamedAssistantContent, StreamedUserContent, StreamingChat},
};

use crate::tools::{ListDirTool, ReadFileTool, RunCommandTool};

pub(crate) enum SessionStreamEvent {
    TextDelta(String),
    ToolCall {
        tool_call: ToolCall,
        internal_call_id: String,
    },
    ToolResult {
        tool_result: ToolResult,
        internal_call_id: String,
    },
    Final {
        response: String,
        history: Vec<Message>,
    },
}

type SessionStream = Pin<Box<dyn futures_util::Stream<Item = Result<SessionStreamEvent>> + Send>>;

pub(crate) enum SessionModel {
    OpenAi(Arc<Agent<openai::responses_api::ResponsesCompletionModel>>),
    OpenRouter(Arc<Agent<openrouter::CompletionModel>>),
    Anthropic(Arc<Agent<anthropic::completion::CompletionModel>>),
    Gemini(Arc<Agent<gemini::completion::CompletionModel>>),
}

impl SessionModel {
    pub(crate) fn from_env(cwd: PathBuf) -> Result<Self> {
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
                    reasoning: Some(Reasoning::new().with_effort(ReasoningEffort::High)),
                    ..Default::default()
                };
                Ok(Self::OpenAi(Arc::new(build_agent(
                    client
                        .agent(&model)
                        .preamble(&preamble)
                        .additional_params(additional_params.to_json()),
                    cwd,
                ))))
            }
            "openrouter" => {
                let client = openrouter::Client::new(&env::var("OPENROUTER_API_KEY")?)?;
                Ok(Self::OpenRouter(Arc::new(build_agent(
                    client.agent(&model).preamble(&preamble),
                    cwd,
                ))))
            }
            "anthropic" => {
                let client = anthropic::Client::new(env::var("ANTHROPIC_API_KEY")?)?;
                Ok(Self::Anthropic(Arc::new(build_agent(
                    client.agent(&model).preamble(&preamble),
                    cwd,
                ))))
            }
            "gemini" => {
                let client = gemini::Client::new(env::var("GEMINI_API_KEY")?)?;
                Ok(Self::Gemini(Arc::new(build_agent(
                    client.agent(&model).preamble(&preamble),
                    cwd,
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
) -> Agent<M>
where
    M: rig::completion::CompletionModel,
{
    builder
        .tool(ListDirTool::new(cwd.clone()))
        .tool(ReadFileTool::new(cwd.clone()))
        .tool(RunCommandTool::new(cwd))
        .build()
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
                MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(text)) => {
                    yield SessionStreamEvent::TextDelta(text.text);
                }
                MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall {
                    tool_call,
                    internal_call_id,
                }) => {
                    yield SessionStreamEvent::ToolCall {
                        tool_call,
                        internal_call_id,
                    };
                }
                MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
                    tool_result,
                    internal_call_id,
                }) => {
                    yield SessionStreamEvent::ToolResult {
                        tool_result,
                        internal_call_id,
                    };
                }
                MultiTurnStreamItem::FinalResponse(final_response) => {
                    yield SessionStreamEvent::Final {
                        response: final_response.response().to_string(),
                        history: final_response.history().unwrap_or(&[]).to_vec(),
                    };
                }
                MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Reasoning(_))
                | MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ReasoningDelta { .. })
                | MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCallDelta { .. })
                | MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Final(_)) => {}
                _ => {}
            }
        }
    }
}
