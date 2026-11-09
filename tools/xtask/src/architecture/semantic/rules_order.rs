//! `ARCH-no-global-order` (ES-I17, `doc/event-store.md` §2.2 and §4.3 rule 1): nothing reads the
//! journal in a global order. What the checker can see of that, and refuses:
//!
//! 1. **Ordering in store SQL.** In a journal adapter crate (`mayfly-adapter-*`: every string
//!    literal of its sources, `src/` and `tests/`, and every `.sql` file under `src/` and
//!    `tests/`), an `ORDER BY` key that names
//!    - a row position: `rowid`, `_rowid_`, `oid`, `ctid`;
//!    - a time: a column ending in `_at` or `_until` (`recorded_at`, `lease_until`), `now()`,
//!      `current_timestamp`, `clock_timestamp`;
//!    - a global position: `sequence`, `seq`, `position`, `global_position` or a column ending
//!      in `_seq`, `_sequence` or `_position`;
//!    - `revision` across streams: unless an earlier key of the same `ORDER BY` is `stream_id`
//!      (key order, then revision within a stream) or the statement pins one stream with
//!      `stream_id =` before the `ORDER BY`.
//!
//!    The dispatch, lease and checkpoint SQL orders by key (`stream_id`) only, as §4.3 asks.
//! 2. **A global read in the API.** In a control-plane or both-planes crate, any function or
//!    method (declared, defined or required by a trait, production or test code) whose name
//!    contains `read_all`, `all_events`, `global_position`, `global_sequence` or `global_order`.
//! 3. **Events of several streams from one call.** In the production code of a control-plane
//!    or both-planes crate (`src/`, outside `#[cfg(test)]` and `#[test]` items; the test kit's
//!    journals included), a function or trait method whose return type names `RecordedEvent`
//!    without a parameter of type `StreamId` or `&StreamId` (a slice or a collection of ids
//!    does not count): such a call has no one stream to bound what it returns (§3.3: "no method
//!    returns events of more than one stream").
//!
//! Limits, kept by review and by the port's shape: SQL built at run time from fragments that are
//! not one literal; an aggregate (`max(recorded_at)`) or a `WHERE` on time feeding a decision;
//! an order taken in Rust (`sort_by_key(|e| e.recorded_at)`), which §5.6 allows for display only;
//! a global read under a name outside the list. SQL outside the store adapters cannot name a
//! journal table at all (`ARCH-journal-sql`). Pinned by
//! `should_refuse_global_orderings_when_store_sql_sorts_by_position_time_or_revision`,
//! `should_allow_key_and_single_stream_orderings_when_store_sql_sorts`,
//! `should_refuse_global_reads_when_an_api_returns_events_of_several_streams` and the negative
//! fixtures `global-order-by-recorded-at` and `read-all-port-method`.

use std::path::Path;

use syn::visit::{self, Visit};
use syn::{ImplItemFn, ItemFn, ItemImpl, ItemMod, ItemTrait, ReturnType, Signature, TraitItemFn};

use super::super::policy::Policy;
use super::super::report::{Diagnostic, Rule};
use super::rules_store::{sql_under, string_literals, tokens, without_sql_comment};
use super::{Facts, is_test_only, relative};

/// The prefix of the journal adapter crates, whose SQL is read.
const ADAPTER_PREFIX: &str = "mayfly-adapter-";
/// Function-name fragments of a global read.
const GLOBAL_READS: &[&str] = &[
    "read_all",
    "all_events",
    "global_position",
    "global_sequence",
    "global_order",
];

/// The diagnostics of this rule for one package. `root` is the workspace root.
pub fn check(root: &Path, facts: &Facts, policy: &Policy) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    if facts.package.starts_with(ADAPTER_PREFIX) {
        for (file, line, text) in sql_texts(root, facts) {
            for reason in global_orderings(&text) {
                out.push(Diagnostic::new(
                    Rule::NoGlobalOrder,
                    format!(
                        "{} SQL orders by {reason}: the journal has no global order, so a query orders by key (`stream_id`) or by revision within one stream (doc/event-store.md section 4.3 rule 1, ES-I17)",
                        facts.package
                    ),
                    format!("{file}:{line}"),
                ));
            }
        }
    }
    let selected = policy
        .planes
        .control
        .iter()
        .chain(&policy.planes.both)
        .any(|name| *name == facts.package);
    if selected {
        let src = if facts.dir.is_empty() {
            "src/".to_owned()
        } else {
            format!("{}/src/", facts.dir)
        };
        for (path, text) in &facts.sources {
            let Ok(file) = syn::parse_file(text) else {
                continue;
            };
            let test_file = !path.starts_with(&src) || is_test_only(&file.attrs);
            let mut scan = Scan {
                package: &facts.package,
                file: path,
                tests: usize::from(test_file),
                out: &mut out,
            };
            scan.visit_file(&file);
        }
    }
    out
}

