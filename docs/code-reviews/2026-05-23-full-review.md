= Full Code Review: glowtail Workspace
:author: Paul Snow
:date: 2026-05-23
:revision: 0.0.0

== Summary

A full-coverage review of the `glowtail` workspace at HEAD `5d88a78` — ~5,800
LOC of Rust 2024 across one engine crate (`glowtail-core`) and four
front-ends (`glowtail-cli`, `glowtail-tui`, `glowtail-gui`, `glowtail-gpui`).
Three parallel `Explore` agents produced an initial sweep; every finding
below was then verified against the source by reading the cited file at the
cited line. Findings that did **not** survive that pass are recorded under
"False positives" so they're not re-litigated.

Totals: **2 HIGH**, **9 MEDIUM**, **7 LOW**. The HIGHs are both in
`glowtail-core` and both are correctness issues rather than panics in
practice — they're flagged HIGH because they affect the engine that every
front-end consumes.

== Architecture & invariants

The UI-neutral-core invariant is enforced by
`crates/glowtail-core/tests/architecture.rs::core_crate_has_no_ui_dependencies`.
That test reads `glowtail-core/Cargo.toml` and asserts the strings
`ratatui`, `crossterm`, `egui`, `gpui`, `wgpu` don't appear. The test
documents its own limitation in a comment: it catches direct deps only and
would miss a transitive UI dep via an intermediate crate. That self-aware
caveat is fine for now — flagged here only so the next maintainer knows the
test's scope.

The inverse direction — UI crates duplicating engine helpers — is **not**
currently checked, and the two desktop UIs duplicate ~90 LOC of identical
init/session/tailer-startup code. See _Cross-cutting recommendations_ below.

== Findings

=== HIGH

==== H1. `Engine::search_results` rebuilds the match list on every keystroke

`crates/glowtail-core/src/viewport.rs:242` — verified ✓

[source,rust]
----
pub fn search_results(&mut self) -> Vec<RowId> {
    let Some(search) = self.search_text.clone() else { return Vec::new(); };
    let search = search.to_ascii_lowercase();
    self.ensure_cache();
    let raw = self.index.rows();
    self.filtered_positions.as_ref()
        .map(|positions| {
            positions.iter().filter_map(|position| {
                let row = &raw[*position];
                if row.raw.to_ascii_lowercase().contains(&search) {
                    Some(row.row_id)
                } else { None }
            }).collect()
        }).unwrap_or_default()
}
----

`next_search_result` calls `search_results()` on every invocation
(viewport.rs:268), and the TUI binds `n`/`N` to call it per keystroke
(`crates/glowtail-tui/src/app.rs:92-93`). Each call:

* allocates a fresh `String` for the lowercased search needle,
* iterates every filtered position,
* `to_ascii_lowercase`s the row's raw text into a fresh `String` per row,
* substring-scans it.

On a 1M-row file with the filter accepting half the rows, every press of
`n` rebuilds ~500k lowercased copies. UI latency tracks log size linearly
where it shouldn't.

**Fix.** Cache `search_results` alongside `filtered_positions` and
invalidate it when either changes (search text, filter, or row append).
Store the lowercased haystack form on `LogRow` lazily, or use a
non-allocating ASCII case-insensitive comparator (the workspace already
has `contains_ascii_ci` at filter.rs:232).

==== H2. `level_compare` ends in `unreachable!()` guarded by an implicit caller invariant

`crates/glowtail-core/src/filter.rs:637-651` — verified ✓

[source,rust]
----
fn level_compare(operator: Token, value: String) -> Result<FilterExpr, FilterError> {
    let level = parse_level(&value)?;
    match operator {
        Token::Eq      => Ok(FilterExpr::LevelEquals(level)),
        Token::NotEq   => Ok(FilterExpr::Not(Box::new(FilterExpr::LevelEquals(level)))),
        Token::Gte     => Ok(FilterExpr::LevelAtLeast(level)),
        Token::Gt      => next_level(level).map(FilterExpr::LevelAtLeast)
                            .ok_or_else(|| FilterError::InvalidQuery("no log level above fatal".into())),
        Token::Lte     => Ok(next_level(level).map(|l| FilterExpr::Not(Box::new(FilterExpr::LevelAtLeast(l))))
                            .unwrap_or(FilterExpr::All)),
        Token::Lt      => Ok(FilterExpr::Not(Box::new(FilterExpr::LevelAtLeast(level)))),
        _ => unreachable!("not an operator"),
    }
}
----

