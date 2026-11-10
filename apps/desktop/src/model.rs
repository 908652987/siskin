use std::cell::RefCell;
use std::collections::VecDeque;

use slint::{Model, ModelTracker};

use crate::MessageRow;

/// Rows materialised around the visible window. The model reports 100_000 rows
/// but only this many exist as owned data at any time (docs/ARCHITECTURE.md §15.2).
const PAGE_ROWS: usize = 50;

/// Synthetic message source for Spike S3. The real UI will replace `row_data`
/// with a page fetched from `mail-core`.
pub struct MessageModel {
    count: usize,
    cache: RefCell<VecDeque<(usize, MessageRow)>>,
    loads: RefCell<u64>,
}

impl MessageModel {
    pub fn new(count: usize) -> Self {
        Self {
            count,
            cache: RefCell::new(VecDeque::with_capacity(PAGE_ROWS)),
            loads: RefCell::new(0),
        }
    }
}

impl Model for MessageModel {
    type Data = MessageRow;

    fn row_count(&self) -> usize {
        self.count
    }

    fn row_data(&self, row: usize) -> Option<MessageRow> {
        if row >= self.count {
            return None;
        }

        if let Some((_, data)) = self.cache.borrow().iter().find(|(index, _)| *index == row) {
            return Some(data.clone());
        }

        *self.loads.borrow_mut() += 1;
        let data = MessageRow {
            subject: format!("Synthetic message {row} — scroll target").into(),
            sender: format!("Sender {}", row % 97).into(),
            date: format!("{:02}:{:02}", 9 + (row / 60) % 12, row % 60).into(),
            unread: row.is_multiple_of(3),
        };

        let mut cache = self.cache.borrow_mut();
        if cache.len() >= PAGE_ROWS {
            cache.pop_front();
        }
        cache.push_back((row, data.clone()));
        Some(data)
    }

    fn set_row_data(&self, _row: usize, _data: MessageRow) {}

    fn model_tracker(&self) -> &dyn ModelTracker {
        &()
    }
}
