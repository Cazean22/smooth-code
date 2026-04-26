use std::path::PathBuf;

use crate::core::Core;

pub struct CoreThread {
    pub(crate) core: Core,
    rollout_path: Option<PathBuf>,
}
