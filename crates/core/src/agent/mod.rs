#![allow(dead_code)]

pub(crate) mod agent_resolver;
pub(crate) mod control;
pub(crate) mod mailbox;
pub(crate) mod registry;
pub(crate) mod status;

pub(crate) use control::AgentControl;
pub(crate) use mailbox::{Mailbox, MailboxReceiver};