/// Every string literal of the package's Rust sources and every line of its `.sql` files under
/// `src/` and `tests/`, with file and line; SQL comments removed.
pub(super) fn sql_texts(root: &Path, facts: &Facts) -> Vec<(String, usize, String)> {
    let mut texts: Vec<(String, usize, String)> = facts
        .sources
        .iter()
        .filter(|(file, _)| file.ends_with(".rs"))
        .flat_map(|(file, text)| {
            string_literals(text)
                .into_iter()
                .map(move |(line, literal)| (file.clone(), line, literal))
        })
        .collect();
    let mut sql_files = Vec::new();
    for sub in ["src", "tests"] {
        let dir = if facts.dir.is_empty() {
            root.join(sub)
        } else {
            root.join(&facts.dir).join(sub)
        };
        sql_under(&dir, &mut sql_files);
    }
    sql_files.sort();
    for path in sql_files {
        if let Ok(text) = std::fs::read_to_string(&path) {
            let file = relative(root, &path);
            let body: Vec<&str> = text.lines().map(without_sql_comment).collect();
            // One statement may span lines: the whole file is one text, reported at the line of
            // its first `ORDER`.
            let joined = body.join("\n");
            let line = body
                .iter()
                .position(|l| l.to_ascii_uppercase().contains("ORDER"))
                .map_or(1, |i| i + 1);
            texts.push((file, line, joined));
        }
    }
    texts
}

/// What each `ORDER BY` of `sql` orders by that ES-I17 forbids, as written in the diagnostic.
fn global_orderings(sql: &str) -> Vec<String> {
    let tokens = tokens(&sql.to_ascii_uppercase());
    let mut found = Vec::new();
    let mut i = 0;
    while i + 1 < tokens.len() {
        if tokens[i] != "ORDER" || tokens[i + 1] != "BY" {
            i += 1;
            continue;
        }
        let statement = tokens[..i]
            .iter()
            .rposition(|t| t == ";")
            .map_or(0, |end| end + 1);
        let pinned = tokens[statement..i]
            .windows(2)
            .any(|w| w[0] == "STREAM_ID" && w[1] == "=");
        let keys = order_keys(&tokens[i + 2..]);
        let mut by_stream = false;
        for key in &keys {
            for token in key {
                if let Some(reason) = forbidden_key(token, by_stream || pinned) {
                    found.push(reason);
                }
            }
            by_stream |= key.first().is_some_and(|t| t == "STREAM_ID");
        }
        i += 2;
    }
    found.dedup();
    found
}

/// The keys of one `ORDER BY` list (the tokens after `BY`), split at top-level commas and ended
/// by a clause keyword, a closing parenthesis of an enclosing group, `;` or the end.
fn order_keys(rest: &[String]) -> Vec<Vec<String>> {
    const ENDS: &[&str] = &[
        "LIMIT",
        "OFFSET",
        "UNION",
        "EXCEPT",
        "INTERSECT",
        "FOR",
        "RETURNING",
        "FETCH",
        "WINDOW",
        "ROWS",
        "RANGE",
        "GROUPS",
        ";",
    ];
    let mut keys = vec![Vec::new()];
    let mut depth = 0_usize;
    for token in rest {
        match token.as_str() {
            "(" => depth += 1,
            ")" if depth == 0 => break,
            ")" => depth -= 1,
            "," if depth == 0 => {
                keys.push(Vec::new());
                continue;
            }
            t if depth == 0 && ENDS.contains(&t) => break,
            _ => {}
        }
        if let Some(key) = keys.last_mut() {
            key.push(token.clone());
        }
    }
    keys.retain(|k| !k.is_empty());
    keys
}

