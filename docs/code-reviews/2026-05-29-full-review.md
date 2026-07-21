= Full Code Review: glowtail Workspace
:author: Paul Snow
:date: 2026-05-29
:revision: 0.0.0

== Summary

A full-coverage review of the `glowtail` workspace covering correctness,
test-driven-development opportunities, and feature parity across the
front-ends. The workspace has grown since the prior two reviews
(`2026-05-23-full-review.md`, `2026-05-26-performance-review.md`): it now
has **eight crates** — one engine (`glowtail-core`), a shared wiring crate
(`glowtail-ui-common`, created as the cross-cutting recommendation from the
2026-05-23 review), a CLI, and **five** UI front-ends (`glowtail-tui`,
`glowtail-gui`, `glowtail-gpui`, `glowtail-iced`, `glowtail-makepad`).

Method: four parallel review agents (core correctness, feature parity,
TDD/coverage, UI/CLI correctness), each cross-checked against the prior
reviews. The single HIGH finding was re-verified line-by-line against the
source.

Headline: the codebase is healthy and well-tested. Nearly every open
finding from the prior reviews is resolved. There is exactly one genuine
correctness **bug** (HIGH), one semantics **MEDIUM**, and the remainder are
parity gaps, quality cleanups, and test-coverage additions.

== Fixes applied (2026-05-29)

Landed in the same session as this review, each verified by `cargo test`
and `cargo clippy --workspace --all-targets` (clean):

[cols="1,3", options="header"]
|===
| Finding | Fix

| HN1 (HIGH)
| `folded_stack_rows` now resolves the row's current Vec position via
  `index.position_of(row.row_id)` before walking, so fold counts stay
  correct under eviction. Test `folded_stack_rows_correct_after_eviction`
  (`viewport.rs`) reproduced the bug (returned 0, expected 2) and now passes.

| LN1 (LOW)
| `timestamp_compare`'s surviving `unreachable!("not an operator")` replaced
  with `FilterError::InvalidQuery`, matching the H2 remediation of its
  siblings (`filter.rs`).

| LN2 (LOW)
| `--json` marked `conflicts_with = "plain"` on all four desktop front-ends
  (gui, gpui, iced, makepad), matching the CLI.

| MN2 (MEDIUM)
| gui and gpui no longer rewrite the session file on every drain tick;
  persistence is on session-mutating actions and `Drop` only, matching
  iced/makepad.

| LN3 (LOW)
| iced and makepad `Drop` now `signal_stop()` their tailers (non-blocking),
  matching gui/gpui, so the tasks stop polling once the window closes.

| MN1 (MEDIUM)
| `level <=`/`<` comparisons now exclude unlevelled (`None`-level) rows via a
  `LevelAtLeast(Trace)` guard, so stack continuations and plain lines no
  longer leak into the result. Test `level_at_most_excludes_unlevelled_rows`.

| LN5 (LOW)
| iced `PageDown` now clamps `first_row` to `total - PAGE_SIZE` (a full last
  page) instead of `total - 1`, matching `snap_to_tail`. Extracted into the
  pure `max_first_row` helper.

| A — pure-fn extraction tests
| Scroll/selection arithmetic lifted out of `&mut self` methods into pure
  free functions, each unit-tested: gui `follow_offset` (the prior-M4
  follow-scroll math), gpui `clamped_item_ix`, iced `next_first_row`/
  `max_first_row`, makepad `clamped_position`.

| Tests
| ~22 new behaviour-named tests. `glowtail-ui-common` (5): unknown
  saved-filter errors, missing-file→default, no-path no-op, parent-dir
  creation + round-trip, bookmark disk round-trip (added `tempfile`
  dev-dep). `glowtail-core/source.rs` (2): rotation/truncation reset,
  source-error on unreadable path. `glowtail-core/filter.rs` (1): MN1.
  `glowtail-tui/app.rs` (5): selection movement, invalid-query status,
  empty-viewport bookmark status. `glowtail-cli` (2 integration):
  `tail --no-follow` substring + level filtering. Plus the pure-fn tests in
  gui/gpui/iced/makepad above.
|===

Verified together: `cargo test --workspace` (140 passed, 0 failed) and
`cargo clippy --workspace --all-targets -- -D warnings` (clean).

