use crate::model::{LogLevel, LogRow, SourceId};
use chrono::{DateTime, TimeDelta, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub enum FilterExpr {
    #[default]
    All,
    LevelAtLeast(LogLevel),
    LevelEquals(LogLevel),
    Contains(String),
    MessageContains(String),
    Regex(String),
    Source(SourceId),
    FieldEquals {
        field: String,
        value: String,
    },
    FieldContains {
        field: String,
        value: String,
    },
    TimestampBetween {
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
    },
    And(Vec<FilterExpr>),
    Or(Vec<FilterExpr>),
    Not(Box<FilterExpr>),
}

#[derive(Debug, Default)]
pub enum CompiledFilter {
    #[default]
    All,
    LevelAtLeast(LogLevel),
    LevelEquals(LogLevel),
    Contains(String),
    MessageContains(String),
    Regex(Regex),
    Source(SourceId),
    FieldEquals {
        field: String,
        value: String,
    },
    FieldContains {
        field: String,
        value: String,
    },
    TimestampBetween {
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
    },
    And(Vec<CompiledFilter>),
    Or(Vec<CompiledFilter>),
    Not(Box<CompiledFilter>),
}

impl FilterExpr {
    pub fn and_all(filters: impl IntoIterator<Item = FilterExpr>) -> Self {
        let filters = filters
            .into_iter()
            .filter(|filter| !matches!(filter, FilterExpr::All))
            .collect::<Vec<_>>();

        match filters.len() {
            0 => FilterExpr::All,
            1 => filters.into_iter().next().expect("one filter exists"),
            _ => FilterExpr::And(filters),
        }
    }
}

/// Composition of the three CLI/UI filter inputs:
///   - `saved_filter`: name of a session-stored filter to start from
///   - `level`: minimum severity (kept if at or above this level)
///   - `contains`: case-insensitive substring filter
///
/// Returns `Ok(filter)` even when all three are `None` (the result is then
/// [`FilterExpr::All`]). Returns `Err(saved_filter)` when a saved-filter name
/// is given but not present in the session, so callers can render an error.
pub fn compose_filter(
    saved_filter: Option<&crate::filter::FilterExpr>,
    level: Option<LogLevel>,
    contains: Option<&str>,
) -> FilterExpr {
    let mut parts = Vec::new();
    if let Some(filter) = saved_filter {
        parts.push(filter.clone());
    }
    if let Some(level) = level {
        parts.push(FilterExpr::LevelAtLeast(level));
    }
    if let Some(text) = contains
        && !text.is_empty()
    {
        parts.push(FilterExpr::Contains(text.to_owned()));
    }
    FilterExpr::and_all(parts)
}

pub fn compose_query_filter(
    saved_filter: Option<&crate::filter::FilterExpr>,
    level: Option<LogLevel>,
    filter_text: Option<&str>,
) -> Result<FilterExpr, FilterError> {
    let mut parts = Vec::new();
    if let Some(filter) = saved_filter {
        parts.push(filter.clone());
    }
    if let Some(level) = level {
        parts.push(FilterExpr::LevelAtLeast(level));
    }
    if let Some(text) = filter_text
        && !text.trim().is_empty()
    {
        parts.push(parse_filter_query(text)?);
    }
    Ok(FilterExpr::and_all(parts))
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FilterError {
    #[error("invalid regex '{pattern}': {source}")]
    InvalidRegex {
        pattern: String,
        source: regex::Error,
    },
    #[error("invalid query: {0}")]
    InvalidQuery(String),
}

impl CompiledFilter {
    pub fn compile(expr: &FilterExpr) -> Result<Self, FilterError> {
        Ok(match expr {
            FilterExpr::All => Self::All,
            FilterExpr::LevelAtLeast(level) => Self::LevelAtLeast(*level),
            FilterExpr::LevelEquals(level) => Self::LevelEquals(*level),
            FilterExpr::Contains(text) => Self::Contains(text.to_ascii_lowercase()),
            FilterExpr::MessageContains(text) => Self::MessageContains(text.to_ascii_lowercase()),
            FilterExpr::Regex(pattern) => {
                Self::Regex(
                    Regex::new(pattern).map_err(|source| FilterError::InvalidRegex {
                        pattern: pattern.clone(),
                        source,
                    })?,
                )
            }
            FilterExpr::Source(id) => Self::Source(*id),
            FilterExpr::FieldEquals { field, value } => Self::FieldEquals {
                field: normalize_field(field),
                value: value.clone(),
            },
            FilterExpr::FieldContains { field, value } => Self::FieldContains {
                field: normalize_field(field),
                value: value.to_ascii_lowercase(),
            },
            FilterExpr::TimestampBetween { start, end } => Self::TimestampBetween {
                start: *start,
                end: *end,
            },
            FilterExpr::And(parts) => {
                Self::And(parts.iter().map(Self::compile).collect::<Result<_, _>>()?)
            }
            FilterExpr::Or(parts) => {
                Self::Or(parts.iter().map(Self::compile).collect::<Result<_, _>>()?)
            }
            FilterExpr::Not(inner) => Self::Not(Box::new(Self::compile(inner)?)),
        })
    }

    pub fn matches(&self, row: &LogRow) -> bool {
        match self {
            Self::All => true,
            Self::LevelAtLeast(min) => row.level.map(|level| level >= *min).unwrap_or(false),
            Self::LevelEquals(level) => row.level == Some(*level),
            Self::Contains(needle) => row.raw.to_ascii_lowercase().contains(needle),
            Self::MessageContains(needle) => row.message.to_ascii_lowercase().contains(needle),
            Self::Regex(regex) => regex.is_match(row.raw.as_ref()),
            Self::Source(source_id) => row.source_id == *source_id,
            Self::FieldEquals { field, value } => row_field(row, field)
                .map(|actual| actual == value.as_str())
                .unwrap_or(false),
            Self::FieldContains { field, value } => row_field(row, field)
                .map(|actual| actual.to_ascii_lowercase().contains(value))
                .unwrap_or(false),
            Self::TimestampBetween { start, end } => row
                .timestamp
                .map(|timestamp| {
                    start.map(|start| timestamp >= start).unwrap_or(true)
                        && end.map(|end| timestamp <= end).unwrap_or(true)
                })
                .unwrap_or(false),
            Self::And(items) => items.iter().all(|f| f.matches(row)),
            Self::Or(items) => items.iter().any(|f| f.matches(row)),
            Self::Not(inner) => !inner.matches(row),
        }
    }
}

pub fn parse_filter_query(input: &str) -> Result<FilterExpr, FilterError> {
    let tokens = tokenize(input)?;
    if tokens.is_empty() {
        return Ok(FilterExpr::All);
    }
    let mut parser = QueryParser { tokens, cursor: 0 };
    let expr = parser.parse_or()?;
    if !parser.is_done() {
        return Err(FilterError::InvalidQuery(format!(
            "unexpected token '{}'",
            parser.peek().unwrap_or("")
        )));
    }
    Ok(expr)
}

fn row_field(row: &LogRow, field: &str) -> Option<String> {
    match normalize_field(field).as_str() {
        "message" => Some(row.message.to_string()),
        "raw" => Some(row.raw.to_string()),
        "level" => row.level.map(|level| format!("{level:?}")),
        "source" | "source_id" => Some(row.source_id.0.to_string()),
        "timestamp" => row.timestamp.map(|timestamp| timestamp.to_rfc3339()),
        field => row
            .fields
            .0
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(field))
            .map(|(_, value)| value.to_string()),
    }
}

fn normalize_field(field: &str) -> String {
    field
        .strip_prefix("json.")
        .unwrap_or(field)
        .to_ascii_lowercase()
}

fn parse_level(value: &str) -> Result<LogLevel, FilterError> {
    LogLevel::parse(value)
        .ok_or_else(|| FilterError::InvalidQuery(format!("unknown log level '{value}'")))
}

fn parse_timestamp(value: &str) -> Result<DateTime<Utc>, FilterError> {
    if let Some(rest) = value.strip_prefix("now()-") {
        return parse_duration(rest).map(|duration| Utc::now() - duration);
    }
    if value == "now()" || value == "now" {
        return Ok(Utc::now());
    }
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .map_err(|err| FilterError::InvalidQuery(format!("invalid timestamp '{value}': {err}")))
}

fn parse_duration(value: &str) -> Result<TimeDelta, FilterError> {
    if value.len() < 2 {
        return Err(FilterError::InvalidQuery(format!(
            "invalid duration '{value}'"
        )));
    }
    let (number, unit) = value.split_at(value.len() - 1);
    let amount: i64 = number
        .parse()
        .map_err(|_| FilterError::InvalidQuery(format!("invalid duration '{value}'")))?;
    match unit {
        "s" => Ok(TimeDelta::seconds(amount)),
        "m" => Ok(TimeDelta::minutes(amount)),
        "h" => Ok(TimeDelta::hours(amount)),
        "d" => Ok(TimeDelta::days(amount)),
        _ => Err(FilterError::InvalidQuery(format!(
            "invalid duration unit '{unit}'"
        ))),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(String),
    String(String),
    Eq,
    NotEq,
    Gte,
    Lte,
    Gt,
    Lt,
    Comma,
    LParen,
    RParen,
}

fn tokenize(input: &str) -> Result<Vec<Token>, FilterError> {
    let mut tokens = Vec::new();
    let mut chars = input.char_indices().peekable();
    while let Some((_, ch)) = chars.peek().copied() {
        match ch {
            c if c.is_whitespace() => {
                chars.next();
            }
            '"' => {
                chars.next();
                let mut value = String::new();
                let mut closed = false;
                while let Some((_, c)) = chars.next() {
                    match c {
                        '"' => {
                            closed = true;
                            break;
                        }
                        '\\' => {
                            if let Some((_, escaped)) = chars.next() {
                                value.push(escaped);
                            }
                        }
                        _ => value.push(c),
                    }
                }
                if !closed {
                    return Err(FilterError::InvalidQuery("unterminated string".into()));
                }
                tokens.push(Token::String(value));
            }
            '=' => {
                chars.next();
                if chars.peek().map(|(_, c)| *c) == Some('=') {
                    chars.next();
                }
                tokens.push(Token::Eq);
            }
            '!' => {
                chars.next();
                if chars.peek().map(|(_, c)| *c) == Some('=') {
                    chars.next();
                    tokens.push(Token::NotEq);
                } else {
                    return Err(FilterError::InvalidQuery("expected != after !".into()));
                }
            }
            '>' => {
                chars.next();
                if chars.peek().map(|(_, c)| *c) == Some('=') {
                    chars.next();
                    tokens.push(Token::Gte);
                } else {
                    tokens.push(Token::Gt);
                }
            }
            '<' => {
                chars.next();
                if chars.peek().map(|(_, c)| *c) == Some('=') {
                    chars.next();
                    tokens.push(Token::Lte);
                } else {
                    tokens.push(Token::Lt);
                }
            }
            ',' => {
                chars.next();
                tokens.push(Token::Comma);
            }
            '(' => {
                chars.next();
                tokens.push(Token::LParen);
            }
            ')' => {
                chars.next();
                tokens.push(Token::RParen);
            }
            _ => {
                let start = chars.peek().map(|(index, _)| *index).unwrap_or(input.len());
                let mut end = start;
                while let Some((index, c)) = chars.peek().copied() {
                    if c.is_whitespace()
                        || matches!(c, '"' | '=' | '!' | '>' | '<' | ',' | '(' | ')')
                    {
                        break;
                    }
                    end = index + c.len_utf8();
                    chars.next();
                }
                tokens.push(Token::Word(input[start..end].to_string()));
            }
        }
    }
    Ok(tokens)
}

struct QueryParser {
    tokens: Vec<Token>,
    cursor: usize,
}

impl QueryParser {
    fn parse_or(&mut self) -> Result<FilterExpr, FilterError> {
        let mut parts = vec![self.parse_and()?];
        while self.consume_keyword("or") {
            parts.push(self.parse_and()?);
        }
        Ok(match parts.len() {
            1 => parts.remove(0),
            _ => FilterExpr::Or(parts),
        })
    }

    fn parse_and(&mut self) -> Result<FilterExpr, FilterError> {
        let mut parts = vec![self.parse_not()?];
        while self.consume_keyword("and") {
            parts.push(self.parse_not()?);
        }
        Ok(FilterExpr::and_all(parts))
    }

    fn parse_not(&mut self) -> Result<FilterExpr, FilterError> {
        if self.consume_keyword("not") {
            return Ok(FilterExpr::Not(Box::new(self.parse_not()?)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<FilterExpr, FilterError> {
        if self.consume(Token::LParen) {
            let expr = self.parse_or()?;
            self.expect(Token::RParen, "')'")?;
            return Ok(expr);
        }

        let Some(first) = self.consume_value() else {
            return Err(FilterError::InvalidQuery("expected expression".into()));
        };

        if self.is_done()
            || self.peek_keyword("and")
            || self.peek_keyword("or")
            || matches!(
                self.tokens.get(self.cursor),
                Some(Token::RParen | Token::Comma)
            )
        {
            return Ok(FilterExpr::Contains(first));
        }

        if self.consume_keyword("contains") {
            let value = self.expect_value("value after contains")?;
            return field_contains(&first, value);
        }

        if self.consume_keyword("in") {
            self.expect(Token::LParen, "'(' after in")?;
            let mut values = Vec::new();
            loop {
                values.push(self.expect_value("value in set")?);
                if !self.consume(Token::Comma) {
                    break;
                }
            }
            self.expect(Token::RParen, "')' after in set")?;
            return field_in(&first, values);
        }

        if self.consume_keyword("between") {
            let start = self.expect_value("start timestamp")?;
            if !self.consume_keyword("and") {
                return Err(FilterError::InvalidQuery(
                    "expected 'and' in timestamp range".into(),
                ));
            }
            let end = self.expect_value("end timestamp")?;
            return field_between(&first, start, end);
        }

        if let Some(operator) = self.consume_operator() {
            let value = self.expect_value("comparison value")?;
            return field_compare(&first, operator, value);
        }

        Err(FilterError::InvalidQuery(format!(
            "expected operator after '{first}'"
        )))
    }

    fn consume_value(&mut self) -> Option<String> {
        match self.tokens.get(self.cursor) {
            Some(Token::Word(value) | Token::String(value)) => {
                self.cursor += 1;
                Some(value.clone())
            }
            _ => None,
        }
    }

    fn expect_value(&mut self, expected: &str) -> Result<String, FilterError> {
        self.consume_value()
            .ok_or_else(|| FilterError::InvalidQuery(format!("expected {expected}")))
    }

    fn consume_operator(&mut self) -> Option<Token> {
        match self.tokens.get(self.cursor) {
            Some(Token::Eq | Token::NotEq | Token::Gte | Token::Lte | Token::Gt | Token::Lt) => {
                let operator = self.tokens[self.cursor].clone();
                self.cursor += 1;
                Some(operator)
            }
            _ => None,
        }
    }

    fn consume_keyword(&mut self, keyword: &str) -> bool {
        if self.peek_keyword(keyword) {
            self.cursor += 1;
            true
        } else {
            false
        }
    }

    fn peek_keyword(&self, keyword: &str) -> bool {
        matches!(
            self.tokens.get(self.cursor),
            Some(Token::Word(value)) if value.eq_ignore_ascii_case(keyword)
        )
    }

    fn consume(&mut self, token: Token) -> bool {
        if self.tokens.get(self.cursor) == Some(&token) {
            self.cursor += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, token: Token, expected: &str) -> Result<(), FilterError> {
        if self.consume(token) {
            Ok(())
        } else {
            Err(FilterError::InvalidQuery(format!("expected {expected}")))
        }
    }

    fn is_done(&self) -> bool {
        self.cursor >= self.tokens.len()
    }

    fn peek(&self) -> Option<&str> {
        match self.tokens.get(self.cursor) {
            Some(Token::Word(value) | Token::String(value)) => Some(value),
            Some(Token::Eq) => Some("="),
            Some(Token::NotEq) => Some("!="),
            Some(Token::Gte) => Some(">="),
            Some(Token::Lte) => Some("<="),
            Some(Token::Gt) => Some(">"),
            Some(Token::Lt) => Some("<"),
            Some(Token::Comma) => Some(","),
            Some(Token::LParen) => Some("("),
            Some(Token::RParen) => Some(")"),
            None => None,
        }
    }
}

fn field_contains(field: &str, value: String) -> Result<FilterExpr, FilterError> {
    match normalize_field(field).as_str() {
        "message" => Ok(FilterExpr::MessageContains(value)),
        "raw" => Ok(FilterExpr::Contains(value)),
        "level" | "timestamp" | "source" | "source_id" => Err(FilterError::InvalidQuery(format!(
            "'{field}' does not support contains"
        ))),
        _ => Ok(FilterExpr::FieldContains {
            field: field.to_string(),
            value,
        }),
    }
}

fn field_in(field: &str, values: Vec<String>) -> Result<FilterExpr, FilterError> {
    if normalize_field(field) == "level" {
        return Ok(FilterExpr::Or(
            values
                .into_iter()
                .map(|value| parse_level(&value).map(FilterExpr::LevelEquals))
                .collect::<Result<_, _>>()?,
        ));
    }

    Ok(FilterExpr::Or(
        values
            .into_iter()
            .map(|value| {
                field_compare(field, Token::Eq, value)
                    .expect("equality comparison is valid for all fields")
            })
            .collect(),
    ))
}

fn field_between(field: &str, start: String, end: String) -> Result<FilterExpr, FilterError> {
    if normalize_field(field) != "timestamp" {
        return Err(FilterError::InvalidQuery(format!(
            "'{field}' does not support between"
        )));
    }
    Ok(FilterExpr::TimestampBetween {
        start: Some(parse_timestamp(&start)?),
        end: Some(parse_timestamp(&end)?),
    })
}

fn field_compare(field: &str, operator: Token, value: String) -> Result<FilterExpr, FilterError> {
    match normalize_field(field).as_str() {
        "level" => level_compare(operator, value),
        "message" => string_compare(FilterExpr::MessageContains, field, operator, value),
        "raw" => string_compare(FilterExpr::Contains, field, operator, value),
        "source" | "source_id" => source_compare(operator, value),
        "timestamp" => timestamp_compare(operator, value),
        _ => field_value_compare(field, operator, value),
    }
}

fn level_compare(operator: Token, value: String) -> Result<FilterExpr, FilterError> {
    let level = parse_level(&value)?;
    match operator {
        Token::Eq => Ok(FilterExpr::LevelEquals(level)),
        Token::NotEq => Ok(FilterExpr::Not(Box::new(FilterExpr::LevelEquals(level)))),
        Token::Gte => Ok(FilterExpr::LevelAtLeast(level)),
        Token::Gt => next_level(level)
            .map(FilterExpr::LevelAtLeast)
            .ok_or_else(|| FilterError::InvalidQuery("no log level above fatal".into())),
        Token::Lte => Ok(next_level(level)
            .map(|level| FilterExpr::Not(Box::new(FilterExpr::LevelAtLeast(level))))
            .unwrap_or(FilterExpr::All)),
        Token::Lt => Ok(FilterExpr::Not(Box::new(FilterExpr::LevelAtLeast(level)))),
        _ => unreachable!("not an operator"),
    }
}

fn next_level(level: LogLevel) -> Option<LogLevel> {
    match level {
        LogLevel::Trace => Some(LogLevel::Debug),
        LogLevel::Debug => Some(LogLevel::Info),
        LogLevel::Info => Some(LogLevel::Warn),
        LogLevel::Warn => Some(LogLevel::Error),
        LogLevel::Error => Some(LogLevel::Fatal),
        LogLevel::Fatal => None,
    }
}

fn string_compare(
    contains: fn(String) -> FilterExpr,
    field: &str,
    operator: Token,
    value: String,
) -> Result<FilterExpr, FilterError> {
    match operator {
        Token::Eq => Ok(contains(value)),
        Token::NotEq => Ok(FilterExpr::Not(Box::new(contains(value)))),
        _ => Err(FilterError::InvalidQuery(format!(
            "'{field}' only supports =, !=, and contains"
        ))),
    }
}

fn source_compare(operator: Token, value: String) -> Result<FilterExpr, FilterError> {
    let source_id = value
        .parse::<u64>()
        .map(SourceId)
        .map_err(|_| FilterError::InvalidQuery(format!("invalid source id '{value}'")))?;
    match operator {
        Token::Eq => Ok(FilterExpr::Source(source_id)),
        Token::NotEq => Ok(FilterExpr::Not(Box::new(FilterExpr::Source(source_id)))),
        _ => Err(FilterError::InvalidQuery(
            "source only supports = and !=".into(),
        )),
    }
}

fn timestamp_compare(operator: Token, value: String) -> Result<FilterExpr, FilterError> {
    let timestamp = parse_timestamp(&value)?;
    match operator {
        Token::Eq => Ok(FilterExpr::TimestampBetween {
            start: Some(timestamp),
            end: Some(timestamp),
        }),
        Token::NotEq => Ok(FilterExpr::Not(Box::new(FilterExpr::TimestampBetween {
            start: Some(timestamp),
            end: Some(timestamp),
        }))),
        Token::Gte | Token::Gt => Ok(FilterExpr::TimestampBetween {
            start: Some(timestamp),
            end: None,
        }),
        Token::Lte | Token::Lt => Ok(FilterExpr::TimestampBetween {
            start: None,
            end: Some(timestamp),
        }),
        _ => unreachable!("not an operator"),
    }
}

fn field_value_compare(
    field: &str,
    operator: Token,
    value: String,
) -> Result<FilterExpr, FilterError> {
    match operator {
        Token::Eq => Ok(FilterExpr::FieldEquals {
            field: field.to_string(),
            value,
        }),
        Token::NotEq => Ok(FilterExpr::Not(Box::new(FilterExpr::FieldEquals {
            field: field.to_string(),
            value,
        }))),
        _ => Err(FilterError::InvalidQuery(format!(
            "'{field}' only supports =, !=, in, and contains"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ByteRange, ParsedFields, RowId};
    use std::sync::Arc;

    fn mk_row(message: &str, level: Option<LogLevel>, source: SourceId) -> LogRow {
        LogRow {
            row_id: RowId(0),
            source_id: source,
            byte_range: ByteRange {
                start: 0,
                end: message.len() as u64,
            },
            timestamp: None,
            level,
            raw: Arc::from(message),
            message: Arc::from(message),
            fields: ParsedFields::default(),
        }
    }

    fn mk_json_row(message: &str, level: Option<LogLevel>, source: SourceId) -> LogRow {
        let mut row = mk_row(message, level, source);
        row.fields.insert("service", "billing");
        row.fields.insert("userId", "123");
        row
    }

    #[test]
    fn contains_is_case_insensitive() {
        let row = mk_row("Database Timeout", None, SourceId(1));
        let compiled = CompiledFilter::compile(&FilterExpr::Contains("timeout".into())).unwrap();
        assert!(compiled.matches(&row));
    }

    #[test]
    fn contains_matches_raw_json_fields() {
        let row = mk_row(
            r#"{"message":"started","service":"billing"}"#,
            Some(LogLevel::Info),
            SourceId(1),
        );
        let compiled = CompiledFilter::compile(&FilterExpr::Contains("billing".into())).unwrap();
        assert!(compiled.matches(&row));
    }

    #[test]
    fn regex_compile_reports_errors() {
        let err = CompiledFilter::compile(&FilterExpr::Regex("(".into())).unwrap_err();
        assert!(format!("{err}").contains("invalid regex"));
    }

    #[test]
    fn composite_filters_work() {
        let row = mk_row("ERROR timeout", Some(LogLevel::Error), SourceId(9));
        let expr = FilterExpr::And(vec![
            FilterExpr::LevelAtLeast(LogLevel::Warn),
            FilterExpr::Contains("timeout".into()),
            FilterExpr::Source(SourceId(9)),
        ]);
        let compiled = CompiledFilter::compile(&expr).unwrap();
        assert!(compiled.matches(&row));
    }

    #[test]
    fn level_equals_regex_or_and_not_filters_work() {
        let error = mk_row("ERROR timeout", Some(LogLevel::Error), SourceId(1));
        let info = mk_row("INFO started", Some(LogLevel::Info), SourceId(2));

        let equals = CompiledFilter::compile(&FilterExpr::LevelEquals(LogLevel::Error)).unwrap();
        assert!(equals.matches(&error));
        assert!(!equals.matches(&info));

        let regex = CompiledFilter::compile(&FilterExpr::Regex("time(out)?".into())).unwrap();
        assert!(regex.matches(&error));

        let source_or_not = CompiledFilter::compile(&FilterExpr::Or(vec![
            FilterExpr::Source(SourceId(2)),
            FilterExpr::Not(Box::new(FilterExpr::Contains("started".into()))),
        ]))
        .unwrap();
        assert!(source_or_not.matches(&error));
        assert!(source_or_not.matches(&info));
    }

    #[test]
    fn and_all_omits_all_filters() {
        let expr = FilterExpr::and_all([FilterExpr::All, FilterExpr::Contains("db".into())]);
        assert_eq!(expr, FilterExpr::Contains("db".into()));
    }

    #[test]
    fn query_parser_keeps_plain_text_as_contains() {
        let expr = parse_filter_query("timeout").unwrap();
        assert_eq!(expr, FilterExpr::Contains("timeout".into()));
    }

    #[test]
    fn query_parser_supports_boolean_level_and_field_filters() {
        let row = mk_json_row("timeout while charging", Some(LogLevel::Error), SourceId(1));
        let expr = parse_filter_query(
            r#"level in (warn, error) and service = "billing" and json.userId = "123" and message contains "charging""#,
        )
        .unwrap();
        let compiled = CompiledFilter::compile(&expr).unwrap();
        assert!(compiled.matches(&row));
    }

    #[test]
    fn query_parser_supports_source_timestamp_and_not_filters() {
        let mut row = mk_json_row("INFO started", Some(LogLevel::Info), SourceId(7));
        row.timestamp = Some("2026-05-21T10:00:00Z".parse().unwrap());

        let expr = parse_filter_query(
            r#"source = 7 and timestamp between "2026-05-21T09:00:00Z" and "2026-05-21T11:00:00Z" and not level = error"#,
        )
        .unwrap();
        let compiled = CompiledFilter::compile(&expr).unwrap();
        assert!(compiled.matches(&row));
    }

    #[test]
    fn query_parser_reports_invalid_expressions() {
        let err = parse_filter_query("level =").unwrap_err();
        assert!(format!("{err}").contains("invalid query"));
    }
}
