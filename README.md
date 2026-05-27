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

## Iced Desktop UI

`glowtail-iced` is a third native desktop front-end built on [Iced](https://iced.rs/), the Elm-inspired retained-mode toolkit with a wgpu renderer. Like the other UIs it consumes `glowtail-core` through `Engine::viewport` and `glowtail-ui-common` for session, filter, and live-tail plumbing — the only crate-local logic is the `SpanKind`→`iced::Color` mapping.

```bash
cargo run -p glowtail-iced -- samples/mixed.log
cargo run -p glowtail-iced -- samples/json.log --json
cargo run -p glowtail-iced -- samples/mixed.log --filter timeout --level warn
cargo run -p glowtail-iced -- samples/mixed.log --session .glowtail-iced-session.json
```

The interactive surface mirrors the GPUI front-end keybindings: a row selection cursor, search-result navigation, bookmarks, saved-filter cycling, level hotkeys, and a JSON detail panel for the selected row. The keyboard subscription emits shortcuts unconditionally; the update handler gates them by an `InputMode` so single letters fire only when the filter/search input doesn't have focus.

| Key | Action |
|-----|--------|
| `1` / `2` / `3` / `4` / `5` / `6` | Set `--level` filter to trace / debug / info / warn / error / fatal |
| `0` | Clear the level filter |
| `s` | Cycle through saved filters loaded from `--session` (none → first → … → last → none) |
| `/` | Focus the filter text input |
| `?` | Focus the search input |
| `enter` | Apply the input you're editing (filter text or search needle) |
| `escape` | Exit input mode and return to Normal |
| `↑` / `↓` / `j` / `k` | Move the row-selection cursor up / down (auto-scrolls; disables follow) |
| `PgUp` / `PgDn` / `Home` / `End` | Scroll a page or jump to the top/bottom (`End` re-engages follow) |
| `f` | Toggle follow mode (auto-scroll to the bottom on appended rows) |
| `b` | Bookmark (or unbookmark) the selected row — persists via `--session` |
| `n` / `N` | Jump to the next / previous search match (auto-scrolls and moves the selection cursor) |

A command palette, source sidebar, and horizontal scrolling are queued follow-ups; the rest of the interactive surface is at parity with `glowtail-gpui`.

## Makepad Desktop UI

`glowtail-makepad` is a fourth native desktop front-end built on [Makepad](https://makepad.nl/), a shader-based renderer with its own `live_design!` DSL. A custom `LogList` widget wraps Makepad's virtualised `PortalList` and is fed the engine's `ViewportSnapshot` each time the live-tail channel produces new rows. A `NextFrame`-driven loop drains the channel without bringing tokio into Makepad's event loop. Per-row colour is split across a static 16-slot `LogRow` template, one Label per `StyledSpan`, mirroring the per-span colouring that the egui/GPUI/Iced front-ends already do.

```bash
cargo run -p glowtail-makepad -- samples/mixed.log
cargo run -p glowtail-makepad -- samples/mixed.log --filter timeout --level warn
cargo run -p glowtail-makepad -- samples/mixed.log --session .glowtail-makepad-session.json
```

The interactive surface is at parity with `glowtail-iced` and `glowtail-gpui`: filter (`/`) and search (`?`) text inputs with an `InputMode` state machine, a row selection cursor (`j`/`k`/`↑`/`↓`), bookmark toggle (`b`), search-result navigation (`n`/`N`), saved-filter cycling (`s`), level hotkeys (`0`-`6`), follow toggle (`f`), stack-trace folding (`z`), a JSON detail panel for the selected row, a 220px source sidebar, and a `Cmd`/`Ctrl+K` command palette. Rows longer than the viewport scroll horizontally via the `<ScrollBars>`-wrapped `LogList`.

Spans beyond the 16-slot cap in a single row fold into a `…` truncation marker — a deliberate trade-off against per-row dynamic widget creation. Realistic engine-produced row span counts top out around 10.

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

The workspace produces five runnable binaries: `glowtail-cli` (terminal), `glowtail-gui` (egui/wgpu), `glowtail-gpui` (GPUI), `glowtail-iced` (Iced/wgpu), and `glowtail-makepad` (Makepad). Release builds land in `target/release/`.

Build one binary:

```bash
cargo build --release -p glowtail-cli      # → target/release/glowtail-cli
cargo build --release -p glowtail-gui      # → target/release/glowtail-gui
cargo build --release -p glowtail-gpui     # → target/release/glowtail-gpui
cargo build --release -p glowtail-iced     # → target/release/glowtail-iced
cargo build --release -p glowtail-makepad  # → target/release/glowtail-makepad
```

Build all five at once:

```bash
cargo build --release --workspace
```

Run a built binary directly (no `cargo` afterwards):

```bash
./target/release/glowtail-cli view samples/mixed.log
./target/release/glowtail-gui samples/mixed.log
./target/release/glowtail-gpui samples/mixed.log
./target/release/glowtail-iced samples/mixed.log
./target/release/glowtail-makepad samples/mixed.log
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
make run-iced   # cargo run -p glowtail-iced -- samples/mixed.log
make run-makepad # cargo run -p glowtail-makepad -- samples/mixed.log
```

For the optional large-viewport smoke benchmark:

```bash
cargo test -p glowtail-core --test large_viewport -- --ignored
```

### Engine viewport sweep

`viewport_perf.rs` measures `Engine::viewport()` across a sweep of viewport sizes (80 / 1 024 / 10 000 / 100 000 rows) and three filter intensities (no filter, `level >= warn`, `message contains "timeout"`). Every UI front-end pays this cost on every render that follows a state change, so the numbers show the shared per-frame floor independent of any framework:

```bash
cargo test --release -p glowtail-core --test viewport_perf -- --ignored --nocapture
```

Indicative numbers on the same Linux laptop, 100 000-row index:

| Scenario | size 80 | size 1 024 | size 10 000 |
|---|---|---|---|
| no filter | ~1.4 ms | ~1.6 ms | ~4.3 ms |
| `level >= warn` | ~230 µs | ~420 µs | ~2.6 ms |
| `contains "timeout"` (warm) | ~1.4 ms | ~1.6 ms | ~3.6 ms |

A no-filter "small viewport" call still costs ~1.4 ms because `ViewportSnapshot` carries metadata aggregates (`level_counts`, `source_summaries`, `timeline`) computed over the full filtered set, not just the requested window — the per-frame floor every UI inherits.

### Per-UI translation seam benches

Each desktop UI ships a `render_perf` test that times the per-frame CPU translation work (iterate viewport rows, compute `SeverityRole`, call `span_colour`/`span_color` per `StyledSpan`). Frame-rate and GPU costs need a real display and aren't measured here — these benches isolate the CPU portion of "what the UI does on top of the shared engine" so it's directly comparable between front-ends.

```bash
cargo test --release -p glowtail-gui     --test render_perf -- --ignored --nocapture
cargo test --release -p glowtail-iced    --test render_perf -- --ignored --nocapture
cargo test --release -p glowtail-gpui    --test render_perf -- --ignored --nocapture
cargo test --release -p glowtail-makepad --test render_perf -- --ignored --nocapture
```

Indicative numbers from one run on a Linux laptop (10 000 rows × 50 iterations ≈ 5.75 M span lookups, release profile):

| Front-end | ns/span | Notes |
|---|---|---|
| `glowtail-gui` (egui `Color32`) | ~4.7 | Packed 4-byte colour |
| `glowtail-iced` (`iced::Color`) | ~4.7 | f32 RGBA |
| `glowtail-makepad` (`Vec4`) | ~4.5 | f32 RGBA |
| `glowtail-gpui` (`gpui::Rgba` via `rgb()` hex) | ~11.2 | Extra hex→RGB extraction per lookup |

The takeaway: the four front-ends do roughly the same per-span CPU work modulo the colour-constructor cost. The dominant frame cost lives in the framework's draw and layout passes, which only a real display can measure.

To exercise the engine against a larger synthetic log, generate one with the helper script and point any front-end at it:

```bash
./scripts/gen-sample.sh 100000 samples/large.log
cargo run --release -p glowtail-cli -- view samples/large.log
```

CI runs `fmt`, `clippy --all-targets --all-features -- -D warnings`, `cargo test --workspace`, and the ignored perf smoke on every push and pull request (`.github/workflows/ci.yml`).
