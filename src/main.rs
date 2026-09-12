mod agent;
mod config;
mod lock;
mod lsp;
mod mcp;
mod plan;
mod prompts;
mod providers;
mod session;
mod tui;

use std::io;

use anyhow::{Context, Result};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut resume_id: Option<String> = None;
    let mut force = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--resume" | "-r" => {
                resume_id = Some(it.next().context("--resume requires a session id")?.clone());
            }
            "--version" | "-V" => {
                println!("sqwai {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--force" => force = true,
            other => {
                anyhow::bail!("unknown argument: {other}\nusage: sqwai [--resume <id>] [--force]")
            }
        }
    }

    let project_root = std::env::current_dir().context("cannot determine project root")?;
    let project_lock = lock::ProjectLock::acquire(&project_root, force)?;
    if let Some(message) = project_lock.status_message() {
        eprintln!("warning: {message}");
    }

    let cfg = match config::Config::load() {
        Ok(cfg) => cfg,
        Err(config::LoadError::Missing(path)) => {
            config::write_template(&path)?;
            eprintln!(
                "no config found; template written to {}\nrun sqwai and press ctrl+p to add a provider and model",
                path.display()
            );
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };

    std::panic::set_hook(Box::new(|info| {
        // TUI is not active yet here (presenter starts inside run()); a plain
        // direct restore is correct. The in-TUI hook installed in run()
        // replaces this one and drains through the presenter first.
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::DisableMouseCapture,
            crossterm::terminal::LeaveAlternateScreen
        );
        let _ = crossterm::terminal::disable_raw_mode();
        eprintln!("{info}");
    }));

    providers::set_http_log(cfg.ui.http_log);

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(run(cfg, resume_id, project_lock.read_only))
}

/// Handles for the dedicated presenter thread (sole terminal writer).
struct PresenterHandles {
    tx: tui::presenter::FrameTx,
    stats_rx: std::sync::mpsc::Receiver<tui::presenter::FrameReport>,
    alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
    size: ratatui::layout::Size,
    join: std::thread::JoinHandle<()>,
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = restore_terminal();
    }
}

async fn run(cfg: config::Config, resume_id: Option<String>, read_only: bool) -> Result<()> {
    let startup = resume_id.is_none();
    let session = match resume_id {
        Some(id) => session::Session::load(&id)?,
        None => {
            let m = cfg.default_model_config()?.clone();
            let mut s = session::Session::new(cfg.default_model.clone(), m.context);
            s.project = Some(std::env::current_dir().unwrap_or_default());
            s
        }
    };

    let pres = init_presenter()?;
    let _guard = TerminalGuard;
    // From here on the presenter is the sole terminal writer. On panic:
    // stop it first (with a grace period), then restore directly — the only
    // legitimate raw-stdout write while the TUI is up (emergency path).
    let panic_tx = pres.tx.clone();
    std::panic::set_hook(Box::new(move |info| {
        panic_tx.shutdown();
        std::thread::sleep(std::time::Duration::from_millis(100));
        let _ = restore_terminal();
        eprintln!("{info}");
    }));
    let app = tui::app::App::new(cfg, session, startup, read_only)?;
    let res = app
        .run(pres.tx.clone(), pres.stats_rx, pres.alive, pres.size)
        .await;
    // Sequenced shutdown: no frame can interleave the restore below,
    // because the presenter has exited before it runs.
    pres.tx.shutdown();
    let _ = pres.join.join();
    restore_terminal()?;
    res
}

/// Start raw mode + alternate screen, then hand the terminal to the
/// dedicated presenter thread. One `terminal::size()` call per session here;
/// steady-state sizes arrive via Resize events. Returns the UI-side handles
/// plus the join handle (joined before restore, so no frame can interleave
/// the restore sequence).
fn init_presenter() -> Result<PresenterHandles> {
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture,
        crossterm::cursor::Hide
    )?;
    let (cols, rows) = crossterm::terminal::size()?;
    // BufWriter coalesces the hundreds of small per-cell writes of a frame
    // into one or two syscalls; every frame still ends with an explicit
    // flush, so no output ever sits in the buffer across frames.
    // TapWriter counts frame wire bytes for the /debug perf log.
    let tap = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let backend = ratatui::backend::CrosstermBackend::new(
        tui::presenter::TapWriter::new(
            io::BufWriter::with_capacity(256 * 1024, io::stdout()),
            std::sync::Arc::clone(&tap),
        ),
    );
    let alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let presenter = tui::presenter::Presenter::new(backend, tap, std::sync::Arc::clone(&alive));
    let (tx, rx) = tui::presenter::mailbox();
    let (stats_tx, stats_rx) = std::sync::mpsc::channel();
    let join = tui::presenter::spawn(presenter, rx, stats_tx);
    Ok(PresenterHandles {
        tx,
        stats_rx,
        alive,
        size: ratatui::layout::Size::new(cols, rows),
        join,
    })
}

fn restore_terminal() -> Result<()> {
    crossterm::execute!(
        io::stdout(),
        crossterm::event::DisableMouseCapture,
        crossterm::cursor::Show,
        crossterm::terminal::LeaveAlternateScreen
    )?;
    crossterm::terminal::disable_raw_mode()?;
    Ok(())
}
