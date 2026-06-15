use std::{
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};

use anyhow::Result;
use app_server_protocol::{
    AskUserQuestionResponse, PlanApprovalDecision, RequestPlanApprovalResponse,
};
use futures_util::stream;
use rig::message::{Message, Text, ToolCall, ToolFunction, UserContent};
use smooth_core::{
    AgentControl, SessionAssistantContent, SessionCompletionEvent, SessionCompletionStream,
    SessionModel, SessionModelDriver, SessionModelFactory, SessionTurnSummary, SystemPromptKind,
    ThreadManagerState,
};
use smooth_protocol::{EventMsg, ThreadId};
use tempfile::TempDir;
use tokio::sync::RwLock;
use tools::AskUserClient;

static CWD_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn final_text_stream(text: &str) -> SessionCompletionStream {
    Box::pin(stream::iter(vec![
        Ok(SessionCompletionEvent::AssistantItem(
            SessionAssistantContent::Text(Text {
                text: text.to_string(),
                additional_params: None,
            }),
        )),
        Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
            assistant_message_id: Some("assistant-final".to_string()),
            response: text.to_string(),
        })),
    ]))
}

fn assert_exit_plan_result(prompt: &Message, expected_snippets: &[&str]) {
    let texts = tool_result_texts(prompt);
    assert_eq!(texts.len(), 1, "expected one exit_plan_mode tool result");
    for snippet in expected_snippets {
        assert!(
            texts[0].contains(snippet),
            "tool result {:?} missing snippet {snippet:?}",
            texts[0]
        );
    }
}

/// The session's plan-mode model: the first stream emits an `exit_plan_mode`
/// tool call. It is streamed again in the same turn only when the session
/// STAYED in plan mode (rejection / failed exit), in which case it asserts the
/// tool result and finishes the turn.
struct PlanSideDriver {
    calls: Mutex<usize>,
    expected_result_snippets: &'static [&'static str],
}

impl SessionModelDriver for PlanSideDriver {
    fn stream_completion_turn(
        &self,
        prompt: Message,
        _history: Vec<Message>,
    ) -> Result<SessionCompletionStream> {
        let mut calls = self
            .calls
            .lock()
            .map_err(|_| anyhow::anyhow!("plan-side calls mutex"))?;
        let call_idx = *calls;
        *calls += 1;
        drop(calls);

        match call_idx {
            0 => {
                let tool_call = ToolCall::new(
                    "exit-plan-1".to_string(),
                    ToolFunction::new(
                        "exit_plan_mode".to_string(),
                        serde_json::json!({ "reason": "plan ready" }),
                    ),
                )
                .with_call_id("call-exit-plan-1".to_string());
                Ok(Box::pin(stream::iter(vec![
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::ToolCall {
                            tool_call,
                            internal_call_id: "internal-exit-plan-1".to_string(),
                        },
                    )),
                    Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
                        assistant_message_id: Some("assistant-exit-plan".to_string()),
                        response: String::new(),
                    })),
                ])))
            }
            1 => {
                assert_exit_plan_result(&prompt, self.expected_result_snippets);
                Ok(final_text_stream("revising plan"))
            }
            other => panic!("unexpected plan-side completion turn {other}"),
        }
    }
}

/// The session's normal-mode model: it is only streamed after an approval
/// flips plan mode off mid-turn, so its first prompt must be the
/// `exit_plan_mode` approval result.
struct NormalSideDriver {
    calls: Mutex<usize>,
    expected_result_snippets: &'static [&'static str],
}

impl SessionModelDriver for NormalSideDriver {
    fn stream_completion_turn(
        &self,
        prompt: Message,
        _history: Vec<Message>,
    ) -> Result<SessionCompletionStream> {
        let mut calls = self
            .calls
            .lock()
            .map_err(|_| anyhow::anyhow!("normal-side calls mutex"))?;
        let call_idx = *calls;
        *calls += 1;
        drop(calls);

        match call_idx {
            0 => {
                assert_exit_plan_result(&prompt, self.expected_result_snippets);
                Ok(final_text_stream("implementing plan"))
            }
            other => panic!("unexpected normal-side completion turn {other}"),
        }
    }
}

struct ExitPlanFactory {
    expected_result_snippets: &'static [&'static str],
}

