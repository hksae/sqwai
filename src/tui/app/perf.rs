//! Per-frame transcript performance log behind the `/debug` menu toggle.
//!
//! One space-separated line per drawn frame plus `EVT` marker lines for
//! tool boundaries, so a laggy session is recorded with `/debug` → perf
//! frame log → on, then read offline:
//!
//! ```text
//! # frame t_ms draw_us rebuild_us bytes pace_us merge fresh renders wraps segs rows tick streaming running view
//! 12 345 1843 1502 4096 8333 splice 1 1 3 9 42 45 1 1 0
//! EVT 1234 tool_start read
//! ```
//!
//! Disabled by default: every method early-returns on `out: None`, so the
//! hot path pays one branch per frame and nothing else.

use std::io::Write;

/// One drawn frame. `renders`/`wraps` travel as running totals and are
/// differenced inside [`PerfLog::frame`], so callers never reset counters.
pub struct FrameStat {
    pub draw_us: u128,
    pub rebuild_us: u128,
    /// terminal bytes produced by the frame (pre-kernel write volume)
    pub bytes: u64,
    /// adaptive pace in force for this frame (µs): separates "paced" lag
    /// (high pace_us) from genuine stalls (low pace_us, high draw_us)
    pub pace_us: u128,
    pub merge: &'static str,
    pub fresh: usize,
    pub segs: usize,
    pub rows: usize,
    pub tick: usize,
    pub streaming: bool,
    pub running: bool,
    /// 0 = main transcript, else the open subagent id
    pub view: u64,
}

pub struct PerfLog {
    out: Option<std::io::BufWriter<std::fs::File>>,
    path: String,
    t0: std::time::Instant,
    frames: u64,
    last_renders: u32,
    last_wraps: usize,
}

impl PerfLog {
    pub fn new() -> Self {
        Self {
            out: None,
            path: String::new(),
            t0: std::time::Instant::now(),
            frames: 0,
            last_renders: 0,
            last_wraps: 0,
        }
    }

    pub fn enabled(&self) -> bool {
        self.out.is_some()
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// Open (or close, when `on` is false) the log file. A fresh file per
    /// toggle-on keeps one lag episode per file; counter baselines resync
    /// so the first deltas are not polluted by earlier frames.
    pub fn set_enabled(
        &mut self,
        on: bool,
        renders_now: u32,
        wraps_now: usize,
    ) -> Result<String, String> {
        if !on {
            self.flush();
            self.out = None;
            self.path.clear();
            return Ok("perf frame log: off".to_string());
        }
        let path = std::env::temp_dir().join(format!("sqwai-perf-{}.log", std::process::id()));
        match std::fs::File::create(&path) {
            Ok(f) => {
                let mut out = std::io::BufWriter::new(f);
                let _ = writeln!(
                    out,
                    "# frame t_ms draw_us rebuild_us bytes pace_us merge fresh renders wraps segs rows tick streaming running view"
                );
                self.path = path.display().to_string();
                self.out = Some(out);
                self.frames = 0;
                self.t0 = std::time::Instant::now();
                self.last_renders = renders_now;
                self.last_wraps = wraps_now;
                Ok(format!("perf frame log: on → {}", self.path))
            }
            Err(e) => Err(format!("perf log open failed: {e}")),
        }
    }

    pub fn frame(&mut self, s: FrameStat, renders_now: u32, wraps_now: usize) {
        let Some(out) = self.out.as_mut() else {
            return;
        };
        let renders = renders_now.wrapping_sub(self.last_renders);
        let wraps = wraps_now.wrapping_sub(self.last_wraps) as u64;
        self.last_renders = renders_now;
        self.last_wraps = wraps_now;
        self.frames += 1;
        let _ = writeln!(
            out,
            "{} {} {} {} {} {} {} {renders} {wraps} {} {} {} {} {} {} {}",
            self.frames,
            self.t0.elapsed().as_millis(),
            s.draw_us,
            s.rebuild_us,
            s.bytes,
            s.pace_us,
            s.merge,
            s.fresh,
            s.segs,
            s.rows,
            s.tick,
            s.streaming as u8,
            s.running as u8,
            s.view,
        );
        if self.frames.is_multiple_of(100) {
            let _ = out.flush();
        }
    }

    /// Free-form marker (tool boundaries): aligns lag spikes with causes.
    pub fn event(&mut self, msg: &str) {
        let Some(out) = self.out.as_mut() else {
            return;
        };
        let _ = writeln!(out, "EVT {} {msg}", self.t0.elapsed().as_millis());
    }

    fn flush(&mut self) {
        if let Some(out) = self.out.as_mut() {
            let _ = out.flush();
        }
    }
}

impl Drop for PerfLog {
    fn drop(&mut self) {
        self.flush();
    }
}
