//! `ARCH-journal-sql` (foundation.md section 6 rule 9, `doc/event-store.md` §4.1 and §4.3 rule
//! 4): the store's tables belong to the store adapter. Refused, in every workspace crate outside
//! `[workspace] tool_only` (the checker's own tests spell the forms it refuses):
//!
//! - SQL naming a store table of §4.1 (`stream_head`, `journal_event`, `journal_identity`,
//!   `event_kind_manifest`, `command_receipt`, `inbox`, the delivery, snapshot, projection and
//!   schema tables) anywhere but the store adapter crate (its modules and migrations under
//!   `src/`, its tests under `tests/`): `mayfly-control`, the test kit and composition reach the
//!   journal through the port only, so a second reader of a journal row cannot bypass the store's
//!   rules (receipts, epochs, no global order);
//! - an `UPDATE`, `DELETE FROM`, `TRUNCATE`, `REPLACE INTO` or `INSERT OR REPLACE` of
//!   `journal_event` or `journal_identity` anywhere, the adapter's own `src/` and migrations
//!   included: history is never rewritten (ES-I7). The one exemption is the adapter's `tests/`,
//!   where JC-27 issues those statements to prove the triggers refuse them.
//!
//! A store table, a whole word compared without case, is named in SQL when the text holds a
//! statement keyword (`SELECT`, `INSERT`, `UPDATE`, `DELETE`, `CREATE`, `ALTER`, `DROP`,
//! `TRUNCATE`, `REPLACE`, `WITH`) or the table follows `FROM`, `INTO`, `JOIN`, `UPDATE`, `TABLE`,
//! `ON` or `REFERENCES` (a fragment of a statement): prose such as "the inbox key" is not SQL;
//! every string literal of a crate's Rust sources and every `.sql` file under its `src/` and
//! `tests/` is read. A constraint name (`journal_identity_pkey`) is another word, so error
//! classification by name stays outside the rule. Limit: SQL assembled at run time from pieces
//! none of which names a table. ES-09 adds the PostgreSQL adapter to [`STORE_ADAPTERS`]. Pinned
//! by `should_refuse_store_tables_outside_the_adapter_when_sql_names_them`,
//! `should_refuse_history_rewrites_when_not_in_the_adapter_tests` and the negative fixtures
//! `journal-sql-outside-adapter` and `journal-event-delete`.

use std::path::Path;

use super::super::policy::Policy;
use super::super::report::{Diagnostic, Rule};
use super::Facts;
use super::rules_order::sql_texts;
use super::rules_store::tokens;

/// The crates that own the store's tables.
pub const STORE_ADAPTERS: &[&str] = &["mayfly-adapter-sqlite"];

/// The tables of `doc/event-store.md` §4.1, upper case.
const STORE_TABLES: &[&str] = &[
    "STREAM_HEAD",
    "JOURNAL_IDENTITY",
    "JOURNAL_EVENT",
    "EVENT_KIND_MANIFEST",
    "COMMAND_RECEIPT",
    "INBOX",
    "DELIVERY_CONSUMER",
    "STREAM_DISPATCH",
    "CONSUMER_CHECKPOINT",
    "CONSUMER_LEASE",
    "AGGREGATE_SNAPSHOT",
    "SNAPSHOT_REQUEST",
    "PROJECTION_ROW",
    "PROJECTION_META",
    "SCHEMA_META",
];

/// The tables whose rows are history.
const HISTORY: &[&str] = &["JOURNAL_EVENT", "JOURNAL_IDENTITY"];

/// Keywords that make a text a statement.
const STATEMENTS: &[&str] = &[
    "SELECT", "INSERT", "UPDATE", "DELETE", "CREATE", "ALTER", "DROP", "TRUNCATE", "REPLACE",
    "WITH",
];

