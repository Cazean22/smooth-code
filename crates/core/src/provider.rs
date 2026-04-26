use std::{env, sync::Arc};

use anyhow::{Result, anyhow, bail};
use futures_util::StreamExt;
use rig::{
    agent::Agent,
    client::CompletionClient,
    message::{AssistantContent, Message},
    providers::{anthropic, gemini, openai, openrouter},
    streaming::{StreamedAssistantContent, StreamingCompletion},
};

pub(crate) enum SessionModel {
    OpenAi(Arc<Agent<openai::responses_api::ResponsesCompletionModel>>),
    OpenRouter(Arc<Agent<openrouter::CompletionModel>>),
    Anthropic(Arc<Agent<anthropic::completion::CompletionModel>>),
    Gemini(Arc<Agent<gemini::completion::CompletionModel>>),
}

impl SessionModel {
    pub(crate) fn from_env() -> Result<Self> {
        let provider = env::var("SMOOTH_CODE_LLM_PROVIDER")
            .unwrap_or_else(|_| "openai".to_string())
            .to_ascii_lowercase();
        let model = env::var("SMOOTH_CODE_LLM_MODEL").unwrap_or_else(|_| "gpt-5.4".to_string());
        let preamble = env::var("SMOOTH_CODE_LLM_PREAMBLE")
            .unwrap_or_else(|_| "You are smooth-code, a code agent.".to_string());

        match provider.as_str() {
            "openai" => {
                let mut builder = openai::Client::builder().api_key(&env::var("OPENAI_API_KEY")?);
                if let Ok(base_url) = env::var("OPENAI_BASE_URL") {
                    builder = builder.base_url(&base_url);
                }
                let client = builder.build()?;
                Ok(Self::OpenAi(Arc::new(
                    client.agent(&model).preamble(&preamble).build(),
                )))
            }
            "openrouter" => {
                let client = openrouter::Client::new(&env::var("OPENROUTER_API_KEY")?)?;
                Ok(Self::OpenRouter(Arc::new(
                    client.agent(&model).preamble(&preamble).build(),
                )))
            }
            "anthropic" => {
                let client = anthropic::Client::new(env::var("ANTHROPIC_API_KEY")?)?;
                Ok(Self::Anthropic(Arc::new(
                    client.agent(&model).preamble(&preamble).build(),
                )))
            }
            "gemini" => {
                let client = gemini::Client::new(env::var("GEMINI_API_KEY")?)?;
                Ok(Self::Gemini(Arc::new(
                    client.agent(&model).preamble(&preamble).build(),
                )))
            }
            other => bail!("unsupported SMOOTH_CODE_LLM_PROVIDER `{other}`"),
        }
    }

    pub(crate) async fn complete_turn(
        &self,
        prompt: Message,
        history: &[Message],
        mut on_text: impl FnMut(String) + Send,
    ) -> Result<String> {
        match self {
            Self::OpenAi(agent) => stream_agent(agent, prompt, history, &mut on_text).await,
            Self::OpenRouter(agent) => stream_agent(agent, prompt, history, &mut on_text).await,
            Self::Anthropic(agent) => stream_agent(agent, prompt, history, &mut on_text).await,
            Self::Gemini(agent) => stream_agent(agent, prompt, history, &mut on_text).await,
        }
    }
}

async fn stream_agent<M>(
    agent: &Arc<Agent<M>>,
    prompt: Message,
    history: &[Message],
    on_text: &mut (impl FnMut(String) + Send),
) -> Result<String>
where
    M: rig::completion::CompletionModel,
    M::StreamingResponse: Clone + Unpin + rig::completion::GetTokenUsage,
{
    let mut stream = agent
        .stream_completion(prompt, history.iter().cloned())
        .await?
        .stream()
        .await?;
    let mut final_text = String::new();

    while let Some(chunk) = stream.next().await {
        match chunk? {
            StreamedAssistantContent::Text(text) => {
                on_text(text.text.clone());
                final_text.push_str(&text.text);
            }
            StreamedAssistantContent::Final(_) => {}
            StreamedAssistantContent::Reasoning(_)
            | StreamedAssistantContent::ReasoningDelta { .. } => {}
            StreamedAssistantContent::ToolCall { tool_call, .. } => {
                return Err(anyhow!(
                    "tool call `{}` is not implemented in smooth-code yet",
                    tool_call.function.name
                ));
            }
            StreamedAssistantContent::ToolCallDelta { .. } => {}
        }
    }

    if !final_text.is_empty() {
        return Ok(final_text);
    }

    let fallback = stream
        .choice
        .clone()
        .into_iter()
        .filter_map(|content| match content {
            AssistantContent::Text(text) => Some(text.text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");

    Ok(fallback)
}
