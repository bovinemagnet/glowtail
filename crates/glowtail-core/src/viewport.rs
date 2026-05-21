use crate::filter::{CompiledFilter, FilterExpr};
use crate::index::RowIndex;
use crate::model::{
    ByteRange, LevelCounts, LogLevel, LogRow, LogSourceKind, RowId, RowPresentation, SourceId,
    SourceInfo, SourceSummary, SpanKind, StyledSpan, TimelineAnalytics, TimelineBucket,
    ViewportRequest, ViewportSnapshot,
};
use crate::parser::LogParser;
use crate::session::InvestigationSession;
use chrono::TimeDelta;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

/// In-memory engine that owns the row index, the active filter/search/state,
/// and the persisted [`InvestigationSession`]. UI front-ends interact with it
/// via [`Engine::viewport`] — they never own the rows themselves.
#[derive(Debug, Default)]
pub struct Engine {
    index: RowIndex,
    filter_expr: FilterExpr,
    compiled_filter: CompiledFilter,
    search_text: Option<String>,
    sources: BTreeMap<SourceId, SourceInfo>,
    session: InvestigationSession,
    collapse_stack_traces: bool,
    /// Cached positions (into `self.index.rows()`) of rows that match the
    /// current filter, sorted by `(timestamp, row_id)`. Built lazily and
    /// invalidated by any mutation that could change which rows pass.
    filtered_positions: Option<Vec<usize>>,
}

impl Engine {
    pub fn with_session(session: InvestigationSession) -> Self {
        Self {
            session,
            ..Self::default()
        }
    }

    pub fn add_source(&mut self, source_id: SourceId, name: impl Into<Arc<str>>) {
        self.sources.insert(
            source_id,
            SourceInfo {
                source_id,
                name: name.into(),
                kind: LogSourceKind::File,
            },
        );
    }

    pub fn append_row(&mut self, row: LogRow) {
        self.sources
            .entry(row.source_id)
            .or_insert_with(|| SourceInfo {
                source_id: row.source_id,
                name: Arc::from(format!("source-{}", row.source_id.0)),
                kind: LogSourceKind::Other(Arc::from("unknown")),
            });
        self.index.append(row);
        // A single append may shift sort order arbitrarily once timestamps are
        // involved, so the cache is dropped wholesale rather than patched.
        self.invalidate_cache();
    }

    pub fn set_filter(&mut self, filter: FilterExpr) -> Result<(), crate::filter::FilterError> {
        self.compiled_filter = CompiledFilter::compile(&filter)?;
        self.session.record_filter(filter.clone());
        self.filter_expr = filter;
        self.invalidate_cache();
        Ok(())
    }

    pub fn clear_filter(&mut self) {
        self.filter_expr = FilterExpr::All;
        self.compiled_filter = CompiledFilter::All;
        self.invalidate_cache();
    }

    pub fn set_search_text(&mut self, search_text: Option<String>) {
        self.search_text = search_text.filter(|text| !text.is_empty());
    }

    pub fn set_stack_trace_folding(&mut self, enabled: bool) {
        if self.collapse_stack_traces != enabled {
            self.collapse_stack_traces = enabled;
            self.invalidate_cache();
        }
    }

    pub fn toggle_bookmark(&mut self, row_id: RowId, label: Option<Arc<str>>) -> bool {
        self.session.toggle_bookmark(row_id, label)
    }

    pub fn save_filter(&mut self, name: impl Into<Arc<str>>) {
        self.session.save_filter(name, self.filter_expr.clone());
    }

    pub fn apply_saved_filter(&mut self, name: &str) -> Result<bool, crate::filter::FilterError> {
        let Some(filter) = self.session.saved_filter(name).cloned() else {
            return Ok(false);
        };
        self.set_filter(filter)?;
        Ok(true)
    }

    pub fn session(&self) -> &InvestigationSession {
        &self.session
    }

    pub fn into_session(self) -> InvestigationSession {
        self.session
    }

    pub fn total_rows(&self) -> usize {
        self.index.len()
    }

    /// Read-only access to all ingested rows in append order. Mainly used by
    /// `tail --no-follow` to print raw lines without spinning up a viewport.
    pub fn rows_snapshot(&self) -> &[LogRow] {
        self.index.rows()
    }

    pub fn matching_rows_count(&mut self) -> usize {
        self.ensure_cache();
        self.filtered_positions
            .as_ref()
            .map(Vec::len)
            .unwrap_or_default()
    }

