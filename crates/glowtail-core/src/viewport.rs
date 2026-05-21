use crate::filter::{CompiledFilter, FilterExpr};
use crate::index::RowIndex;
use crate::model::{
    LevelCounts, LogLevel, LogRow, LogSourceKind, RowId, RowPresentation, SourceId, SourceInfo,
    SourceSummary, SpanKind, StyledSpan, TimelineBucket, ViewportRequest, ViewportSnapshot,
};
use crate::session::InvestigationSession;
use chrono::TimeDelta;
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Debug, Default)]
pub struct Engine {
    index: RowIndex,
    filter_expr: FilterExpr,
    compiled_filter: CompiledFilter,
    search_text: Option<String>,
    sources: BTreeMap<SourceId, SourceInfo>,
    session: InvestigationSession,
    collapse_stack_traces: bool,
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
    }

    pub fn set_filter(&mut self, filter: FilterExpr) -> Result<(), crate::filter::FilterError> {
        self.compiled_filter = CompiledFilter::compile(&filter)?;
        self.session.record_filter(filter.clone());
        self.filter_expr = filter;
        Ok(())
    }

    pub fn clear_filter(&mut self) {
        self.filter_expr = FilterExpr::All;
        self.compiled_filter = CompiledFilter::All;
    }

    pub fn set_search_text(&mut self, search_text: Option<String>) {
        self.search_text = search_text.filter(|text| !text.is_empty());
    }

    pub fn set_stack_trace_folding(&mut self, enabled: bool) {
        self.collapse_stack_traces = enabled;
    }

    pub fn toggle_bookmark(&mut self, row_id: RowId, label: Option<String>) -> bool {
        self.session.toggle_bookmark(row_id, label)
    }

    pub fn save_filter(&mut self, name: impl Into<String>) {
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

    pub fn matching_rows_count(&self) -> usize {
        self.filtered_rows().len()
    }

    pub fn viewport(&self, request: ViewportRequest) -> ViewportSnapshot {
        let filtered = self.filtered_rows();
        let total = filtered.len();
        let start = request.first_row.min(total);
        let end = (start + request.row_count).min(total);
        let rows = filtered[start..end]
            .iter()
            .map(|row| self.present_row(row))
            .collect();

        ViewportSnapshot {
            rows,
            total_rows: self.total_rows(),
            total_matching_rows: total,
            has_more_before: start > 0,
            has_more_after: end < total,
            level_counts: Self::level_counts(&filtered),
            source_summaries: self.source_summaries(&filtered),
            timeline: Self::timeline(&filtered, 24),
        }
    }

    fn filtered_rows(&self) -> Vec<&LogRow> {
        let mut rows = self
            .index
            .rows()
            .iter()
            .filter(|row| self.compiled_filter.matches(row))
            .filter(|row| !self.collapse_stack_traces || !Self::is_stack_trace_continuation(row))
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| {
            left.timestamp
                .cmp(&right.timestamp)
                .then(left.row_id.cmp(&right.row_id))
        });
        rows
    }

    pub fn filtered_position_for_row(&self, row_id: RowId) -> Option<usize> {
        self.filtered_rows()
            .iter()
            .position(|row| row.row_id == row_id)
    }

    pub fn search_results(&self) -> Vec<RowId> {
        let Some(search) = self.search_text.as_ref() else {
            return Vec::new();
        };
        let search = search.to_ascii_lowercase();
        self.filtered_rows()
            .into_iter()
            .filter(|row| row.raw.to_ascii_lowercase().contains(&search))
            .map(|row| row.row_id)
            .collect()
    }

    pub fn next_search_result(&self, current: Option<RowId>, reverse: bool) -> Option<RowId> {
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

    pub fn context_around(&self, row_id: RowId, before: usize, after: usize) -> ViewportSnapshot {
        let row_number = row_id.0 as usize;
        let first_row = row_number.saturating_sub(before);
        let row_count = before + after + 1;
        let rows = self
            .index
            .iter_range(first_row, row_count)
            .into_iter()
            .map(|row| self.present_row(row))
            .collect::<Vec<_>>();
        let all_rows = self.index.rows().iter().collect::<Vec<_>>();

        ViewportSnapshot {
            rows,
            total_rows: self.total_rows(),
            total_matching_rows: row_count.min(self.total_rows().saturating_sub(first_row)),
            has_more_before: first_row > 0,
            has_more_after: first_row + row_count < self.total_rows(),
            level_counts: Self::level_counts(&all_rows),
            source_summaries: self.source_summaries(&all_rows),
            timeline: Self::timeline(&all_rows, 24),
        }
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
            let name = self
                .sources
                .get(&row.source_id)
                .map(|source| source.name.clone())
                .unwrap_or_else(|| Arc::from(format!("source-{}", row.source_id.0)));
            let summary = summaries.entry(row.source_id).or_insert(SourceSummary {
                source_id: row.source_id,
                name,
                rows: 0,
                level_counts: LevelCounts::default(),
            });
            summary.rows += 1;
            summary.level_counts.record(row.level);
        }
        summaries.into_values().collect()
    }

    fn timeline(rows: &[&LogRow], max_buckets: usize) -> Vec<TimelineBucket> {
        let timestamps = rows
            .iter()
            .filter_map(|row| row.timestamp)
            .collect::<Vec<_>>();
        let (Some(first), Some(last)) = (timestamps.iter().min(), timestamps.iter().max()) else {
            return Vec::new();
        };

        let bucket_count = max_buckets.min(timestamps.len()).max(1);
        let total_ms = (*last - *first).num_milliseconds().max(1);
        let bucket_ms = (total_ms / bucket_count as i64).max(1);
        let mut buckets = (0..bucket_count)
            .map(|bucket| {
                let start = *first + TimeDelta::milliseconds(bucket as i64 * bucket_ms);
                TimelineBucket {
                    start,
                    end: start + TimeDelta::milliseconds(bucket_ms),
                    total: 0,
                    warn: 0,
                    error: 0,
                }
            })
            .collect::<Vec<_>>();

        for row in rows {
            let Some(timestamp) = row.timestamp else {
                continue;
            };
            let offset = (timestamp - *first).num_milliseconds().max(0);
            let index = ((offset / bucket_ms) as usize).min(bucket_count - 1);
            let bucket = &mut buckets[index];
            bucket.total += 1;
            match row.level {
                Some(LogLevel::Warn) => bucket.warn += 1,
                Some(LogLevel::Error | LogLevel::Fatal) => bucket.error += 1,
                _ => {}
            }
        }

        buckets
            .into_iter()
            .filter(|bucket| bucket.total > 0)
            .collect()
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
                let end = start + search.len();
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
    fn bookmarks_and_search_navigation_use_stable_row_ids() {
        let mut engine = Engine::default();
        engine.append_row(mk_row(0, "INFO ok", Some(LogLevel::Info)));
        engine.append_row(mk_row(1, "ERROR timeout", Some(LogLevel::Error)));
        engine.append_row(mk_row(2, "WARN timeout", Some(LogLevel::Warn)));

        assert!(engine.toggle_bookmark(RowId(1), Some("root cause".into())));
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
    }
}