impl SessionModelFactory for ExitPlanFactory {
    fn build(
        &self,
        _cwd: PathBuf,
        _thread_id: ThreadId,
        _ask_user_client: Option<AskUserClient>,
        _current_turn_id: Arc<RwLock<Option<String>>>,
        _system_prompt_kind: SystemPromptKind,
        _agent_control: AgentControl,
        plan_mode: bool,
    ) -> Result<SessionModel> {
        if plan_mode {
            Ok(SessionModel::Stub(Arc::new(PlanSideDriver {
                calls: Mutex::new(0),
                expected_result_snippets: self.expected_result_snippets,
            })))
        } else {
            Ok(SessionModel::Stub(Arc::new(NormalSideDriver {
                calls: Mutex::new(0),
                expected_result_snippets: self.expected_result_snippets,
            })))
        }
    }
}

struct ApprovalProbe {
    plans: Mutex<Vec<String>>,
    calls: Mutex<usize>,
}

fn approval_client(
    decision: PlanApprovalDecision,
    feedback: Option<&'static str>,
    probe: Arc<ApprovalProbe>,
) -> AskUserClient {
    AskUserClient::new(
        |_params| async {
            Ok(AskUserQuestionResponse {
                answers: Vec::new(),
            })
        },
        |_thread_id| async {},
    )
    .with_plan_approval(move |params| {
        let probe = Arc::clone(&probe);
        async move {
            if let Ok(mut plans) = probe.plans.lock() {
                plans.push(params.plan);
            }
            if let Ok(mut calls) = probe.calls.lock() {
                *calls += 1;
            }
            Ok(RequestPlanApprovalResponse {
                decision,
                feedback: feedback.map(str::to_string),
            })
        }
    })
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

struct PlanModeRun {
    plan_mode_changes: Vec<bool>,
    last_assistant_message: Option<String>,
    probe: Arc<ApprovalProbe>,
}

/// Shared harness: enter plan mode, optionally pre-write the plan file, run a
/// turn whose stub model calls `exit_plan_mode`, and collect every
/// `PlanModeChanged` event seen before the turn completes.
async fn run_exit_plan_turn(
    decision: PlanApprovalDecision,
    feedback: Option<&'static str>,
    plan_content: Option<&str>,
    expected_result_snippets: &'static [&'static str],
) -> Result<PlanModeRun> {
    let _cwd_guard = CWD_LOCK.lock().map_err(|_| anyhow::anyhow!("cwd lock"))?;
    let workspace = TempDir::new()?;
    let original_cwd = std::env::current_dir()?;
    std::env::set_current_dir(workspace.path())?;

    let probe = Arc::new(ApprovalProbe {
        plans: Mutex::new(Vec::new()),
        calls: Mutex::new(0),
    });
    let manager = ThreadManagerState::new(
        Some(approval_client(decision, feedback, Arc::clone(&probe))),
        Some(Arc::new(ExitPlanFactory {
            expected_result_snippets,
        })),
    )
    .await?;
    let started = manager.start_thread().await?;
    let root_id = started.thread_id;
    let mut events = manager.subscribe(root_id).await?;

    manager.set_plan_mode(root_id, true).await?;

    if let Some(plan_content) = plan_content {
        // The session resolves the plan file against its creation-time cwd, so
        // compute it the same way instead of from the TempDir path (which may
        // be a symlink on macOS).
        let cwd = std::env::current_dir()?;
        let plan_path = tools::plan_file_path(&cwd, root_id);
        if let Some(parent) = plan_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&plan_path, plan_content)?;
    }

    manager
        .start_user_input(root_id, "exit plan mode".to_string())
        .await?;

    let mut plan_mode_changes = Vec::new();
    let last_assistant_message = loop {
        let event = tokio::time::timeout(Duration::from_secs(2), events.recv()).await??;
        match event.msg {
            EventMsg::PlanModeChanged(change) => plan_mode_changes.push(change.enabled),
            EventMsg::TurnCompleted(turn) => break turn.last_assistant_message,
            _ => {}
        }
    };

    std::env::set_current_dir(original_cwd)?;
    Ok(PlanModeRun {
        plan_mode_changes,
        last_assistant_message,
        probe,
    })
}

