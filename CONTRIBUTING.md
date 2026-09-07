# Contributing to sqwai

Thank you for your interest in contributing to sqwai!

## Read the Design First

sqwai is built around strict execution integrity on long tasks. Before proposing architectural changes or new features, please read [DESIGN.md](DESIGN.md):
- Reading order for newcomers: **§0 → §1 → §2 → §3**.
- Project status and roadmaps live exclusively in **§7 (work queue)**.
- If code and `DESIGN.md` disagree, update both in the same commit.

## Core Invariants

1. **Host-enforced integrity:** Goals (`/goal`), plans, evidence requirements, checkpoints, and safety are strictly enforced by host code, not by model prompts.
2. **Deterministic verification:** No state transition that claims work was done (`finish`, `verify`, `complete`) succeeds without host-recorded evidence in the journal.
3. **Session hygiene:** Empty placeholder sessions are never persisted to disk merely because the application opened. New, resumed, and switched sessions remain distinct.
4. **TUI resilience:**
   - The TUI must remain usable in narrow terminals (down to 30–40 columns) and with long lines.
   - Measure terminal widths in display columns (`UnicodeWidthStr`/`UnicodeWidthChar`), never in bytes or character counts.
   - Invalidate render caches whenever dimensions, layout, or content change.
   - Mouse release (`mouse_up`), focus changes, and overlays must cleanly reset dragging and selection states.

## Development Workflow

### Compile & Check
```bash
cargo check
```

### Formatting & Linting
CI checks code formatting and clippy warnings on Linux, macOS, and Windows:
```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
```

### Running Tests
Always include unit tests for bug fixes and new functionality:
```bash
# Run tests for a specific module
cargo test tui::markdown

# Run the full test suite
cargo test --locked
```

## Submitting Pull Requests

- Keep PRs focused on a single logical change or bugfix.
- Reference the corresponding issue in the PR description (`Closes #...`).
- Ensure all CI jobs (rustfmt, clippy, and cross-platform tests) pass before requesting review.