The caller is expected to pre-filter to operator tokens, but the
`Token` enum has many non-operator variants (`Word`, `LParen`, `Comma`,
…). A future refactor that routes a non-operator into this function is
one easy mistake away from a production panic. The same pattern appears
in `string_compare`, `source_compare`, and `field_value_compare` —
review all of them.

**Fix.** Replace the catch-all with an explicit error:

[source,rust]
----
other => Err(FilterError::InvalidQuery(format!(
    "operator {other:?} not valid for level comparison"))),
----

Or split the operator subset into its own enum so the type system rules
out the unreachable arm at compile time.

=== MEDIUM

==== M1. CLI `tail --follow` source-completion break is unreachable in follow mode

`crates/glowtail-cli/src/main.rs:175-194` — verified ✓ (downgraded from
the original HIGH "hangs forever" claim)

[source,rust]
----
let mut removed_sources = 0usize;
while let Some(event) = rx.recv().await {
    match event {
        LogEvent::RowAppended(row) => { /* … */ }
        LogEvent::SourceRemoved { .. } => {
            removed_sources += 1;
            if removed_sources >= source_count { break; }
        }
        // …
    }
}
----

In follow mode (`source.rs:131`'s `if !follow { break; }` doesn't fire),
the inner tailer loop never returns naturally; `SourceRemoved` is only
emitted on explicit `tailer.stop()` (source.rs:136, after the outer
loop). So the `removed_sources >= source_count` break path is dead in
the case the surrounding control-flow most cares about: live tailing. A
user reading this code reasonably expects sources to "complete" when
they hit unrecoverable errors and the CLI to exit then — neither
happens. The loop exits today only because `rx.recv()` returns `None`
once every tailer's `tx` clone has been dropped (after the function's
`drop(tx)` at line 161 and natural task end on stop). That works, but
the `removed_sources` accounting is misleading.

**Fix.** Either:

* delete `source_count`/`removed_sources` and document that the loop
  ends when the channel closes, or
* on `SourceError`, decide whether to count the source as removed
  (after N retries) and emit a `SourceRemoved` from `source.rs` in that
  branch.

==== M2. `--json` and `--plain` silently prefer JSON

`crates/glowtail-cli/src/args.rs:17-20,40-42` and
`crates/glowtail-cli/src/main.rs:100-108` — verified ✓

[source,rust]
----
fn parser_from_flags(json: bool, plain: bool) -> Arc<dyn LogParser> {
    if json { Arc::new(JsonLineParser) }
    else if plain { Arc::new(PlainTextParser) }
    else { Arc::new(CompositeParser::default()) }
}
----

`glowtail tail --json --plain samples/mixed.log` runs with the JSON
parser, no warning. Per the README/CLAUDE.md, the intent is "pass one
to force a parser; omit both for composite" — they're mutually
exclusive.

**Fix.** Mark the flags as conflicting on the clap derive:

[source,rust]
----
#[arg(long, conflicts_with = "plain")]
json: bool,
#[arg(long)]
plain: bool,
----

(apply to both `View` and `Tail` subcommands).

==== M3. Empty `--filter` string is not normalised in the CLI

`crates/glowtail-cli/src/main.rs:227-251` vs
`crates/glowtail-tui/src/app.rs:131` — verified ✓

The TUI explicitly clears the filter when the input is empty:

[source,rust]
----
if state.input.trim().is_empty() {
    engine.clear_filter();
}
----

