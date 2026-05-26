= Performance Review: glowtail Workspace
:author: Paul Snow
:date: 2026-05-26
:revision: 0.0.0

== Summary

A performance-focused pass over the `glowtail` workspace at HEAD
`1450f98`. The prior full review (`2026-05-23-full-review.md`) was
correctness/architecture focused; this one inspects only hot-path cost
and the gaps in the benchmark suite. The recent
"Raise live-tail throughput by tuning channel and UI poll cadence"
commit lifted the pipeline ceiling from ~10k to ~400k rows/s by
widening the bounded channel (1024 → 16384) and tightening the UI
drain cadence (100ms → 16ms), and added the first dedicated perf
harness at `crates/glowtail-core/tests/tail_perf.rs`. The engine itself
sustains ~3M rows/s on append, so the engine is not currently the
pipeline bottleneck — but the *per-viewport* call cost still scales
linearly with the filtered row count for aggregates, and the JSON
parser allocates an `Arc<str>` per field per row. Both are addressable.

Totals: **2 HIGH**, **5 MEDIUM**, **5 LOW**, and **7 new benchmarks**
to land before the recommended fixes so each subsequent change can be
graded against a stable baseline.

The HIGHs are both on the engine read/parse path. The MEDIUMs are
split across engine and UI front-ends. None of the findings are
correctness regressions — they're cost we'd rather not pay.

== Method

Three parallel `Explore` agents mapped, respectively, the engine
modules (`parser`, `index`, `filter`, `viewport`, `session`, `source`,
`events`, `model`), the live-tail / IO / events pipeline, and the four
UI front-ends. Each finding was then re-read against the cited source
line. Recommendations marked "out of scope for this PR" are
deliberately not implemented here so the benches land first.

== Architecture & invariants (perf-relevant)

The invariants from the 2026-05-23 review still hold. Specifically:

* `Engine::viewport` takes no lock on the read path — the engine is
  driven from a single UI thread that also serialises appends via the
  drain loop.
* The filtered-position cache
  (`crates/glowtail-core/src/viewport.rs:30-36`) is incremental on
  append for monotonic timestamps (O(1) push) and falls back to
  `binary_search_insert` for out-of-order arrivals (O(log n)).
* The search cache landed for H1 in the prior review and is invalidated
  in lock-step with `filtered_positions`. The regression test at
  `viewport.rs:1091` (`search_cache_invalidates_on_row_append_and_filter_change`)
  guards it.
* Field values matched by `contains_ascii_ci`
  (`crates/glowtail-core/src/filter.rs:233-251`) do **not** allocate
  per row — the needle is lowercased once at compile time and the
  haystack is byte-folded in place.

Nothing in this review threatens those invariants. The fixes proposed
below are amplifications, not changes of direction.

== Findings

=== HIGH

==== PH1. Aggregates rebuilt from scratch on every `viewport()` call

`crates/glowtail-core/src/viewport.rs:307-308` — verified ✓

[source,rust]
----
let (level_counts, source_summaries, timeline, timeline_analytics) =
    self.aggregate_for_positions(positions);
----

`aggregate_for_positions` walks the full set of filtered positions and
re-derives `LevelCounts`, `source_summaries` (per-source counts), and
the timeline histogram on every call (`viewport.rs:554-707`). The
filtered-position cache is incremental, but the aggregates derived
from it are not. The GUI repaints at 60Hz on live tails; with a 1M-row
filtered set that is 60M `LogRow` field reads per second wasted on
work whose inputs haven't changed since the previous frame.

`metadata_snapshot()` exists as a "rows-free" sibling
(`viewport.rs:343`) but pays exactly the same aggregate cost — the
saving is only in the per-row `present_row()` calls, not in the
aggregates.

**Fix.** Maintain incremental aggregates alongside `filtered_positions`:

* On append, when a row passes the filter, increment the matching
  level / source counter and bump the row's timeline bucket. O(1) per
  append.
* On `set_filter` / `clear_filter` / `set_stack_trace_folding`,
  invalidate the aggregates the same way `filtered_positions` is
  invalidated, and rebuild lazily on the next `viewport()` call.
