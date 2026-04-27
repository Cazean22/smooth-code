use smooth_protocol::Op;

#[derive(Debug)]
pub(crate) enum AppEvent {
    SubmitThreadOp { op: Op },
}