The CLI forwards `filter_text.as_deref()` to
`compose_query_filter(saved.as_ref(), level, filter_text.as_deref())`
without that normalisation. Whether `compose_query_filter` returns
`FilterExpr::All` or an error on `Some("")` is an implementation
detail of the engine that the two call sites disagree about.

**Fix.** Normalise once in `apply_filters_and_save`:

[source,rust]
----
let filter_text = filter_text.as_deref().filter(|s| !s.trim().is_empty());
----

and pass that. Or, better, push the rule into
`compose_query_filter` itself so both front-ends inherit it.

==== M4. GUI follow-mode scroll offset places the last row off-screen

`crates/glowtail-gui/src/main.rs:490-496` — verified ✓

[source,rust]
----
let total_matching_rows = self.engine.matching_rows_count();
let mut scroll = egui::ScrollArea::vertical().auto_shrink([false, false]);
if self.follow && total_matching_rows > 0 {
    scroll = scroll.vertical_scroll_offset(total_matching_rows as f32 * ROW_HEIGHT);
}
----

Setting the offset to `total * ROW_HEIGHT` means "place row `total` at
the top of the viewport" — i.e. the last actual row (`total - 1`)
ends up just above the viewport, and the visible area is the empty
space below the data. egui's `ScrollArea` then clamps internally, so
in practice the user sees something — but the math doesn't say "keep
the last page visible".

**Fix.** Use the viewport height to scroll to "show the last page":

[source,rust]
----
let viewport_h = ui.available_height();
let needed = (total_matching_rows as f32 * ROW_HEIGHT - viewport_h).max(0.0);
scroll = scroll.vertical_scroll_offset(needed);
----

==== M5. Both desktop UIs block the UI thread in `Drop`

`crates/glowtail-gui/src/main.rs:809-817` and
`crates/glowtail-gpui/src/main.rs:351-359` — verified ✓

[source,rust]
----
impl Drop for GlowtailGui {  // (and GlowtailGpui — same shape)
    fn drop(&mut self) {
        self.save_session();
        if let Some(mut live_tail) = self.live_tail.take() {
            for tailer in live_tail.tailers.drain(..) {
                self.runtime.block_on(tailer.stop());
            }
        }
    }
}
----

`tailer.stop()` flips an `AtomicBool` and `.await`s the spawned task.
If a tailer is mid-`File::open` or mid-`read_line` on a slow or hung
file (NFS, network mount, paused mock), this `block_on` blocks the UI
thread closing the window. Worse, blocking inside a `Drop` that runs
from a Tokio runtime context is exactly the pattern Tokio docs warn
against — `block_on` from a worker thread panics.

**Fix.** Replace with a non-blocking shutdown:

[source,rust]
----
fn drop(&mut self) {
    self.save_session();
    if let Some(live_tail) = self.live_tail.take() {
        for tailer in live_tail.tailers {
            tailer.signal_stop();  // store the AtomicBool; don't await
        }
        // let the runtime drop drive the tasks to completion
    }
}
----

i.e. split `stop()` into a non-async `signal_stop()` and the `await`able
join. The runtime's `Drop` will then drive task shutdown on its own
threads without blocking the UI.

==== M6. First file-open error in either GUI tears down the whole session

`crates/glowtail-gui/src/main.rs:57-62` and
`crates/glowtail-gpui/src/main.rs:63-67` — verified ✓

[source,rust]
----
for path in &args.paths {
    engine
        .load_file(path, parser.as_ref())
        .with_context(|| format!("failed to read {}", path.display()))?;
}
----

`glowtail-gui samples/exists.log samples/typo.log` exits with the typo
error before the GUI ever launches. The CLI has the same shape but
that's acceptable for a CLI. For a desktop UI, a single bad path
should be a warning visible inside the app, not a no-launch.

**Fix.** Collect per-path errors, load what loads, surface failures in
a status message or modal:

[source,rust]
----
let mut load_errors = Vec::new();
for path in &args.paths {
    if let Err(err) = engine.load_file(path, parser.as_ref()) {
        load_errors.push((path.clone(), err));
    }
}
// pass load_errors into the UI; render in the status bar / a dismissible toast.
----

