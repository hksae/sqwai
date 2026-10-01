//! Thread scheduling for the two latency-critical std threads: the presenter
//! (sole terminal writer) and the crossterm input reader. Everything else in
//! the process is batch work and yields to them. Under a saturated CPU — the
//! agent's own test runs at full tilt — normal-priority threads are sliced
//! round-robin with the load; ABOVE_NORMAL makes the OS preempt the load
//! instead of the UI, which is exactly what the user is watching.

#[cfg(windows)]
pub fn raise_this_thread() {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
    };
    // pseudo-handle, per-process lifetime: nothing to close
    unsafe {
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }
}

#[cfg(not(windows))]
pub fn raise_this_thread() {}
