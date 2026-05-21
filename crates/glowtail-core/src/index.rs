use crate::model::{LogRow, RowId};

#[derive(Debug, Default)]
pub struct RowIndex {
    rows: Vec<LogRow>,
}

impl RowIndex {
    pub fn append(&mut self, mut row: LogRow) -> RowId {
        let id = RowId(self.rows.len() as u64);
        row.row_id = id;
        self.rows.push(row);
        id
    }

    pub fn get(&self, row_id: RowId) -> Option<&LogRow> {
        self.rows.get(row_id.0 as usize)
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
}
