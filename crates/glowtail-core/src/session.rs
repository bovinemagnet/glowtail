use crate::filter::FilterExpr;
use crate::model::RowId;
use serde::{Deserialize, Serialize};
use std::path::Path;

const MAX_FILTER_HISTORY: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedFilter {
    pub name: String,
    pub filter: FilterExpr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bookmark {
    pub row_id: RowId,
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct InvestigationSession {
    pub filter_history: Vec<FilterExpr>,
    pub saved_filters: Vec<SavedFilter>,
    pub bookmarks: Vec<Bookmark>,
}

impl InvestigationSession {
    pub fn record_filter(&mut self, filter: FilterExpr) {
        if matches!(filter, FilterExpr::All) || self.filter_history.last() == Some(&filter) {
            return;
        }
        self.filter_history.push(filter);
        if self.filter_history.len() > MAX_FILTER_HISTORY {
            self.filter_history.remove(0);
        }
    }

    pub fn save_filter(&mut self, name: impl Into<String>, filter: FilterExpr) {
        let name = name.into();
        if let Some(saved) = self
            .saved_filters
            .iter_mut()
            .find(|saved| saved.name == name)
        {
            saved.filter = filter;
        } else {
            self.saved_filters.push(SavedFilter { name, filter });
        }
    }

    pub fn saved_filter(&self, name: &str) -> Option<&FilterExpr> {
        self.saved_filters
            .iter()
            .find(|saved| saved.name == name)
            .map(|saved| &saved.filter)
    }

    pub fn toggle_bookmark(&mut self, row_id: RowId, label: Option<String>) -> bool {
        if let Some(index) = self
            .bookmarks
            .iter()
            .position(|bookmark| bookmark.row_id == row_id)
        {
            self.bookmarks.remove(index);
            false
        } else {
            self.bookmarks.push(Bookmark { row_id, label });
            self.bookmarks.sort_by_key(|bookmark| bookmark.row_id);
            true
        }
    }

    pub fn is_bookmarked(&self, row_id: RowId) -> bool {
        self.bookmarks
            .iter()
            .any(|bookmark| bookmark.row_id == row_id)
    }

    pub fn save_to_path(&self, path: impl AsRef<Path>) -> Result<(), SessionIoError> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self, SessionIoError> {
        let json = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&json)?)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SessionIoError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LogLevel;

    #[test]
    fn records_bounded_filter_history() {
        let mut session = InvestigationSession::default();
        session.record_filter(FilterExpr::Contains("timeout".into()));
        session.record_filter(FilterExpr::Contains("timeout".into()));
        session.record_filter(FilterExpr::LevelAtLeast(LogLevel::Warn));

        assert_eq!(session.filter_history.len(), 2);
    }

    #[test]
    fn toggles_bookmarks_by_row_id() {
        let mut session = InvestigationSession::default();
        assert!(session.toggle_bookmark(RowId(7), Some("interesting".into())));
        assert!(session.is_bookmarked(RowId(7)));
        assert!(!session.toggle_bookmark(RowId(7), None));
        assert!(!session.is_bookmarked(RowId(7)));
    }

    #[test]
    fn saves_and_loads_json_session() {
        let mut session = InvestigationSession::default();
        session.save_filter("warnings", FilterExpr::LevelAtLeast(LogLevel::Warn));
        session.toggle_bookmark(RowId(9), None);

        let json = serde_json::to_string(&session).unwrap();
        let restored: InvestigationSession = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.saved_filters.len(), 1);
        assert!(restored.is_bookmarked(RowId(9)));
    }
}
