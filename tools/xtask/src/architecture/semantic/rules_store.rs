//! `ARCH-store-sql` (`doc/event-store.md` §4.3 rule 4, ES-02): the SQLite adapter's statements
//! keep to the SQL both engines accept, so no behaviour of the journal can rest on a form only
//! SQLite has and the PostgreSQL adapter (ES-09) would have to fake. Refused:
//!
//! - `INSERT OR …` (`IGNORE`, `REPLACE`, `ABORT`, `FAIL`, `ROLLBACK`) and `REPLACE INTO`: a
//!   conflict is written as `ON CONFLICT … DO NOTHING`/`DO UPDATE … WHERE`, and never on a
//!   journal table, where it must abort;
//! - `AUTOINCREMENT`, `rowid`, `_rowid_` and `oid`: a commit-ordered position no consumer may
//!   read (ES-I17; `WITHOUT ROWID` tables are allowed, §4.4);
//! - JSON functions and operators (`json…(`, `->`, `->>`): the store never parses a payload.
//!
//! What is read: every string literal of the package's `src/` files (test modules included: a
//! test that shows a forbidden form belongs under `tests/`) and every `.sql` file under `src/`,
//! without their comments. Rust comments and doc comments are not SQL and are skipped.

use std::fs;
use std::path::Path;

use super::super::report::{Diagnostic, Rule};
use super::Facts;

/// The package whose SQL the rule reads.
pub const ADAPTER: &str = "mayfly-adapter-sqlite";

/// One diagnostic per forbidden form in [`ADAPTER`]'s SQL. `root` is the workspace root.
pub fn check(root: &Path, facts: &Facts) -> Vec<Diagnostic> {
    if facts.package != ADAPTER {
        return Vec::new();
    }
    let src = format!("{}/src/", facts.dir);
    let mut texts: Vec<(String, usize, String)> = facts
        .sources
        .iter()
        .filter(|(file, _)| file.starts_with(&src) && file.ends_with(".rs"))
        .flat_map(|(file, text)| {
            string_literals(text)
                .into_iter()
                .map(move |(line, literal)| (file.clone(), line, literal))
        })
        .collect();
    let mut sql_files = Vec::new();
    sql_under(&root.join(&src), &mut sql_files);
    sql_files.sort();
    for path in sql_files {
        if let Ok(text) = fs::read_to_string(&path) {
            let file = super::relative(root, &path);
            texts.extend(
                text.lines()
                    .enumerate()
                    .map(|(n, line)| (file.clone(), n + 1, without_sql_comment(line).to_owned())),
            );
        }
    }
    let mut out = Vec::new();
    for (file, line, text) in texts {
        for form in forbidden(&text) {
            out.push(Diagnostic::new(
                Rule::StoreSql,
                format!(
                    "{ADAPTER} SQL uses `{form}`, a form only SQLite has: keep to the SQL both \
                     store engines accept (doc/event-store.md section 4.3)"
                ),
                format!("{file}:{line}"),
            ));
        }
    }
    out.sort();
    out.dedup();
    out
}

fn sql_under(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            sql_under(&path, out);
        } else if path.extension().is_some_and(|e| e == "sql") {
            out.push(path);
        }
    }
}

fn without_sql_comment(line: &str) -> &str {
    line.split_once("--").map_or(line, |(code, _)| code)
}

/// The forbidden forms in `sql`, as written in the diagnostic.
fn forbidden(sql: &str) -> Vec<&'static str> {
    let upper = sql.to_ascii_uppercase();
    let tokens = tokens(&upper);
    let mut found = Vec::new();
    for (i, token) in tokens.iter().enumerate() {
        let next = tokens.get(i + 1).map(String::as_str);
        let previous = i
            .checked_sub(1)
            .and_then(|p| tokens.get(p))
            .map(String::as_str);
        let form = match (token.as_str(), next) {
            ("INSERT", Some("OR")) => Some("INSERT OR"),
            ("REPLACE", Some("INTO")) => Some("REPLACE INTO"),
            ("AUTOINCREMENT", _) => Some("AUTOINCREMENT"),
            ("ROWID", _) if previous != Some("WITHOUT") => Some("rowid"),
            ("_ROWID_", _) => Some("_rowid_"),
            ("OID", _) => Some("oid"),
            ("->" | "->>", _) => Some("a JSON operator"),
            (word, Some("(")) if word.starts_with("JSON") => Some("a JSON function"),
            _ => None,
        };
        found.extend(form);
    }
    found
}

/// Words (`[A-Z0-9_]+`), the operators `->` and `->>`, and single other characters.
fn tokens(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let chars: Vec<char> = sql.chars().collect();
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        if c.is_ascii_alphanumeric() || c == '_' {
            let start = i;
            while chars
                .get(i)
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
            {
                i += 1;
            }
            out.push(chars[start..i].iter().collect());
        } else if c == '-' && chars.get(i + 1) == Some(&'>') {
            let long = chars.get(i + 2) == Some(&'>');
            out.push(if long { "->>" } else { "->" }.to_owned());
            i += if long { 3 } else { 2 };
        } else {
            if !c.is_whitespace() {
                out.push(c.to_string());
            }
            i += 1;
        }
    }
    out
}

