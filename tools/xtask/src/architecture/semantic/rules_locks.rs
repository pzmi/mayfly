//! `ARCH-migration-frozen` and `ARCH-fixtures-lock` (foundation.md sections 3.3 and 6,
//! `doc/event-store.md` §6.1, verification.md §4): content locks over the files a later build
//! must find exactly as an earlier one left them.
//!
//! - **`migrations.lock`** lists every store migration, `<package>/src/migrations/**.sql` of
//!   every workspace package. A migration is **released once its line is in the lock**: listing
//!   it is the act of freezing it, made in the commit that adds it. Editing a listed migration,
//!   removing or renaming one, or adding one without its line fails. There is no sanctioned way
//!   to change a released line: a schema change is a new migration (expand-only, forward-only).
//! - **`fixtures.lock`** lists every golden fixture, every file under `<package>/tests/fixtures/`
//!   of every workspace package (events, protocol messages, decisions, determinism matrices,
//!   intent digests, the frozen schema, recorded tool output). Changing, removing or adding one
//!   without the matching line fails. Event and protocol fixtures are frozen for ever
//!   (verification.md §4 rule 1): their lines never change. Every other family may be
//!   regenerated deliberately (rule 2): its line is updated in the same commit, whose body
//!   names the fixture and why.
//!
//! Both locks have the `sha256sum` format, so `sha256sum -c` reads them too: one
//! `<64 lowercase hex>  <workspace-relative path>` per line, `#` comments and blank lines
//! allowed, a path listed once. A diagnostic for a new file carries the exact line to add; the
//! checker never writes a lock. A workspace with no covered file needs no lock.
//!
//! What a lock cannot see: a commit that changes a released migration *and* its line together
//! passes this check. That edit is the one the lock makes visible in review (and the review
//! skill refuses it for a migration and for an event or protocol fixture); it is never needed
//! to get green, since a changed file without it fails. Pinned by
//! `should_report_edits_removals_and_unlisted_files_when_a_lock_drifts`,
//! `should_pass_when_the_lock_lists_every_file_by_its_hash`,
//! `should_refuse_a_malformed_or_duplicate_line_when_a_lock_is_read` and the negative fixtures
//! `released-migration-edited`, `migration-without-lock-line` and `frozen-fixture-changed`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::super::report::{Diagnostic, Rule};
use super::{Facts, relative};

/// The migrations lock, at the workspace root.
pub const MIGRATIONS_LOCK: &str = "migrations.lock";
/// The golden fixtures lock, at the workspace root.
pub const FIXTURES_LOCK: &str = "fixtures.lock";

/// One locked family: which lock, which rule, which files.
#[derive(Clone, Copy)]
struct Family {
    lock: &'static str,
    rule: Rule,
    /// The covered directory inside a package.
    dir: &'static str,
    /// Only files with this extension, when set.
    extension: Option<&'static str>,
    /// What a covered file is called in a message.
    noun: &'static str,
    /// What to do about a changed file.
    on_change: &'static str,
    /// Whether a changed file's message carries its new hash: only where updating the line is
    /// sanctioned, so a released migration's message offers no line to paste.
    new_hash: bool,
}

const MIGRATIONS: Family = Family {
    lock: MIGRATIONS_LOCK,
    rule: Rule::MigrationFrozen,
    dir: "src/migrations",
    extension: Some("sql"),
    noun: "released migration",
    on_change: "a released migration is never edited, removed or renamed: change the schema with a new, forward-only migration (foundation.md section 6 rule 6)",
    new_hash: false,
};

const FIXTURES: Family = Family {
    lock: FIXTURES_LOCK,
    rule: Rule::FixturesLock,
    dir: "tests/fixtures",
    extension: None,
    noun: "golden fixture",
    on_change: "event and protocol fixtures are frozen and never change; another family is regenerated only deliberately, with its line updated in the same commit and the reason in the commit body (verification.md section 4)",
    new_hash: true,
};

/// The diagnostics of both lock rules for the workspace at `root` whose packages are `every`.
pub fn check(root: &Path, every: &[Facts]) -> Vec<Diagnostic> {
    let dirs: Vec<&str> = every.iter().map(|f| f.dir.as_str()).collect();
    let mut out = Vec::new();
    for family in [MIGRATIONS, FIXTURES] {
        let files = covered(root, &dirs, family);
        let lock = fs::read_to_string(root.join(family.lock)).ok();
        out.extend(compare(family, lock.as_deref(), &files));
    }
    out
}