#[tokio::test]
async fn approved_plan_unlocks_plan_mode_mid_turn() -> Result<()> {
    let run = run_exit_plan_turn(
        PlanApprovalDecision::Approved,
        None,
        Some("# The grand plan\n\n1. Refactor."),
        &["Plan approved by the user", "implement the plan now"],
    )
    .await?;

    assert_eq!(
        run.plan_mode_changes,
        vec![true, false],
        "approval should flip plan mode off mid-turn"
    );
    // The turn continued on the normal-mode model after approval.
    assert_eq!(
        run.last_assistant_message.as_deref(),
        Some("implementing plan")
    );
    let plans = run
        .probe
        .plans
        .lock()
        .map_err(|_| anyhow::anyhow!("plans mutex"))?;
    assert_eq!(
        plans.as_slice(),
        ["# The grand plan\n\n1. Refactor."],
        "the approval request must carry the plan_write content"
    );
    Ok(())
}

#[tokio::test]
async fn rejected_plan_stays_in_plan_mode_and_surfaces_feedback() -> Result<()> {
    let run = run_exit_plan_turn(
        PlanApprovalDecision::Rejected,
        Some("use sqlite instead"),
        Some("# The grand plan"),
        &[
            "rejected the plan",
            "still in plan mode",
            "use sqlite instead",
        ],
    )
    .await?;

    assert_eq!(
        run.plan_mode_changes,
        vec![true],
        "a rejection must not leave plan mode"
    );
    // The turn continued on the plan-mode model after rejection.
    assert_eq!(run.last_assistant_message.as_deref(), Some("revising plan"));
    Ok(())
}

#[tokio::test]
async fn exit_without_plan_file_fails_and_never_asks_for_approval() -> Result<()> {
    let run = run_exit_plan_turn(
        PlanApprovalDecision::Approved,
        None,
        None,
        &["no plan found", "plan_write"],
    )
    .await?;

    assert_eq!(
        run.plan_mode_changes,
        vec![true],
        "a failed exit must not leave plan mode"
    );
    assert_eq!(run.last_assistant_message.as_deref(), Some("revising plan"));
    let calls = run
        .probe
        .calls
        .lock()
        .map_err(|_| anyhow::anyhow!("calls mutex"))?;
    assert_eq!(*calls, 0, "approval must not be requested without a plan");
    Ok(())
}

/// Plan-side root driver for the spawn-coercion test: spawns a child with an
/// explicit non-Explore `subagent_type`, then finishes once the child result
/// arrives.
struct PlanSpawnDriver {
    calls: Mutex<usize>,
}

impl SessionModelDriver for PlanSpawnDriver {
    fn stream_completion_turn(
        &self,
        _prompt: Message,
        _history: Vec<Message>,
    ) -> Result<SessionCompletionStream> {
        let mut calls = self
            .calls
            .lock()
            .map_err(|_| anyhow::anyhow!("spawn calls mutex"))?;
        let call_idx = *calls;
        *calls += 1;
        drop(calls);

        match call_idx {
            0 => {
                let tool_call = ToolCall::new(
                    "spawn-1".to_string(),
                    ToolFunction::new(
                        "spawn_agent".to_string(),
                        serde_json::json!({
                            "description": "implement something",
                            "prompt": "go implement",
                            "subagent_type": "default"
                        }),
                    ),
                )
                .with_call_id("call-spawn-1".to_string());
                Ok(Box::pin(stream::iter(vec![
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::ToolCall {
                            tool_call,
                            internal_call_id: "internal-spawn-1".to_string(),
                        },
                    )),
                    Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
                        assistant_message_id: Some("assistant-spawn".to_string()),
                        response: String::new(),
                    })),
                ])))
            }
            1 => Ok(final_text_stream("spawn done")),
            other => panic!("unexpected spawn-parent completion turn {other}"),
        }
    }
}

struct ChildStubDriver;

impl SessionModelDriver for ChildStubDriver {
    fn stream_completion_turn(
        &self,
        _prompt: Message,
        _history: Vec<Message>,
    ) -> Result<SessionCompletionStream> {
        Ok(final_text_stream("explored"))
    }
}

struct SpawnRecordingFactory {
    builds: Arc<Mutex<Vec<(SystemPromptKind, bool)>>>,
}

