use crate::model::{LogLevel, LogRow, SourceId};
use regex::Regex;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
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

#[derive(Debug, thiserror::Error)]
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
            Self::Contains(needle) => row.message.to_ascii_lowercase().contains(needle),
            Self::Regex(regex) => regex.is_match(row.message.as_ref()),
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
}