Still open from the findings below: LN4 (iced/makepad disconnect status),
LN6 (`next_source_id` overflow), and the larger parity *features* that need a
GUI visual check — timeline in iced/makepad, TUI `apply_filters` drift +
interactive level filter, GPUI `z` stack-fold toggle, and bookmark navigation
(needs a new core API plus wiring across all five front-ends).

== Prior-review findings: status

Resolved since the prior reviews (verified at current line numbers):

* **H2** — `unreachable!` panics in `level_compare`/`string_compare`/
  `source_compare`/`field_value_compare` replaced with `FilterError`
  (`filter.rs:655` and siblings; test at `filter.rs:881`). *Caveat:* one
  surviving `unreachable!` in `timestamp_compare` — see L-new below.
* **M1** — CLI `run_tail_follow` rewritten to terminate on channel-close;
  misleading `removed_sources` accounting removed (`cli/main.rs:147-199`).
* **M2** — `--json`/`--plain` marked `conflicts_with` on the CLI with a
  regression test (`cli/args.rs:18,46,77`). *Caveat:* the four desktop
  crates do not carry the guard — see L-new below.
* **M4** — GUI follow-scroll now pins the last row to the viewport bottom
  (`gui/main.rs:435-442`).
* **M5** — gui/gpui use `signal_stop()` in `Drop` (`source.rs:170`); no
  `block_on` anywhere.
* **M6** — all desktop UIs accumulate per-path load errors and still launch
  (gui/gpui/iced/makepad).
* **M7** — gpui pre-snapshots detail-row state before lazy children render
  (`gpui/main.rs:1250-1256`).
* **M8** — `filter_history` capped at 100, O(1) `VecDeque` rotation.
* **H1** — search results cached alongside `filtered_positions`.
* **M9, L1, L2, L4, L5, L7** — all resolved (`ByteRange` doc; tailer
  send-error handling; `#[non_exhaustive] LogEvent`; TUI status TTL; TUI
  empty-viewport bookmark status; `DEFAULT_TAILER_CHANNEL_CAPACITY` used
  everywhere).

Still open (by design / deferred): the performance findings **PH1**
(incremental aggregates), **PH2** (JSON key interning), **PM1/PM2**
(per-call allocations), **PM4** (200 ms poll latency), **PL2/PL4**. Benches
landed first; fixes deferred. **L3** (`pub` fields on `RowId`/`SourceId`)
remains, intentional-looking.

== Findings (new)

=== HIGH

==== HN1. `folded_stack_rows` mixes RowId-space with Vec-position-space

`crates/glowtail-core/src/viewport.rs:845-846` — verified ✓

[source,rust]
----
let mut index = row.row_id.0 as usize + 1;                  // RowId space (absolute)
while let Some(next) = self.index.find_by_row_number(index) // Vec-position space
----

`RowId = evicted + vec_position` (`index.rs:23`) — a globally monotonic
counter that survives eviction. `find_by_row_number(n)` is
`self.rows.get(n)`, i.e. **Vec-position indexed** (`index.rs:62`). The two
spaces coincide only while `evicted == 0`. Once any row is evicted
(whenever `--max-rows` is set and the cap trips, or on a tail rotation),
every `RowId` is `evicted` larger than its Vec position, so the walk reads
the wrong rows: it counts unrelated later rows as folded, or stops early
and reports `0`. No panic (`.get` is bounds-checked) — just silently
incorrect stack-fold counts, and stack folding is a headline feature.

**Fix.** Walk by Vec position using the existing primitive:

[source,rust]
----
let mut pos = match self.index.position_of(row.row_id) {
    Some(pos) => pos + 1,
    None => return 0,
};
while let Some(next) = self.index.find_by_row_number(pos) {
    if !Self::is_stack_trace_continuation(next) { break; }
    count += 1;
    pos += 1;
}
----

**Test.** `folded_stack_rows_correct_after_eviction` — set `max_rows`,
append stack-trace continuation rows past the cap, assert the fold count is
correct (currently fails).

=== MEDIUM

==== MN1. `level <= fatal` / `level < trace` silently match unlevelled rows

`crates/glowtail-core/src/filter.rs:646-648` — verified ✓

`level <= fatal` compiles to `FilterExpr::All` (matches everything,
including `level == None` rows such as stack continuations and plain
lines); `level <= trace` → `Not(LevelAtLeast(Debug))`, also `true` for
`None`-level rows. A user writing a `<=`/`<` level bound gets unlevelled
rows leaking into the result.