==== M7. GPUI render constructs two `Rc<RefCell<Engine>>`-borrowing children per frame

`crates/glowtail-gpui/src/main.rs:362-389,~513,~582` — verified ✓
(structural; needs runtime check to confirm panic frequency)

[source,rust]
----
impl Render for GlowtailGpui {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.drain_live_events();
        let metadata = Arc::clone(&self.metadata);
        let engine = Rc::clone(&self.engine);
        div()
            // …
            .child(log_viewport(engine.clone(), self.list_state.clone()))
            .child(detail_panel(engine))
    }
}
----

`log_viewport`'s row-render closures and `detail_panel`'s body each
call `engine.borrow_mut()`. GPUI's element tree is lazy in places —
list items are rendered on demand. If GPUI's layout or paint passes
ever interleave a list-row callback with a `detail_panel` body
evaluation while a previous `borrow_mut` is still live, this panics
with `RefCell already borrowed`.

**Fix.** Take one snapshot per frame and pass concrete data to
children, not the engine handle:

[source,rust]
----
let snapshot = self.engine.borrow_mut().viewport(/* the visible range */);
.child(log_viewport(&snapshot, self.list_state.clone()))
.child(detail_panel(&snapshot, self.detail_row))
----

Or use `borrow()` everywhere read-only and gate the mutable
operations to a single explicit "tick" step at the top of `render`.

==== M8. Session `filter_history` evicts the oldest entry silently past 20

`crates/glowtail-core/src/session.rs:58-67` — verified ✓

[source,rust]
----
pub fn record_filter(&mut self, filter: FilterExpr) {
    if matches!(filter, FilterExpr::All) || self.filter_history.last() == Some(&filter) {
        return;
    }
    self.filter_history.push(filter);
    if self.filter_history.len() > MAX_FILTER_HISTORY {
        self.filter_history.remove(0);
    }
}
----

The session is persisted to disk and presented to users as a history.
Twenty entries is small for a long-running investigation; rotation is
silent. `Vec::remove(0)` is also O(n).

**Fix.** Either raise the cap and switch to `VecDeque::pop_front`
(O(1)), or document the limit in the session file's top-level doc
comment. Suggested cap: 100, matching common shell history defaults.

==== M9. `FileTailer` `ByteRange` covers terminator; `parser.parse_line` text doesn't — document the asymmetry

`crates/glowtail-core/src/source.rs:96-107` — verified ✓ (consistent
with `viewport.rs:706-739`; downgraded from "bug" to "doc gap")

[source,rust]
----
Ok(n) => {
    let end = offset + n as u64;
    let trimmed = line.trim_end_matches(['\n', '\r']);
    let row = parser.parse_line(
        source_id,
        RowId(next_row),
        ByteRange { start: offset, end },   // includes terminators
        trimmed,                             // doesn't
    );
}
----

`viewport.rs::ingest_bytes` has the same asymmetry deliberately —
`ByteRange` is a raw-bytes anchor for seeking back into the source
file, and the text passed to the parser is the human-readable line.
That's defensible but nowhere documented; a reader hitting either
site assumes the two should match.

**Fix.** Document on `model::ByteRange` itself: "byte range into the
source file's raw bytes; includes the line terminator. The text
passed alongside a `ByteRange` to `LogParser::parse_line` has
terminators stripped."

=== LOW

==== L1. `mpsc::Sender::send` errors swallowed throughout `FileTailer`

`crates/glowtail-core/src/source.rs:32,53,73,107,110,122,136` —
verified ✓

Every `sender.send(...)` is `let _ = sender.send(...).await`. If a UI
detaches, all subsequent events vanish silently. Acceptable as
default, but log at `debug!` on the first dropped send per source —
otherwise the only signal that a UI lost its channel is "rows stopped
arriving".

==== L2. `LogEvent` is not `#[non_exhaustive]`

`crates/glowtail-core/src/events.rs:1-21` — verified ✓

