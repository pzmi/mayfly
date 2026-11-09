//! Third-party dependencies (foundation.md section 3.2 rules 4 and 5):
//! each workspace crate's direct third-party edges against `[external.<kind>]`, the features
//! those edges enable, and the source of every package in the resolved graph.

use std::collections::{BTreeMap, BTreeSet};

use cargo_metadata::{DependencyKind, Metadata, Package};

use super::POLICY_FILE;
use super::graph::{Graph, Kind, render_path};
use super::policy::{Features, Policy};
use super::report::{Diagnostic, Rule};

/// The source of crates.io packages, as `cargo metadata` reports it.
const CRATES_IO: &str = "registry+https://github.com/rust-lang/crates.io-index";

/// One direct edge from a workspace crate to a third-party package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    /// The workspace crate.
    pub from: String,
    /// The third-party package name.
    pub to: String,
    /// Dependency kind.
    pub kind: Kind,
    /// Features this edge enables on `to`: declared ones, `default` unless disabled, and
    /// those the workspace crate's own active features enable through `to/<feature>`.
    pub requested: BTreeSet<String>,
    /// `to`'s feature table, used to expand implied features.
    pub table: BTreeMap<String, Vec<String>>,
}

/// A package outside the workspace and where it comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    /// Package name.
    pub name: String,
    /// The `source` field; `None` for a path dependency.
    pub source: Option<String>,
    /// Shortest path from a workspace crate, as names.
    pub path: Vec<String>,
}

/// Collects the direct third-party edges of every workspace crate.
pub fn edges(metadata: &Metadata) -> Vec<Edge> {
    let packages: BTreeMap<&str, &Package> = metadata
        .packages
        .iter()
        .map(|p| (p.id.repr.as_str(), p))
        .collect();
    let members: BTreeSet<&str> = metadata
        .workspace_members
        .iter()
        .map(|id| id.repr.as_str())
        .collect();
    let mut out = Vec::new();
    for node in metadata.resolve.iter().flat_map(|r| &r.nodes) {
        let Some(from) = packages.get(node.id.repr.as_str()) else {
            continue;
        };
        if !members.contains(node.id.repr.as_str()) {
            continue;
        }
        let active: Vec<&str> = node.features.iter().map(|f| f.as_str()).collect();
        for dep in &node.deps {
            if members.contains(dep.pkg.repr.as_str()) {
                continue;
            }
            let Some(to) = packages.get(dep.pkg.repr.as_str()) else {
                continue;
            };
            let kinds: BTreeSet<Kind> = dep.dep_kinds.iter().map(|k| kind(k.kind)).collect();
            for kind in kinds {
                out.push(Edge {
                    from: from.name.to_string(),
                    to: to.name.to_string(),
                    kind,
                    requested: requested_features(from, &to.name, kind, &active),
                    table: to.features.clone(),
                });
            }
        }
    }
    out
}

fn kind(kind: DependencyKind) -> Kind {
    match kind {
        DependencyKind::Development => Kind::Dev,
        DependencyKind::Build => Kind::Build,
        _ => Kind::Normal,
    }
}

/// Features `from` asks of `to` through its declarations of that kind and its active
/// features. Declarations for every target are united, which can only over-report.
fn requested_features(
    from: &Package,
    to: &str,
    edge_kind: Kind,
    active: &[&str],
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut keys = BTreeSet::new();
    for dep in from
        .dependencies
        .iter()
        .filter(|d| d.name == to && kind(d.kind) == edge_kind)
    {
        out.extend(dep.features.iter().cloned());
        if dep.uses_default_features {
            out.insert("default".to_owned());
        }
        keys.insert(dep.rename.clone().unwrap_or_else(|| dep.name.clone()));
    }
    for feature in active {
        for entry in from.features.get(*feature).into_iter().flatten() {
            if let Some((dep, feature)) = entry.split_once('/')
                && keys.contains(dep.trim_end_matches('?'))
            {
                out.insert(feature.to_owned());
            }
        }
    }
    out
}

/// `features` plus every feature they imply through `table`. Entries naming a dependency
/// (`dep:x`, `x/y`) are not features of this package and are skipped. A feature the
/// package does not define, such as `default` on a crate without defaults, is dropped.
pub fn expand(
    features: &BTreeSet<String>,
    table: &BTreeMap<String, Vec<String>>,
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut work: Vec<&str> = features.iter().map(String::as_str).collect();
    while let Some(feature) = work.pop() {
        let Some(implied) = table.get(feature) else {
            continue;
        };
        if out.insert(feature.to_owned()) {
            work.extend(
                implied
                    .iter()
                    .map(String::as_str)
                    .filter(|e| !e.contains('/') && !e.starts_with("dep:")),
            );
        }
    }
    out
}