**Fix.** Decide the semantics; if `None` should be excluded, `And` the
result with a `HasLevel` predicate (new `FilterExpr` variant), or at
minimum document the inclusion. Add a `level_lte_excludes_unlevelled_rows`
test once the behaviour is chosen.

==== MN2. GUI and GPUI rewrite the session file on every drain tick

`crates/glowtail-gui/src/main.rs:620`, `crates/glowtail-gpui/src/main.rs:578`
— verified ✓

`save_session` (full JSON serialise + file rewrite) runs whenever the drain
loop processes appended rows — up to ~60×/s on a busy tail
(`LIVE_REFRESH_MS = 16`). The session holds only saved filters + bookmarks,
neither of which changes on append, so this is pure wasted I/O and risks
interleaving with the `Drop` save. iced/makepad correctly persist on `Drop`
only.

**Fix.** Persist on session-mutating actions (bookmark / save-filter) and
on `Drop`, not on the row-append path.

=== LOW

* **LN1. Surviving `unreachable!("not an operator")`** in
  `timestamp_compare` (`filter.rs:720`) — same class H2 fixed elsewhere;
  make it return `FilterError::InvalidQuery`.
* **LN2. Desktop `--json`/`--plain` not mutually exclusive** —
  gui/gpui/iced/makepad `Args` lack `conflicts_with = "plain"` (the CLI has
  it). Four one-line clap fixes.
* **LN3. iced/makepad never `signal_stop` tailers on exit** (`iced/main.rs:1075`,
  `makepad/main.rs:1166`). Covered by runtime-drop on normal close, but
  makepad's `runtime`/`live_tail` field order gives no guarantee. Mirror
  gui/gpui for robustness.
* **LN4. iced/makepad don't distinguish channel `Disconnected` from
  `Empty`** (`iced/main.rs:323`) — they keep ticking after the tail ends
  without telling the user; gui/gpui surface "live tail disconnected".
* **LN5. iced `PageDown` clamps `first_row` to `total-1`** (`iced/main.rs:525`)
  while `snap_to_tail` uses `total-PAGE_SIZE` — the two scroll paths
  disagree on "the end".
* **LN6. `next_source_id` is O(n) and can overflow/collide** (`viewport.rs:550`)
  — `max(ids)+1` panics in debug / wraps in release if `SourceId(u64::MAX)`
  was ever added. Track a counter or `saturating_add`. Not reachable in
  normal flows.

== Feature parity across the five front-ends

Legend: ✅ full · ⚠️ partial · ❌ absent. The engine capability is the
baseline; the question is which UIs surface it.

[cols="3,1,1,1,1,1", options="header"]
|===
| Feature | TUI | GUI | GPUI | Iced | Makepad

| Follow/tail + toggle | ✅ | ✅ | ✅ | ✅ | ✅
| Interactive text filter | ✅ | ✅ | ✅ | ✅ | ✅
| Interactive level filter (keys) | ❌ | ⚠️ | ✅ | ✅ | ✅
| Search next/prev (n/N) | ✅ | ✅ | ✅ | ✅ | ✅
| Per-span search highlight | ✅ | ✅ | ✅ | ✅ | ⚠️ row-bg
| Bookmarks toggle + persist | ✅ | ✅ | ✅ | ✅ | ✅
| Bookmark navigation (jump) | ❌ | ❌ | ❌ | ❌ | ❌
| Saved-filter create in-app | ❌ | ✅ | ❌ | ❌ | ❌
| Saved-filter apply/cycle | ❌ | ✅ | ✅ | ✅ | ✅
| Filter history surfaced | ❌ | ❌ | ❌ | ❌ | ❌
| Source sidebar + counts | ⚠️ | ✅ | ✅ | ✅ | ⚠️
| Timeline/histogram | ⚠️ | ✅ | ✅ | ❌ | ❌
| JSON detail panel | ❌ | ✅ | ✅ | ✅ | ✅
| Stack-trace fold (z) | ✅ | ✅ | ❌ | ✅ | ✅
| Keyboard row nav | ✅ | ❌ | ✅ | ✅ | ✅
| Scroll virtualisation | ✅ | ✅ | ✅ | ⚠️ | ✅
| Evicted-rows badge | ❌ | ✅ | ✅ | ❌ | ❌
| Tailer signal_stop on exit | ✅ | ✅ | ✅ | ❌ | ❌
|===

=== Prioritised parity gaps

