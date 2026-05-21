use crate::model::{LogRow, RowId};

/// Append-only row store. Every appended row is reassigned a `RowId` equal to
/// its position in the store; callers do not control the resulting id. This
/// keeps `RowId` globally unique even when multiple sources each generate
/// their own monotonic counters from zero, and lets [`position_of`] resolve
/// any `RowId` in O(1) without a side table.
#[derive(Debug, Default)]
pub struct RowIndex {
    // Phase 1 uses Vec-backed append-only storage. The API keeps storage
    // replaceable by chunked rows, mmap offsets, or columnar metadata later.
    rows: Vec<LogRow>,
}

impl RowIndex {
    /// Append a row. The row's `row_id` is overwritten with one matching its
    /// position in the index; the returned id is the canonical id for the
    /// row.
    pub fn append(&mut self, mut row: LogRow) -> RowId {
        let id = RowId(self.rows.len() as u64);
        row.row_id = id;
        self.rows.push(row);
        id
    }

    pub fn get(&self, row_id: RowId) -> Option<&LogRow> {
        self.position_of(row_id)
            .and_then(|position| self.rows.get(position))
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn iter_range(&self, start: usize, count: usize) -> Vec<&LogRow> {
        self.rows.iter().skip(start).take(count).collect()
    }

    /// Resolve a `RowId` to its append-order position. Returns `None` if the
    /// id is out of range. Use this in preference to a raw `row_id.0 as usize`
    /// cast so the invariant has one place to update if it ever changes.
    pub fn position_of(&self, row_id: RowId) -> Option<usize> {
        let position = row_id.0 as usize;
        // The store reassigns row_id == position on append, so this is a
        // direct lookup. Verify defensively so a future deviation breaks tests
        // instead of silently returning the wrong row.
        let row = self.rows.get(position)?;
        if row.row_id == row_id {
            Some(position)
        } else {
            None
        }
    }

    pub fn find_by_row_number(&self, row_number: usize) -> Option<&LogRow> {
        self.rows.get(row_number)
    }

    pub fn rows(&self) -> &[LogRow] {
        &self.rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ByteRange, ParsedFields, SourceId};
    use std::sync::Arc;

    fn mk_row(msg: &str) -> LogRow {
        LogRow {
            row_id: RowId(999),
            source_id: SourceId(1),
            byte_range: ByteRange {
                start: 0,
                end: msg.len() as u64,
            },
            timestamp: None,
            level: None,
            raw: Arc::from(msg),
            message: Arc::from(msg),
            fields: ParsedFields::default(),
        }
    }

    #[test]
    fn append_returns_stable_ids() {
        let mut index = RowIndex::default();
        let a = index.append(mk_row("a"));
        let b = index.append(mk_row("b"));
        assert_eq!(a, RowId(0));
        assert_eq!(b, RowId(1));
        assert_eq!(index.get(a).map(|r| r.message.as_ref()), Some("a"));
    }

    #[test]
    fn iter_range_returns_subset() {
        let mut index = RowIndex::default();
        for n in 0..5 {
            index.append(mk_row(&format!("{n}")));
        }
        let subset = index.iter_range(1, 2);
        assert_eq!(subset.len(), 2);
        assert_eq!(subset[0].message.as_ref(), "1");
        assert_eq!(subset[1].message.as_ref(), "2");
    }

    #[test]
    fn position_of_resolves_canonical_row_ids() {
        let mut index = RowIndex::default();
        let a = index.append(mk_row("a"));
        let b = index.append(mk_row("b"));
        assert_eq!(index.position_of(a), Some(0));
        assert_eq!(index.position_of(b), Some(1));
        assert_eq!(index.position_of(RowId(999)), None);
    }
}
