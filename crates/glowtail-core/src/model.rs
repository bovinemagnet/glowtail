use chrono::{DateTime, Utc};
use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RowId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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
pub enum LogSourceKind {
    File,
    Stdin,
    Other(Arc<str>),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledSpan {
    pub kind: SpanKind,
    pub text: Arc<str>,
    pub byte_range: Option<Range<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowPresentation {
    pub row_id: RowId,
    pub spans: Vec<StyledSpan>,
    pub level: Option<LogLevel>,
    pub is_match: bool,
    pub is_selected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewportRequest {
    pub first_row: usize,
    pub row_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewportSnapshot {
    pub rows: Vec<RowPresentation>,
    pub total_matching_rows: usize,
    pub has_more_before: bool,
    pub has_more_after: bool,
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
        };

        assert!(
            presentation
                .spans
                .iter()
                .any(|span| span.kind == SpanKind::SearchMatch)
        );
    }
}
