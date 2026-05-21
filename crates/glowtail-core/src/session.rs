use crate::filter::FilterExpr;
use crate::model::RowId;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

const MAX_FILTER_HISTORY: usize = 20;

/// On-disk schema version for [`InvestigationSession`]. Bump when adding
/// non-backwards-compatible fields and add a migration in `migrate_from`.
pub const SESSION_VERSION: u32 = 1;

fn default_session_version() -> u32 {
    SESSION_VERSION
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedFilter {
    pub name: Arc<str>,
    pub filter: FilterExpr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bookmark {
    pub row_id: RowId,
    pub label: Option<Arc<str>>,
}

/// Investigation state persisted to disk by `--session <path>`. The schema is
/// versioned (`version`) and rejects unknown fields so future additions are
/// either explicit migrations or hard errors instead of silent data loss.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvestigationSession {
    #[serde(default = "default_session_version")]
    pub version: u32,
    #[serde(default)]
    pub filter_history: Vec<FilterExpr>,
    #[serde(default)]
    pub saved_filters: Vec<SavedFilter>,
    #[serde(default)]
    pub bookmarks: Vec<Bookmark>,
}

impl Default for InvestigationSession {
    fn default() -> Self {
        Self {
            version: SESSION_VERSION,
            filter_history: Vec::new(),
            saved_filters: Vec::new(),
            bookmarks: Vec::new(),
        }
    }
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

    pub fn save_filter(&mut self, name: impl Into<Arc<str>>, filter: FilterExpr) {
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
            .find(|saved| saved.name.as_ref() == name)
            .map(|saved| &saved.filter)
    }

    pub fn toggle_bookmark(&mut self, row_id: RowId, label: Option<Arc<str>>) -> bool {
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
        let session: Self = serde_json::from_str(&json)?;
        if session.version > SESSION_VERSION {
            return Err(SessionIoError::UnsupportedVersion {
                file_version: session.version,
                supported: SESSION_VERSION,
            });
        }
        Ok(session)
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SessionIoError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error(
        "session was written by a newer glowtail (file version {file_version}, this build supports up to {supported}); upgrade or remove the session file"
    )]
    UnsupportedVersion { file_version: u32, supported: u32 },
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
        assert!(session.toggle_bookmark(RowId(7), Some(Arc::from("interesting"))));
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

        assert_eq!(restored.version, SESSION_VERSION);
        assert_eq!(restored.saved_filters.len(), 1);
        assert!(restored.is_bookmarked(RowId(9)));
    }

    #[test]
    fn pre_versioned_session_defaults_to_version_one() {
        let json = r#"{"filter_history":[],"saved_filters":[],"bookmarks":[]}"#;
        let restored: InvestigationSession = serde_json::from_str(json).unwrap();
        assert_eq!(restored.version, SESSION_VERSION);
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let json =
            r#"{"version":1,"filter_history":[],"saved_filters":[],"bookmarks":[],"extra":1}"#;
        assert!(serde_json::from_str::<InvestigationSession>(json).is_err());
    }

    #[test]
    fn newer_session_version_is_rejected_on_disk_load() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            format!(
                r#"{{"version":{},"filter_history":[],"saved_filters":[],"bookmarks":[]}}"#,
                SESSION_VERSION + 99
            ),
        )
        .unwrap();
        let err = InvestigationSession::load_from_path(tmp.path()).unwrap_err();
        assert!(matches!(err, SessionIoError::UnsupportedVersion { .. }));
    }
}