/// Why ordering by `token` is a global order, if it is. `within_stream`: a revision orders
/// events of one stream (the statement pins one, or an earlier key is `stream_id`).
fn forbidden_key(token: &str, within_stream: bool) -> Option<String> {
    let position = ["SEQUENCE", "SEQ", "POSITION", "GLOBAL_POSITION"];
    let time = ["NOW", "CURRENT_TIMESTAMP", "CLOCK_TIMESTAMP"];
    match token {
        "ROWID" | "_ROWID_" | "OID" | "CTID" => {
            Some(format!("`{}` (a row position)", token.to_ascii_lowercase()))
        }
        t if time.contains(&t) || t.ends_with("_AT") || t.ends_with("_UNTIL") => {
            Some(format!("`{}` (a time)", t.to_ascii_lowercase()))
        }
        t if position.contains(&t)
            || t.ends_with("_SEQ")
            || t.ends_with("_SEQUENCE")
            || t.ends_with("_POSITION") =>
        {
            Some(format!("`{}` (a global position)", t.to_ascii_lowercase()))
        }
        "REVISION" if !within_stream => Some(
            "`revision` across streams (no `stream_id =` and no `stream_id` key before it)"
                .to_owned(),
        ),
        _ => None,
    }
}

struct Scan<'a> {
    package: &'a str,
    file: &'a str,
    /// Depth of test-only scopes being visited (the file itself counts as one).
    tests: usize,
    out: &'a mut Vec<Diagnostic>,
}

impl Scan<'_> {
    fn in_test(&mut self, test: bool, visit: impl FnOnce(&mut Self)) {
        self.tests += usize::from(test);
        visit(self);
        self.tests -= usize::from(test);
    }

    fn signature(&mut self, sig: &Signature) {
        let name = sig.ident.to_string();
        let line = sig.ident.span().start().line;
        if let Some(fragment) = GLOBAL_READS.iter().find(|f| name.contains(*f)) {
            self.out.push(Diagnostic::new(
                Rule::NoGlobalOrder,
                format!(
                    "{} declares `{name}`, a global read (`{fragment}`): the journal is read stream by stream, enumerated by key through `streams` (ES-I17)",
                    self.package
                ),
                format!("{}:{line}", self.file),
            ));
        }
        if self.tests == 0 && returns_events(&sig.output) && !takes_one_stream(sig) {
            self.out.push(Diagnostic::new(
                Rule::NoGlobalOrder,
                format!(
                    "{} `{name}` returns recorded events without a `StreamId` parameter: no store call returns events of more than one stream (doc/event-store.md section 3.3, ES-I17)",
                    self.package
                ),
                format!("{}:{line}", self.file),
            ));
        }
    }
}

fn returns_events(output: &ReturnType) -> bool {
    let ReturnType::Type(_, ty) = output else {
        return false;
    };
    let mut names = TypeNames(false);
    names.visit_type(ty);
    names.0
}

/// Whether a visited type names `RecordedEvent` anywhere.
struct TypeNames(bool);

impl<'ast> Visit<'ast> for TypeNames {
    fn visit_path_segment(&mut self, segment: &'ast syn::PathSegment) {
        self.0 |= segment.ident == "RecordedEvent";
        visit::visit_path_segment(self, segment);
    }
}

/// Whether `sig` has a parameter of type `StreamId` or a reference to it.
fn takes_one_stream(sig: &Signature) -> bool {
    sig.inputs.iter().any(|input| {
        let syn::FnArg::Typed(typed) = input else {
            return false;
        };
        let ty = match &*typed.ty {
            syn::Type::Reference(r) => &*r.elem,
            other => other,
        };
        matches!(ty, syn::Type::Path(p) if p.qself.is_none()
            && p.path.segments.last().is_some_and(|s| s.ident == "StreamId" && s.arguments.is_none()))
    })
}