/// Keywords that, right before a table name, make a fragment SQL.
const BEFORE_TABLE: &[&str] = &[
    "FROM",
    "INTO",
    "JOIN",
    "UPDATE",
    "TABLE",
    "ON",
    "REFERENCES",
];

/// The diagnostics of this rule for one package. `root` is the workspace root.
pub fn check(root: &Path, facts: &Facts, policy: &Policy) -> Vec<Diagnostic> {
    let tool = policy
        .workspace
        .tool_only
        .as_ref()
        .is_some_and(|tools| tools.contains(&facts.package));
    if tool {
        return Vec::new();
    }
    let adapter = STORE_ADAPTERS.contains(&facts.package.as_str());
    let tests = if facts.dir.is_empty() {
        "tests/".to_owned()
    } else {
        format!("{}/tests/", facts.dir)
    };
    let mut out = Vec::new();
    for (file, line, text) in sql_texts(root, facts) {
        let found = store_sql(&text);
        if !adapter {
            for table in &found.tables {
                out.push(Diagnostic::new(
                    Rule::JournalSql,
                    format!(
                        "{} names the store table `{}` in SQL: only the store adapter ({}) reads or writes the store's tables; everything else goes through the journal port (doc/event-store.md section 4.3)",
                        facts.package,
                        table.to_ascii_lowercase(),
                        STORE_ADAPTERS.join(", ")
                    ),
                    format!("{file}:{line}"),
                ));
            }
        }
        if !(adapter && file.starts_with(&tests)) {
            for (statement, table) in &found.rewrites {
                out.push(Diagnostic::new(
                    Rule::JournalSql,
                    format!(
                        "{} SQL rewrites history: `{statement} {}`; journal rows are never updated or deleted (ES-I7, foundation.md section 6 rule 9)",
                        facts.package,
                        table.to_ascii_lowercase()
                    ),
                    format!("{file}:{line}"),
                ));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// What one SQL text names: store tables, and statements rewriting history.
#[derive(Debug, Default, PartialEq, Eq)]
struct StoreSql {
    tables: Vec<String>,
    rewrites: Vec<(&'static str, String)>,
}

fn store_sql(sql: &str) -> StoreSql {
    let tokens = tokens(&sql.to_ascii_uppercase());
    let mut found = StoreSql::default();
    let statement = tokens.iter().any(|t| STATEMENTS.contains(&t.as_str()));
    for (i, token) in tokens.iter().enumerate() {
        let after_keyword = i
            .checked_sub(1)
            .and_then(|p| tokens.get(p))
            .is_some_and(|p| BEFORE_TABLE.contains(&p.as_str()));
        if STORE_TABLES.contains(&token.as_str())
            && (statement || after_keyword)
            && !found.tables.contains(token)
        {
            found.tables.push(token.clone());
        }
        let at = |k: usize| tokens.get(i + k).map(String::as_str);
        let target = match (token.as_str(), at(1), at(2), at(3)) {
            ("UPDATE", Some("OR"), _, Some(t)) => Some(("UPDATE", t)),
            ("UPDATE", Some(t), _, _) => Some(("UPDATE", t)),
            ("DELETE", Some("FROM"), Some(t), _) => Some(("DELETE FROM", t)),
            ("TRUNCATE", Some("TABLE"), Some(t), _) | ("TRUNCATE", Some(t), _, _) => {
                Some(("TRUNCATE", t))
            }
            ("REPLACE", Some("INTO"), Some(t), _)
                if i == 0 || tokens.get(i - 1).is_none_or(|p| p != "OR") =>
            {
                Some(("REPLACE INTO", t))
            }
            ("INSERT", Some("OR"), Some("REPLACE"), Some("INTO")) => tokens
                .get(i + 4)
                .map(|t| ("INSERT OR REPLACE INTO", t.as_str())),
            _ => None,
        };
        if let Some((statement, table)) = target
            && HISTORY.contains(&table)
        {
            found.rewrites.push((statement, table.to_owned()));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::super::testing::{facts_for, policy, rendered};
    use super::*;

    #[test]
    fn should_refuse_store_tables_outside_the_adapter_when_sql_names_them() {
        // given
        let source = "pub const READ: &str = \"SELECT payload FROM journal_event WHERE stream_id = ?1\";\npub const DEDUPE: &str = \"select 1 from inbox where source = ?1\";\npub const PROSE: &str = \"the inbox key of a message from GitHub\";\npub const FRAGMENT: &str = \" FROM command_receipt WHERE command_id = ?1\";\npub const NAME: &str = \"journal_identity_pkey\";\n";

        // when
        let control = rendered(&check(
            Path::new("."),
            &facts_for("mayfly-control", &[("src/lib.rs", source)]),
            &policy(),
        ));
        let adapter = check(
            Path::new("."),
            &facts_for("mayfly-adapter-sqlite", &[("src/lib.rs", source)]),
            &policy(),
        );
        let tool = check(
            Path::new("."),
            &facts_for("xtask", &[("src/lib.rs", source)]),
            &policy(),
        );

        // then
        assert_eq!(control.len(), 3, "{control:#?}");
        assert!(
            control[0].contains("names the store table `command_receipt`")
                && control[0].ends_with("src/lib.rs:4)"),
            "{control:#?}"
        );
        assert!(
            control[1].contains("`inbox`") && control[1].ends_with("src/lib.rs:2)"),
            "{control:#?}"
        );
        assert!(
            control[2].contains("`journal_event`") && control[2].ends_with("src/lib.rs:1)"),
            "{control:#?}"
        );
        assert_eq!(rendered(&adapter), Vec::<String>::new());
        assert_eq!(rendered(&tool), Vec::<String>::new());
    }

    #[test]
    fn should_refuse_history_rewrites_when_not_in_the_adapter_tests() {
        // given
        let statements = [
            "UPDATE journal_event SET payload = '{}'",
            "DELETE FROM journal_identity WHERE event_id = ?1",
            "TRUNCATE TABLE journal_event",
            "INSERT OR REPLACE INTO journal_event VALUES (1)",
            "CREATE TRIGGER t BEFORE UPDATE ON journal_event BEGIN SELECT 1; END",
            "UPDATE stream_head SET revision = ?1 WHERE stream_id = ?2",
            "DELETE FROM stream_dispatch WHERE consumer = ?1",
        ];
        let jc27 = "pub const JC27: &str = \"DELETE FROM journal_event\";\n";

        // when
        let found: Vec<Vec<(&str, String)>> =
            statements.iter().map(|s| store_sql(s).rewrites).collect();
        let in_src = rendered(&check(
            Path::new("."),
            &facts_for("mayfly-adapter-sqlite", &[("src/lib.rs", jc27)]),
            &policy(),
        ));
        let mut facts = facts_for(
            "mayfly-adapter-sqlite",
            &[("src/lib.rs", "pub fn f() {}\n")],
        );
        facts.sources.insert(
            "crates/mayfly-adapter-sqlite/tests/journal_contract.rs".to_owned(),
            jc27.to_owned(),
        );
        let in_tests = check(Path::new("."), &facts, &policy());

        // then
        assert_eq!(
            found,
            [
                vec![("UPDATE", "JOURNAL_EVENT".to_owned())],
                vec![("DELETE FROM", "JOURNAL_IDENTITY".to_owned())],
                vec![("TRUNCATE", "JOURNAL_EVENT".to_owned())],
                vec![("INSERT OR REPLACE INTO", "JOURNAL_EVENT".to_owned())],
                vec![],
                vec![],
                vec![],
            ]
        );
        assert_eq!(in_src.len(), 1, "{in_src:#?}");
        assert!(
            in_src[0].contains("rewrites history: `DELETE FROM journal_event`"),
            "{in_src:#?}"
        );
        assert_eq!(rendered(&in_tests), Vec::<String>::new());
    }
}
