//! UI-neutral log engine. Parsing, indexing, filtering, tailing, and viewport
//! logic live here; the crate intentionally has no dependency on any UI
//! framework. UI front-ends consume the [`Engine`](viewport::Engine) via the
//! [`prelude`] and translate semantic [`RowPresentation`](model::RowPresentation)
//! spans into their own styling.

pub mod error;
pub mod events;
pub mod filter;
pub mod index;
pub mod model;
pub mod parser;
pub mod session;
pub mod source;
pub mod viewport;

/// Curated re-exports of the dozen types most UI crates need. New imports
/// should prefer this prelude so the API surface has one place to evolve.
pub mod prelude {
    pub use crate::events::LogEvent;
    pub use crate::filter::{
        FilterError, FilterExpr, compose_filter, compose_query_filter, parse_filter_query,
    };
    pub use crate::model::{
        ByteRange, LogLevel, LogRow, RowId, RowPresentation, SeverityRole, SourceId, SpanKind,
        StyledSpan, ViewportRequest, ViewportSnapshot,
    };
    pub use crate::parser::{CompositeParser, JsonLineParser, LogParser, PlainTextParser};
    pub use crate::session::{InvestigationSession, SessionIoError};
    pub use crate::source::FileTailer;
    pub use crate::viewport::Engine;
}
