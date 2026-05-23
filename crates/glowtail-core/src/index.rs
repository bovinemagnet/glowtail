use crate::model::{LogRow, RowId};

/// Append-only row store. Every appended row is reassigned a `RowId` that
/// equals `evicted_count + rows.len()` at append time — i.e. a globally
/// monotonic counter that survives eviction. Resolving a `RowId` to its
/// current Vec position therefore subtracts `evicted_count`; `RowId`s for
/// evicted rows return `None` and stay stable in the session/bookmarks.
#[derive(Debug, Default)]
pub struct RowIndex {
    // Phase 1 uses Vec-backed append-only storage. The API keeps storage
    // replaceable by chunked rows, mmap offsets, or columnar metadata later.
    rows: Vec<LogRow>,
    /// Number of rows that have been evicted from the front of `rows`. Added
    /// to the Vec position to derive a stable monotonic `RowId`.
    evicted: u64,
}

impl RowIndex {
    /// Append a row. The row's `row_id` is overwritten with the next
    /// monotonic id (`evicted + rows.len()`); the returned id is the
    /// canonical id for the row.
    pub fn append(&mut self, mut row: LogRow) -> RowId {
        let id = RowId(self.evicted + self.rows.len() as u64);
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

    /// Resolve a `RowId` to its current Vec position. Returns `None` if the
    /// row has been evicted or never existed.
    pub fn position_of(&self, row_id: RowId) -> Option<usize> {
        if row_id.0 < self.evicted {
            return None;
        }
        let position = (row_id.0 - self.evicted) as usize;
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

    /// Drop the oldest `n` rows from the front of the store and return how
    /// many were actually evicted (capped at the current row count). The
    /// monotonic `RowId` counter is preserved so bookmarks and saved
    /// references remain meaningful (they just no longer resolve to a row).
    pub fn evict_oldest(&mut self, n: usize) -> usize {
        let to_evict = n.min(self.rows.len());
        if to_evict == 0 {
            return 0;
        }
        self.rows.drain(0..to_evict);
        self.evicted += to_evict as u64;
        to_evict
    }

    /// Total number of rows evicted across the lifetime of this index.
    /// UIs can surface this so users see history has been truncated.
    pub fn evicted_count(&self) -> u64 {
        self.evicted
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

    #[test]
    fn evict_oldest_drops_front_and_preserves_row_id_monotonicity() {
        // Review perf P2: evicted rows return None from position_of but the
        // monotonic RowId counter must not reset — subsequent appends get
        // ids past the evicted range so bookmarks/saved RowIds stay unique.
        let mut index = RowIndex::default();
        let a = index.append(mk_row("a")); // RowId(0)
        let b = index.append(mk_row("b")); // RowId(1)
        let c = index.append(mk_row("c")); // RowId(2)
        assert_eq!(index.evict_oldest(2), 2);
        assert_eq!(index.evicted_count(), 2);

        // Evicted ids no longer resolve.
        assert!(index.position_of(a).is_none());
        assert!(index.position_of(b).is_none());
        // Surviving row now sits at Vec position 0.
        assert_eq!(index.position_of(c), Some(0));

        // Next append gets RowId(3), not RowId(1) — counter survives eviction.
        let d = index.append(mk_row("d"));
        assert_eq!(d, RowId(3));
        assert_eq!(index.position_of(d), Some(1));
    }

    #[test]
    fn evict_oldest_capped_at_current_row_count() {
        let mut index = RowIndex::default();
        index.append(mk_row("a"));
        index.append(mk_row("b"));
        // Asking to evict more than exists evicts only what's there.
        assert_eq!(index.evict_oldest(99), 2);
        assert_eq!(index.len(), 0);
        assert_eq!(index.evicted_count(), 2);
    }
}