* On eviction, decrement the relevant counters before dropping the
  row. The existing eviction path
  (`crates/glowtail-core/src/index.rs:73-81`) already iterates the
  evicted rows, so the bookkeeping is local to it.

Bench B2 (`viewport_with_aggregates_latency`) lands a baseline first
so the fix has something to land against.

==== PH2. `JsonLineParser` allocates an `Arc<str>` per field per row

`crates/glowtail-core/src/parser.rs:98-128` — verified ✓

[source,rust]
----
fn parse_fields(value: &Value) -> ParsedFields {
    let mut fields = ParsedFields::default();
    if let Some(map) = value.as_object() {
        for (k, v) in map {
            // …
            fields.insert(
                Arc::<str>::from(k.clone()),
                Arc::<str>::from(Self::field_value(v)),
            );
        }
    }
    fields
}
----

Every JSON line allocates a fresh `Arc<str>` for **each** field key,
even though production JSON logs reuse a small fixed set of keys
(`service`, `request_id`, `trace_id`, etc.) — often ~10 keys repeated
across millions of rows. The key strings themselves are dominated by
short identifiers that fit in the inline-Arc small-string optimisation
but still pay the `Arc` allocator/refcount cost.

This is also where the architectural cost lives: `ParsedFields` is a
`BTreeMap<Arc<str>, Arc<str>>` and never shrinks, so the allocator
overhead is paid once per row forever.

**Fix.** A per-parser interning map keyed by the JSON key string:

[source,rust]
----
pub struct JsonLineParser {
    keys: DashMap<String, Arc<str>>,  // or RwLock<HashMap<…>>
}

fn intern(&self, key: &str) -> Arc<str> {
    if let Some(arc) = self.keys.get(key) { return arc.clone(); }
    let arc: Arc<str> = Arc::from(key);
    self.keys.insert(key.to_string(), arc.clone());
    arc
}
----

The map is bounded by the source's actual schema (tens of keys, not
millions), and post-warmup every JSON line pays a single hashmap read
instead of an allocator round-trip. `JsonLineParser` is currently
`Copy`; the fix requires upgrading it to a non-`Copy` struct, which
ripples to `CompositeParser` and a handful of `Arc<dyn LogParser>`
sites in `glowtail-ui-common` and the CLI. Manageable.

Bench B1 (`parser_perf`) measures the current rows/sec for both
parsers so the fix's impact is visible.

=== MEDIUM

==== PM1. `RowIndex::iter_range` allocates a fresh `Vec<&LogRow>` per call

`crates/glowtail-core/src/index.rs:42-44` — verified ✓

[source,rust]
----
pub fn iter_range(&self, start: usize, end: usize) -> Vec<&LogRow> {
    self.rows[start..end].iter().collect()
}
----

The caller chains another iterator over the result and never indexes
into it. Returning `impl Iterator<Item = &LogRow> + '_` removes the
`Vec` allocation entirely. Cheap, mechanical change; the regression
risk is only in a public-API rename — manageable since `iter_range`
has a single caller in core.

==== PM2. `present_row()` allocates a `Vec<StyledSpan>` per row per viewport call

`crates/glowtail-core/src/viewport.rs:710+` — verified ✓

`present_row` builds a new `Vec<StyledSpan>` (and may push 3–5 spans
into it) on every render of every visible row. With a 60Hz GUI and a
40-row viewport that's ~2,400 small `Vec` allocations per second
steady-state, plus the per-`StyledSpan` `Arc<str>` clones from the row
text.

**Fix.** Two options, in order of effort:

. Pool the `Vec<StyledSpan>` on the `Engine` so `present_row` can
  borrow a reused buffer and the snapshot moves it out at the end.
  Roughly halves render-path allocations.
. Make the engine yield spans through a per-snapshot arena
  (`bumpalo` or a `Vec<StyledSpan>` indexed by row-relative offset
  ranges). More invasive but eliminates the small-Vec churn entirely.

Bench B3 (`viewport_steady_state_latency`) is the regression guard.

==== PM3. TUI has no row virtualisation; spans are stringified every frame

`crates/glowtail-tui/src/widgets.rs:54-105` — verified ✓

