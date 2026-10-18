//! Pure parser and matcher for the Processes tab's filter language.

use std::cmp::Ordering;

/// A term rejected by the query parser or unavailable in the sampled row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::monitor) struct InvalidTerm {
    pub text: String,
    pub reason: &'static str,
}

/// Fields exposed by a process row. The optional accessors allow row types to
/// add data without making the query language depend on the collector.
pub(in crate::monitor) trait QueryRow {
    fn name(&self) -> &str;
    fn pid(&self) -> u32;
    fn owner_keywords(&self) -> &str;
    fn cpu_pct(&self) -> Option<f32>;
    fn rss_bytes(&self) -> Option<u64>;
    fn age_secs(&self) -> Option<u64>;
    fn owner_id(&self) -> Option<u32> {
        None
    }
    fn user(&self) -> Option<&str> {
        None
    }
    fn state(&self) -> Option<char> {
        None
    }
    fn threads(&self) -> Option<u32> {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Contains,
    Greater,
    Less,
    GreaterEqual,
    LessEqual,
    Equal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    User,
    State,
    Cpu,
    Mem,
    Age,
    Threads,
    Pid,
    Owner,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq)]
enum Value {
    Text(String),
    Number(f64),
    States(String),
}

#[derive(Debug, Clone, PartialEq)]
enum Term {
    Bare(String),
    Field { field: Field, op: Op, value: Value },
    Not(Box<Term>),
    Ignored,
}

/// Parsed AND query. Malformed and currently unavailable terms are retained
/// for the header's future error presentation and ignored by matching.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::monitor) struct Query {
    terms: Vec<Term>,
    invalid: Vec<InvalidTerm>,
}

impl Query {
    /// Parse space-separated AND terms. Syntax errors never fail the query.
    pub(in crate::monitor) fn parse(input: &str) -> Self {
        let mut query = Self {
            terms: Vec::new(),
            invalid: Vec::new(),
        };
        for text in input.split_whitespace() {
            let term = query.parse_term(text);
            query.terms.push(term);
        }
        query
    }

    fn parse_term(&mut self, text: &str) -> Term {
        let mut inner = text;
        let mut negated = false;
        while let Some(rest) = inner.strip_prefix('!') {
            negated = !negated;
            inner = rest;
        }
        if inner.is_empty() {
            self.invalid.push(InvalidTerm {
                text: text.to_string(),
                reason: "expected a term after !",
            });
            return Term::Ignored;
        }

        let mut term = self.parse_positive_term(inner);
        if negated && !matches!(term, Term::Ignored) {
            term = Term::Not(Box::new(term));
        }
        term
    }

    fn parse_positive_term(&mut self, text: &str) -> Term {
        let Some((name, op, raw_value)) = split_field(text) else {
            return Term::Bare(text.to_ascii_lowercase());
        };
        if raw_value.is_empty() {
            self.invalid.push(InvalidTerm {
                text: text.to_string(),
                reason: "empty value",
            });
            return Term::Ignored;
        }
        let field = match name.to_ascii_lowercase().as_str() {
            "user" => Field::User,
            "s" => Field::State,
            "cpu" => Field::Cpu,
            "mem" => Field::Mem,
            "age" => Field::Age,
            "thr" => Field::Threads,
            "pid" => Field::Pid,
            "owner" => Field::Owner,
            "wt" | "pane" | "ctr" | "io" => Field::Unavailable,
            _ => {
                self.invalid.push(InvalidTerm {
                    text: text.to_string(),
                    reason: "unknown field",
                });
                return Term::Ignored;
            }
        };
        if field == Field::Unavailable {
            self.invalid.push(InvalidTerm {
                text: text.to_string(),
                reason: "not available yet",
            });
            return Term::Ignored;
        }
        let value = match field {
            Field::Cpu => parse_percent(raw_value).map(Value::Number),
            Field::Mem => parse_size(raw_value).map(Value::Number),
            Field::Age => parse_duration(raw_value).map(Value::Number),
            Field::Pid => raw_value
                .parse::<u32>()
                .ok()
                .map(|n| Value::Number(f64::from(n))),
            Field::Owner | Field::User => Some(Value::Text(raw_value.to_ascii_lowercase())),
            Field::State if matches!(op, Op::Contains | Op::Equal) => {
                Some(Value::States(raw_value.to_ascii_uppercase()))
            }
            Field::Threads => raw_value
                .parse::<u32>()
                .ok()
                .map(|n| Value::Number(f64::from(n))),
            Field::State | Field::Unavailable => None,
        };
        let Some(value) = value else {
            self.invalid.push(InvalidTerm {
                text: text.to_string(),
                reason: "bad value",
            });
            return Term::Ignored;
        };
        // Availability is judged against ProcSample until P-2 adds these fields.
        if matches!(field, Field::User | Field::State | Field::Threads) {
            self.invalid.push(InvalidTerm {
                text: text.to_string(),
                reason: "not available yet",
            });
            return Term::Ignored;
        }
        if field == Field::Owner && !matches!(op, Op::Contains | Op::Equal) {
            self.invalid.push(InvalidTerm {
                text: text.to_string(),
                reason: "operator not supported for field",
            });
            return Term::Ignored;
        }
        Term::Field { field, op, value }
    }

