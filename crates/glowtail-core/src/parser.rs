use crate::model::{ByteRange, LogLevel, LogRow, ParsedFields, RowId, SourceId};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::sync::Arc;

pub trait LogParser: Send + Sync {
    fn name(&self) -> &'static str;
    fn parse_line(
        &self,
        source_id: SourceId,
        row_id: RowId,
        byte_range: ByteRange,
        line: &str,
    ) -> LogRow;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct PlainTextParser;

impl PlainTextParser {
    fn detect_level(line: &str) -> Option<LogLevel> {
        [
            "TRACE", "DEBUG", "INFO", "WARN", "WARNING", "ERROR", "FATAL",
        ]
        .iter()
        .find(|token| line.contains(**token))
        .and_then(|token| LogLevel::parse(token))
    }
}

impl LogParser for PlainTextParser {
    fn name(&self) -> &'static str {
        "plain"
    }

    fn parse_line(
        &self,
        source_id: SourceId,
        row_id: RowId,
        byte_range: ByteRange,
        line: &str,
    ) -> LogRow {
        let raw: Arc<str> = Arc::from(line);
        LogRow {
            row_id,
            source_id,
            byte_range,
            timestamp: None,
            level: Self::detect_level(line),
            raw: raw.clone(),
            message: raw,
            fields: ParsedFields::default(),
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct JsonLineParser;

impl JsonLineParser {
    fn parse_timestamp(value: &Value) -> Option<DateTime<Utc>> {
        ["timestamp", "time", "ts", "@timestamp"]
            .iter()
            .find_map(|key| value.get(key))
            .and_then(Value::as_str)
            .and_then(|ts| ts.parse::<DateTime<Utc>>().ok())
    }

    fn parse_level(value: &Value) -> Option<LogLevel> {
        let nested = value
            .get("log")
            .and_then(|v| v.get("level"))
            .and_then(Value::as_str);
        nested
            .or_else(|| value.get("level").and_then(Value::as_str))
            .or_else(|| value.get("severity").and_then(Value::as_str))
            .and_then(LogLevel::parse)
    }

    fn parse_message(value: &Value) -> Option<&str> {
        value
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| value.get("msg").and_then(Value::as_str))
            .or_else(|| value.get("log").and_then(Value::as_str))
    }

    fn parse_fields(value: &Value) -> ParsedFields {
        let mut fields = ParsedFields::default();
        if let Some(map) = value.as_object() {
            for (k, v) in map {
                if matches!(
                    k.as_str(),
                    "timestamp"
                        | "time"
                        | "ts"
                        | "@timestamp"
                        | "level"
                        | "severity"
                        | "message"
                        | "msg"
                ) {
                    continue;
                }
                if k == "log" {
                    if let Some(level) = v.get("level").and_then(Value::as_str) {
                        fields.insert(Arc::<str>::from("log.level"), Arc::<str>::from(level));
                    }
                    continue;
                }
                fields.insert(Arc::<str>::from(k.clone()), Arc::<str>::from(v.to_string()));
            }
        }
        fields
    }

    pub fn try_parse_line(
        &self,
        source_id: SourceId,
        row_id: RowId,
        byte_range: ByteRange,
        line: &str,
    ) -> Option<LogRow> {
        let parsed = serde_json::from_str::<Value>(line).ok()?;
        let raw: Arc<str> = Arc::from(line);
        let message = Self::parse_message(&parsed).unwrap_or(line);

        Some(LogRow {
            row_id,
            source_id,
            byte_range,
            timestamp: Self::parse_timestamp(&parsed),
            level: Self::parse_level(&parsed),
            raw,
            message: Arc::from(message),
            fields: Self::parse_fields(&parsed),
        })
    }
}

impl LogParser for JsonLineParser {
    fn name(&self) -> &'static str {
        "jsonl"
    }

    fn parse_line(
        &self,
        source_id: SourceId,
        row_id: RowId,
        byte_range: ByteRange,
        line: &str,
    ) -> LogRow {
        self.try_parse_line(source_id, row_id, byte_range, line)
            .unwrap_or_else(|| PlainTextParser.parse_line(source_id, row_id, byte_range, line))
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CompositeParser {
    json: JsonLineParser,
    plain: PlainTextParser,
}

impl LogParser for CompositeParser {
    fn name(&self) -> &'static str {
        "composite"
    }

    fn parse_line(
        &self,
        source_id: SourceId,
        row_id: RowId,
        byte_range: ByteRange,
        line: &str,
    ) -> LogRow {
        self.json
            .try_parse_line(source_id, row_id, byte_range, line)
            .unwrap_or_else(|| self.plain.parse_line(source_id, row_id, byte_range, line))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_parser_detects_level() {
        let parser = PlainTextParser;
        let row = parser.parse_line(
            SourceId(1),
            RowId(1),
            ByteRange { start: 0, end: 4 },
            "WARN retrying request",
        );
        assert_eq!(row.level, Some(LogLevel::Warn));
    }

    #[test]
    fn json_parser_extracts_known_fields() {
        let parser = JsonLineParser;
        let line = r#"{"timestamp":"2026-05-21T10:15:30Z","level":"ERROR","message":"failed","service":"billing"}"#;
        let row = parser.parse_line(SourceId(1), RowId(1), ByteRange { start: 0, end: 1 }, line);
        assert_eq!(row.level, Some(LogLevel::Error));
        assert_eq!(row.message.as_ref(), "failed");
        assert_eq!(
            row.fields.0.get("service").map(AsRef::as_ref),
            Some("\"billing\"")
        );
        assert!(row.timestamp.is_some());
    }

    #[test]
    fn composite_falls_back_to_plain_on_invalid_json() {
        let parser = CompositeParser::default();
        let row = parser.parse_line(
            SourceId(1),
            RowId(1),
            ByteRange { start: 0, end: 1 },
            "java.lang.NullPointerException: boom",
        );
        assert_eq!(row.message.as_ref(), "java.lang.NullPointerException: boom");
        assert!(row.level.is_none());
    }

    #[test]
    fn parser_does_not_panic_on_malformed_input() {
        let parser = CompositeParser::default();
        let row = parser.parse_line(
            SourceId(1),
            RowId(1),
            ByteRange { start: 0, end: 1 },
            "{this is not json",
        );
        assert_eq!(row.raw.as_ref(), "{this is not json");
    }
}
