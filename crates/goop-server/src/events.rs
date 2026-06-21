// Re-export shared types so internal code continues to use `crate::events::*`.
pub use goop_shared::{
    ClientMessage, EditContent, LogEntry, PromptSource, ServerMessage, SessionEvent, TurnEndReason,
};
