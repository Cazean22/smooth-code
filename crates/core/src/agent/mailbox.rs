use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};

use smooth_protocol::InterAgentCommunication;
use tokio::sync::mpsc;

#[derive(Clone)]
pub(crate) struct Mailbox {
    tx: mpsc::UnboundedSender<InterAgentCommunication>,
    pending_trigger_turn: Arc<AtomicI64>,
}

pub(crate) struct MailboxReceiver {
    rx: mpsc::UnboundedReceiver<InterAgentCommunication>,
    pending_trigger_turn: Arc<AtomicI64>,
}

impl Mailbox {
    pub(crate) fn new() -> (Self, MailboxReceiver) {
        let (tx, rx) = mpsc::unbounded_channel();
        let pending_trigger_turn = Arc::new(AtomicI64::new(0));
        (
            Self {
                tx,
                pending_trigger_turn: Arc::clone(&pending_trigger_turn),
            },
            MailboxReceiver {
                rx,
                pending_trigger_turn,
            },
        )
    }

    pub(crate) fn send(&self, communication: InterAgentCommunication) -> Result<(), String> {
        let increments_trigger = communication.trigger_turn;
        if increments_trigger {
            self.pending_trigger_turn.fetch_add(1, Ordering::SeqCst);
        }

        if self.tx.send(communication).is_err() {
            if increments_trigger {
                self.pending_trigger_turn.fetch_sub(1, Ordering::SeqCst);
            }
            return Err("mailbox receiver dropped".to_string());
        }

        Ok(())
    }

    pub(crate) fn has_trigger_turn_pending(&self) -> bool {
        self.pending_trigger_turn.load(Ordering::SeqCst) > 0
    }
}

impl MailboxReceiver {
    pub(crate) fn try_drain_all(&mut self) -> Vec<InterAgentCommunication> {
        let mut drained = Vec::new();
        let mut drained_trigger_turn = 0_i64;

        while let Ok(communication) = self.rx.try_recv() {
            if communication.trigger_turn {
                drained_trigger_turn += 1;
            }
            drained.push(communication);
        }

        if drained_trigger_turn > 0 {
            self.pending_trigger_turn
                .fetch_sub(drained_trigger_turn, Ordering::SeqCst);
        }

        drained
    }
}

#[cfg(test)]
mod tests {
    use smooth_protocol::AgentPath;

    use super::Mailbox;

    fn message(content: &str, trigger_turn: bool) -> smooth_protocol::InterAgentCommunication {
        smooth_protocol::InterAgentCommunication::new(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("path"),
            vec![],
            content.to_string(),
            trigger_turn,
        )
    }

    #[test]
    fn drain_preserves_fifo_and_trigger_count() {
        let (mailbox, mut rx) = Mailbox::new();
        mailbox.send(message("a", true)).expect("send a");
        mailbox.send(message("b", false)).expect("send b");
        mailbox.send(message("c", true)).expect("send c");

        assert!(mailbox.has_trigger_turn_pending());

        let drained = rx.try_drain_all();
        let contents = drained
            .into_iter()
            .map(|communication| communication.content)
            .collect::<Vec<_>>();

        assert_eq!(contents, vec!["a", "b", "c"]);
        assert!(!mailbox.has_trigger_turn_pending());
    }
}