/// Checks direct edges against `[external.<kind>]` and their features against the lists.
pub fn check_edges(
    policy: &Policy,
    edges: &[Edge],
    manifests: &BTreeMap<String, String>,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for edge in edges {
        let at = manifests
            .get(&edge.from)
            .cloned()
            .unwrap_or_else(|| edge.from.clone());
        let path = render_path(&[edge.from.as_str(), edge.to.as_str()]);
        let tables: &[&str] = match edge.kind {
            Kind::Normal => &["normal"],
            Kind::Dev => &["dev", "normal"],
            Kind::Build => &["build"],
        };
        let lists: Vec<(&str, &Features)> = policy
            .external
            .tables()
            .into_iter()
            .filter(|(key, _)| tables.contains(key))
            .filter_map(|(key, table)| Some((key, table.get(&edge.from)?.get(&edge.to)?)))
            .collect();
        let kind = tables[0];
        if lists.is_empty() {
            out.push(Diagnostic::new(
                Rule::UnlistedExternal,
                format!(
                    "{kind} dependency {path} is not in `[external.{kind}] {}`",
                    edge.from
                ),
                at,
            ));
            continue;
        }
        let mut allowed = BTreeSet::new();
        for (_, features) in &lists {
            match features {
                Features::Any => break,
                Features::Only(list) => allowed.extend(list.iter().cloned()),
            }
        }
        if lists.iter().any(|(_, f)| **f == Features::Any) {
            continue;
        }
        let allowed = expand(&allowed, &edge.table);
        let extra: Vec<String> = expand(&edge.requested, &edge.table)
            .difference(&allowed)
            .cloned()
            .collect();
        if !extra.is_empty() {
            let keys: Vec<String> = lists
                .iter()
                .map(|(k, _)| format!("[external.{k}]"))
                .collect();
            out.push(Diagnostic::new(
                Rule::ExternalFeature,
                format!(
                    "{kind} dependency {path} enables features [{}] outside {} {}",
                    extra.join(", "),
                    keys.join(" or "),
                    edge.from
                ),
                at,
            ));
        }
    }
    out
}

/// Every package outside the workspace, with the shortest path to it from a workspace crate.
pub fn origins(metadata: &Metadata, graph: &Graph) -> Vec<Origin> {
    let mut paths: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for id in &metadata.workspace_members {
        for (to, path) in graph.shortest_paths(&id.repr, &[Kind::Normal, Kind::Dev, Kind::Build]) {
            let keep = paths.get(&to).is_none_or(|p| path.len() < p.len());
            if keep {
                paths.insert(to, path);
            }
        }
    }
    let members: BTreeSet<&str> = metadata
        .workspace_members
        .iter()
        .map(|id| id.repr.as_str())
        .collect();
    metadata
        .packages
        .iter()
        .filter(|p| !members.contains(p.id.repr.as_str()))
        .filter_map(|p| {
            let path = paths.get(&p.id.repr)?;
            Some(Origin {
                name: p.name.to_string(),
                source: p.source.as_ref().map(|s| s.repr.clone()),
                path: path.iter().map(|id| graph.name_of(id).to_owned()).collect(),
            })
        })
        .collect()
}