    /// Terms to display as invalid. Invalid terms are ignored by `matches`.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "header display of invalid terms lands in batch 47 (tabs/procs.rs)"
        )
    )]
    pub(in crate::monitor) fn invalid(&self) -> &[InvalidTerm] {
        &self.invalid
    }

    pub(in crate::monitor) fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// Match all valid terms. Invalid terms are treated as true.
    pub(in crate::monitor) fn matches(&self, row: &impl QueryRow) -> bool {
        self.terms
            .iter()
            .all(|term| matches_term(term, row).is_none_or(|matched| matched))
    }
}

fn split_field(text: &str) -> Option<(&str, Op, &str)> {
    let mut earliest: Option<(usize, usize, Op)> = None;
    for (needle, op) in [
        (">=", Op::GreaterEqual),
        ("<=", Op::LessEqual),
        (":", Op::Contains),
        (">", Op::Greater),
        ("<", Op::Less),
        ("=", Op::Equal),
    ] {
        if let Some(position) = text.find(needle)
            && earliest.is_none_or(|(current, len, _)| {
                position < current || (position == current && needle.len() > len)
            })
        {
            earliest = Some((position, needle.len(), op));
        }
    }
    let (position, len, op) = earliest?;
    let name = &text[..position];
    if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return None;
    }
    Some((name, op, &text[position + len..]))
}

fn parse_percent(value: &str) -> Option<f64> {
    let raw = value.strip_suffix('%').unwrap_or(value);
    let parsed = raw.parse::<f64>().ok()?;
    parsed.is_finite().then_some(parsed)
}

fn parse_size(value: &str) -> Option<f64> {
    let split = value
        .find(|ch: char| !ch.is_ascii_digit() && ch != '.')
        .unwrap_or(value.len());
    let (number, suffix) = value.split_at(split);
    let magnitude = number.parse::<f64>().ok()?;
    let multiplier = match suffix.to_ascii_lowercase().as_str() {
        "" => 1_f64,
        "k" | "ki" | "kib" => 1024_f64,
        "m" | "mi" | "mib" => 1024_f64.powi(2),
        "g" | "gi" | "gib" => 1024_f64.powi(3),
        "t" | "ti" | "tib" => 1024_f64.powi(4),
        _ => return None,
    };
    (magnitude * multiplier)
        .is_finite()
        .then_some(magnitude * multiplier)
}

fn parse_duration(value: &str) -> Option<f64> {
    let split = value.find(|ch: char| !ch.is_ascii_digit() && ch != '.')?;
    let (number, suffix) = value.split_at(split);
    let magnitude = number.parse::<f64>().ok()?;
    let multiplier = match suffix.to_ascii_lowercase().as_str() {
        "s" => 1_f64,
        "m" => 60_f64,
        "h" => 3600_f64,
        "d" => 86400_f64,
        _ => return None,
    };
    magnitude.is_finite().then_some(magnitude * multiplier)
}