/// Every covered file of `family` under the package directories `dirs`, by workspace-relative
/// path, with its SHA-256.
fn covered(root: &Path, dirs: &[&str], family: Family) -> BTreeMap<String, String> {
    let mut paths = Vec::new();
    for dir in dirs {
        let base = if dir.is_empty() {
            root.join(family.dir)
        } else {
            root.join(dir).join(family.dir)
        };
        files_under(&base, &mut paths);
    }
    paths
        .into_iter()
        .filter(|p| {
            family
                .extension
                .is_none_or(|e| p.extension().is_some_and(|x| x == e))
        })
        .filter_map(|p| {
            let bytes = fs::read(&p).ok()?;
            Some((relative(root, &p), sha256_hex(&bytes)))
        })
        .collect()
}

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files_under(&path, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

/// The lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// One parsed lock: path to (hash, line), plus the lines that did not parse or repeat a path.
struct Lock {
    entries: BTreeMap<String, (String, usize)>,
    problems: Vec<(usize, String)>,
}

fn parse(text: &str) -> Lock {
    let mut entries = BTreeMap::new();
    let mut problems = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let trimmed = raw.trim_end();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let parsed = trimmed.split_once("  ").filter(|(hash, path)| {
            hash.len() == 64
                && hash
                    .chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
                && !path.is_empty()
                && !path.starts_with(' ')
        });
        let Some((hash, path)) = parsed else {
            problems.push((
                line,
                "is not `<64 lowercase hex>  <path>` (the sha256sum format)".to_owned(),
            ));
            continue;
        };
        if entries
            .insert(path.to_owned(), (hash.to_owned(), line))
            .is_some()
        {
            problems.push((line, format!("lists `{path}` a second time")));
        }
    }
    Lock { entries, problems }
}

