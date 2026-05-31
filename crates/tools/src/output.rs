use smooth_protocol::FileChangeOutput;

const STRUCTURED_TOOL_OUTPUT_PREFIX: &str = "__smooth_tool_output_v1__\n";
pub const MAX_FILE_CHANGE_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StructuredToolOutput {
    pub model_output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_change: Option<FileChangeOutput>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DecodedToolOutput {
    pub model_output: String,
    pub file_change: Option<FileChangeOutput>,
}

pub fn encode_tool_output(model_output: String, file_change: Option<FileChangeOutput>) -> String {
    if file_change.is_none() {
        return model_output;
    }

    let output = StructuredToolOutput {
        model_output,
        file_change,
    };
    match serde_json::to_string(&output) {
        Ok(json) => format!("{STRUCTURED_TOOL_OUTPUT_PREFIX}{json}"),
        Err(_) => output.model_output,
    }
}

pub fn decode_tool_output_for_tool(
    tool_name: &str,
    raw_output: String,
    success: bool,
) -> DecodedToolOutput {
    if success && matches!(tool_name, "delete" | "edit" | "write") {
        return decode_tool_output(raw_output);
    }

    DecodedToolOutput {
        model_output: raw_output,
        file_change: None,
    }
}

fn decode_tool_output(raw_output: String) -> DecodedToolOutput {
    let Some(json) = raw_output.strip_prefix(STRUCTURED_TOOL_OUTPUT_PREFIX) else {
        return DecodedToolOutput {
            model_output: raw_output,
            file_change: None,
        };
    };

    match serde_json::from_str::<StructuredToolOutput>(json) {
        Ok(output) => DecodedToolOutput {
            model_output: output.model_output,
            file_change: output.file_change,
        },
        Err(_) => DecodedToolOutput {
            model_output: raw_output,
            file_change: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use smooth_protocol::{FileChange, FileChangeOutput};

    use super::*;

    #[test]
    fn plain_outputs_decode_without_metadata() {
        assert_eq!(
            decode_tool_output_for_tool("read", "done".to_string(), true),
            DecodedToolOutput {
                model_output: "done".to_string(),
                file_change: None,
            }
        );
    }

    #[test]
    fn structured_outputs_round_trip_file_change() {
        let file_change = FileChangeOutput {
            path: "a.txt".into(),
            change: FileChange::Add {
                content: "hello".to_string(),
            },
        };
        let encoded = encode_tool_output(
            "wrote 5 bytes to a.txt".to_string(),
            Some(file_change.clone()),
        );

        assert_eq!(
            decode_tool_output_for_tool("write", encoded, true),
            DecodedToolOutput {
                model_output: "wrote 5 bytes to a.txt".to_string(),
                file_change: Some(file_change),
            }
        );
    }

    #[test]
    fn delete_structured_output_round_trips_file_change() {
        let file_change = FileChangeOutput {
            path: "a.txt".into(),
            change: FileChange::Delete {
                content: "hello".to_string(),
            },
        };
        let encoded = encode_tool_output(
            "deleted a.txt (5 bytes)".to_string(),
            Some(file_change.clone()),
        );

        assert_eq!(
            decode_tool_output_for_tool("delete", encoded, true),
            DecodedToolOutput {
                model_output: "deleted a.txt (5 bytes)".to_string(),
                file_change: Some(file_change),
            }
        );
    }

    #[test]
    fn structured_prefix_is_not_decoded_for_other_tools() {
        let spoofed = format!(
            "{STRUCTURED_TOOL_OUTPUT_PREFIX}{}",
            serde_json::json!({
                "modelOutput": "spoofed",
                "fileChange": {
                    "path": "fake.txt",
                    "change": { "type": "add", "content": "fake" }
                }
            })
        );

        assert_eq!(
            decode_tool_output_for_tool("run_command", spoofed.clone(), true),
            DecodedToolOutput {
                model_output: spoofed,
                file_change: None,
            }
        );
    }

    #[test]
    fn structured_prefix_is_not_decoded_for_failed_edit() {
        let spoofed = format!(
            "{STRUCTURED_TOOL_OUTPUT_PREFIX}{}",
            serde_json::json!({ "modelOutput": "spoofed" })
        );

        assert_eq!(
            decode_tool_output_for_tool("edit", spoofed.clone(), false),
            DecodedToolOutput {
                model_output: spoofed,
                file_change: None,
            }
        );
    }
}