fn matches_term(term: &Term, row: &impl QueryRow) -> Option<bool> {
    match term {
        Term::Bare(needle) => Some(
            contains_ascii_case_insensitive(row.name(), needle)
                || number_contains(row.pid(), needle)
                || row.owner_keywords().contains(needle)
                || row.owner_id().is_some_and(|id| number_contains(id, needle)),
        ),
        Term::Not(term) => matches_term(term, row).map(|matched| !matched),
        Term::Ignored => None,
        Term::Field { field, op, value } => match (field, value) {
            (Field::Cpu, Value::Number(expected)) => row
                .cpu_pct()
                .map(|actual| compare(f64::from(actual), *expected, *op)),
            (Field::Mem, Value::Number(expected)) => row
                .rss_bytes()
                .map(|actual| compare(actual as f64, *expected, *op)),
            (Field::Age, Value::Number(expected)) => row
                .age_secs()
                .map(|actual| compare(actual as f64, *expected, *op)),
            (Field::Pid, Value::Number(expected)) => {
                Some(compare(f64::from(row.pid()), *expected, *op))
            }
            (Field::Owner, Value::Text(expected)) => match *op {
                Op::Contains => Some(
                    row.owner_keywords().contains(expected)
                        || row
                            .owner_id()
                            .is_some_and(|id| number_contains(id, expected)),
                ),
                Op::Equal => Some(
                    row.owner_keywords() == expected
                        || row.owner_id().is_some_and(|id| number_equals(id, expected)),
                ),
                _ => Some(true),
            },
            (Field::User, Value::Text(expected)) => row
                .user()
                .map(|actual| contains_ascii_case_insensitive(actual, expected)),
            (Field::State, Value::States(states)) => row
                .state()
                .map(|actual| states.contains(actual.to_ascii_uppercase())),
            (Field::Threads, Value::Number(expected)) => row
                .threads()
                .map(|actual| compare(f64::from(actual), *expected, *op)),
            (Field::Unavailable, _) => None,
            _ => Some(true),
        },
    }
}

