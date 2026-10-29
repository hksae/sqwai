# AGENTS.md — sqwai development rules

## Build and test
- Use `cargo check` for a fast compile check (pinned toolchain via rust-toolchain.toml).
- Use targeted `cargo test <filter>` when verifying a completed change; run the full `cargo test` suite ONLY when all the work is finished, or the user asked for it — a large diff is not a reason by itself.
- Build once: `cargo test --no-run`, then run the built test binary with a filter. A filtered `cargo test` still recompiles and relinks the whole crate, and that build costs more than the tests.
- Review insta snapshots by hand before accepting them; never bulk-accept a refresh.
- Do not build a release executable automatically unless explicitly asked (the user builds it); run only tests.

## Git and workflow
- Commit completed changes, even the smallest fixes, adjustments, and tweaks, once verified.

## TUI invariants
- Keep the TUI usable in narrow terminals and with long lines.
- Every rendered row must respect the available width.
- Preserve keyboard, mouse, focus, resize, scrolling, overlay, and mode-transition behavior when changing views.

## Project behavior
- Preserve host-owned plan, evidence, checkpoint, safety, and untrusted-content behavior.
