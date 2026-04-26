use smooth_protocol::{Op, ThreadId};

#[derive(Debug)]
pub(crate) enum AppEvent {
    SubmitThreadOp { thread_id: ThreadId, op: Op },
}
