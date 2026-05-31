use std::sync::Arc;

use app_server_protocol::{
    AskUserQuestion, AskUserQuestionOption, AskUserQuestionParams, AskUserQuestionResponse,
};
use rig::{completion::ToolDefinition, tool::Tool};
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::{AskUserClient, ToolError};

const MAX_QUESTIONS: usize = 4;
const MIN_OPTIONS: usize = 2;
const MAX_OPTIONS: usize = 4;
const MAX_HEADER_CHARS: usize = 12;

const DESCRIPTION: &str = r#"Ask the user multiple-choice questions to gather information, clarify ambiguity, understand preferences, or offer choices. The TUI renders an inline picker for each question.

Usage:
- Provide 1-4 questions in a single call; each question has 2-4 options.
- Each question has a short `header` chip label (max 12 characters) used in the picker UI. Examples: "Auth method", "Library", "Approach".
- Each option has a `label` (1-5 words, displayed) and a `description` (explains the choice or its trade-offs).
- The TUI automatically appends an "Other" free-text option to every question; do NOT include it yourself.
- Set `multi_select: true` only when choices are not mutually exclusive (the user can pick more than one).
- If you have a recommended choice, list it first and append " (Recommended)" to its label.
- An optional `preview` field on an option carries a short markdown snippet (e.g. a mockup, code sketch, or example) shown alongside the option."#;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AskUserQuestionArgs {
    /// 1-4 questions to present to the user as a single batched form.
    pub questions: Vec<AskUserQuestionInput>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AskUserQuestionInput {
    /// The complete question text to ask the user. Should end with "?".
    pub question: String,
    /// Short header chip label for this question (max 12 characters).
    pub header: String,
    /// 2-4 answer options. Do not include an "Other" option; it is appended automatically by the UI.
    pub options: Vec<AskUserQuestionOptionInput>,
    /// Set to true to allow the user to select multiple options. Use only when choices are not mutually exclusive.
    #[serde(default)]
    pub multi_select: bool,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AskUserQuestionOptionInput {
    /// Displayed choice label (concise, 1-5 words).
    pub label: String,
    /// Explanation of what this option means or what choosing it implies.
    pub description: String,
    /// Optional markdown preview shown alongside the option (e.g. a mockup or code snippet).
    #[serde(default)]
    pub preview: Option<String>,
}

#[derive(Clone)]
pub struct AskUserQuestionTool {
    thread_id: smooth_protocol::ThreadId,
    client: Arc<dyn AskUserClient>,
    current_turn_id: Arc<RwLock<Option<String>>>,
}

impl AskUserQuestionTool {
    pub fn new(
        thread_id: smooth_protocol::ThreadId,
        client: Arc<dyn AskUserClient>,
        current_turn_id: Arc<RwLock<Option<String>>>,
    ) -> Self {
        Self {
            thread_id,
            client,
            current_turn_id,
        }
    }
}

impl Tool for AskUserQuestionTool {
    const NAME: &'static str = "ask_user_question";

    type Error = ToolError;
    type Args = AskUserQuestionArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: DESCRIPTION.to_string(),
            parameters: schema_for!(AskUserQuestionArgs).to_value(),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        validate_args(&args)?;

        let turn_id = self
            .current_turn_id
            .read()
            .await
            .clone()
            .ok_or_else(|| ToolError::invalid_arguments("no active turn id"))?;

        let questions: Vec<AskUserQuestion> = args
            .questions
            .into_iter()
            .map(|q| AskUserQuestion {
                question: q.question,
                header: q.header,
                multi_select: q.multi_select,
                options: q
                    .options
                    .into_iter()
                    .map(|opt| AskUserQuestionOption {
                        label: opt.label,
                        description: opt.description,
                        preview: opt.preview,
                    })
                    .collect(),
            })
            .collect();

        let params = AskUserQuestionParams {
            thread_id: self.thread_id.to_string(),
            turn_id,
            call_id: Uuid::new_v4().to_string(),
            questions,
        };

        let response: AskUserQuestionResponse = self
            .client
            .ask(params)
            .await
            .map_err(|err| ToolError::client(err.message))?;

        Ok(format_tool_result(&response))
    }
}

