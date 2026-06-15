use cazean_protocol::{AgentStatus, EventMsg};

pub(crate) fn agent_status_from_event(msg: &EventMsg) -> Option<AgentStatus> {
    match msg {
        EventMsg::AgentStatusChanged(event) => Some(event.status.clone()),
        EventMsg::TurnCompleted(event) => {
            Some(AgentStatus::Completed(event.last_assistant_message.clone()))
        }
        EventMsg::TurnInterrupted(_) => Some(AgentStatus::Interrupted),
        EventMsg::Error(event) => Some(AgentStatus::Errored(event.error.clone())),
        _ => None,
    }
}

pub(crate) fn is_final(status: &AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::Interrupted
            | AgentStatus::Completed(_)
            | AgentStatus::Errored(_)
            | AgentStatus::Shutdown
            | AgentStatus::NotFound
    )
}

pub(crate) fn last_assistant_message(status: &AgentStatus) -> Option<String> {
    match status {
        AgentStatus::Completed(message) => message.clone(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use cazean_protocol::{
        ErrorEvent, ErrorInfo, EventMsg, TurnCompletedEvent, TurnInterruptedEvent,
    };

    use super::{agent_status_from_event, is_final, last_assistant_message};

    #[test]
    fn maps_turn_events_to_agent_status() {
        let Some(completed) =
            agent_status_from_event(&EventMsg::TurnCompleted(TurnCompletedEvent {
                thread_id: "thread".to_string(),
                turn_id: "1".to_string(),
                last_assistant_message: Some("done".to_string()),
            }))
        else {
            panic!("completed status");
        };
        assert_eq!(
            completed,
            cazean_protocol::AgentStatus::Completed(Some("done".to_string()))
        );

        let Some(interrupted) =
            agent_status_from_event(&EventMsg::TurnInterrupted(TurnInterruptedEvent {
                thread_id: "thread".to_string(),
                turn_id: "2".to_string(),
                reason: "interrupt".to_string(),
            }))
        else {
            panic!("interrupted status");
        };
        assert_eq!(interrupted, cazean_protocol::AgentStatus::Interrupted);

        let error = ErrorInfo::new("turn_failed", "boom");
        let errored = agent_status_from_event(&EventMsg::Error(ErrorEvent {
            error: error.clone(),
        }));
        let Some(errored) = errored else {
            panic!("errored status");
        };
        assert_eq!(errored, cazean_protocol::AgentStatus::Errored(error));
    }

    #[test]
    fn final_statuses_match_contract() {
        assert!(is_final(&cazean_protocol::AgentStatus::Interrupted));
        assert!(is_final(&cazean_protocol::AgentStatus::Completed(None)));
        assert!(is_final(&cazean_protocol::AgentStatus::Errored(
            ErrorInfo::new("turn_failed", "x")
        )));
        assert!(is_final(&cazean_protocol::AgentStatus::Shutdown));
        assert!(!is_final(&cazean_protocol::AgentStatus::Running));
    }

    #[test]
    fn completed_status_exposes_last_assistant_message() {
        assert_eq!(
            last_assistant_message(&cazean_protocol::AgentStatus::Completed(Some(
                "done".to_string()
            ))),
            Some("done".to_string())
        );
        assert_eq!(
            last_assistant_message(&cazean_protocol::AgentStatus::Errored(ErrorInfo::new(
                "turn_failed",
                "boom"
            ))),
            None
        );
    }
}
