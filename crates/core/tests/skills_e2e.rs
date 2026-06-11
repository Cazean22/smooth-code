//! End-to-end: the model invokes the `skill` tool during a turn and the
//! skill's instructions come back as the tool result the next stream sees.

use std::{
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex},
};

use anyhow::Result;
use futures_util::stream;
use rig::message::{Message, Text, ToolCall, ToolFunction, UserContent};
use smooth_core::{
    AgentControl, SessionAssistantContent, SessionCompletionEvent, SessionCompletionStream,
    SessionModel, SessionModelDriver, SessionModelFactory, SessionTurnSummary, SystemPromptKind,
    ThreadManagerState,
};
use smooth_protocol::ThreadId;
use tempfile::TempDir;
use tokio::sync::RwLock;
use tools::AskUserClient;

static CWD_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn final_text_stream(text: &str) -> SessionCompletionStream {
    Box::pin(stream::iter(vec![
        Ok(SessionCompletionEvent::AssistantItem(
            SessionAssistantContent::Text(Text {
                text: text.to_string(),
            }),
        )),
        Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
            assistant_message_id: Some("assistant-final".to_string()),
            response: text.to_string(),
        })),
    ]))
}

fn tool_result_texts(message: &Message) -> Vec<String> {
    match message {
        Message::User { content } => content
            .iter()
            .filter_map(|item| match item {
                UserContent::ToolResult(tool_result) => {
                    tool_result.content.iter().find_map(|item| match item {
                        rig::message::ToolResultContent::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                }
                _ => None,
            })
            .collect(),
        other => panic!("expected tool result message, got {other:?}"),
    }
}

/// First stream emits a `skill` tool call; the second asserts the tool result
/// carries the skill body. `call_tool` executes the real `SkillTool` against
/// the session cwd, exercising the same dispatch path the rig toolset uses.
struct SkillCallDriver {
    calls: Mutex<usize>,
    cwd: PathBuf,
}

impl SessionModelDriver for SkillCallDriver {
    fn stream_completion_turn(
        &self,
        prompt: Message,
        _history: Vec<Message>,
    ) -> Result<SessionCompletionStream> {
        let mut calls = self
            .calls
            .lock()
            .map_err(|_| anyhow::anyhow!("calls mutex"))?;
        let call_idx = *calls;
        *calls += 1;
        drop(calls);

        match call_idx {
            0 => {
                let tool_call = ToolCall::new(
                    "skill-1".to_string(),
                    ToolFunction::new(
                        "skill".to_string(),
                        serde_json::json!({ "skill": "deploy", "args": "to staging" }),
                    ),
                )
                .with_call_id("call-skill-1".to_string());
                Ok(Box::pin(stream::iter(vec![
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::ToolCall {
                            tool_call,
                            internal_call_id: "internal-skill-1".to_string(),
                        },
                    )),
                    Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
                        assistant_message_id: Some("assistant-skill".to_string()),
                        response: String::new(),
                    })),
                ])))
            }
            1 => {
                let texts = tool_result_texts(&prompt);
                assert_eq!(texts.len(), 1, "expected one skill tool result");
                assert!(
                    texts[0].contains("<skill-invocation skill=\"deploy\">"),
                    "tool result missing skill wrapper: {:?}",
                    texts[0]
                );
                assert!(
                    texts[0].contains("Run make deploy."),
                    "tool result missing skill body: {:?}",
                    texts[0]
                );
                assert!(
                    texts[0].ends_with("to staging"),
                    "tool result missing args: {:?}",
                    texts[0]
                );
                Ok(final_text_stream("skill followed"))
            }
            other => panic!("unexpected completion turn {other}"),
        }
    }

    fn call_tool(
        &self,
        tool_name: &str,
        args: &str,
    ) -> futures_util::future::BoxFuture<'static, Result<String>> {
        let tool_name = tool_name.to_string();
        let args = args.to_string();
        let cwd = self.cwd.clone();
        Box::pin(async move {
            assert_eq!(tool_name, "skill");
            let parsed = serde_json::from_str::<tools::SkillArgs>(&args)?;
            rig::tool::Tool::call(&tools::SkillTool::new(cwd), parsed)
                .await
                .map_err(Into::into)
        })
    }
}

struct SkillCallFactory;

impl SessionModelFactory for SkillCallFactory {
    fn build(
        &self,
        cwd: PathBuf,
        _thread_id: ThreadId,
        _ask_user_client: Option<AskUserClient>,
        _current_turn_id: Arc<RwLock<Option<String>>>,
        _system_prompt_kind: SystemPromptKind,
        _agent_control: AgentControl,
        _plan_mode: bool,
    ) -> Result<SessionModel> {
        Ok(SessionModel::Stub(Arc::new(SkillCallDriver {
            calls: Mutex::new(0),
            cwd,
        })))
    }
}

#[tokio::test]
async fn skill_tool_call_returns_skill_instructions_to_the_model() -> Result<()> {
    let _cwd_guard = CWD_LOCK.lock().map_err(|_| anyhow::anyhow!("cwd lock"))?;
    let workspace = TempDir::new()?;
    let original_cwd = std::env::current_dir()?;
    std::env::set_current_dir(workspace.path())?;

    // The session resolves paths against its creation-time cwd, so derive the
    // skills dir the same way (TempDir may be a symlink on macOS).
    let cwd = std::env::current_dir()?;
    let skill_dir = tools::skills_dir(&cwd).join("deploy");
    std::fs::create_dir_all(&skill_dir)?;
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\ndescription: Deploy the app\n---\nRun make deploy.",
    )?;

    let manager = ThreadManagerState::new(None, Some(Arc::new(SkillCallFactory))).await?;
    let started = manager.start_thread().await?;
    let mut events = manager.subscribe(started.thread_id).await?;

    manager
        .start_user_input(started.thread_id, "deploy this".to_string())
        .await?;

    let last_assistant_message = loop {
        let event =
            tokio::time::timeout(std::time::Duration::from_secs(2), events.recv()).await??;
        if let smooth_protocol::EventMsg::TurnCompleted(turn) = event.msg {
            break turn.last_assistant_message;
        }
    };
    // The second stream's assertions ran (the driver panics otherwise) and the
    // turn finished on the post-tool response.
    assert_eq!(last_assistant_message.as_deref(), Some("skill followed"));

    std::env::set_current_dir(original_cwd)?;
    Ok(())
}
