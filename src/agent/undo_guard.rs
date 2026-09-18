//! S1 remainder: writer lock for undo restores + running-child registry.
//!
//! `/undo` is refused while a turn streams, so in practice no tool dispatch
//! runs during a restore — but nothing said so in code, and the restore
//! itself (blob/shadow writes) had no guard at all. The lock makes the
//! invariant explicit: while held, the dispatcher refuses file mutations
//! with `code: writer_locked`. Reentrancy (undo calling into undo) shares
//! the outer hold instead of deadlocking.
//!
//! The child registry closes the other half: `reopen` bumps the epoch and
//! stale-epoch refusal stops the *next* mutation, but nothing actively told
//! a running child to stop. Undo now cancels whatever is registered before
//! it touches the tree. Children always join inside their `task` call, so
//! the registry is normally empty — the signal path exists for the day it
//! is not, and for tests to prove it.

use std::collections::HashMap;
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
};

static RESTORE: AtomicBool = AtomicBool::new(false);

/// RAII hold on the restore lock. Dropped (released) at end of scope.
pub struct RestoreGuard {
    outer: bool,
}

/// Take the restore lock. `outer == false` means this call took it and
/// releases on drop; `true` means it was already held (nested undo) and
/// there is nothing to release.
pub fn hold_restore() -> RestoreGuard {
    let outer = RESTORE.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err();
    RestoreGuard { outer }
}

pub(crate) fn restore_active() -> bool {
    RESTORE.load(Ordering::SeqCst)
}

impl Drop for RestoreGuard {
    fn drop(&mut self) {
        if !self.outer {
            RESTORE.store(false, Ordering::SeqCst);
        }
    }
}

/// The control half of a running child: enough to stop it without owning
/// its event stream.
pub struct ChildControl {
    abort: tokio::task::AbortHandle,
    cancel: Arc<AtomicBool>,
}

impl ChildControl {
    pub(crate) fn new(abort: tokio::task::AbortHandle, cancel: Arc<AtomicBool>) -> Self {
        Self { abort, cancel }
    }

    /// Cooperative stop first (a `bash` child kills its own tree on this),
    /// then tear down hard — the same order as the subagent timeout path.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.abort.abort();
    }
}

fn children() -> &'static Mutex<HashMap<u64, ChildControl>> {
    static RUNNING: OnceLock<Mutex<HashMap<u64, ChildControl>>> = OnceLock::new();
    RUNNING.get_or_init(|| Mutex::new(HashMap::new()))
}

/// RAII registration: the child leaves the map on every exit path,
/// including early returns.
pub struct ChildRegistration(u64);

pub fn track_child(id: u64, control: ChildControl) -> ChildRegistration {
    if let Ok(mut map) = children().lock() {
        map.insert(id, control);
    }
    ChildRegistration(id)
}

impl Drop for ChildRegistration {
    fn drop(&mut self) {
        if let Ok(mut map) = children().lock() {
            map.remove(&self.0);
        }
    }
}

/// Cancel every registered child. Returns how many were signalled.
pub fn cancel_running_children() -> usize {
    let mut n = 0;
    if let Ok(map) = children().lock() {
        for control in map.values() {
            control.cancel();
            n += 1;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn restore_guard_is_exclusive_and_reentrant() {
        let outer = hold_restore();
        assert!(!outer.outer);
        assert!(restore_active());
        let nested = hold_restore();
        assert!(nested.outer);
        drop(nested);
        assert!(restore_active(), "nested drop must not release the outer hold");
        drop(outer);
        assert!(!restore_active());
    }

    #[tokio::test]
    async fn cancel_registry_signals_and_drains() {
        assert_eq!(cancel_running_children(), 0);
        let flag = Arc::new(AtomicBool::new(false));
        let probe = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        });
        let reg = track_child(4242, ChildControl::new(probe.abort_handle(), flag.clone()));
        assert_eq!(cancel_running_children(), 1);
        assert!(flag.load(Ordering::Relaxed));
        // abort is scheduled, not synchronous: joining proves the teardown
        assert!(probe.await.unwrap_err().is_cancelled());
        drop(reg);
        assert_eq!(cancel_running_children(), 0);
    }
}