/// Rejects git and foreign-registry packages unless `[exceptions.sources]` approves the
/// source with an ADR that exists, and path packages outside the workspace always.
pub fn check_sources(
    policy: &Policy,
    origins: &[Origin],
    manifests: &BTreeMap<String, String>,
    adr_exists: &dyn Fn(&str) -> bool,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for origin in origins {
        let (from, note) = match origin.source.as_deref() {
            Some(CRATES_IO) => continue,
            Some(source) => {
                let base = source.split(['?', '#']).next().unwrap_or(source);
                let note = match policy.exceptions.sources.get(base) {
                    Some(exception) if adr_exists(&exception.adr) => continue,
                    Some(exception) => format!(
                        "; its [exceptions.sources] entry cites ADR {}, which is not in doc/adr/",
                        exception.adr
                    ),
                    None => String::new(),
                };
                (format!("`{base}`"), note)
            }
            None => ("a path outside the workspace".to_owned(), String::new()),
        };
        let first = origin.path.first().map_or("", String::as_str);
        out.push(Diagnostic::new(
            Rule::ExternalSource,
            format!(
                "package `{}` comes from {from}, not crates.io: {}{note}",
                origin.name,
                render_path(&origin.path)
            ),
            manifests
                .get(first)
                .cloned()
                .unwrap_or_else(|| POLICY_FILE.to_owned()),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::architecture::policy;

    const POLICY: &str = r#"schema = 1
[workspace]
production = ["app"]
[dependencies.normal]
app = []
[external.normal]
app = { tokio = ["sync"], log = "*" }
[external.dev]
app = { tempfile = ["default"] }
"#;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    fn tokio_table() -> BTreeMap<String, Vec<String>> {
        [
            ("default", vec![]),
            ("sync", vec![]),
            ("rt", vec![]),
            (
                "full",
                vec!["sync".to_owned(), "rt".to_owned(), "dep:bytes".to_owned()],
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect()
    }

    fn edge(to: &str, kind: Kind, requested: &[&str]) -> Edge {
        Edge {
            from: "app".to_owned(),
            to: to.to_owned(),
            kind,
            requested: set(requested),
            table: tokio_table(),
        }
    }

    fn run(edges: &[Edge]) -> Vec<Diagnostic> {
        check_edges(&policy::parse(POLICY).unwrap(), edges, &BTreeMap::new())
    }

    #[test]
    fn should_follow_implications_when_feature_set_is_expanded() {
        // given
        let requested = set(&["full", "unknown"]);

        // when
        let expanded = expand(&requested, &tokio_table());

        // then
        assert_eq!(expanded, set(&["full", "rt", "sync"]));
    }

    #[test]
    fn should_pass_when_edge_enables_only_listed_features() {
        // given
        let edges = [
            edge("tokio", Kind::Normal, &["sync"]),
            edge("log", Kind::Normal, &["anything"]),
            edge("tokio", Kind::Dev, &["sync"]),
        ];

        // when
        let diagnostics = run(&edges);

        // then
        assert_eq!(diagnostics, []);
    }

    #[test]
    fn should_report_implied_feature_when_edge_enables_full() {
        // given
        let edges = [edge("tokio", Kind::Normal, &["full"])];

        // when
        let diagnostics = run(&edges);

        // then
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].rule, Rule::ExternalFeature);
        assert!(
            diagnostics[0].message.contains("[full, rt]"),
            "{}",
            diagnostics[0].message
        );
    }

    #[test]
    fn should_report_default_when_edge_keeps_default_features_unlisted() {
        // given
        let edges = [edge("tokio", Kind::Normal, &["sync", "default"])];

        // when
        let diagnostics = run(&edges);

        // then
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].message.contains("[default]"));
    }

    #[test]
    fn should_report_unlisted_when_dev_only_crate_is_a_normal_dependency() {
        // given
        let edges = [edge("tempfile", Kind::Normal, &["default"])];

        // when
        let diagnostics = run(&edges);

        // then
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].rule, Rule::UnlistedExternal);
        assert!(diagnostics[0].message.contains("[external.normal] app"));
    }

    #[test]
    fn should_report_unlisted_when_build_dependency_is_only_listed_as_normal() {
        // given
        let edges = [edge("tokio", Kind::Build, &["sync"])];

        // when
        let diagnostics = run(&edges);

        // then
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].message.contains("[external.build] app"));
    }

    fn origin(source: Option<&str>) -> Origin {
        Origin {
            name: "x".to_owned(),
            source: source.map(str::to_owned),
            path: vec!["app".to_owned(), "x".to_owned()],
        }
    }

    #[test]
    fn should_accept_git_source_when_exception_cites_existing_adr() {
        // given
        let text = format!(
            "{POLICY}[exceptions.sources]\n\"git+https://example.invalid/x\" = {{ adr = \"0099\" }}\n"
        );
        let origins = [origin(Some("git+https://example.invalid/x?rev=1#abc"))];

        // when
        let diagnostics = check_sources(
            &policy::parse(&text).unwrap(),
            &origins,
            &BTreeMap::new(),
            &|adr| adr == "0099",
        );

        // then
        assert_eq!(diagnostics, []);
    }

    #[test]
    fn should_reject_sources_when_not_crates_io_and_not_excepted() {
        // given
        let origins = [
            origin(Some(CRATES_IO)),
            origin(Some("sparse+https://other.invalid/index/")),
            origin(None),
        ];

        // when
        let diagnostics = check_sources(
            &policy::parse(POLICY).unwrap(),
            &origins,
            &BTreeMap::new(),
            &|_| true,
        );

        // then
        assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
        assert!(diagnostics.iter().all(|d| d.rule == Rule::ExternalSource));
        assert!(
            diagnostics[1]
                .message
                .contains("comes from a path outside the workspace")
        );
    }
}
