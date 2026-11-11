//! Parser for the line-oriented `wt new --batch` input file.

use std::fmt;

const MAX_LANES: usize = 64;

/// One requested lane from a batch file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchLane {
    pub name: String,
    pub base: Option<String>,
    pub line: usize,
}

/// A malformed batch file, retaining the source line where possible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchError {
    Empty,
    TooMany {
        line: usize,
        max: usize,
    },
    Duplicate {
        line: usize,
        first: usize,
        name: String,
    },
    ExtraToken {
        line: usize,
    },
}

impl fmt::Display for BatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("batch file contains no lanes"),
            Self::TooMany { line, max } => {
                write!(f, "line {line}: batch file has more than {max} lanes")
            }
            Self::Duplicate { line, first, name } => write!(
                f,
                "line {line}: duplicate lane name {name:?} (first used on line {first})"
            ),
            Self::ExtraToken { line } => {
                write!(f, "line {line}: expected a lane name and optional base")
            }
        }
    }
}

impl std::error::Error for BatchError {}

/// Parse a UTF-8 batch file. Blank lines and comments are ignored; each
/// remaining line contains `<name>` or `<name> <base>`.
pub fn parse_batch(text: &str) -> Result<Vec<BatchLane>, BatchError> {
    let mut lanes = Vec::new();
    let mut names = std::collections::HashMap::<&str, usize>::new();

    for (line_index, line) in text.lines().enumerate() {
        let line_number = line_index + 1;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let mut fields = line.split_whitespace();
        let name = fields.next().expect("non-empty line has a field");
        let base = fields.next().map(str::to_string);
        if fields.next().is_some() {
            return Err(BatchError::ExtraToken { line: line_number });
        }
        if let Some(first) = names.insert(name, line_number) {
            return Err(BatchError::Duplicate {
                line: line_number,
                first,
                name: name.to_string(),
            });
        }
        if lanes.len() == MAX_LANES {
            return Err(BatchError::TooMany {
                line: line_number,
                max: MAX_LANES,
            });
        }
        lanes.push(BatchLane {
            name: name.to_string(),
            base,
            line: line_number,
        });
    }

    if lanes.is_empty() {
        return Err(BatchError::Empty);
    }
    Ok(lanes)
}

#[cfg(test)]
mod tests {
    use super::{BatchError, parse_batch};

    #[test]
    fn parses_comments_blanks_bases_and_crlf() {
        assert_eq!(
            parse_batch("\r\n # ignored\r\nalpha\r\nbeta release/1\r\n").unwrap(),
            vec![
                super::BatchLane {
                    name: "alpha".into(),
                    base: None,
                    line: 3,
                },
                super::BatchLane {
                    name: "beta".into(),
                    base: Some("release/1".into()),
                    line: 4,
                },
            ]
        );
    }

    #[test]
    fn rejects_empty_duplicate_extra_and_too_many() {
        assert_eq!(parse_batch("# only\n\n").unwrap_err(), BatchError::Empty);
        assert!(matches!(
            parse_batch("one\ntwo\none\n"),
            Err(BatchError::Duplicate {
                line: 3,
                first: 1,
                ..
            })
        ));
        assert_eq!(
            parse_batch("one base extra\n").unwrap_err(),
            BatchError::ExtraToken { line: 1 }
        );

        let many = (0..65)
            .map(|index| format!("lane-{index}\n"))
            .collect::<String>();
        assert!(matches!(
            parse_batch(&many),
            Err(BatchError::TooMany { line: 65, max: 64 })
        ));
    }

    #[test]
    fn displays_every_batch_error_variant() {
        assert_eq!(
            BatchError::Empty.to_string(),
            "batch file contains no lanes"
        );
        assert_eq!(
            BatchError::TooMany { line: 65, max: 64 }.to_string(),
            "line 65: batch file has more than 64 lanes"
        );
        assert_eq!(
            BatchError::Duplicate {
                line: 4,
                first: 2,
                name: "lane".into(),
            }
            .to_string(),
            "line 4: duplicate lane name \"lane\" (first used on line 2)"
        );
        assert_eq!(
            BatchError::ExtraToken { line: 7 }.to_string(),
            "line 7: expected a lane name and optional base"
        );
    }
}
