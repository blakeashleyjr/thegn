//! `git blame --porcelain` parsing — the data the blame view renders and the
//! semantic layer groups by entity. Lives in core (pure, no I/O) so the entity
//! analyzer (`semantic::blame_entities`) can consume it under the coverage gate;
//! the host re-exports `BlameRow`/`parse_blame_porcelain` from its panel module.

/// One annotated line from `git blame --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameRow {
    /// 7-character short SHA of the commit that introduced this line.
    pub sha: String,
    pub author: String,
    /// Unix timestamp of the authoring commit.
    pub date: i64,
    /// 1-based final line number in the file.
    pub lineno: usize,
    /// Raw source line (without the leading tab from porcelain format).
    pub content: String,
}

/// Parse `git blame --porcelain` stdout into a `Vec<BlameRow>`. Lines are
/// returned in file order (ascending `lineno`). Malformed groups are silently
/// skipped — the view degrades gracefully to whatever porcelain returns.
pub fn parse_blame_porcelain(text: &str) -> Vec<BlameRow> {
    let mut rows: Vec<BlameRow> = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(header) = lines.next() {
        // Each group starts with "<40-sha> <orig> <final> [<num-lines>]".
        let parts: Vec<&str> = header.split_whitespace().collect();
        if parts.len() < 3 || parts[0].len() < 7 {
            continue;
        }
        let sha = parts[0][..7].to_string();
        let lineno: usize = parts[2].parse().unwrap_or(0);
        let mut author = String::new();
        let mut date: i64 = 0;
        // Consume metadata lines until we reach the content line (starts with TAB).
        loop {
            match lines.peek() {
                Some(line) if line.starts_with('\t') => {
                    let content = lines.next().unwrap()[1..].to_string();
                    if lineno > 0 {
                        rows.push(BlameRow {
                            sha,
                            author,
                            date,
                            lineno,
                            content,
                        });
                    }
                    break;
                }
                Some(line) if line.starts_with("author ") && !line.starts_with("author-") => {
                    author = lines.next().unwrap()["author ".len()..].to_string();
                }
                Some(line) if line.starts_with("author-time ") => {
                    date = lines.next().unwrap()["author-time ".len()..]
                        .trim()
                        .parse()
                        .unwrap_or(0);
                }
                Some(_) => {
                    lines.next();
                }
                None => break,
            }
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_blame_porcelain_extracts_rows_in_order() {
        let input = "\
abc1234abc1234abc1234abc1234abc1234abc1234 1 1 1\n\
author Alice\n\
author-mail <alice@example.com>\n\
author-time 1700000000\n\
author-tz +0000\n\
committer Alice\n\
committer-mail <alice@example.com>\n\
committer-time 1700000000\n\
committer-tz +0000\n\
summary first commit\n\
filename src/main.rs\n\
\tfirst line\n\
def5678def5678def5678def5678def5678def5678 2 2 1\n\
author Bob\n\
author-mail <bob@example.com>\n\
author-time 1710000000\n\
author-tz +0000\n\
committer Bob\n\
committer-mail <bob@example.com>\n\
committer-time 1710000000\n\
committer-tz +0000\n\
summary second commit\n\
filename src/main.rs\n\
\tsecond line\n\
";
        let rows = parse_blame_porcelain(input);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].sha, "abc1234");
        assert_eq!(rows[0].author, "Alice");
        assert_eq!(rows[0].date, 1700000000);
        assert_eq!(rows[0].lineno, 1);
        assert_eq!(rows[0].content, "first line");
        assert_eq!(rows[1].sha, "def5678");
        assert_eq!(rows[1].author, "Bob");
        assert_eq!(rows[1].lineno, 2);
        assert_eq!(rows[1].content, "second line");
    }

    #[test]
    fn parse_blame_porcelain_skips_malformed_groups() {
        // A header with too-short SHA is skipped; a valid group after it still parses.
        let input = "\
short 1 1 1\n\
author X\n\
author-time 0\n\
\tsome content\n\
abc1234abc1234abc1234abc1234abc1234abc1234 3 3 1\n\
author Valid\n\
author-time 1000\n\
\tvalid line\n\
";
        let rows = parse_blame_porcelain(input);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].author, "Valid");
        assert_eq!(rows[0].lineno, 3);
    }
}