impl<'ast> Visit<'ast> for Scan<'_> {
    fn visit_item_mod(&mut self, module: &'ast ItemMod) {
        self.in_test(is_test_only(&module.attrs), |scan| {
            visit::visit_item_mod(scan, module);
        });
    }

    fn visit_item_impl(&mut self, item: &'ast ItemImpl) {
        self.in_test(is_test_only(&item.attrs), |scan| {
            visit::visit_item_impl(scan, item);
        });
    }

    fn visit_item_trait(&mut self, item: &'ast ItemTrait) {
        self.in_test(is_test_only(&item.attrs), |scan| {
            visit::visit_item_trait(scan, item);
        });
    }

    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        self.in_test(is_test_only(&item.attrs), |scan| {
            scan.signature(&item.sig);
            visit::visit_item_fn(scan, item);
        });
    }

    fn visit_impl_item_fn(&mut self, item: &'ast ImplItemFn) {
        self.in_test(is_test_only(&item.attrs), |scan| {
            scan.signature(&item.sig);
            visit::visit_impl_item_fn(scan, item);
        });
    }

    fn visit_trait_item_fn(&mut self, item: &'ast TraitItemFn) {
        self.in_test(is_test_only(&item.attrs), |scan| {
            scan.signature(&item.sig);
            visit::visit_trait_item_fn(scan, item);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{facts_for, policy, rendered};
    use super::*;

    #[test]
    fn should_refuse_global_orderings_when_store_sql_sorts_by_position_time_or_revision() {
        // given
        let statements = [
            "SELECT payload FROM journal_event ORDER BY rowid",
            "SELECT payload FROM journal_event ORDER BY recorded_at DESC LIMIT 10",
            "SELECT * FROM journal_event ORDER BY revision",
            "SELECT * FROM journal_event WHERE event_type = ?1 ORDER BY revision, stream_id",
            "SELECT consumer FROM consumer_lease ORDER BY lease_until",
            "SELECT * FROM t ORDER BY global_position",
            "SELECT * FROM (SELECT * FROM t ORDER BY commit_seq) WHERE a = 1",
            "SELECT * FROM t ORDER BY now() - b",
            "SELECT 1 FROM t WHERE stream_id = ?1; SELECT * FROM journal_event ORDER BY revision",
        ];

        // when
        let found: Vec<Vec<String>> = statements.iter().map(|s| global_orderings(s)).collect();

        // then
        let first: Vec<&str> = found
            .iter()
            .map(|f| f.first().map_or("", String::as_str))
            .collect();
        assert_eq!(
            first,
            [
                "`rowid` (a row position)",
                "`recorded_at` (a time)",
                "`revision` across streams (no `stream_id =` and no `stream_id` key before it)",
                "`revision` across streams (no `stream_id =` and no `stream_id` key before it)",
                "`lease_until` (a time)",
                "`global_position` (a global position)",
                "`commit_seq` (a global position)",
                "`now` (a time)",
                "`revision` across streams (no `stream_id =` and no `stream_id` key before it)",
            ]
        );
    }

    #[test]
    fn should_allow_key_and_single_stream_orderings_when_store_sql_sorts() {
        // given
        let statements = [
            "SELECT revision FROM journal_event WHERE stream_id = ?1 AND revision > ?2 ORDER BY revision",
            "SELECT stream_id FROM stream_head WHERE ?1 IS NULL OR stream_id > ?1 ORDER BY stream_id LIMIT ?2",
            "SELECT * FROM journal_event ORDER BY stream_id, revision",
            "SELECT version FROM schema_meta ORDER BY version DESC LIMIT 1",
            "SELECT event_type FROM event_kind_manifest ORDER BY event_type, schema_version",
            "SELECT recorded_at FROM journal_event WHERE stream_id = ?1",
        ];

        // when
        let found: Vec<Vec<String>> = statements.iter().map(|s| global_orderings(s)).collect();

        // then
        assert!(found.iter().all(Vec::is_empty), "{found:?}");
    }

    #[test]
    fn should_refuse_global_reads_when_an_api_returns_events_of_several_streams() {
        // given
        let source = "pub struct RecordedEvent;\npub struct StreamId;\npub trait Journal {\n    fn read_stream(&self, stream: &StreamId) -> Vec<RecordedEvent>;\n    fn read_all(&self) -> Vec<RecordedEvent>;\n    fn tail(&self, streams: &[StreamId]) -> Option<RecordedEvent>;\n}\npub fn events_by_kind(kind: &str) -> Vec<(StreamId, RecordedEvent)> { let _ = kind; Vec::new() }\npub fn decode(event: &RecordedEvent) -> u8 { let _ = event; 0 }\n#[cfg(test)]\nmod tests {\n    fn everything() -> Vec<super::RecordedEvent> { Vec::new() }\n}\n";
        let data =
            "pub struct RecordedEvent;\npub fn read_all() -> Vec<RecordedEvent> { Vec::new() }\n";

        // when
        let got = rendered(&check(
            Path::new("."),
            &facts_for("mayfly-control", &[("src/lib.rs", source)]),
            &policy(),
        ));
        let elsewhere = check(
            Path::new("."),
            &facts_for("mayfly-data", &[("src/lib.rs", data)]),
            &policy(),
        );

        // then
        let at: Vec<&str> = got
            .iter()
            .map(|d| d.rsplit(':').next().unwrap_or(""))
            .collect();
        assert_eq!(at, ["5)", "5)", "6)", "8)"], "{got:#?}");
        assert!(
            got[0].contains("declares `read_all`, a global read"),
            "{got:#?}"
        );
        assert!(
            got[1].contains("`read_all` returns recorded events without"),
            "{got:#?}"
        );
        assert_eq!(rendered(&elsewhere), Vec::<String>::new());
    }
}
