# Issues Found during Analysis

Here is a list of issues found during a code review of the `sqwai` project. (These can be copied and created as GitHub issues).

## Issue 1: Flaky Test in Project Lock (TOCTOU on File Creation Timestamp)

**File:** `src/lock.rs`
**Test:** `first_instance_is_writable_and_second_is_read_only`

**Description:**
The `ProjectLock::acquire` function determines if a lock is read-only or writable by checking if the lock file's creation/modification time is older than the other lock files in the `.sqwai/lock` directory. In the test `first_instance_is_writable_and_second_is_read_only`, two locks are acquired back-to-back. File modification timestamps (`mtime`) often have low precision (like 1ms or even 1s). Because of this, both lock files can end up with the exact same timestamp.

When this happens, the `is_lock_superseded` function falls back to comparing filenames. Filenames are generated using `Uuid::new_v4().to_string()`. If the first lock happens to get a smaller UUID string than the second lock, the second lock is incorrectly considered older (superseded), causing the assertion `assert!(second.read_only);` to occasionally fail when the test is run multiple times in a loop.

**Fix/Recommendation:**
Add a small `std::thread::sleep(std::time::Duration::from_millis(10));` in the test between acquiring the first and second lock, to guarantee a strictly higher timestamp for the second lock. Or, reverse the filename comparison logic in the tie-breaker `is_lock_superseded`.

---

## Issue 2: Unnecessary Boolean Complexity (Clippy Warning)

**File:** `src/agent/graph_index.rs`
**Line:** 298

**Description:**
The boolean expression `!(edge.to.starts_with("file:") && !retained_paths.contains(&edge.to["file:".len()..]))` is more complex than it needs to be, as flagged by `cargo clippy`.

**Fix/Recommendation:**
By De Morgan's laws, it can be simplified to:
`!edge.to.starts_with("file:") || retained_paths.contains(&edge.to["file:".len()..])`

This improves readability and clears the Clippy warning.