/// The diagnostics of `family` for its lock text (absent: `None`) against the covered `files`.
fn compare(
    family: Family,
    lock: Option<&str>,
    files: &BTreeMap<String, String>,
) -> Vec<Diagnostic> {
    let Some(text) = lock else {
        if files.is_empty() {
            return Vec::new();
        }
        return vec![Diagnostic::new(
            family.rule,
            format!(
                "{} {} file(s) exist but {} is missing: list each as `<sha256>  <path>` (the checker reports each line to add once the file exists)",
                files.len(),
                family.noun,
                family.lock
            ),
            family.lock,
        )];
    };
    let lock = parse(text);
    let mut out: Vec<Diagnostic> = lock
        .problems
        .iter()
        .map(|(line, problem)| {
            Diagnostic::new(
                family.rule,
                format!("{} line {line} {problem}", family.lock),
                format!("{}:{line}", family.lock),
            )
        })
        .collect();
    let listed: BTreeSet<&String> = lock.entries.keys().collect();
    for (path, (hash, line)) in &lock.entries {
        match files.get(path) {
            None => out.push(Diagnostic::new(
                family.rule,
                format!(
                    "{} lists {} `{path}`, which is gone or is not a covered file: {}",
                    family.lock, family.noun, family.on_change
                ),
                format!("{}:{line}", family.lock),
            )),
            Some(actual) if actual != hash => out.push(Diagnostic::new(
                family.rule,
                format!(
                    "{} `{path}` differs from its {} line{}: {}",
                    family.noun,
                    family.lock,
                    if family.new_hash {
                        format!(" (sha256 {actual})")
                    } else {
                        String::new()
                    },
                    family.on_change
                ),
                path.clone(),
            )),
            Some(_) => {}
        }
    }
    for (path, hash) in files {
        if !listed.contains(path) {
            out.push(Diagnostic::new(
                family.rule,
                format!(
                    "`{path}` is not in {}: a new file is listed in the commit that adds it, as the line `{hash}  {path}`",
                    family.lock
                ),
                path.clone(),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(p, content)| ((*p).to_owned(), sha256_hex(content.as_bytes())))
            .collect()
    }

    fn line(path: &str, content: &str) -> String {
        format!("{}  {path}\n", sha256_hex(content.as_bytes()))
    }

    fn rendered(diagnostics: &[Diagnostic]) -> Vec<String> {
        diagnostics.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn should_report_edits_removals_and_unlisted_files_when_a_lock_drifts() {
        // given
        let lock = format!(
            "# migrations\n{}{}",
            line(
                "crates/a/src/migrations/0001.sql",
                "CREATE TABLE t (a TEXT);"
            ),
            line(
                "crates/a/src/migrations/0002.sql",
                "CREATE TABLE u (a TEXT);"
            ),
        );
        let now = files(&[
            (
                "crates/a/src/migrations/0001.sql",
                "CREATE TABLE t (a TEXT, b TEXT);",
            ),
            (
                "crates/a/src/migrations/0003.sql",
                "CREATE TABLE v (a TEXT);",
            ),
        ]);

        // when
        let got = rendered(&compare(MIGRATIONS, Some(&lock), &now));

        // then
        assert_eq!(got.len(), 3, "{got:#?}");
        assert!(got[0].starts_with("ARCH-migration-frozen: released migration `crates/a/src/migrations/0001.sql` differs from its migrations.lock line: a released"), "{got:#?}");
        assert!(
            got[1].contains(
                "lists released migration `crates/a/src/migrations/0002.sql`, which is gone"
            ) && got[1].ends_with("(at migrations.lock:3)"),
            "{got:#?}"
        );
        assert!(
            got[2].contains(&format!(
                "as the line `{}  crates/a/src/migrations/0003.sql`",
                sha256_hex(b"CREATE TABLE v (a TEXT);")
            )),
            "{got:#?}"
        );
    }

    #[test]
    fn should_pass_when_the_lock_lists_every_file_by_its_hash() {
        // given
        let lock = format!(
            "{}\n# a comment\n{}",
            line("crates/c/tests/fixtures/events/x/v1.json", "{}"),
            line("crates/p/tests/fixtures/hello/v1.json", "{\"v\":1}"),
        );
        let now = files(&[
            ("crates/c/tests/fixtures/events/x/v1.json", "{}"),
            ("crates/p/tests/fixtures/hello/v1.json", "{\"v\":1}"),
        ]);

        // when
        let locked = compare(FIXTURES, Some(&lock), &now);
        let none_needed = compare(FIXTURES, None, &BTreeMap::new());
        let missing = rendered(&compare(FIXTURES, None, &now));

        // then
        assert_eq!(rendered(&locked), Vec::<String>::new());
        assert_eq!(rendered(&none_needed), Vec::<String>::new());
        assert_eq!(missing.len(), 1);
        assert!(
            missing[0].starts_with(
                "ARCH-fixtures-lock: 2 golden fixture file(s) exist but fixtures.lock is missing"
            ),
            "{missing:?}"
        );
    }

    #[test]
    fn should_refuse_a_malformed_or_duplicate_line_when_a_lock_is_read() {
        // given
        let good = line("crates/a/tests/fixtures/f.json", "1");
        let lock = format!(
            "{good}{good}ABCDEF  crates/a/tests/fixtures/g.json\n{} crates/a/tests/fixtures/h.json\n",
            sha256_hex(b"2")
        );
        let now = files(&[("crates/a/tests/fixtures/f.json", "1")]);

        // when
        let got = rendered(&compare(FIXTURES, Some(&lock), &now));

        // then
        assert_eq!(
            got.iter()
                .map(|d| d.rsplit("(at ").next().unwrap_or(""))
                .collect::<Vec<_>>(),
            ["fixtures.lock:2)", "fixtures.lock:3)", "fixtures.lock:4)"],
            "{got:#?}"
        );
        assert!(got[0].contains("a second time"));
        assert!(got[1].contains("sha256sum format"));
    }

    #[test]
    fn should_hash_only_covered_files_when_a_workspace_is_scanned() {
        // given
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        for (path, text) in [
            (
                "crates/a/src/migrations/0001_init.sql",
                "CREATE TABLE t (a TEXT);",
            ),
            ("crates/a/src/migrations/README.md", "not a migration"),
            ("crates/a/src/schema.sql", "not under migrations"),
            ("crates/a/tests/fixtures/events/x/v1.json", "{}"),
            ("crates/a/tests/other.rs", "fn main() {}"),
        ] {
            fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
            fs::write(root.join(path), text).unwrap();
        }

        // when
        let migrations = covered(root, &["crates/a"], MIGRATIONS);
        let fixtures = covered(root, &["crates/a"], FIXTURES);

        // then
        assert_eq!(
            migrations.keys().collect::<Vec<_>>(),
            ["crates/a/src/migrations/0001_init.sql"]
        );
        assert_eq!(
            fixtures.keys().collect::<Vec<_>>(),
            ["crates/a/tests/fixtures/events/x/v1.json"]
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