fn validate_args(args: &AskUserQuestionArgs) -> Result<(), ToolError> {
    if args.questions.is_empty() || args.questions.len() > MAX_QUESTIONS {
        return Err(ToolError::invalid_arguments(format!(
            "ask_user_question requires 1 to {MAX_QUESTIONS} questions, got {}",
            args.questions.len()
        )));
    }
    for q in &args.questions {
        if q.header.chars().count() > MAX_HEADER_CHARS {
            return Err(ToolError::invalid_arguments(format!(
                "header must be at most {MAX_HEADER_CHARS} characters (\"{}\" is {})",
                q.header,
                q.header.chars().count()
            )));
        }
        if q.options.len() < MIN_OPTIONS || q.options.len() > MAX_OPTIONS {
            return Err(ToolError::invalid_arguments(format!(
                "each question requires {MIN_OPTIONS} to {MAX_OPTIONS} options, question \"{}\" has {}",
                q.question,
                q.options.len()
            )));
        }
    }
    Ok(())
}

fn format_tool_result(response: &AskUserQuestionResponse) -> String {
    let parts: Vec<String> = response
        .answers
        .iter()
        .map(|answer| {
            let joined = answer.selected.join(", ");
            let mut piece = format!("\"{}\"=\"{}\"", answer.question, joined);
            if let Some(preview) = &answer.preview {
                piece.push_str(&format!(" (selected preview:\n{preview})"));
            }
            piece
        })
        .collect();

    format!(
        "User has answered your questions: {}, . You can now continue with the user's answers in mind.",
        parts.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use app_server_protocol::{AskUserQuestionAnswer, JsonRpcError};
    use futures_util::future::BoxFuture;
    use smooth_protocol::ErrorInfo;

    use super::*;

    struct StubAskUserClient {
        last_params: Mutex<Option<AskUserQuestionParams>>,
        result: Result<AskUserQuestionResponse, JsonRpcError>,
    }

    impl StubAskUserClient {
        fn ok(response: AskUserQuestionResponse) -> Self {
            Self {
                last_params: Mutex::new(None),
                result: Ok(response),
            }
        }

        fn err(message: &str) -> Self {
            Self {
                last_params: Mutex::new(None),
                result: Err(JsonRpcError::new(
                    -32001,
                    ErrorInfo::new("client_error", message).with_source("test"),
                )),
            }
        }
    }

    impl AskUserClient for StubAskUserClient {
        fn ask(
            &self,
            params: AskUserQuestionParams,
        ) -> BoxFuture<'static, Result<AskUserQuestionResponse, JsonRpcError>> {
            if let Ok(mut last_params) = self.last_params.lock() {
                *last_params = Some(params);
            }
            let result = self.result.clone();
            Box::pin(async move { result })
        }

        fn abort_pending_server_requests(&self) -> BoxFuture<'static, ()> {
            Box::pin(async {})
        }
    }

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn simple_args() -> AskUserQuestionArgs {
        AskUserQuestionArgs {
            questions: vec![AskUserQuestionInput {
                question: "Which database?".to_string(),
                header: "Database".to_string(),
                multi_select: false,
                options: vec![
                    AskUserQuestionOptionInput {
                        label: "Postgres".to_string(),
                        description: "Relational".to_string(),
                        preview: None,
                    },
                    AskUserQuestionOptionInput {
                        label: "SQLite".to_string(),
                        description: "Embedded".to_string(),
                        preview: None,
                    },
                ],
            }],
        }
    }

    #[tokio::test]
    async fn formats_single_select_answer() -> TestResult {
        let stub = Arc::new(StubAskUserClient::ok(AskUserQuestionResponse {
            answers: vec![AskUserQuestionAnswer {
                question: "Which database?".to_string(),
                selected: vec!["Postgres".to_string()],
                preview: None,
            }],
        }));
        let tool = AskUserQuestionTool::new(
            smooth_protocol::ThreadId::new(),
            stub.clone(),
            Arc::new(RwLock::new(Some("turn-1".to_string()))),
        );

        let out = tool.call(simple_args()).await?;
        assert_eq!(
            out,
            "User has answered your questions: \"Which database?\"=\"Postgres\", . You can now continue with the user's answers in mind."
        );
        let params = stub
            .last_params
            .lock()
            .map_err(|_| std::io::Error::other("stub params mutex should lock"))?
            .clone()
            .ok_or_else(|| std::io::Error::other("params should be recorded"))?;
        assert_eq!(params.turn_id, "turn-1");
        assert_eq!(params.questions.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn formats_multi_select_answer_with_preview() -> TestResult {
        let stub = Arc::new(StubAskUserClient::ok(AskUserQuestionResponse {
            answers: vec![AskUserQuestionAnswer {
                question: "Pick frameworks".to_string(),
                selected: vec!["React".to_string(), "Vue".to_string()],
                preview: Some("<App/>".to_string()),
            }],
        }));
        let tool = AskUserQuestionTool::new(
            smooth_protocol::ThreadId::new(),
            stub,
            Arc::new(RwLock::new(Some("turn-2".to_string()))),
        );

        let out = tool.call(simple_args()).await?;
        assert_eq!(
            out,
            "User has answered your questions: \"Pick frameworks\"=\"React, Vue\" (selected preview:\n<App/>), . You can now continue with the user's answers in mind."
        );
        Ok(())
    }

    #[tokio::test]
    async fn fails_without_active_turn() -> TestResult {
        let stub = Arc::new(StubAskUserClient::ok(AskUserQuestionResponse {
            answers: vec![],
        }));
        let tool = AskUserQuestionTool::new(
            smooth_protocol::ThreadId::new(),
            stub,
            Arc::new(RwLock::new(None)),
        );

        let Err(err) = tool.call(simple_args()).await else {
            panic!("call should fail without an active turn");
        };
        assert_eq!(err.to_string(), "no active turn id");
        Ok(())
    }

    #[tokio::test]
    async fn surfaces_client_error_as_tool_failure() -> TestResult {
        let stub = Arc::new(StubAskUserClient::err("user declined to answer"));
        let tool = AskUserQuestionTool::new(
            smooth_protocol::ThreadId::new(),
            stub,
            Arc::new(RwLock::new(Some("turn-3".to_string()))),
        );

        let Err(err) = tool.call(simple_args()).await else {
            panic!("client error should surface as ToolError");
        };
        assert_eq!(err.to_string(), "user declined to answer");
        Ok(())
    }

    #[test]
    fn validates_question_count() {
        let mut args = simple_args();
        args.questions.clear();
        let Err(err) = validate_args(&args) else {
            panic!("zero questions should fail");
        };
        assert!(err.to_string().contains("1 to 4 questions"));

        args.questions = (0..5)
            .map(|_| {
                let mut args = simple_args();
                args.questions.remove(0)
            })
            .collect();
        let Err(err) = validate_args(&args) else {
            panic!("5 questions should fail");
        };
        assert!(err.to_string().contains("1 to 4 questions"));
    }

    #[test]
    fn validates_option_count() {
        let mut args = simple_args();
        args.questions[0].options.pop();
        let Err(err) = validate_args(&args) else {
            panic!("1 option should fail");
        };
        assert!(err.to_string().contains("2 to 4 options"));

        args.questions[0].options = (0..5)
            .map(|i| AskUserQuestionOptionInput {
                label: format!("opt-{i}"),
                description: String::new(),
                preview: None,
            })
            .collect();
        let Err(err) = validate_args(&args) else {
            panic!("5 options should fail");
        };
        assert!(err.to_string().contains("2 to 4 options"));
    }

    #[test]
    fn validates_header_length() {
        let mut args = simple_args();
        args.questions[0].header = "ThirteenChars".to_string();
        let Err(err) = validate_args(&args) else {
            panic!("13-char header should fail");
        };
        assert!(err.to_string().contains("at most 12 characters"));
    }
}
