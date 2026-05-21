use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RowId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SourceId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[non_exhaustive]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}

impl LogLevel {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "trace" => Some(Self::Trace),
            "debug" => Some(Self::Debug),
            "info" => Some(Self::Info),
            "warn" | "warning" => Some(Self::Warn),
            "error" => Some(Self::Error),
            "fatal" => Some(Self::Fatal),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LogSourceKind {
    File,
    Stdin,
    Other(Arc<str>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceInfo {
    pub source_id: SourceId,
    pub name: Arc<str>,
    pub kind: LogSourceKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LevelCounts {
    pub trace: usize,
    pub debug: usize,
    pub info: usize,
    pub warn: usize,
    pub error: usize,
    pub fatal: usize,
    pub unknown: usize,
}

impl LevelCounts {
    pub fn record(&mut self, level: Option<LogLevel>) {
        match level {
            Some(LogLevel::Trace) => self.trace += 1,
            Some(LogLevel::Debug) => self.debug += 1,
            Some(LogLevel::Info) => self.info += 1,
            Some(LogLevel::Warn) => self.warn += 1,
            Some(LogLevel::Error) => self.error += 1,
            Some(LogLevel::Fatal) => self.fatal += 1,
            None => self.unknown += 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSummary {
    pub source_id: SourceId,
    pub name: Arc<str>,
    pub rows: usize,
    pub level_counts: LevelCounts,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineBucket {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub total: usize,
    pub warn: usize,
    pub error: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedFields(pub BTreeMap<Arc<str>, Arc<str>>);

impl ParsedFields {
    pub fn insert(&mut self, key: impl Into<Arc<str>>, value: impl Into<Arc<str>>) {
        self.0.insert(key.into(), value.into());
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRow {
    pub row_id: RowId,
    pub source_id: SourceId,
    pub byte_range: ByteRange,
    pub timestamp: Option<DateTime<Utc>>,
    pub level: Option<LogLevel>,
    pub raw: Arc<str>,
    pub message: Arc<str>,
    pub fields: ParsedFields,
}

/// Semantic role of a span inside a [`RowPresentation`]. UI front-ends translate
/// these into their own styling at the seam (e.g. `SpanKind::Error` →
/// `ratatui::style::Color::Red`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SpanKind {
    Timestamp,
    Level,
    Source,
    Message,
    JsonKey,
    JsonValue,
    SearchMatch,
    Error,
    Warning,
    StackTrace,
}

/// Semantic colour role for a log row's severity band. UIs map this onto their
/// native colour type at the seam; the core never names colours directly.
/// Mirrors [`LogLevel`] plus an `Unknown` variant for unparseable severities;
/// not marked `#[non_exhaustive]` because any new variant here will land in
/// lockstep with one on `LogLevel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SeverityRole {
    Fatal,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
    Unknown,
}

impl SeverityRole {
    pub fn from_level(level: Option<LogLevel>) -> Self {
        match level {
            Some(LogLevel::Fatal) => Self::Fatal,
            Some(LogLevel::Error) => Self::Error,
            Some(LogLevel::Warn) => Self::Warn,
            Some(LogLevel::Info) => Self::Info,
            Some(LogLevel::Debug) => Self::Debug,
            Some(LogLevel::Trace) => Self::Trace,
            None => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledSpan {
    pub kind: SpanKind,
    pub text: Arc<str>,
    pub byte_range: Option<Range<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowPresentation {
    pub row_id: RowId,
    pub source_id: SourceId,
    pub source_name: Option<Arc<str>>,
    pub spans: Vec<StyledSpan>,
    pub level: Option<LogLevel>,
    pub is_match: bool,
    pub is_selected: bool,
    pub is_bookmarked: bool,
    pub is_stack_continuation: bool,
    pub folded_stack_rows: usize,
}

impl RowPresentation {
    /// Iterate semantic JSON key/value pairs present on this row. Both UIs use
    /// the same logic to build a structured-field detail panel; the core owns
    /// it so they don't drift.
    pub fn json_fields(&self) -> Vec<(Arc<str>, Arc<str>)> {
        let mut fields = Vec::new();
        let mut pending_key: Option<Arc<str>> = None;
        for span in &self.spans {
            match span.kind {
                SpanKind::JsonKey => pending_key = Some(span.text.clone()),
                SpanKind::JsonValue => {
                    if let Some(key) = pending_key.take() {
                        fields.push((key, span.text.clone()));
                    }
                }
                _ => {}
            }
        }
        fields
    }

    /// Convenience accessor for the row's severity role. UIs use this to map
    /// onto their native colour for the severity gutter.
    pub fn severity_role(&self) -> SeverityRole {
        SeverityRole::from_level(self.level)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewportRequest {
    pub first_row: usize,
    pub row_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewportSnapshot {
    pub rows: Vec<RowPresentation>,
    pub total_rows: usize,
    pub total_matching_rows: usize,
    pub has_more_before: bool,
    pub has_more_after: bool,
    pub level_counts: LevelCounts,
    pub source_summaries: Vec<SourceSummary>,
    pub timeline: Vec<TimelineBucket>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_represents_plain_text_line() {
        let row = LogRow {
            row_id: RowId(1),
            source_id: SourceId(1),
            byte_range: ByteRange { start: 0, end: 10 },
            timestamp: None,
            level: None,
            raw: Arc::from("hello world"),
            message: Arc::from("hello world"),
            fields: ParsedFields::default(),
        };

        assert_eq!(row.message.as_ref(), "hello world");
        assert!(row.fields.is_empty());
    }

    #[test]
    fn model_represents_error_line() {
        let row = LogRow {
            row_id: RowId(2),
            source_id: SourceId(1),
            byte_range: ByteRange { start: 10, end: 30 },
            timestamp: None,
            level: Some(LogLevel::Error),
            raw: Arc::from("ERROR failed"),
            message: Arc::from("ERROR failed"),
            fields: ParsedFields::default(),
        };
        assert_eq!(row.level, Some(LogLevel::Error));
    }

    #[test]
    fn model_represents_json_line_with_fields() {
        let mut fields = ParsedFields::default();
        fields.insert(Arc::<str>::from("service"), Arc::<str>::from("billing"));
        let row = LogRow {
            row_id: RowId(3),
            source_id: SourceId(1),
            byte_range: ByteRange {
                start: 30,
                end: 120,
            },
            timestamp: None,
            level: Some(LogLevel::Info),
            raw: Arc::from("{}"),
            message: Arc::from("started"),
            fields,
        };

        assert_eq!(
            row.fields.0.get("service").map(AsRef::as_ref),
            Some("billing")
        );
    }

    #[test]
    fn model_represents_search_highlight_spans() {
        let presentation = RowPresentation {
            row_id: RowId(4),
            source_id: SourceId(1),
            source_name: None,
            spans: vec![
                StyledSpan {
                    kind: SpanKind::Message,
                    text: Arc::from("timeout while contacting "),
                    byte_range: None,
                },
                StyledSpan {
                    kind: SpanKind::SearchMatch,
                    text: Arc::from("db"),
                    byte_range: Some(25..27),
                },
            ],
            level: Some(LogLevel::Warn),
            is_match: true,
            is_selected: false,
            is_bookmarked: false,
            is_stack_continuation: false,
            folded_stack_rows: 0,
        };

        assert!(
            presentation
                .spans
                .iter()
                .any(|span| span.kind == SpanKind::SearchMatch)
        );
    }
}
