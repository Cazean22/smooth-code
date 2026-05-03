use smooth_protocol::{AgentStatus, EventMsg};

pub(crate) fn agent_status_from_event(msg: &EventMsg) -> Option<AgentStatus> {
    match msg {
        EventMsg::AgentStatusChanged(event) => Some(event.status.clone()),
        EventMsg::TurnCompleted(event) => {
            Some(AgentStatus::Completed(event.last_assistant_message.clone()))
        }
        EventMsg::TurnInterrupted(_) => Some(AgentStatus::Interrupted),
        EventMsg::Error(event) => Some(AgentStatus::Errored(event.message.clone())),
        _ => None,
    }
}

pub(crate) fn is_final(status: &AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::Completed(_)
            | AgentStatus::Errored(_)
            | AgentStatus::Shutdown
            | AgentStatus::NotFound
    )
}

#[cfg(test)]
mod tests {
    use smooth_protocol::{ErrorEvent, EventMsg, TurnCompletedEvent, TurnInterruptedEvent};

    use super::{agent_status_from_event, is_final};

    #[test]
    fn maps_turn_events_to_agent_status() {
        let completed = agent_status_from_event(&EventMsg::TurnCompleted(TurnCompletedEvent {
            thread_id: "thread".to_string(),
            turn_id: "1".to_string(),
            last_assistant_message: Some("done".to_string()),
        }))
        .expect("completed status");
        assert_eq!(
            completed,
            smooth_protocol::AgentStatus::Completed(Some("done".to_string()))
        );

        let interrupted =
            agent_status_from_event(&EventMsg::TurnInterrupted(TurnInterruptedEvent {
                thread_id: "thread".to_string(),
                turn_id: "2".to_string(),
                reason: "interrupt".to_string(),
            }))
            .expect("interrupted status");
        assert_eq!(interrupted, smooth_protocol::AgentStatus::Interrupted);

        let errored = agent_status_from_event(&EventMsg::Error(ErrorEvent {
            message: "boom".to_string(),
            codex_error_info: None,
        }))
        .expect("errored status");
        assert_eq!(
            errored,
            smooth_protocol::AgentStatus::Errored("boom".to_string())
        );
    }

    #[test]
    fn final_statuses_match_contract() {
        assert!(is_final(&smooth_protocol::AgentStatus::Completed(None)));
        assert!(is_final(&smooth_protocol::AgentStatus::Errored(
            "x".to_string()
        )));
        assert!(is_final(&smooth_protocol::AgentStatus::Shutdown));
        assert!(!is_final(&smooth_protocol::AgentStatus::Running));
    }
}
