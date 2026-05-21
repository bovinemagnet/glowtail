use crate::model::{LogRow, SourceId};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub enum LogEvent {
    SourceAdded {
        source_id: SourceId,
        path: PathBuf,
    },
    SourceRemoved {
        source_id: SourceId,
    },
    RowAppended(LogRow),
    SourceRotated {
        source_id: SourceId,
    },
    SourceError {
        source_id: SourceId,
        message: String,
    },
}
