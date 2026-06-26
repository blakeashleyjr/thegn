use crate::log::parser::{LogLevel, ParsedLog};

pub enum Filter {
    Text(String),
    Level(LogLevel),
    ExactField(String, String),
    And(Vec<Filter>),
}

impl Filter {
    pub fn matches(&self, log: &ParsedLog) -> bool {
        match self {
            Filter::Text(text) => log.message.contains(text) || log.original.contains(text),
            Filter::Level(level) => log.level == *level, // exact level for now, could be >=
            Filter::ExactField(_key, _val) => {
                // Fields removed for Eq derive simplicity, can add back using BTreeMap
                false
            }
            Filter::And(filters) => filters.iter().all(|f| f.matches(log)),
        }
    }
}