impl SessionModelFactory for SpawnRecordingFactory {
    fn build(
        &self,
        _cwd: PathBuf,
        _thread_id: ThreadId,
        _ask_user_client: Option<AskUserClient>,
        _current_turn_id: Arc<RwLock<Option<String>>>,
        system_prompt_kind: SystemPromptKind,
        _agent_control: AgentControl,
        plan_mode: bool,
    ) -> Result<SessionModel> {
        self.builds
            .lock()
            .map_err(|_| anyhow::anyhow!("builds mutex"))?
            .push((system_prompt_kind, plan_mode));
        match system_prompt_kind {
            SystemPromptKind::Root => {
                if plan_mode {
                    Ok(SessionModel::Stub(Arc::new(PlanSpawnDriver {
                        calls: Mutex::new(0),
                    })))
                } else {
                    Ok(SessionModel::Stub(Arc::new(ChildStubDriver)))
                }
            }
            SystemPromptKind::Explore => Ok(SessionModel::Stub(Arc::new(ChildStubDriver))),
            SystemPromptKind::DefaultSubagent => Err(anyhow::anyhow!(
                "plan-mode spawns must be coerced to Explore, got DefaultSubagent"
            )),
        }
    }
}

#[tokio::test]
async fn plan_mode_survives_resume_with_fresh_manager() -> Result<()> {
    let _cwd_guard = CWD_LOCK.lock().map_err(|_| anyhow::anyhow!("cwd lock"))?;
    let workspace = TempDir::new()?;
    let original_cwd = std::env::current_dir()?;
    std::env::set_current_dir(workspace.path())?;

    let builds = Arc::new(Mutex::new(Vec::new()));
    let manager = ThreadManagerState::new(
        None,
        Some(Arc::new(SpawnRecordingFactory {
            builds: Arc::clone(&builds),
        })),
    )
    .await?;
    let started = manager.start_thread().await?;
    let root_id = started.thread_id;
    manager.set_plan_mode(root_id, true).await?;
    assert!(manager.plan_mode(root_id).await?);
    drop(manager);

    let resumed_manager = ThreadManagerState::new(
        None,
        Some(Arc::new(SpawnRecordingFactory {
            builds: Arc::clone(&builds),
        })),
    )
    .await?;
    let resumed = resumed_manager.resume_thread(root_id).await?;

    assert!(
        resumed_manager.plan_mode(root_id).await?,
        "a thread persisted in plan mode must resume in plan mode"
    );
    assert!(
        resumed.initial_messages.iter().any(|event| matches!(
            event,
            EventMsg::PlanModeChanged(change) if change.enabled
        )),
        "resume replay should carry the PlanModeChanged event for the UI badge"
    );

    std::env::set_current_dir(original_cwd)?;
    Ok(())
}

#[tokio::test]
async fn plan_mode_spawns_are_coerced_to_explore() -> Result<()> {
    let _cwd_guard = CWD_LOCK.lock().map_err(|_| anyhow::anyhow!("cwd lock"))?;
    let workspace = TempDir::new()?;
    let original_cwd = std::env::current_dir()?;
    std::env::set_current_dir(workspace.path())?;

    let builds = Arc::new(Mutex::new(Vec::new()));
    let manager = ThreadManagerState::new(
        None,
        Some(Arc::new(SpawnRecordingFactory {
            builds: Arc::clone(&builds),
        })),
    )
    .await?;
    let started = manager.start_thread().await?;
    let root_id = started.thread_id;
    let mut events = manager.subscribe(root_id).await?;

    manager.set_plan_mode(root_id, true).await?;
    manager
        .start_user_input(root_id, "spawn a child".to_string())
        .await?;

    loop {
        let event = tokio::time::timeout(Duration::from_secs(2), events.recv()).await??;
        if matches!(event.msg, EventMsg::TurnCompleted(_)) {
            break;
        }
    }

    let builds = builds.lock().map_err(|_| anyhow::anyhow!("builds mutex"))?;
    let child_kinds: Vec<SystemPromptKind> = builds
        .iter()
        .filter(|(kind, _)| !matches!(kind, SystemPromptKind::Root))
        .map(|(kind, _)| *kind)
        .collect();
    assert!(
        !child_kinds.is_empty(),
        "expected the spawned child to build a model"
    );
    assert!(
        child_kinds
            .iter()
            .all(|kind| matches!(kind, SystemPromptKind::Explore)),
        "plan-mode spawn with subagent_type=default must run as Explore, got {child_kinds:?}"
    );

    std::env::set_current_dir(original_cwd)?;
    Ok(())
}
