use crate::model::{LogRow, SourceId};
use std::path::PathBuf;

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum LogEvent {
    SourceAdded {
        source_id: SourceId,
        path: PathBuf,
    },
    SourceRemoved {
        source_id: SourceId,
    },
    RowAppended(LogRow),
    /// A burst of rows read in one pass. The tailer batches rows so the
    /// channel pays one send per read burst instead of one per row;
    /// consumers should handle both this and [`LogEvent::RowAppended`].
    RowsAppended(Vec<LogRow>),
    SourceRotated {
        source_id: SourceId,
    },
    SourceError {
        source_id: SourceId,
        message: String,
    },
}