`LogEvent` is the engine's outward-facing event stream and is matched
on by every UI. Adding a new variant is a breaking change for
downstream consumers (including the CLI's `_ => {}` in
`run_tail_follow` at main.rs:192 — that one is fine, but the TUI
`drain_events` in app.rs:202-213 has a more selective match).

**Fix.** Add `#[non_exhaustive]` and document the policy in
`events.rs`'s module doc.

==== L3. `RowId` and `SourceId` expose `pub` fields

`crates/glowtail-core/src/model.rs` — verified ✓

[source,rust]
----
pub struct RowId(pub u64);
pub struct SourceId(pub u64);
----

The `Engine` is the canonical mint for both, but downstream code can
construct any value (e.g. `RowId(0)` to "select the first row") and
the engine has no defence against it. Currently used safely in
practice; flag for future hardening if either ID grows non-counter
semantics (e.g. a generational tag).

==== L4. TUI clears `status_message` on every keypress, including unhandled keys

`crates/glowtail-tui/src/app.rs:54-60` — verified ✓

[source,rust]
----
if event::poll(Duration::from_millis(50))?
    && let Event::Key(key) = event::read()?
{
    if key.kind != KeyEventKind::Press { continue; }
    state.status_message = None;
    // …
}
----

A filter-error status set by `apply_input` (app.rs:136) is cleared by
the user pressing any key — including keys the TUI doesn't act on.
Either dismiss only on relevant input, or auto-expire status messages
after N seconds (track an `Instant` alongside the message).

==== L5. TUI silently no-ops bookmark on an empty viewport

`crates/glowtail-tui/src/app.rs:79-87` — verified ✓

[source,rust]
----
KeyCode::Char('b') => {
    if let Some(row) = snapshot.rows.get(
        state.selected_offset.min(snapshot.rows.len().saturating_sub(1)),
    ) {
        engine.toggle_bookmark(row.row_id, None);
    }
}
----

Pressing `b` on an empty viewport (no matches against current
filter) does nothing visibly. Surface a status: `"no row to
bookmark"`.

==== L6. Neither UI crate has tests; CLI args also untested

`crates/glowtail-gui/src/main.rs`, `crates/glowtail-gpui/src/main.rs`,
`crates/glowtail-cli/`, `crates/glowtail-tui/`

`glowtail-core` is well-tested (parser, viewport, filter, session,
architecture invariant). Front-ends have:

* CLI: no tests for args parsing, no tests for `apply_filters_and_save`
  composition (the place where the empty-filter normalisation bug
  lives, M3).
* TUI: no tests for `TuiState` transitions
  (`clamp_view`/`move_selection_down`/`move_selection_up`).
* GUI/GPUI: no tests at all.

The empty-filter divergence (M3) and the follow-scroll math (M4)
would have been caught by tests on `TuiState::default()` and on the
scroll offset calculation.

**Fix.** Extract pure-logic helpers out of the `Render`/event-loop
sites and unit-test them. For the TUI specifically,
`clamp_view`/`move_selection_*` are already pure — they just need
`#[cfg(test)] mod tests` next to them.

==== L7. Background channel capacity hardcoded at 1024 in three places

`crates/glowtail-cli/src/main.rs:69,147`,
`crates/glowtail-gui/src/main.rs:153`,
`crates/glowtail-gpui/src/main.rs:132` — verified ✓

Three independent declarations of the same constant. When tailing a
chatty file, exhausted sender capacity stalls the tailer task with no
diagnostic. Either centralise the constant in `glowtail-core` or
expose a builder option.

== Cross-cutting recommendations

=== UI duplication: lift to a shared crate

`crates/glowtail-gui/src/main.rs:49-210` and
`crates/glowtail-gpui/src/main.rs:57-224` duplicate ~90 LOC:

* `parser_from_flags` (gui:112-120, gpui:152-160) — 9 lines, byte-identical
* `apply_filters` (gui:122-145, gpui:162-185) — 24 lines, byte-identical
* `load_session` (gui:173-182, gpui:187-196) — 10 lines, byte-identical
* `save_session` (gui:184-197, gpui:198-211) — 14 lines, byte-identical
* `start_tailers` (gui:147-171, gpui:126-150) — 25 lines
* `LevelArg` enum and `From<LevelArg> for LogLevel` (gui:39-47+210,
  gpui:47-55+224) — 10 lines

