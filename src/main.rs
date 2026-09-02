mod agent;
mod config;
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
            other => anyhow::bail!("unknown argument: {other}\nusage: sqwai [--resume <id>]"),
        }
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
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::DisableMouseCapture,
            crossterm::terminal::LeaveAlternateScreen
        );
        let _ = crossterm::terminal::disable_raw_mode();
        eprintln!("{info}");
    }));

    providers::set_http_log(cfg.ui.http_log);
    tui::theme::set_theme(cfg.ui.theme);
    if let Some(i) = cfg.ui.anim_theme {
        tui::theme::set_anim_theme(i);
    }

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(run(cfg, resume_id))
}

async fn run(cfg: config::Config, resume_id: Option<String>) -> Result<()> {
    let startup = resume_id.is_none();
    let session = match resume_id {
        Some(id) => session::Session::load(&id)?,
        None => {
            let m = cfg.default_model_config()?.clone();
            session::Session::new(cfg.default_model.clone(), m.context)
        }
    };

    let terminal = init_terminal()?;
    let res = tui::app::App::new(cfg, session, startup)?
        .run(terminal)
        .await;
    restore_terminal()?;
    res
}

fn init_terminal() -> Result<tui::app::Terminal> {
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture,
        crossterm::event::EnableBracketedPaste,
        crossterm::cursor::Hide
    )?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    Ok(tui::app::Terminal::new(backend)?)
}

fn restore_terminal() -> Result<()> {
    crossterm::execute!(
        io::stdout(),
        crossterm::event::DisableMouseCapture,
        crossterm::event::DisableBracketedPaste,
        crossterm::cursor::Show,
        crossterm::terminal::LeaveAlternateScreen
    )?;
    crossterm::terminal::disable_raw_mode()?;
    Ok(())
}