fn contains_ascii_case_insensitive(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn number_contains(number: u32, needle: &str) -> bool {
    number_matches(number, |text| text.contains(needle))
}

fn number_equals(number: u32, expected: &str) -> bool {
    number_matches(number, |text| text == expected)
}

fn number_matches(number: u32, predicate: impl FnOnce(&str) -> bool) -> bool {
    let mut digits = [0_u8; 10];
    let mut value = number;
    let mut start = digits.len();
    loop {
        start -= 1;
        digits[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    std::str::from_utf8(&digits[start..]).is_ok_and(predicate)
}

fn compare(actual: f64, expected: f64, op: Op) -> bool {
    match op {
        Op::Contains | Op::Equal => actual.total_cmp(&expected) == Ordering::Equal,
        Op::Greater => actual > expected,
        Op::Less => actual < expected,
        Op::GreaterEqual => actual >= expected,
        Op::LessEqual => actual <= expected,
    }
}

#[cfg(test)]
mod tests {
    use super::{Query, QueryRow};

    #[derive(Clone)]
    struct FakeRow {
        name: &'static str,
        pid: u32,
        owner: &'static str,
        cpu: Option<f32>,
        rss: Option<u64>,
        age: Option<u64>,
        user: Option<&'static str>,
        state: Option<char>,
        threads: Option<u32>,
    }

    impl QueryRow for FakeRow {
        fn name(&self) -> &str {
            self.name
        }

        fn pid(&self) -> u32 {
            self.pid
        }

        fn owner_keywords(&self) -> &str {
            self.owner
        }

        fn cpu_pct(&self) -> Option<f32> {
            self.cpu
        }

        fn rss_bytes(&self) -> Option<u64> {
            self.rss
        }

        fn age_secs(&self) -> Option<u64> {
            self.age
        }

        fn user(&self) -> Option<&str> {
            self.user
        }

        fn state(&self) -> Option<char> {
            self.state
        }

        fn threads(&self) -> Option<u32> {
            self.threads
        }
    }

    fn row() -> FakeRow {
        FakeRow {
            name: "Firefox",
            pid: 42,
            owner: "pane",
            cpu: Some(10.0),
            rss: Some(1024_u64.pow(3)),
            age: Some(299),
            user: Some("blake"),
            state: Some('D'),
            threads: Some(64),
        }
    }

    #[test]
    fn parse_reports_unknown_bad_value_empty_value_and_lone_negation() {
        let query = Query::parse("mystery:x mem>2XB user: pid:abc !");
        assert_eq!(query.invalid().len(), 5);
        assert_eq!(query.invalid()[0].reason, "unknown field");
        assert_eq!(query.invalid()[1].reason, "bad value");
        assert_eq!(query.invalid()[2].reason, "empty value");
        assert_eq!(query.invalid()[3].reason, "bad value");
        assert_eq!(query.invalid()[4].reason, "expected a term after !");
        assert!(query.matches(&row()), "invalid terms are ignored");
    }

    #[test]
    fn every_operator_and_value_family_matches_at_its_boundary() {
        let base = row();
        for (query, expected) in [
            ("cpu>10", false),
            ("cpu>=10%", true),
            ("cpu<11", true),
            ("cpu<=10", true),
            ("cpu=10", true),
            ("mem>1G", false),
            ("mem>=1g", true),
            ("mem>512M", true),
            ("mem>10k", true),
            ("age<5m", true),
            ("age<=5m", true),
            ("age>30s", true),
            ("pid:42", true),
            ("pid:420", false),
            ("owner:pane", true),
            ("!firefox", false),
            ("Firefox pane", true),
        ] {
            assert_eq!(Query::parse(query).matches(&base), expected, "{query}");
        }
        assert!(!Query::parse("age<5m").matches(&FakeRow {
            age: Some(300),
            ..base.clone()
        }));
        assert!(Query::parse("age>2h").matches(&FakeRow {
            age: Some(7201),
            ..base.clone()
        }));
        assert!(Query::parse("age<1d").matches(&FakeRow {
            age: Some(86_399),
            ..base
        }));
    }

    #[test]
    fn adding_a_bare_term_never_increases_matches() {
        let rows = [
            row(),
            FakeRow {
                name: "bash",
                pid: 7,
                ..row()
            },
        ];
        for (base, addition) in [("", "fire"), ("pid:42", "fox"), ("cpu>0", "missing")] {
            let query = Query::parse(base);
            let base_matches = rows.iter().filter(|item| query.matches(*item)).count();
            let extended = Query::parse(&format!("{base} {addition}"));
            let extended_matches = rows.iter().filter(|item| extended.matches(*item)).count();
            assert!(extended_matches <= base_matches, "{base} + {addition}");
        }
    }

    #[test]
    fn unavailable_optional_fields_remain_ignored_under_negation() {
        let row = FakeRow {
            threads: None,
            ..row()
        };
        assert!(Query::parse("!thr>50").matches(&row));
    }

    #[test]
    fn large_negation_prefix_is_parsed_without_recursive_nesting() {
        let query = format!("{}firefox", "!".repeat(100_000));
        assert!(Query::parse(&query).matches(&row()));
    }

    #[test]
    fn field_split_uses_earliest_operator_and_bare_sizes_are_bytes() {
        let row = row();
        assert!(
            Query::parse("user:a>=b")
                .invalid()
                .iter()
                .any(|item| item.reason == "not available yet")
        );
        let bytes = Query::parse("mem>=1073741824");
        assert!(bytes.invalid().is_empty());
        assert!(bytes.matches(&row));
        let huge_digits = format!("1{}", "0".repeat(308));
        let huge_size = format!("mem>{huge_digits}T");
        assert!(!Query::parse(&huge_size).invalid().is_empty());
    }

    #[test]
    fn unavailable_field_with_bad_value_reports_only_bad_value() {
        let query = Query::parse("thr>abc");
        assert_eq!(query.invalid().len(), 1);
        assert_eq!(query.invalid()[0].reason, "bad value");

        for query in ["user:blake", "s:D", "thr>50"] {
            let parsed = Query::parse(query);
            assert_eq!(parsed.invalid().len(), 1, "{query}");
            assert_eq!(parsed.invalid()[0].reason, "not available yet", "{query}");
            assert!(parsed.matches(&row()), "{query}");
        }
    }
}