The TUI rebuilds `Vec<Span>` + `Vec<Line>` per visible row per frame
and converts every `StyledSpan.text` to an owned `String` via
`.to_string()` (~line 95). The egui GUI and GPUI front-ends virtualise
their scroll regions; the TUI does not.

Two cheap wins:

. Use `ratatui::text::Span::raw(text)` where `text: &str` borrowed
  from the engine-owned `Arc<str>`. The TUI never outlives the
  snapshot it's rendering, so the borrow is sound.
. The source-summary string (`widgets.rs:36-38`) is rebuilt every
  frame even when source counts haven't changed. Cache the
  pre-formatted line on the `App` and invalidate it when
  `source_summaries` changes. See PL3.

==== PM4. Source polling at 200ms caps live-tail first-byte latency

`crates/glowtail-core/src/source.rs:156` — verified ✓

[source,rust]
----
tokio::time::sleep(Duration::from_millis(200)).await;
----

The recent channel/poll tuning lifts throughput. Latency for the first
byte of a new line is still bounded by the file-poll loop — 200ms
worst case, ~100ms average. Acceptable for current use cases (this is
a viewer, not an alerting tool) but worth measuring before the user
notices.

`notify` (inotify/kqueue/ReadDirectoryChangesW) would cut it to
<10ms but adds platform-dependent rotation/truncation handling and a
non-trivial dep. **Document, don't fix here.** Bench B7
(`source_first_byte_latency`) makes the current trade-off measurable.

==== PM5. GPUI rebuilds `ViewportSnapshot` on every live event

`crates/glowtail-gpui/src/main.rs:587-590` — observed by exploration

The GPUI binding takes a metadata `ViewportSnapshot` whenever the
drain loop processes any event, even when only one row was appended
and nothing the metadata represents has changed visually. Because the
snapshot is `Arc`-wrapped the *clone* cost is tiny; the *build* cost
(`aggregate_for_positions`) is PH1 again.

**Fix is downstream of PH1.** Once aggregates are incremental, the
GPUI binding can skip the `metadata_snapshot()` call entirely when
neither filter nor row count has changed since the previous frame.
No standalone work.

=== LOW

==== PL1. CLI follow loop clones `row.raw` only to read it

`crates/glowtail-cli/src/main.rs:182` — verified ✓ (per the prior
exploration; the surrounding `run_tail_follow` body is the relevant
function).

`println!("{raw}")` (or whatever the formatter prints) doesn't need
ownership. Pass `&row.raw` directly. Single line change.

==== PL2. `PlainTextParser` creates two `Arc<str>` clones of the same string

`crates/glowtail-core/src/parser.rs:53,60-61` — verified ✓

[source,rust]
----
let raw: Arc<str> = Arc::from(line);
LogRow {
    // …
    raw: raw.clone(),
    message: raw,
    // …
}
----

That's fine as written (it's an Arc clone, not a string copy). The
real cost is upstream: when no level prefix is stripped, `raw` and
`message` are semantically identical, but `LogRow` stores them as
two `Arc<str>` fields anyway, doubling the per-row `Arc` overhead
(16-32B on most targets) for every plain-text log row.

**Fix.** Store `message` as `Option<Arc<str>>` and have UIs fall back
to `raw` when `None`. Or store a single `Arc<str>` plus a byte range
into it for `message`. Either avoids the second `Arc` entirely on the
common path. Defer until benches confirm it matters at scale (B5
will).

==== PL3. TUI rebuilds the source-summary string every frame

`crates/glowtail-tui/src/widgets.rs:36-38` — verified ✓ via Explore.

`format!()` + `join` over the source summaries on every paint, even
when nothing about the summary has changed. Cache the rendered string
on the `App` struct, invalidate it on snapshot change. Mechanical.

==== PL4. `BufReader` uses Tokio's default (~8KB) for the source reader

`crates/glowtail-core/src/source.rs:111` — verified ✓

For JSON logs with long structured lines (>1KB is common) the default
buffer fills mid-line, forcing a second syscall per logical row.
`BufReader::with_capacity(64 * 1024, file)` would halve syscalls on
busy sources. Bench B6 will tell us if the syscall count is actually
hot.

==== PL5. No allocation / render telemetry

