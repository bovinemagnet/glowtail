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

## Commands

`view` starts the Ratatui interface:

```bash
cargo run -p glowtail-cli -- view samples/mixed.log
cargo run -p glowtail-cli -- view samples/json.log --json
cargo run -p glowtail-cli -- view samples/mixed.log --filter billing --level warn
```

`tail` prints matching raw log lines to stdout and is useful for scripts or quick checks:

```bash
cargo run -p glowtail-cli -- tail samples/mixed.log --level warn --no-follow
cargo run -p glowtail-cli -- tail samples/json.log --json --filter billing --no-follow
```

Use `--json` to force JSON-lines parsing, `--plain` to force plain-text parsing, or omit both to use the composite parser. `--filter` performs a case-insensitive contains match against the raw log line, including JSON fields. `--level` keeps rows at or above the selected level.

## Native GPU UI

Phase 3 adds `glowtail-gui`, a native egui/wgpu desktop interface that uses the same `glowtail-core` viewport API as the terminal UI.

```bash
cargo run -p glowtail-gui -- samples/mixed.log
cargo run -p glowtail-gui -- samples/json.log --json
cargo run -p glowtail-gui -- samples/mixed.log --filter timeout --level warn
cargo run -p glowtail-gui -- samples/mixed.log --session .glowtail-gui-session.json
```

The desktop UI includes live tailing, a source sidebar, virtualized log viewport, severity color bands, search highlights, stack folding, timeline/minimap strip, JSON field detail panel, saved filters, bookmarks, search navigation, and a command palette. Press Cmd/Ctrl+K or the Command button to open the palette.

By default the GUI follows appended lines. Use `--no-follow` for a static desktop inspection, or `--from-start` to have the live tailer replay current file contents instead of preloading them. `--session`, `--use-filter`, and `--save-filter` work in the GUI as they do in the terminal commands.

## GPUI Desktop UI

`glowtail-gpui` is a second native desktop prototype built with the GPUI library from the Zed ecosystem. It uses the same `glowtail-core` viewport snapshots and renders GPUI components for the source sidebar, virtualized log list, severity bands, first-row JSON detail panel, and timeline.

```bash
cargo run -p glowtail-gpui -- samples/mixed.log
cargo run -p glowtail-gpui -- samples/json.log --json
cargo run -p glowtail-gpui -- samples/mixed.log --filter timeout --level warn
```

## Following and Existing Content

By default, commands follow files for appended lines. Add `--no-follow` for one-shot reads that exit after current content is processed.

```bash
cargo run -p glowtail-cli -- tail samples/mixed.log --no-follow
cargo run -p glowtail-cli -- view samples/mixed.log --from-start
```

## Interactive TUI Keys

- `q`: quit
- `j`/Down and `k`/Up: scroll
- `g` and `G`: jump to top or bottom
- `f`: toggle follow mode
- `/`: enter search text
- `n`/`N`: jump to next or previous search result
- `F`: enter a contains filter
- `b`: bookmark the first visible row
- `z`: toggle stack-trace folding
- `Esc`: leave input mode

The status bar shows matching rows, total rows, warning/error counts, source summaries, follow mode, stack folding state, and timeline bucket count.

## Sessions and Saved Filters

Use `--session` to persist investigation state as JSON. A session can store filter history, saved filters, and bookmarks.

```bash
cargo run -p glowtail-cli -- tail samples/mixed.log \
  --level warn --no-follow \
  --session .glowtail-session.json \
  --save-filter warnings

cargo run -p glowtail-cli -- tail samples/mixed.log \
  --no-follow \
  --session .glowtail-session.json \
  --use-filter warnings
```

The same flags work with `view`, so bookmarks made in the TUI can be saved when the app exits.

## Development

```bash
make fmt       # cargo fmt --all
make clippy    # cargo clippy --all-targets --all-features -- -D warnings
make test      # cargo test
make run-sample
make run-gui
```

For the optional large-viewport smoke benchmark:

```bash
cargo test -p glowtail-core --test large_viewport -- --ignored
```
