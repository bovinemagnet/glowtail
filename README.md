# glowtail

A Rust-first multi-log viewer with a UI-neutral core.

## UI boundary

`glowtail-core` contains parsing, indexing, filtering, tailing, and viewport logic.
It intentionally does **not** depend on terminal or GPU UI frameworks.

The first UI is implemented in `glowtail-tui` with Ratatui/crossterm, and it maps semantic
`RowPresentation` spans from core into terminal styling locally.

This boundary keeps the engine reusable for future GPU UIs (for example GPUI, egui + wgpu,
or custom wgpu rendering) without rewriting parsing or indexing.

## Quick start

```bash
cargo run -p glowtail-cli -- view samples/plain.log
cargo run -p glowtail-cli -- view samples/json.log
cargo run -p glowtail-cli -- tail samples/mixed.log --level warn --no-follow --from-start
```
