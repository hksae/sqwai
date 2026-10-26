//! Built-in tool registry (phase 2).
//!
//! Each tool declares its JSON schema for the model and a handler. Handlers
//! receive a [`ToolCtx`] carrying the project root and session-scoped guard
//! state (which files were read, checkpoint events).

mod ctx;
mod dispatch;
mod exec;
mod fs;
mod git;
mod outline;
mod policy;
mod specs;
mod verify;
mod view_document;
mod view_image;
pub(crate) mod web;

pub use ctx::ReadLedger;
pub(crate) use ctx::{ReadState, ToolCtx};
pub(crate) use dispatch::{
    FileDiff, Outcome, bg_running_commands_for, execute, kill_remaining_jobs,
};
pub(crate) use git::porcelain_paths;
pub(crate) use policy::{
    bash_scope_hit, first_outside_target, register_subagent_scope, take_subagent_scope,
};
pub(crate) use specs::{
    Kind, call_path, call_summary, decode_child_output, is_mutating_call, merge_specs, tool_names,
    tool_specs, trim_middle,
};

#[cfg(test)]
mod tests;