. **No timeline in Iced & Makepad.** Engine always computes it; GUI/GPUI
  render it. Biggest functional gap in the two newest UIs. Reference: GUI
  `timeline_panel`.
. **TUI has no interactive level filter** and bypasses `apply_filters`
  (see wiring drift) — level filtering only via the query mini-language.
. **Filter history is dead capability everywhere.** Engine records +
  persists `filter_history`; no UI reads it.
. **Bookmark navigation missing in all five.** Toggle exists; no jump.
. **Saved-filter creation only in GUI;** others apply/cycle only.
. **GPUI lacks the `z` stack-fold toggle** (every other UI has it).
. TUI has no JSON detail panel; GUI has no keyboard row nav; Makepad caps
  spans at 16 (row-level highlight); Iced/Makepad/TUI lack the
  evicted-rows badge.

=== Wiring drift (the thing `glowtail-ui-common` exists to prevent)

* **TUI and GUI call `parse_filter_query` + `engine.set_filter` directly**
  for interactive edits (`tui/app.rs:130`, `gui/main.rs:221`) instead of
  `apply_filters`, losing level/saved-filter composition. GPUI/Iced/Makepad
  route everything through `apply_filters`. Root cause of parity gap #2 and
  the highest drift risk.
* **CLI inlines the `FileTailer::start` loop** rather than calling
  `start_tailers` (`cli/main.rs:73-81,151-160`) — benign duplication that
  must stay in sync on channel capacity + flags.
* `normalise_max_rows` is duplicated verbatim in four crates (cli, gui,
  iced, makepad) — natural `glowtail-ui-common` home; only three test it.

== Test-driven-development opportunities

Inventory: ~104 active tests + ~22 ignored benches. `glowtail-core` is
well-covered (viewport 22, filter 13, session 7). Thinnest:
`glowtail-cli/src/main.rs` (0), `glowtail-tui/src/app.rs` state
transitions, `glowtail-core/src/source.rs` (3 — no rotation / truncation /
error tests), and pure UI helpers locked behind `&mut self`.

Prioritised test recommendations (behaviour-named, matching repo
convention):

Catch a known/suspected bug:

. `folded_stack_rows_correct_after_eviction` — `viewport.rs`. Catches HN1.
. `tailer_resets_and_emits_rotated_on_truncation` — `source.rs:92`. The
  rotation/truncation path is entirely untested.
. `follow_offset_pins_last_row_to_viewport_bottom` — extract the inline
  follow-offset (`gui/main.rs:441`) into a pure fn and test it (the prior
  M4 bug class is fixed but still untested).
. `apply_input_with_invalid_query_sets_filter_error_status` — `tui/app.rs:130`.

Lock in untested behaviour with large blast radius:

. `save_session_creates_missing_parent_directories` — `ui-common/lib.rs:106`.
. `load_session_of_missing_file_returns_default` — `ui-common/lib.rs:93`.
. `session_round_trips_through_disk` — save→load with bookmarks + saved
  filters (only in-memory round-trip is tested today).
. `apply_filters_with_unknown_use_filter_name_errors` — `ui-common/lib.rs:74`.
. `tail_no_follow_prints_only_matching_rows` — new
  `cli/tests/cli_command.rs`; the CLI's whole purpose is exercised only by
  an ignored bench.
. `tailer_reports_source_error_on_unreadable_path` — `source.rs:75`.
. `move_selection_down_advances_offset_then_scrolls_window` /
  `move_selection_up_walks_first_row_back` — `tui/app.rs:178,187`.

Refactor-to-test (extract pure fns): gpui `clamped_item_ix`
(`main.rs:622`), iced `next_first_row`/`snap_to_tail` (`main.rs:347,354`),
makepad `move_selection` clamp (`main.rs:535`), gui `follow_offset`
(`main.rs:441`).

Also: `source.rs:211`'s `assert!(rows <= 2)` is a weak assertion that
passes even at 0 rows — should be `assert_eq!`.

== Verification

The HIGH finding (HN1) was verified by reading `viewport.rs:839-862`,
`index.rs:18-88` (RowId minting, `position_of`, `find_by_row_number`,
`evict_oldest`), and confirming the RowId-vs-Vec-position divergence under
eviction. Other findings cite exact `file:line` from the agent sweep and
should be re-confirmed at fix time. No source files were modified by this
review.

Smoke check after acting on this review:

[source,bash]
----
cargo test --workspace
cargo clippy --all-targets --all-features -- -D warnings
----