    pub fn viewport(&mut self, request: ViewportRequest) -> ViewportSnapshot {
        self.ensure_cache();
        let positions = self
            .filtered_positions
            .as_ref()
            .expect("cache populated by ensure_cache");
        let total = positions.len();
        let start = request.first_row.min(total);
        let end = (start + request.row_count).min(total);
        let raw_rows = self.index.rows();
        let rows = positions[start..end]
            .iter()
            .map(|position| self.present_row(&raw_rows[*position]))
            .collect();

        let (level_counts, source_summaries, timeline, timeline_analytics) =
            self.aggregate_for_positions(positions);

        ViewportSnapshot {
            rows,
            total_rows: self.total_rows(),
            total_matching_rows: total,
            has_more_before: start > 0,
            has_more_after: end < total,
            level_counts,
            source_summaries,
            timeline,
            timeline_analytics,
        }
    }

    /// Lazy single-row accessor. Returns the `RowPresentation` for the row at
    /// `position` in the current filtered+sorted view, or `None` if the
    /// position is out of range. Used by UIs (e.g. GPUI's `list`) that render
    /// rows on demand by index instead of materialising a whole-snapshot
    /// `Vec<RowPresentation>`.
    pub fn present_row_at(&mut self, position: usize) -> Option<RowPresentation> {
        self.ensure_cache();
        let positions = self
            .filtered_positions
            .as_ref()
            .expect("cache populated by ensure_cache");
        let index = *positions.get(position)?;
        let raw = self.index.rows();
        Some(self.present_row(&raw[index]))
    }

    /// Cheap snapshot that returns only the engine-wide statistics
    /// (`level_counts`, `source_summaries`, `timeline`) without populating
    /// rows. Use this for sidebar/timeline UI elements that need the
    /// aggregates but never read `rows`.
    pub fn metadata_snapshot(&mut self) -> ViewportSnapshot {
        self.ensure_cache();
        let positions = self
            .filtered_positions
            .as_ref()
            .expect("cache populated by ensure_cache");
        let total = positions.len();
        let (level_counts, source_summaries, timeline, timeline_analytics) =
            self.aggregate_for_positions(positions);

        ViewportSnapshot {
            rows: Vec::new(),
            total_rows: self.total_rows(),
            total_matching_rows: total,
            has_more_before: false,
            has_more_after: total > 0,
            level_counts,
            source_summaries,
            timeline,
            timeline_analytics,
        }
    }

    fn ensure_cache(&mut self) {
        if self.filtered_positions.is_some() {
            return;
        }
        let raw = self.index.rows();
        let mut positions: Vec<usize> = raw
            .iter()
            .enumerate()
            .filter(|(_, row)| self.compiled_filter.matches(row))
            .filter(|(_, row)| {
                !self.collapse_stack_traces || !Self::is_stack_trace_continuation(row)
            })
            .map(|(position, _)| position)
            .collect();
        positions.sort_by(|left, right| {
            raw[*left]
                .timestamp
                .cmp(&raw[*right].timestamp)
                .then(raw[*left].row_id.cmp(&raw[*right].row_id))
        });
        self.filtered_positions = Some(positions);
    }

    fn invalidate_cache(&mut self) {
        self.filtered_positions = None;
    }

    pub fn filtered_position_for_row(&mut self, row_id: RowId) -> Option<usize> {
        self.ensure_cache();
        let raw = self.index.rows();
        self.filtered_positions
            .as_ref()
            .and_then(|positions| positions.iter().position(|p| raw[*p].row_id == row_id))
    }

