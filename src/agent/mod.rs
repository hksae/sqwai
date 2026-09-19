#[cfg(test)]
pub mod bench_harness;
pub mod blobs;
pub mod checkpoints;
pub mod context;
// temporary: the turn hook wires criticism next; drop this allow with it.
#[allow(dead_code)]
pub mod criticism;
pub mod diary;
pub mod graph;
pub mod graph_index;
pub mod graph_lang;
pub mod graph_memory;
pub mod journal;
pub mod lint;
pub mod loop_task;
pub mod memory;
pub mod notify;
pub mod safety;
pub mod secrets;
pub mod shadow;
pub mod shell;
pub mod tools;
pub mod undo_guard;