/// Every string literal of Rust source `text` (plain, raw and byte strings), with its first
/// line, outside comments. Character literals and lifetimes are told apart so a `'"'` does
/// not open a string.
fn string_literals(text: &str) -> Vec<(usize, String)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut line = 1;
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        match c {
            '\n' => {
                line += 1;
                i += 1;
            }
            '/' if chars.get(i + 1) == Some(&'/') => {
                while chars.get(i).is_some_and(|c| *c != '\n') {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                    if chars[i] == '\n' {
                        line += 1;
                    }
                    i += 1;
                }
                i += 2;
            }
            '\'' => {
                // A char literal is `'x'` or `'\…'`; anything else is a lifetime or a label.
                if chars.get(i + 1) == Some(&'\\') {
                    i += 2;
                    while chars.get(i).is_some_and(|c| *c != '\'') {
                        i += 1;
                    }
                    i += 1;
                } else if chars.get(i + 2) == Some(&'\'') {
                    i += 3;
                } else {
                    i += 1;
                }
            }
            'r' if matches!(chars.get(i + 1), Some('"' | '#'))
                && (i == 0 || !is_ident(chars[i - 1])) =>
            {
                let mut hashes = 0;
                let mut j = i + 1;
                while chars.get(j) == Some(&'#') {
                    hashes += 1;
                    j += 1;
                }
                if chars.get(j) != Some(&'"') {
                    i += 1;
                    continue;
                }
                let start_line = line;
                let mut literal = String::new();
                j += 1;
                while j < chars.len() {
                    if chars[j] == '"' && (1..=hashes).all(|k| chars.get(j + k) == Some(&'#')) {
                        break;
                    }
                    if chars[j] == '\n' {
                        line += 1;
                    }
                    literal.push(chars[j]);
                    j += 1;
                }
                out.push((start_line, literal));
                i = j + 1 + hashes;
            }
            '"' => {
                let start_line = line;
                let mut literal = String::new();
                i += 1;
                while let Some(&c) = chars.get(i) {
                    match c {
                        '\\' => {
                            if chars.get(i + 1) == Some(&'\n') {
                                line += 1;
                            }
                            literal.push(' ');
                            i += 2;
                        }
                        '"' => break,
                        _ => {
                            if c == '\n' {
                                line += 1;
                            }
                            literal.push(c);
                            i += 1;
                        }
                    }
                }
                out.push((start_line, literal));
                i += 1;
            }
            _ => i += 1,
        }
    }
    out
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_find_sqlite_only_forms_when_sql_uses_them() {
        // given
        let statements = [
            "insert or ignore into t values (1)",
            "INSERT\n   OR REPLACE INTO t VALUES (1)",
            "REPLACE INTO t VALUES (1)",
            "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT)",
            "SELECT rowid FROM t ORDER BY rowid",
            "SELECT _rowid_, oid FROM t",
            "SELECT json_extract(payload, '$.a') FROM t",
            "SELECT payload ->> '$.a' FROM t",
        ];

        // when
        let found: Vec<Vec<&str>> = statements.iter().map(|s| forbidden(s)).collect();

        // then
        assert_eq!(
            found,
            [
                vec!["INSERT OR"],
                vec!["INSERT OR", "REPLACE INTO"],
                vec!["REPLACE INTO"],
                vec!["AUTOINCREMENT"],
                vec!["rowid", "rowid"],
                vec!["_rowid_", "oid"],
                vec!["a JSON function"],
                vec!["a JSON operator"],
            ]
        );
    }

    #[test]
    fn should_pass_portable_sql_when_it_only_looks_alike() {
        // given
        let statements = [
            "INSERT INTO t (a) VALUES (1) ON CONFLICT (a) DO NOTHING",
            "CREATE TABLE t (a TEXT) STRICT, WITHOUT ROWID",
            "SELECT replace(a, 'x', 'y'), json_text FROM t WHERE a = 'or'",
            "SELECT a FROM t WHERE b > -1",
        ];

        // when
        let found: Vec<Vec<&str>> = statements.iter().map(|s| forbidden(s)).collect();

        // then
        assert!(found.iter().all(Vec::is_empty), "{found:?}");
    }

    #[test]
    fn should_read_string_literals_outside_comments_when_rust_source_is_scanned() {
        // given
        let source = "// \"INSERT OR IGNORE\" in a comment\n/// docs: \"rowid\"\nfn f<'a>(x: &'a str) -> char {\n    let _ = \"one\";\n    let _ = r#\"two \"quoted\"\n\"#;\n    let _ = b\"three\\\"\";\n    /* \"four\" */\n    '\"'\n}\n";

        // when
        let literals = string_literals(source);

        // then
        assert_eq!(
            literals,
            [
                (4, "one".to_owned()),
                (5, "two \"quoted\"\n".to_owned()),
                (7, "three ".to_owned()),
            ]
        );
    }
}
