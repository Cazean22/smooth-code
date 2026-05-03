mod client;
mod close_agent;
mod list_agents;
mod send_message;
mod spawn_agent;
mod wait_agent;

pub use client::{
    AgentInfo, AgentWaitOutcome, DynMultiAgentClient, MultiAgentClient, SpawnAgentParams,
    WaitAgentParams,
};
pub use close_agent::CloseAgentTool;
pub use list_agents::ListAgentsTool;
pub use send_message::SendMessageTool;
pub use spawn_agent::SpawnAgentTool;
pub use wait_agent::WaitAgentTool;

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures_util::future::BoxFuture;
    use rig::tool::Tool;

    use super::{
        AgentInfo, AgentWaitOutcome, CloseAgentTool, DynMultiAgentClient, ListAgentsTool,
        MultiAgentClient, SendMessageTool, SpawnAgentParams, SpawnAgentTool, WaitAgentParams,
        WaitAgentTool,
    };
    use crate::ToolFailure;

    struct StubMultiAgentClient {
        calls: Mutex<Vec<String>>,
    }

    impl StubMultiAgentClient {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl MultiAgentClient for StubMultiAgentClient {
        fn spawn(
            &self,
            params: SpawnAgentParams,
        ) -> BoxFuture<'static, Result<AgentInfo, ToolFailure>> {
            self.calls
                .lock()
                .expect("calls mutex should lock")
                .push(format!("spawn:{}", params.message));
            Box::pin(async move {
                Ok(AgentInfo {
                    thread_id: "thread-1".to_string(),
                    agent_path: "/root/child".to_string(),
                    agent_nickname: Some("child".to_string()),
                    agent_role: params.agent_type,
                    status: None,
                })
            })
        }

        fn send_message(
            &self,
            target: String,
            content: String,
            trigger_turn: bool,
        ) -> BoxFuture<'static, Result<String, ToolFailure>> {
            self.calls
                .lock()
                .expect("calls mutex should lock")
                .push(format!("send:{target}:{content}:{trigger_turn}"));
            Box::pin(async move { Ok("queued".to_string()) })
        }

        fn wait_agent(
            &self,
            params: WaitAgentParams,
        ) -> BoxFuture<'static, Result<AgentWaitOutcome, ToolFailure>> {
            self.calls
                .lock()
                .expect("calls mutex should lock")
                .push(format!("wait:{}", params.target));
            Box::pin(async move {
                Ok(AgentWaitOutcome {
                    target: params.target,
                    status: "completed".to_string(),
                })
            })
        }

        fn list_agents(
            &self,
            path_prefix: Option<String>,
        ) -> BoxFuture<'static, Result<Vec<AgentInfo>, ToolFailure>> {
            self.calls
                .lock()
                .expect("calls mutex should lock")
                .push(format!("list:{}", path_prefix.unwrap_or_default()));
            Box::pin(async move {
                Ok(vec![AgentInfo {
                    thread_id: "thread-1".to_string(),
                    agent_path: "/root/child".to_string(),
                    agent_nickname: Some("child".to_string()),
                    agent_role: Some("worker".to_string()),
                    status: Some("running".to_string()),
                }])
            })
        }

        fn close_agent(&self, target: String) -> BoxFuture<'static, Result<String, ToolFailure>> {
            self.calls
                .lock()
                .expect("calls mutex should lock")
                .push(format!("close:{target}"));
            Box::pin(async move { Ok("shutdown".to_string()) })
        }
    }

    fn client() -> DynMultiAgentClient {
        Arc::new(StubMultiAgentClient::new())
    }

    #[tokio::test]
    async fn spawn_agent_tool_definition_and_call() {
        let tool = SpawnAgentTool::new(client(), "spawn desc".to_string());
        let definition = tool.definition(String::new()).await;
        assert_eq!(definition.name, "spawn_agent");
        assert_eq!(definition.description, "spawn desc");

        let output = tool
            .call(super::spawn_agent::SpawnAgentArgs {
                message: "hello".to_string(),
                agent_type: Some("worker".to_string()),
                model: None,
                fork_context: false,
            })
            .await
            .expect("spawn call should succeed");
        assert!(output.contains("\"threadId\":\"thread-1\""));
    }

    #[tokio::test]
    async fn other_multi_agent_tools_round_trip() {
        let send = SendMessageTool::new(client());
        assert_eq!(
            send.call(super::send_message::SendMessageArgs {
                target: "/root/child".to_string(),
                content: "ping".to_string(),
                trigger_turn: true,
            })
            .await
            .expect("send should succeed"),
            "queued"
        );

        let wait = WaitAgentTool::new(client());
        let waited = wait
            .call(super::wait_agent::WaitAgentArgs {
                target: "/root/child".to_string(),
                timeout_ms: Some(10),
            })
            .await
            .expect("wait should succeed");
        assert!(waited.contains("\"status\":\"completed\""));

        let list = ListAgentsTool::new(client());
        let listed = list
            .call(super::list_agents::ListAgentsArgs {
                path_prefix: Some("/root".to_string()),
            })
            .await
            .expect("list should succeed");
        assert!(listed.contains("\"agentPath\":\"/root/child\""));

        let close = CloseAgentTool::new(client());
        assert_eq!(
            close
                .call(super::close_agent::CloseAgentArgs {
                    target: "/root/child".to_string(),
                })
                .await
                .expect("close should succeed"),
            "shutdown"
        );
    }
}
