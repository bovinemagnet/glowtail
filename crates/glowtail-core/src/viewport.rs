use crate::filter::{CompiledFilter, FilterExpr};
use crate::index::RowIndex;
use crate::model::{
    LogLevel, LogRow, RowPresentation, SpanKind, StyledSpan, ViewportRequest, ViewportSnapshot,
};
use std::sync::Arc;

#[derive(Debug, Default)]
pub struct Engine {
    index: RowIndex,
    filter_expr: FilterExpr,
    compiled_filter: CompiledFilter,
    search_text: Option<String>,
}

impl Engine {
    pub fn append_row(&mut self, row: LogRow) {
        self.index.append(row);
    }

    pub fn set_filter(&mut self, filter: FilterExpr) -> Result<(), crate::filter::FilterError> {
        self.compiled_filter = CompiledFilter::compile(&filter)?;
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
            total_matching_rows: total,
            has_more_before: start > 0,
            has_more_after: end < total,
        }
    }

    fn filtered_rows(&self) -> Vec<&LogRow> {
        self.index
            .rows()
            .iter()
            .filter(|row| self.compiled_filter.matches(row))
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

        if spans.is_empty() || !is_match {
            spans.push(StyledSpan {
                kind: SpanKind::Message,
                text: row.message.clone(),
                byte_range: None,
            });
        }

        RowPresentation {
            row_id: row.row_id,
            spans,
            level: row.level,
            is_match,
            is_selected: false,
        }
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
}
