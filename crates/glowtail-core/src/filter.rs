use crate::model::{LogLevel, LogRow, SourceId};
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
    Regex(String),
    Source(SourceId),
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
    Regex(Regex),
    Source(SourceId),
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

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FilterError {
    #[error("invalid regex '{pattern}': {source}")]
    InvalidRegex {
        pattern: String,
        source: regex::Error,
    },
}

impl CompiledFilter {
    pub fn compile(expr: &FilterExpr) -> Result<Self, FilterError> {
        Ok(match expr {
            FilterExpr::All => Self::All,
            FilterExpr::LevelAtLeast(level) => Self::LevelAtLeast(*level),
            FilterExpr::LevelEquals(level) => Self::LevelEquals(*level),
            FilterExpr::Contains(text) => Self::Contains(text.to_ascii_lowercase()),
            FilterExpr::Regex(pattern) => {
                Self::Regex(
                    Regex::new(pattern).map_err(|source| FilterError::InvalidRegex {
                        pattern: pattern.clone(),
                        source,
                    })?,
                )
            }
            FilterExpr::Source(id) => Self::Source(*id),
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
            Self::Regex(regex) => regex.is_match(row.raw.as_ref()),
            Self::Source(source_id) => row.source_id == *source_id,
            Self::And(items) => items.iter().all(|f| f.matches(row)),
            Self::Or(items) => items.iter().any(|f| f.matches(row)),
            Self::Not(inner) => !inner.matches(row),
        }
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
}
