use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::RequestId;

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartParams {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartResponse {
    pub thread_id: String,
    pub rollout_path: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadResumeParams {
    pub thread_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadResumeResponse {
    pub thread_id: String,
    pub rollout_path: String,
    pub initial_messages: Vec<smooth_protocol::EventMsg>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadListParams {
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadListItem {
    pub thread_id: String,
    pub rollout_path: String,
    pub created_at: String,
    pub updated_at: String,
    pub last_user_message: Option<String>,
    pub last_assistant_message: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadListResponse {
    pub data: Vec<ThreadListItem>,
    pub next_cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartParams {
    pub thread_id: String,
    pub input: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartResponse {
    pub thread_id: String,
    pub turn_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnCancelParams {
    pub thread_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnCancelResponse {
    pub thread_id: String,
    pub cancelled_thread_ids: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetPlanModeParams {
    pub thread_id: String,
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetPlanModeResponse {
    pub thread_id: String,
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AskUserQuestionParams {
    pub thread_id: String,
    pub turn_id: String,
    pub call_id: String,
    pub questions: Vec<AskUserQuestion>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AskUserQuestion {
    pub question: String,
    pub header: String,
    pub options: Vec<AskUserQuestionOption>,
    pub multi_select: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AskUserQuestionOption {
    pub label: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AskUserQuestionResponse {
    pub answers: Vec<AskUserQuestionAnswer>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AskUserQuestionAnswer {
    pub question: String,
    pub selected: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[allow(clippy::large_enum_variant)]
pub enum ServerRequestPayload {
    AskUserQuestion(AskUserQuestionParams),
}

impl ServerRequestPayload {
    pub fn request_with_id(self, request_id: RequestId) -> ServerRequest {
        match self {
            Self::AskUserQuestion(params) => ServerRequest::AskUserQuestion { request_id, params },
        }
    }
}

#[doc = r" Request from the client to the server."]
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(tag = "method", rename_all = "camelCase")]
pub enum ClientRequest {
    ThreadStart {
        #[serde(rename = "id")]
        request_id: RequestId,
        params: ThreadStartParams,
    },
    TurnStart {
        #[serde(rename = "id")]
        request_id: RequestId,
        params: TurnStartParams,
    },
    TurnCancel {
        #[serde(rename = "id")]
        request_id: RequestId,
        params: TurnCancelParams,
    },
    ThreadResume {
        #[serde(rename = "id")]
        request_id: RequestId,
        params: ThreadResumeParams,
    },
    ThreadList {
        #[serde(rename = "id")]
        request_id: RequestId,
        params: ThreadListParams,
    },
    SetPlanMode {
        #[serde(rename = "id")]
        request_id: RequestId,
        params: SetPlanModeParams,
    },
}

#[doc = r" Request initiated from the server and sent to the client."]
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[allow(clippy::large_enum_variant)]
#[serde(tag = "method", rename_all = "camelCase")]
pub enum ServerRequest {
    #[doc = r" Ask the user one or more multiple-choice questions interactively."]
    #[serde(rename = "item/ask_user_question")]
    AskUserQuestion {
        #[serde(rename = "id")]
        request_id: RequestId,
        params: AskUserQuestionParams,
    },
}

impl ServerRequest {
    pub fn id(&self) -> &RequestId {
        match self {
            Self::AskUserQuestion { request_id, .. } => request_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn thread_resume_response_serializes_initial_user_messages() -> TestResult {
        let response = ThreadResumeResponse {
            thread_id: "018f6f32-7a31-7c22-8c95-3c3dfb63dce1".to_string(),
            rollout_path: "session.jsonl".to_string(),
            initial_messages: vec![smooth_protocol::EventMsg::UserMessage {
                text: "hello".to_string(),
            }],
        };

        let value = serde_json::to_value(&response)?;
        assert_eq!(
            value,
            json!({
                "threadId": "018f6f32-7a31-7c22-8c95-3c3dfb63dce1",
                "rolloutPath": "session.jsonl",
                "initialMessages": [
                    {
                        "type": "user_message",
                        "text": "hello",
                    },
                ],
            })
        );
        let decoded: ThreadResumeResponse = serde_json::from_value(value)?;
        assert_eq!(decoded, response);
        Ok(())
    }

    #[test]
    fn turn_cancel_request_round_trips_and_is_in_schema() -> TestResult {
        let request = ClientRequest::TurnCancel {
            request_id: RequestId(7),
            params: TurnCancelParams {
                thread_id: "018f6f32-7a31-7c22-8c95-3c3dfb63dce1".to_string(),
            },
        };

        let value = serde_json::to_value(&request)?;
        assert_eq!(
            value,
            json!({
                "method": "turnCancel",
                "id": 7,
                "params": {
                    "threadId": "018f6f32-7a31-7c22-8c95-3c3dfb63dce1",
                },
            })
        );
        let decoded: ClientRequest = serde_json::from_value(value)?;
        assert_eq!(decoded, request);

        let schema = serde_json::to_value(schemars::schema_for!(ClientRequest))?;
        assert!(schema.to_string().contains("turnCancel"));
        Ok(())
    }

    #[test]
    fn turn_cancel_response_round_trips() -> TestResult {
        let response = TurnCancelResponse {
            thread_id: "root-thread".to_string(),
            cancelled_thread_ids: vec!["root-thread".to_string(), "child-thread".to_string()],
        };

        let value = serde_json::to_value(&response)?;
        assert_eq!(
            value,
            json!({
                "threadId": "root-thread",
                "cancelledThreadIds": ["root-thread", "child-thread"],
            })
        );
        let decoded: TurnCancelResponse = serde_json::from_value(value)?;
        assert_eq!(decoded, response);
        Ok(())
    }
}
