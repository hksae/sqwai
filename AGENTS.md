# AGENTS.md — sqwai development rules

## Build and test
- Use `C:\Users\Asus\.cargo\bin\cargo.exe check` for a fast compile check.
- Use `C:\Users\Asus\.cargo\bin\cargo.exe test` for the full test suite when requested or when verifying a completed change.
- Do not build a release executable automatically unless explicitly asked (the user builds it); run only tests.

## Git and workflow
- Commit completed changes, even the smallest fixes, adjustments, and tweaks, once verified.

## TUI invariants
- Keep the TUI usable in narrow terminals and with long lines.
- Every rendered row must respect the available width.
- Invalidate render caches when content, layout, or dimensions change.
- Preserve keyboard, mouse, focus, resize, scrolling, overlay, and mode-transition behavior when changing views.

## Project behavior
- Keep new, resumed, and switched sessions distinct.
- Do not persist an empty placeholder session merely because the application opened.
- Preserve host-owned plan, evidence, checkpoint, memory, safety, and untrusted-content behavior.
