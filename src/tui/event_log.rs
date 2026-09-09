use std::fmt::Display;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

static LOG: OnceLock<Option<Mutex<File>>> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();

fn file() -> &'static Option<Mutex<File>> {
    LOG.get_or_init(|| {
        std::env::var_os("SQWAI_EVENT_LOG")
            .and_then(|path| OpenOptions::new().create(true).append(true).open(path).ok())
            .map(Mutex::new)
    })
}

/// Cheap gate so callers can skip building the (sometimes O(paste))
/// description string entirely when logging is off.
pub fn is_enabled() -> bool {
    file().is_some()
}

pub fn log(tag: &str, message: impl Display) {
    let Some(file) = file() else {
        return;
    };
    let start = START.get_or_init(Instant::now);
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    if let Ok(mut file) = file.lock() {
        let _ = writeln!(file, "{elapsed_ms:>10.3}ms {tag} {message}");
        let _ = file.flush();
    }
}

pub fn describe(event: &crossterm::event::Event) -> String {
    use crossterm::event::Event;
    match event {
        Event::Key(key) => format!(
            "code={:?} mods={:?} kind={:?} state={:?}",
            key.code, key.modifiers, key.kind, key.state
        ),
        Event::Paste(text) => format!(
            "len={} newlines={} text={:?}",
            text.chars().count(),
            text.chars().filter(|ch| *ch == '\n').count(),
            preview(text)
        ),
        other => format!("{other:?}"),
    }
}

fn preview(text: &str) -> String {
    text.chars().take(120).collect()
}
