use crate::log::parser::{ParsedLog, parse_log};
use std::collections::VecDeque;

/// A bounded ring buffer for logs to ensure strictly bounded memory growth.
pub struct LogBuffer {
    buffer: VecDeque<ParsedLog>,
    capacity: usize,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            buffer: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn push(&mut self, line: &str) {
        if self.buffer.len() == self.capacity {
            self.buffer.pop_front();
        }
        self.buffer.push_back(parse_log(line));
    }

    pub fn push_parsed(&mut self, log: ParsedLog) {
        if self.buffer.len() == self.capacity {
            self.buffer.pop_front();
        }
        self.buffer.push_back(log);
    }

    pub fn iter(&self) -> impl Iterator<Item = &ParsedLog> {
        self.buffer.iter()
    }

    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
    }
}