This is the kind of duplication where the next change will silently
diverge the two. Two options:

. Move all of it into `glowtail-core::ui_common` (cheap, keeps the crate
  graph flat). Pros: zero new crate boilerplate. Cons: the engine
  crate grows a "for UIs" surface, which clashes with the strict
  UI-neutral framing.
. Add a `glowtail-ui-common` crate that depends only on `glowtail-core`,
  `tokio`, and `serde`. Both desktop UIs depend on it; the CLI can
  too (replaces its own `parser_from_flags`/`load_session`/`save_session`).
  Pros: respects the existing layering. Cons: one more crate.

I'd lean toward (2) — the CLI shares `parser_from_flags` and the
session helpers verbatim with the GUIs (compare cli/main.rs:100-108,
253-277 with the GUI versions), so the shared surface is bigger than
just "for UIs".

=== Tests to add

In priority order (each catches a finding in this report):

. `clamp_view` on `total = 0` — catches the empty-viewport edge in L5
  and the follow-mode `visible_rows = 0` corner.
. `apply_filters_and_save` with `Some("")` and `Some("   ")` — catches M3.
. GUI follow scroll math, pure function — catches M4.
. `level_compare` called with `Token::Word("x")` — once the
  `unreachable!` is replaced with an error (H2), this becomes a real test.
. `Engine::search_results` benchmark — catches H1 regressions if
  someone re-introduces the per-call rebuild.
. Architecture invariant: the existing test catches direct UI deps in
  `glowtail-core`; consider adding the symmetric check that
  `glowtail-core::ui_common` (if introduced) doesn't depend on
  `glowtail-gui`/`glowtail-gpui`.

== False positives recorded for future reviewers

These were raised by the initial sweep and verified to NOT apply.
Listed here so the next pass doesn't re-raise them.

* **"GPUI loads files in the wrong follow mode."** The two UIs use
  different surface syntax but equivalent semantics. GUI
  (`gui/main.rs:53`): `if !no_follow && from_start { skip } else { load }`.
  GPUI (`gpui/main.rs:62`): `if no_follow || !from_start { load }`. By
  De Morgan, `!(!no_follow && from_start)` ≡ `no_follow || !from_start`.
  Identical behaviour.

* **"`RowId` overflow on `engine.total_rows() as u64`"**
  (`viewport.rs:731`). `total_rows()` returns `usize`. On every Rust
  target, `usize <= u64`, so the cast is widening. No overflow.

* **"`snap_char_boundary` produces invalid UTF-8 slices."**
  (`viewport.rs:695`). The function uses stdlib `str::is_char_boundary`,
  which is authoritative over valid UTF-8. The upstream lowercasing in
  the search path is ASCII-only (`to_ascii_lowercase` on
  ASCII-equivalent byte mapping), so byte offsets in the lowercased
  copy match offsets in the original. No invariant violation.

* **"TUI terminal is left in raw mode when `terminal.draw()?` returns
  an error."** `TerminalState`'s `Drop` impl (terminal.rs:24-29) runs
  during the error unwind from `run_tui_with_events`. The legitimate
  residual concern is SIGTERM/SIGKILL (Drop doesn't run on signal kill)
  and `panic=abort` profiles; the `?` path is fine.

* **"`source_count` hangs the CLI tail forever on permission-denied."**
  Re-verified: the loop exits via channel close, not via the
  `removed_sources` counter, in follow mode. The counter is misleading
  (M1), but there's no hang.

== Verification of this review

Every `file:line` reference in this document was opened at the cited
line and the cited code confirmed before publish. No source files
were modified.

Smoke check (run after publishing this review):

[source,bash]
----
cargo check --workspace
cargo test --workspace
----

Both should pass — the review made no changes to the tree.