    pub fn search_results(&mut self) -> Vec<RowId> {
        let Some(search) = self.search_text.clone() else {
            return Vec::new();
        };
        let search = search.to_ascii_lowercase();
        self.ensure_cache();
        let raw = self.index.rows();
        self.filtered_positions
            .as_ref()
            .map(|positions| {
                positions
                    .iter()
                    .filter_map(|position| {
                        let row = &raw[*position];
                        if row.raw.to_ascii_lowercase().contains(&search) {
                            Some(row.row_id)
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn next_search_result(&mut self, current: Option<RowId>, reverse: bool) -> Option<RowId> {
        let results = self.search_results();
        if results.is_empty() {
            return None;
        }

        let Some(current) = current else {
            return if reverse {
                results.last().copied()
            } else {
                results.first().copied()
            };
        };

        let position = results
            .iter()
            .position(|row_id| *row_id == current)
            .unwrap_or(0);
        if reverse {
            Some(results[(position + results.len() - 1) % results.len()])
        } else {
            Some(results[(position + 1) % results.len()])
        }
    }

    /// Snapshot containing `before` rows before and `after` rows after the
    /// given `row_id`, ignoring the active filter. Statistics in the returned
    /// snapshot describe the context window only, not the entire engine.
    pub fn context_around(&self, row_id: RowId, before: usize, after: usize) -> ViewportSnapshot {
        let Some(position) = self.index.position_of(row_id) else {
            return ViewportSnapshot {
                rows: Vec::new(),
                total_rows: self.total_rows(),
                total_matching_rows: 0,
                has_more_before: false,
                has_more_after: false,
                level_counts: LevelCounts::default(),
                source_summaries: Vec::new(),
                timeline: Vec::new(),
                timeline_analytics: TimelineAnalytics::default(),
            };
        };
        let first_row = position.saturating_sub(before);
        let row_count = before + after + 1;
        let window: Vec<&LogRow> = self.index.iter_range(first_row, row_count);
        let rows = window.iter().map(|row| self.present_row(row)).collect();
        let (timeline, timeline_analytics) = Self::timeline(&window, 24);

        ViewportSnapshot {
            rows,
            total_rows: self.total_rows(),
            total_matching_rows: window.len(),
            has_more_before: first_row > 0,
            has_more_after: first_row + window.len() < self.total_rows(),
            level_counts: Self::level_counts(&window),
            source_summaries: self.source_summaries(&window),
            timeline,
            timeline_analytics,
        }
    }

    /// Append every line of `path` as a row using `parser`. Honours CRLF and
    /// bare-LF terminators (the old hand-rolled CLI loader assumed exactly one
    /// terminator byte, producing off-by-one byte ranges on CRLF inputs).
    pub fn load_file(
        &mut self,
        path: impl AsRef<Path>,
        parser: &dyn LogParser,
    ) -> std::io::Result<SourceId> {
        let path = path.as_ref();
        let contents = std::fs::read(path)?;
        let source_id = self.next_source_id();
        self.add_source(source_id, path.display().to_string());
        self.ingest_bytes(source_id, parser, &contents);
        Ok(source_id)
    }

    /// Append every line of every path in order. Each path becomes a new
    /// `SourceId` starting at 1.
    pub fn load_paths<P: AsRef<Path>>(
        &mut self,
        paths: &[P],
        parser: &dyn LogParser,
    ) -> std::io::Result<()> {
        for path in paths {
            self.load_file(path, parser)?;
        }
        Ok(())
    }

    /// Ingest a raw byte slice into the engine as a series of
    /// `parser.parse_line` calls, splitting on `\n` and honouring an optional
    /// preceding `\r` so byte ranges remain accurate on both LF and CRLF
    /// inputs. Async callers (the CLI) read with `tokio::fs` and then call
    /// this directly to avoid sync IO on the runtime.
    pub fn ingest_bytes(&mut self, source_id: SourceId, parser: &dyn LogParser, bytes: &[u8]) {
        ingest_bytes(self, source_id, parser, bytes);
    }

    /// Next monotonic `SourceId` for callers that need to allocate one
    /// without depending on the internal counter.
    pub fn next_source_id(&self) -> SourceId {
        SourceId(self.sources.keys().map(|id| id.0).max().unwrap_or(0) + 1)
    }

    fn aggregate_for_positions(
        &self,
        positions: &[usize],
    ) -> (
        LevelCounts,
        Vec<SourceSummary>,
        Vec<TimelineBucket>,
        TimelineAnalytics,
    ) {
        let raw = self.index.rows();
        let rows: Vec<&LogRow> = positions.iter().map(|p| &raw[*p]).collect();
        let (timeline, timeline_analytics) = Self::timeline(&rows, 24);
        (
            Self::level_counts(&rows),
            self.source_summaries(&rows),
            timeline,
            timeline_analytics,
        )
    }

    fn level_counts(rows: &[&LogRow]) -> LevelCounts {
        let mut counts = LevelCounts::default();
        for row in rows {
            counts.record(row.level);
        }
        counts
    }

    fn source_summaries(&self, rows: &[&LogRow]) -> Vec<SourceSummary> {
        let mut summaries = BTreeMap::<SourceId, SourceSummary>::new();
        for row in rows {
            let summary = summaries.entry(row.source_id).or_insert_with(|| {
                let name = self
                    .sources
                    .get(&row.source_id)
                    .map(|source| source.name.clone())
                    .unwrap_or_else(|| Arc::from(format!("source-{}", row.source_id.0)));
                SourceSummary {
                    source_id: row.source_id,
                    name,
                    rows: 0,
                    level_counts: LevelCounts::default(),
                }
            });
            summary.rows += 1;
            summary.level_counts.record(row.level);
        }
        summaries.into_values().collect()
    }

    fn timeline(rows: &[&LogRow], max_buckets: usize) -> (Vec<TimelineBucket>, TimelineAnalytics) {
        let mut first = None;
        let mut last = None;
        let mut timestamped_rows = 0usize;
        for row in rows {
            if let Some(timestamp) = row.timestamp {
                timestamped_rows += 1;
                first = Some(first.map_or(timestamp, |f: chrono::DateTime<chrono::Utc>| {
                    f.min(timestamp)
                }));
                last = Some(last.map_or(timestamp, |l: chrono::DateTime<chrono::Utc>| {
                    l.max(timestamp)
                }));
            }
        }
        let (Some(first), Some(last)) = (first, last) else {
            return (
                Vec::new(),
                TimelineAnalytics {
                    untimestamped_rows: rows.len(),
                    ..TimelineAnalytics::default()
                },
            );
        };

        let bucket_count = max_buckets.min(timestamped_rows).max(1);
        let total_ms = (last - first).num_milliseconds().max(1);
        let bucket_ms = (total_ms / bucket_count as i64).max(1);
        struct BucketBuilder {
            bucket: TimelineBucket,
            sources: BTreeMap<SourceId, usize>,
        }

        let mut buckets = (0..bucket_count)
            .map(|bucket| {
                let start = first + TimeDelta::milliseconds(bucket as i64 * bucket_ms);
                BucketBuilder {
                    bucket: TimelineBucket {
                        start,
                        end: start + TimeDelta::milliseconds(bucket_ms),
                        total: 0,
                        level_counts: LevelCounts::default(),
                        source_count: 0,
                        top_source_id: None,
                        top_source_rows: 0,
                    },
                    sources: BTreeMap::new(),
                }
            })
            .collect::<Vec<_>>();

        for row in rows {
            let Some(timestamp) = row.timestamp else {
                continue;
            };
            let offset = (timestamp - first).num_milliseconds().max(0);
            let index = ((offset / bucket_ms) as usize).min(bucket_count - 1);
            let builder = &mut buckets[index];
            builder.bucket.total += 1;
            builder.bucket.level_counts.record(row.level);
            *builder.sources.entry(row.source_id).or_default() += 1;
        }

        let buckets = buckets
            .into_iter()
            .filter_map(|mut builder| {
                if builder.bucket.total == 0 {
                    return None;
                }
                builder.bucket.source_count = builder.sources.len();
                if let Some((source_id, rows)) =
                    builder.sources.into_iter().max_by_key(|(_, rows)| *rows)
                {
                    builder.bucket.top_source_id = Some(source_id);
                    builder.bucket.top_source_rows = rows;
                }
                Some(builder.bucket)
            })
            .collect::<Vec<_>>();

        let mut analytics = TimelineAnalytics {
            timestamped_rows,
            untimestamped_rows: rows.len().saturating_sub(timestamped_rows),
            first_timestamp: Some(first),
            last_timestamp: Some(last),
            ..TimelineAnalytics::default()
        };
        let mut peak_total = None;
        let mut peak_error = None;
        let mut peak_warn = None;
        for (index, bucket) in buckets.iter().enumerate() {
            let errors = bucket.error_count();
            let warns = bucket.warn_count();
            analytics.error_rows += errors;
            analytics.warn_rows += warns;
            update_peak(&mut peak_total, index, bucket.total, true);
            update_peak(&mut peak_error, index, errors, false);
            update_peak(&mut peak_warn, index, warns, false);
        }
        analytics.peak_total_bucket = peak_total.map(|(index, _)| index);
        analytics.peak_error_bucket = peak_error.map(|(index, _)| index);
        analytics.peak_warn_bucket = peak_warn.map(|(index, _)| index);

        (buckets, analytics)
    }

    fn present_row(&self, row: &LogRow) -> RowPresentation {
        let mut spans = Vec::new();
        if let Some(timestamp) = row.timestamp {
            spans.push(StyledSpan {
                kind: SpanKind::Timestamp,
                text: Arc::from(timestamp.to_rfc3339()),
                byte_range: None,
            });
            spans.push(StyledSpan {
                kind: SpanKind::Message,
                text: Arc::from(" "),
                byte_range: None,
            });
        }

        if let Some(level) = row.level {
            let kind = match level {
                LogLevel::Error | LogLevel::Fatal => SpanKind::Error,
                LogLevel::Warn => SpanKind::Warning,
                _ => SpanKind::Level,
            };
            spans.push(StyledSpan {
                kind,
                text: Arc::from(format!("{level:?}")),
                byte_range: None,
            });
            spans.push(StyledSpan {
                kind: SpanKind::Message,
                text: Arc::from(" "),
                byte_range: None,
            });
        }

        let mut is_match = false;
        if let Some(search) = self.search_text.as_ref() {
            let lower_message = row.message.to_ascii_lowercase();
            let search_lower = search.to_ascii_lowercase();
            if let Some(start) = lower_message.find(&search_lower) {
                is_match = true;
                // The lower-cased search is byte-aligned with the original
                // message only for ASCII inputs. Snap any non-char-boundary
                // edges back to a valid UTF-8 boundary so slicing is safe.
                let end = start + search_lower.len();
                let start = snap_char_boundary(&row.message, start);
                let end = snap_char_boundary(&row.message, end);
                if start > 0 {
                    spans.push(StyledSpan {
                        kind: SpanKind::Message,
                        text: Arc::from(&row.message[..start]),
                        byte_range: None,
                    });
                }
                spans.push(StyledSpan {
                    kind: SpanKind::SearchMatch,
                    text: Arc::from(&row.message[start..end]),
                    byte_range: Some(start..end),
                });
                if end < row.message.len() {
                    spans.push(StyledSpan {
                        kind: SpanKind::Message,
                        text: Arc::from(&row.message[end..]),
                        byte_range: None,
                    });
                }
            }
        }

        if !is_match {
            spans.push(StyledSpan {
                kind: SpanKind::Message,
                text: row.message.clone(),
                byte_range: None,
            });
        }

        if !row.fields.is_empty() {
            spans.push(StyledSpan {
                kind: SpanKind::Message,
                text: Arc::from(" {"),
                byte_range: None,
            });
            for (index, (key, value)) in row.fields.0.iter().enumerate() {
                if index > 0 {
                    spans.push(StyledSpan {
                        kind: SpanKind::Message,
                        text: Arc::from(", "),
                        byte_range: None,
                    });
                }
                spans.push(StyledSpan {
                    kind: SpanKind::JsonKey,
                    text: key.clone(),
                    byte_range: None,
                });
                spans.push(StyledSpan {
                    kind: SpanKind::Message,
                    text: Arc::from("="),
                    byte_range: None,
                });
                spans.push(StyledSpan {
                    kind: SpanKind::JsonValue,
                    text: value.clone(),
                    byte_range: None,
                });
            }
            spans.push(StyledSpan {
                kind: SpanKind::Message,
                text: Arc::from("}"),
                byte_range: None,
            });
        }

        RowPresentation {
            row_id: row.row_id,
            source_id: row.source_id,
            source_name: self
                .sources
                .get(&row.source_id)
                .map(|source| source.name.clone()),
            spans,
            level: row.level,
            is_match,
            is_selected: false,
            is_bookmarked: self.session.is_bookmarked(row.row_id),
            is_stack_continuation: Self::is_stack_trace_continuation(row),
            folded_stack_rows: self.folded_stack_rows(row),
        }
    }

    fn folded_stack_rows(&self, row: &LogRow) -> usize {
        if Self::is_stack_trace_continuation(row) {
            return 0;
        }

        let mut count = 0;
        let mut index = row.row_id.0 as usize + 1;
        while let Some(next) = self.index.find_by_row_number(index) {
            if !Self::is_stack_trace_continuation(next) {
                break;
            }
            count += 1;
            index += 1;
        }
        count
    }

    fn is_stack_trace_continuation(row: &LogRow) -> bool {
        let raw = row.raw.as_ref();
        raw.starts_with(char::is_whitespace)
            || raw.starts_with("at ")
            || raw.starts_with("Caused by:")
            || raw.starts_with("... ")
    }
}

/// Track the index of the bucket with the highest `value`. When `allow_zero`
/// is false the slot is left untouched for empty buckets, so peaks for
/// "any warn/error" stay `None` rather than pointing at the first bucket.
fn update_peak(peak: &mut Option<(usize, usize)>, index: usize, value: usize, allow_zero: bool) {
    if value == 0 && !allow_zero {
        return;
    }
    if peak.is_none_or(|(_, current)| value > current) {
        *peak = Some((index, value));
    }
}

fn snap_char_boundary(s: &str, byte_index: usize) -> usize {
    if byte_index >= s.len() {
        return s.len();
    }
    let mut i = byte_index;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Ingest a raw byte slice into `engine` as a series of `parser.parse_line`
/// calls, splitting on `\n` and honouring an optional preceding `\r` so byte
/// ranges remain accurate on both LF and CRLF inputs.
fn ingest_bytes(engine: &mut Engine, source_id: SourceId, parser: &dyn LogParser, bytes: &[u8]) {
    let mut start = 0u64;
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        // Find the end of the next line (exclusive of terminator).
        let mut line_end = cursor;
        while line_end < bytes.len() && bytes[line_end] != b'\n' {
            line_end += 1;
        }
        let mut consumed_end = line_end;
        if line_end < bytes.len() {
            consumed_end = line_end + 1; // include LF
        }

        let mut text_end = line_end;
        if text_end > cursor && bytes[text_end - 1] == b'\r' {
            text_end -= 1;
        }
        let line = std::str::from_utf8(&bytes[cursor..text_end]).unwrap_or("");
        let end = start + (consumed_end - cursor) as u64;
        let row = parser.parse_line(
            source_id,
            RowId(engine.total_rows() as u64),
            ByteRange { start, end },
            line,
        );
        engine.append_row(row);
        start = end;
        cursor = consumed_end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ByteRange, ParsedFields, RowId, SourceId};

    fn mk_row(id: u64, message: &str, level: Option<LogLevel>) -> LogRow {
        LogRow {
            row_id: RowId(id),
            source_id: SourceId(1),
            byte_range: ByteRange {
                start: id,
                end: id + 1,
            },
            timestamp: None,
            level,
            raw: Arc::from(message),
            message: Arc::from(message),
            fields: ParsedFields::default(),
        }
    }

    fn mk_row_with_source_time(
        id: u64,
        source_id: SourceId,
        timestamp: &str,
        message: &str,
        level: Option<LogLevel>,
    ) -> LogRow {
        LogRow {
            row_id: RowId(id),
            source_id,
            byte_range: ByteRange {
                start: id,
                end: id + 1,
            },
            timestamp: Some(timestamp.parse().unwrap()),
            level,
            raw: Arc::from(message),
            message: Arc::from(message),
            fields: ParsedFields::default(),
        }
    }

    #[test]
    fn viewport_returns_requested_rows_only() {
        let mut engine = Engine::default();
        for i in 0..10 {
            engine.append_row(mk_row(i, &format!("line-{i}"), None));
        }
        let snapshot = engine.viewport(ViewportRequest {
            first_row: 2,
            row_count: 3,
        });
        assert_eq!(snapshot.rows.len(), 3);
        assert_eq!(snapshot.rows[0].row_id.0, 2);
        assert!(snapshot.has_more_before);
        assert!(snapshot.has_more_after);
    }

    #[test]
    fn filter_affects_viewport() {
        let mut engine = Engine::default();
        engine.append_row(mk_row(0, "INFO ok", Some(LogLevel::Info)));
        engine.append_row(mk_row(1, "ERROR failed", Some(LogLevel::Error)));
        engine
            .set_filter(FilterExpr::LevelAtLeast(LogLevel::Warn))
            .unwrap();

        let snapshot = engine.viewport(ViewportRequest {
            first_row: 0,
            row_count: 10,
        });
        assert_eq!(snapshot.total_matching_rows, 1);
        assert_eq!(snapshot.rows[0].row_id.0, 1);
    }

    #[test]
    fn search_text_creates_highlight_span() {
        let mut engine = Engine::default();
        engine.append_row(mk_row(
            0,
            "timeout while contacting db",
            Some(LogLevel::Warn),
        ));
        engine.set_search_text(Some("db".into()));
        let snapshot = engine.viewport(ViewportRequest {
            first_row: 0,
            row_count: 1,
        });
        assert!(
            snapshot.rows[0]
                .spans
                .iter()
                .any(|span| span.kind == SpanKind::SearchMatch)
        );
    }

    #[test]
    fn search_match_handles_utf8_multibyte_messages() {
        let mut engine = Engine::default();
        // "café" contains a 2-byte UTF-8 character (é). Search for "fé"
        // which spans an ASCII boundary on one side and a multibyte one on
        // the other.
        engine.append_row(mk_row(0, "café timeout", Some(LogLevel::Warn)));
        engine.set_search_text(Some("fé".into()));
        let snapshot = engine.viewport(ViewportRequest {
            first_row: 0,
            row_count: 1,
        });
        assert!(snapshot.rows[0].is_match);
    }

    #[test]
    fn row_ids_remain_stable_with_filtering() {
        let mut engine = Engine::default();
        for i in 0..1000 {
            let level = if i % 2 == 0 {
                Some(LogLevel::Error)
            } else {
                Some(LogLevel::Info)
            };
            engine.append_row(mk_row(i, "line", level));
        }
        engine
            .set_filter(FilterExpr::LevelEquals(LogLevel::Error))
            .unwrap();
        let snapshot = engine.viewport(ViewportRequest {
            first_row: 20,
            row_count: 20,
        });
        assert_eq!(snapshot.rows.len(), 20);
        assert_eq!(snapshot.rows[0].row_id.0, 40);
    }

    #[test]
    fn viewport_merges_multiple_sources_by_timestamp() {
        let mut engine = Engine::default();
        engine.add_source(SourceId(1), "one.log");
        engine.add_source(SourceId(2), "two.log");
        engine.append_row(mk_row_with_source_time(
            0,
            SourceId(1),
            "2026-05-21T10:00:02Z",
            "second",
            Some(LogLevel::Info),
        ));
        engine.append_row(mk_row_with_source_time(
            1,
            SourceId(2),
            "2026-05-21T10:00:01Z",
            "first",
            Some(LogLevel::Warn),
        ));

        let snapshot = engine.viewport(ViewportRequest {
            first_row: 0,
            row_count: 10,
        });

        assert_eq!(snapshot.rows[0].row_id, RowId(1));
        assert_eq!(snapshot.rows[0].source_name.as_deref(), Some("two.log"));
        assert_eq!(snapshot.source_summaries.len(), 2);
        assert_eq!(snapshot.level_counts.warn, 1);
        assert!(!snapshot.timeline.is_empty());
    }

    #[test]
    fn timeline_analytics_identifies_peaks_and_source_concentration() {
        let mut engine = Engine::default();
        engine.add_source(SourceId(1), "one.log");
        engine.add_source(SourceId(2), "two.log");
        engine.append_row(mk_row_with_source_time(
            0,
            SourceId(1),
            "2026-05-21T10:00:00Z",
            "INFO ok",
            Some(LogLevel::Info),
        ));
        engine.append_row(mk_row_with_source_time(
            1,
            SourceId(1),
            "2026-05-21T10:00:01Z",
            "ERROR failed",
            Some(LogLevel::Error),
        ));
        engine.append_row(mk_row_with_source_time(
            2,
            SourceId(2),
            "2026-05-21T10:00:01Z",
            "WARN slow",
            Some(LogLevel::Warn),
        ));
        engine.append_row(mk_row(3, "untimestamped", Some(LogLevel::Info)));

        let snapshot = engine.metadata_snapshot();

        assert_eq!(snapshot.timeline_analytics.timestamped_rows, 3);
        assert_eq!(snapshot.timeline_analytics.untimestamped_rows, 1);
        assert_eq!(snapshot.timeline_analytics.error_rows, 1);
        assert_eq!(snapshot.timeline_analytics.warn_rows, 1);
        let peak = snapshot
            .timeline_analytics
            .peak_total_bucket
            .expect("peak bucket");
        assert!(snapshot.timeline[peak].total >= 1);
        assert!(snapshot.timeline.iter().any(|bucket| {
            bucket.source_count >= 1
                && bucket.top_source_id.is_some()
                && bucket.top_source_rows >= 1
        }));
    }

    #[test]
    fn bookmarks_and_search_navigation_use_stable_row_ids() {
        let mut engine = Engine::default();
        engine.append_row(mk_row(0, "INFO ok", Some(LogLevel::Info)));
        engine.append_row(mk_row(1, "ERROR timeout", Some(LogLevel::Error)));
        engine.append_row(mk_row(2, "WARN timeout", Some(LogLevel::Warn)));

        assert!(engine.toggle_bookmark(RowId(1), Some(Arc::from("root cause"))));
        engine.set_search_text(Some("timeout".into()));

        let snapshot = engine.viewport(ViewportRequest {
            first_row: 0,
            row_count: 10,
        });
        assert!(snapshot.rows.iter().any(|row| row.is_bookmarked));
        assert_eq!(engine.next_search_result(None, false), Some(RowId(1)));
        assert_eq!(
            engine.next_search_result(Some(RowId(1)), false),
            Some(RowId(2))
        );
    }

    #[test]
    fn context_around_ignores_active_filter() {
        let mut engine = Engine::default();
        engine.append_row(mk_row(0, "INFO before", Some(LogLevel::Info)));
        engine.append_row(mk_row(1, "ERROR failed", Some(LogLevel::Error)));
        engine.append_row(mk_row(2, "INFO after", Some(LogLevel::Info)));
        engine
            .set_filter(FilterExpr::LevelEquals(LogLevel::Error))
            .unwrap();

        let context = engine.context_around(RowId(1), 1, 1);
        assert_eq!(context.rows.len(), 3);
        assert_eq!(context.rows[0].row_id, RowId(0));
        assert_eq!(context.rows[2].row_id, RowId(2));
        // Stats are over the context window, not the whole engine.
        assert_eq!(context.total_matching_rows, 3);
        assert_eq!(context.level_counts.info, 2);
        assert_eq!(context.level_counts.error, 1);
    }

    #[test]
    fn context_around_returns_empty_for_unknown_row_id() {
        let engine = Engine::default();
        let context = engine.context_around(RowId(42), 1, 1);
        assert!(context.rows.is_empty());
        assert_eq!(context.total_matching_rows, 0);
    }

    #[test]
    fn stack_trace_folding_hides_continuations() {
        let mut engine = Engine::default();
        engine.append_row(mk_row(0, "ERROR failed", Some(LogLevel::Error)));
        engine.append_row(mk_row(1, "    at com.example.Service.run", None));
        engine.append_row(mk_row(2, "INFO recovered", Some(LogLevel::Info)));

        engine.set_stack_trace_folding(true);
        let snapshot = engine.viewport(ViewportRequest {
            first_row: 0,
            row_count: 10,
        });

        assert_eq!(snapshot.rows.len(), 2);
        assert!(!snapshot.rows.iter().any(|row| row.row_id == RowId(1)));
        assert_eq!(snapshot.rows[0].folded_stack_rows, 1);
    }

    #[test]
    fn json_fields_are_presented_as_semantic_spans() {
        let mut engine = Engine::default();
        let mut row = mk_row(0, "started", Some(LogLevel::Info));
        row.fields.insert("service", "billing");
        engine.append_row(row);

        let snapshot = engine.viewport(ViewportRequest {
            first_row: 0,
            row_count: 1,
        });

        assert!(
            snapshot.rows[0]
                .spans
                .iter()
                .any(|span| span.kind == SpanKind::JsonKey && span.text.as_ref() == "service")
        );
        let fields = snapshot.rows[0].json_fields();
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].0.as_ref(), "service");
        assert_eq!(fields[0].1.as_ref(), "billing");
    }

    #[test]
    fn metadata_snapshot_carries_aggregates_without_rows() {
        let mut engine = Engine::default();
        for i in 0..10 {
            engine.append_row(mk_row(i, "line", Some(LogLevel::Info)));
        }
        let snapshot = engine.metadata_snapshot();
        assert!(snapshot.rows.is_empty());
        assert_eq!(snapshot.total_matching_rows, 10);
        assert_eq!(snapshot.level_counts.info, 10);
        assert_eq!(snapshot.source_summaries.len(), 1);
    }

    #[test]
    fn filtered_position_cache_survives_repeated_viewport_calls() {
        let mut engine = Engine::default();
        for i in 0u64..200 {
            let level = if i.is_multiple_of(2) {
                Some(LogLevel::Error)
            } else {
                Some(LogLevel::Info)
            };
            engine.append_row(mk_row(i, "line", level));
        }
        engine
            .set_filter(FilterExpr::LevelEquals(LogLevel::Error))
            .unwrap();
        // Multiple viewport calls should produce the same matching count
        // without re-scanning the index every time (the cache being
        // populated is a precondition).
        for _ in 0..5 {
            let snapshot = engine.viewport(ViewportRequest {
                first_row: 0,
                row_count: 10,
            });
            assert_eq!(snapshot.total_matching_rows, 100);
        }
    }

    #[test]
    fn append_invalidates_filter_cache() {
        let mut engine = Engine::default();
        engine.append_row(mk_row(0, "INFO", Some(LogLevel::Info)));
        engine
            .set_filter(FilterExpr::LevelEquals(LogLevel::Error))
            .unwrap();
        assert_eq!(engine.matching_rows_count(), 0);
        engine.append_row(mk_row(1, "ERROR", Some(LogLevel::Error)));
        assert_eq!(engine.matching_rows_count(), 1);
    }

    #[test]
    fn load_file_honours_crlf_terminators() {
        use crate::parser::CompositeParser;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"first\r\nsecond\r\n").unwrap();

        let mut engine = Engine::default();
        engine
            .load_file(tmp.path(), &CompositeParser::default())
            .unwrap();
        assert_eq!(engine.total_rows(), 2);

        let rows = engine.index.rows();
        // First row: "first" (5 bytes) + CRLF (2 bytes) → range 0..7
        assert_eq!(rows[0].byte_range.start, 0);
        assert_eq!(rows[0].byte_range.end, 7);
        // Second row: starts at 7, "second" (6 bytes) + CRLF (2 bytes) → ends at 15
        assert_eq!(rows[1].byte_range.start, 7);
        assert_eq!(rows[1].byte_range.end, 15);
        // Raw text is the line without the terminator.
        assert_eq!(rows[0].raw.as_ref(), "first");
        assert_eq!(rows[1].raw.as_ref(), "second");
    }
}
