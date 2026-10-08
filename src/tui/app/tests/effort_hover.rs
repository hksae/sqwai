//! Keyboard-open Effort jitter guard: pure-keyboard flow with no prior mouse
//! position. The first Moved only reports where the resting mouse already
//! sits, so the fresh card must keep the cursor on the selected level.
//! Repro: model on High, mouse resting over Off, nothing clicked, Ctrl+E —
//! cursor must stay on High, not jump to Off. Clicks, arrows and Enter keep
//! working; other menus are untouched (see the frozen interaction suite).
use super::*;
use crate::config::{Config, ModelConfig, ProviderConfig, WireFormat};
use crate::session::Session;
use crate::tui::app::menus::Menu;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use std::collections::BTreeMap;

fn jitter_test_app() -> App {
    let mut providers = BTreeMap::new();
    providers.insert(
        "p".to_string(),
        ProviderConfig {
            format: WireFormat::Openai,
            base_url: "http://127.0.0.1:9/v1".into(),
            api_key: Some("test-key".into()),
            api_key_env: None,
            continuation: true,
        },
    );
    let mut models = BTreeMap::new();
    models.insert(
        "m".to_string(),
        ModelConfig {
            provider: "p".into(),
            id: "test-model".into(),
            context: 1000,
            effort: EffortLevel::Off,
            effort_control: None,
            effort_always_on: false,
            fallback: None,
            status: crate::config::ModelStatus::Active,
        },
    );
    let cfg = Config {
        last_model: "m".into(),
        legacy_default_model: String::new(),
        providers,
        models,
        safety: Default::default(),
        web: Default::default(),
        ui: Default::default(),
        mcp: Default::default(),
        lsp: Default::default(),
        skills: Default::default(),
        memory: Default::default(),
        compaction: Default::default(),
        plan: Default::default(),
        verify: Default::default(),
        secrets: Default::default(),
        undo: Default::default(),
    };
    let session = Session::new("m".into(), 1000);
    App::new(cfg, session, false, false).unwrap()
}

#[test]
fn effort_keyboard_open_no_prior_mouse_keeps_selection() {
    use crossterm::event::MouseEventKind;
    let mut app = jitter_test_app();
    app.cfg.models.get_mut("m").unwrap().effort = EffortLevel::High;
    app.model_cfg.effort = EffortLevel::High;
    // fresh app: no mouse reports yet, so the open snapshots None
    assert!(app.last_mouse.is_none());
    let (tx, rx) = std::sync::mpsc::channel();
    let me = |kind: MouseEventKind, row: u16, col: u16| {
        Event::Mouse(MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::empty(),
        })
    };
    let ctrl_e = Event::Key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
    // keyboard-open: cursor lands on the session level (High)
    tx.send(ctrl_e.clone()).unwrap();
    app.poll_input(&rx).unwrap();
    assert!(
        matches!(app.cur_menu(), Some(Menu::Effort { .. })),
        "Ctrl+E must open the Effort menu"
    );
    let want = EffortLevel::SELECTABLE
        .iter()
        .position(|l| *l == EffortLevel::High)
        .unwrap();
    assert_eq!(app.menu_sel, want, "fresh card opens on High");
    assert!(
        app.menu_open_mouse.is_none(),
        "no prior mouse: open snapshots None"
    );
    // narrow paint: slider hits stay empty, plain row list owns geometry
    let area = Rect::new(0, 0, 40, 20);
    let mut buf = Buffer::empty(area);
    app.draw_menu(&mut buf, area);
    assert!(app.effort_hits.is_empty());
    let off_row = app.menu_rect.y + 2;
    let col = app.menu_rect.x + 5;
    // ambient jitter on the resting mouse: cursor must not move
    tx.send(me(MouseEventKind::Moved, off_row, col)).unwrap();
    app.poll_input(&rx).unwrap();
    assert_eq!(
        app.menu_sel, want,
        "first Moved with no prior position must not yank the cursor to Off"
    );
    tx.send(me(MouseEventKind::Moved, off_row + 1, col))
        .unwrap();
    app.poll_input(&rx).unwrap();
    assert_eq!(
        app.menu_sel, want,
        "±1 jitter inside the deadzone must not move the cursor"
    );
    // full click gesture on the Off row still commits (clicks work)
    tx.send(me(MouseEventKind::Down(MouseButton::Left), off_row, col))
        .unwrap();
    app.poll_input(&rx).unwrap();
    assert!(
        matches!(app.cur_menu(), Some(Menu::Effort { .. })),
        "Down must not leave the Effort menu"
    );
    tx.send(me(MouseEventKind::Moved, off_row, col)).unwrap();
    app.poll_input(&rx).unwrap();
    tx.send(me(MouseEventKind::Up(MouseButton::Left), off_row, col))
        .unwrap();
    app.poll_input(&rx).unwrap();
    assert_eq!(app.model_cfg.effort, EffortLevel::Off);
    assert!(app.cur_menu().is_none(), "click commits and closes");
    // arrows + Enter still work: reopen (now on Off), step down, commit
    tx.send(ctrl_e).unwrap();
    app.poll_input(&rx).unwrap();
    assert!(
        matches!(app.cur_menu(), Some(Menu::Effort { .. })),
        "Ctrl+E must reopen the Effort menu"
    );
    assert_eq!(app.menu_sel, 0, "reopened card opens on Off");
    let mut buf2 = Buffer::empty(area);
    app.draw_menu(&mut buf2, area);
    tx.send(Event::Key(KeyEvent::new(
        KeyCode::Down,
        KeyModifiers::empty(),
    )))
    .unwrap();
    app.poll_input(&rx).unwrap();
    assert_eq!(app.menu_sel, 1, "arrows move the cursor");
    tx.send(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::empty(),
    )))
    .unwrap();
    app.poll_input(&rx).unwrap();
    assert_eq!(app.model_cfg.effort, EffortLevel::Low);
    assert!(app.cur_menu().is_none(), "Enter commits and closes");
}
