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
cargo run -p glowtail-cli -- view samples/mixed.log --filter 'service = "billing" and level >= warn'
```

`tail` prints matching raw log lines to stdout and is useful for scripts or quick checks:

```bash
cargo run -p glowtail-cli -- tail samples/mixed.log --level warn --no-follow
cargo run -p glowtail-cli -- tail samples/json.log --json --filter 'service = "billing"' --no-follow
```

Use `--json` to force JSON-lines parsing, `--plain` to force plain-text parsing, or omit both to use the composite parser. `--filter timeout` still performs a case-insensitive contains match against the raw log line. Query filters also support `level = error`, `level in (warn, error)`, `message contains "timeout"`, `service = "billing"`, `json.userId = "123"`, `source = 1`, and timestamp ranges such as `timestamp between "2026-05-21T09:00:00Z" and "2026-05-21T10:00:00Z"`. `--level` keeps rows at or above the selected level and composes with `--filter`.

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

`glowtail-gpui` is a second native desktop prototype built with the GPUI library from the Zed ecosystem. It shares the engine with the other front-ends and renders GPUI components for the source sidebar, lazily virtualised log list, severity bands, first-row JSON detail panel, and timeline. Rows are fetched on demand via `Engine::present_row_at`, so opening a million-line file does not materialise a million `RowPresentation` objects per frame.

```bash
cargo run -p glowtail-gpui -- samples/mixed.log
cargo run -p glowtail-gpui -- samples/json.log --json
cargo run -p glowtail-gpui -- samples/mixed.log --filter timeout --level warn
cargo run -p glowtail-gpui -- samples/mixed.log --from-start
cargo run -p glowtail-gpui -- samples/mixed.log --no-follow
cargo run -p glowtail-gpui -- samples/mixed.log --session .glowtail-gpui-session.json
```

By default the GPUI app follows appended lines through the shared `glowtail-core` tailer. Use `--no-follow` for a static preload, or `--from-start` to have the live tailer replay existing file contents before following new lines. `--session`, `--use-filter`, and `--save-filter` work as they do in the terminal commands and the GUI.

The GPUI prototype now has row selection, bookmarks, in-window search with n/N navigation, a Cmd/Ctrl+K command palette, and keyboard-driven filtering — feature parity with `glowtail-gui` for the core interactive surface.

| Key | Action |
|-----|--------|
| `1` / `2` / `3` / `4` / `5` / `6` | Set `--level` filter to trace / debug / info / warn / error / fatal |
| `0` | Clear the level filter |
| `s` | Cycle through saved filters loaded from `--session` (none → first → … → last → none) |
| `/` | Focus the filter text input |
| `enter` | Apply the typed filter text (composes with the active level and saved filter) |
| `escape` | Cancel filter input editing and restore the previously applied text |
| `backspace` | Delete the trailing character while editing |
| `↑` / `↓` / `PgUp` / `PgDn` / `Home` / `End` | Scroll vertically; `End` re-engages follow mode |
| `←` / `→` / `Cmd+←` | Scroll horizontally / reset to the line start |
| `f` | Toggle follow mode (auto-scroll to the bottom on appended rows) |
| `j` / `k` | Move the row-selection cursor down / up (auto-scrolls to keep the cursor visible; disables follow) |
| `Shift+j` / `Shift+k` | Move the selection cursor by a page |
| `b` | Bookmark (or unbookmark) the selected row — persists via `--session` |
| `?` | Focus the search input (`enter` applies, `escape` cancels — same modal model as `/` for filters) |
| `n` / `Shift+n` | Jump to the next / previous search match (auto-scrolls and moves the selection cursor) |
| `Cmd+Shift+f` / `Ctrl+Shift+f` | Clear the active search |
| `Cmd+k` / `Ctrl+k` | Toggle the command palette (j/k or arrows to select, `enter` to run, `escape` to close) |

The text input is intentionally minimal: append-only, no cursor positioning, no IME composition. Use `escape` to discard a partial edit.

### Tailing a log file with glowtail-gpui

The GPUI front-end follows appended lines by default — point it at a log file and new lines stream in live through the shared `glowtail-core` tailer:

```bash
# Follow a single log file as new lines are appended (default behaviour)
cargo run -p glowtail-gpui -- /var/log/myapp.log

# Follow several files at once — each appears in the source sidebar
cargo run -p glowtail-gpui -- /var/log/app/server.log /var/log/app/worker.log

# Replay existing content through the tailer, then keep following new lines
cargo run -p glowtail-gpui -- /var/log/myapp.log --from-start

# Tail and only show rows at warn level or above
cargo run -p glowtail-gpui -- /var/log/myapp.log --level warn

# Tail and narrow to lines containing a token (case-insensitive substring)
cargo run -p glowtail-gpui -- /var/log/myapp.log --filter timeout

