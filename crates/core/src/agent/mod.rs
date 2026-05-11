#![allow(dead_code)]

pub(crate) mod agent_resolver;
pub(crate) mod control;
pub(crate) mod fork;
pub(crate) mod mailbox;
pub(crate) mod registry;
pub(crate) mod role;
pub(crate) mod status;

pub use control::AgentControl;
pub(crate) use control::InlineChildCompletionReceiver;
pub(crate) use mailbox::{Mailbox, MailboxReceiver};