The engine emits no per-call timing. Adding optional `tracing::span!`
spans around `viewport()`, `present_row()`, and
`aggregate_for_positions()` (gated behind a `trace` feature) would
let the bench harness attribute cost honestly instead of inferring it
from wall time. Trivial; ship when PH1 fix lands so the before/after
numbers are unambiguous.

== Benchmark recommendations

Seven additions to the existing `#[ignore]`d perf-test pattern (no new
deps, no `criterion`). Invocation matches the current convention:

[source,bash]
----
cargo test -p glowtail-core --test parser_perf  -- --ignored --nocapture
cargo test -p glowtail-core --test tail_perf    -- --ignored --nocapture
cargo test -p glowtail-core --test search_perf  -- --ignored --nocapture
cargo test -p glowtail-cli  --test cli_tail_perf -- --ignored --nocapture
----

[cols="1,2,3", options="header"]
|===
| ID | Bench | Purpose

| B1
| `parser_perf::parser_throughput_plain` + `parser_throughput_jsonl`
| Rows/sec for `PlainTextParser` vs `JsonLineParser` over 100k synthetic
  lines. Quantifies PH2 today and grades the intern-table fix.

| B2
| `tail_perf::viewport_with_aggregates_cost`
| 100k pre-filtered rows. Compares `viewport()` (rows + aggregates) to
  the same window served by N × `present_row_at()` calls (rows only).
  The difference is PH1's cost.

| B3
| `tail_perf::viewport_steady_state_latency`
| 1M pre-filtered rows. p50/p99/max for `viewport()` across varying
  `row_count` values (10, 80, 200). Locks in PM2's allocation cost.

| B4
| `search_perf::search_results_cold_then_hot`
| 200k rows with mixed message text. Cold `search_results()` call
  (cache miss) vs subsequent calls (cache hit). Regression guard for
  H1's fix from the prior review.

| B5
| `tail_perf::memory_footprint_per_million_rows`
| Linux-only via `/proc/self/status`. Reports RSS delta after loading
  1M synthetic rows. Tracks PH2 / PL2 regressions.

| B6
| `cli_tail_perf::cli_tail_no_follow_throughput`
| Spawns the `glowtail-cli` binary against a 1M-line temp file with
  `--no-follow --from-start`, times wall-clock to drain. End-to-end
  smoke that exercises BufReader, FileTailer, and Engine together.

| B7
| `tail_perf::source_first_byte_latency`
| Empty file, live `FileTailer`. Appends one line, measures wall time
  until it appears in the engine. Captures PM4's polling cost.
|===

The existing `large_viewport.rs` opt-in smoke stays as-is — B3 and B5
cover the same ground at higher row counts.

== False positives

These looked like perf wins in the initial sweep but did not survive
verification:

* "Filter substring match allocates per row." It doesn't —
  `contains_ascii_ci` (`crates/glowtail-core/src/filter.rs:233-251`)
  byte-folds in place. The needle is lowercased once at filter compile
  time.
* "Bookmark lookups are O(n) per check." They're not —
  `bookmarks` is kept sorted by `row_id` and queried via binary search
  (`crates/glowtail-core/src/session.rs:108-117`). PM8 from the prior
  review covered the related history-rotation O(n) case and is already
  resolved (VecDeque rotation at session.rs:70-75).
* "Search rebuilds the match list per keystroke." Was H1 in the prior
  review; resolved by the search cache landed at
  `viewport.rs:443+`. The B4 bench is a regression guard, not a fix.
* "Polling is the throughput ceiling." Was the case before commit
  `1450f98`; the channel+cadence tuning raised the ceiling above the
  engine's own append capacity. Polling now only bounds *latency*
  (PM4), not *throughput*.

== Status

| Finding | Status
| PH1     | Open — bench B2 first, then incremental aggregates
| PH2     | Open — bench B1 first, then key intern table
| PM1     | Open — mechanical iterator-return
| PM2     | Open — bench B3 first, then pooled buffer
| PM3     | Open — `Span::raw` over borrowed engine `Arc<str>`
| PM4     | Documented trade-off — bench B7 makes it measurable
| PM5     | Falls out of PH1
| PL1–PL5 | Opportunistic cleanup