# Tail a JSON-lines log with a field-aware query filter
cargo run -p glowtail-gpui -- /var/log/myapp.jsonl --json \
  --filter 'service = "billing" and level >= error'

# Cap the in-memory ring buffer so long-running tails don't grow unbounded
cargo run -p glowtail-gpui -- /var/log/myapp.log --max-rows 50000

# Re-use a saved filter from a session while tailing
cargo run -p glowtail-gpui -- /var/log/myapp.log \
  --session .glowtail-gpui-session.json --use-filter warnings

# One-shot snapshot of current content, no follow
cargo run -p glowtail-gpui -- /var/log/myapp.log --no-follow

# Reproduce a tail against a sample log shipped with the repo
cargo run -p glowtail-gpui -- samples/mixed.log --from-start --level warn
```

`--no-follow` preloads the file once and stops; `--from-start` is the right choice when you want the live tailer to replay current content before streaming new lines (useful when an active writer is rotating the file out from under you).

## Following and Existing Content

By default, commands follow files for appended lines. Add `--no-follow` for one-shot reads that exit after current content is processed.

```bash
cargo run -p glowtail-cli -- tail samples/mixed.log --no-follow
cargo run -p glowtail-cli -- view samples/mixed.log --from-start
```

## Interactive TUI Keys

- `q`: quit
- `j`/Down and `k`/Up: move the selection cursor (scrolls the viewport at the edges)
- `g` and `G`: jump to top or bottom
- `f`: toggle follow mode
- `/`: enter search text
- `n`/`N`: jump to next or previous search result
- `F`: enter a contains filter
- `b`: bookmark the currently selected row
- `z`: toggle stack-trace folding
- `Esc`: leave input mode

The status bar shows matching rows, total rows, warning/error counts, source summaries, follow mode, stack folding state, timeline bucket count, and any transient error (for example, a filter that failed to compile). Timeline metadata also tracks timestamp coverage, warning/error peaks, per-bucket severity counts, and the dominant source in each bucket for UI analytics.

## Sessions and Saved Filters

Use `--session` to persist investigation state as JSON. A session can store filter history, saved filters, and bookmarks. The on-disk format carries a `version` field and rejects unknown keys, so a future build that adds new state will either migrate old sessions or fail loudly instead of silently dropping data.

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

## Building release binaries

The workspace produces three runnable binaries: `glowtail-cli` (terminal), `glowtail-gui` (egui/wgpu), and `glowtail-gpui` (GPUI). Release builds land in `target/release/`.

Build one binary:

```bash
cargo build --release -p glowtail-cli      # → target/release/glowtail-cli
cargo build --release -p glowtail-gui      # → target/release/glowtail-gui
cargo build --release -p glowtail-gpui     # → target/release/glowtail-gpui
```

Build all three at once:

```bash
cargo build --release --workspace
```

Run a built binary directly (no `cargo` afterwards):

```bash
./target/release/glowtail-cli view samples/mixed.log
./target/release/glowtail-gui samples/mixed.log
./target/release/glowtail-gpui samples/mixed.log
```

Install into `~/.cargo/bin/` so the binary is on `$PATH`:

```bash
cargo install --path crates/glowtail-cli
glowtail-cli view samples/mixed.log
```

Optional: strip debug info to shrink the binary, or add a `[profile.release]` block to `Cargo.toml` with `lto = "thin"`, `codegen-units = 1`, and `strip = true` for an aggressively optimised build:

```bash
strip target/release/glowtail-cli
```

On Linux the GPU front-ends need a few system libraries at build *and* run time; the CLI binary has no such requirements. On Debian/Ubuntu:

```bash
sudo apt-get install libxkbcommon-dev libwayland-dev libxcb-shape0-dev libxcb-xfixes0-dev pkg-config
```

## Development

```bash
make fmt        # cargo fmt --all
make clippy     # cargo clippy --all-targets --all-features -- -D warnings
make test       # cargo test
make run-sample # cargo run -p glowtail-cli -- view samples/mixed.log
make run-gui    # cargo run -p glowtail-gui  -- samples/mixed.log
make run-gpui   # cargo run -p glowtail-gpui -- samples/mixed.log
```

For the optional large-viewport smoke benchmark:

```bash
cargo test -p glowtail-core --test large_viewport -- --ignored
```

To exercise the engine against a larger synthetic log, generate one with the helper script and point any front-end at it:

```bash
./scripts/gen-sample.sh 100000 samples/large.log
cargo run --release -p glowtail-cli -- view samples/large.log
```

CI runs `fmt`, `clippy --all-targets --all-features -- -D warnings`, `cargo test --workspace`, and the ignored perf smoke on every push and pull request (`.github/workflows/ci.yml`).
