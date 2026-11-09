//! Workspace edges (foundation.md section 2.1, section 3.2 rules 2, 3 and 6): the direct
//! allowlist per dependency kind, the production closure and an acyclic policy, all over
//! the resolved graph from `cargo metadata`, with full dependency paths in failures.
//!
//! Cargo itself rejects a cycle of normal or build edges, so a cyclic workspace fails as
//! `ARCH-metadata`; this module rejects a cyclic allowlist, so the declared layering is a
//! partial order as well.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use cargo_metadata::{DependencyKind, Metadata};

use super::POLICY_FILE;
use super::policy::{Category, Policy};
use super::report::{Diagnostic, Rule};

/// The independent oracles (D11, foundation.md section 2.6): they share no code with
/// production, so they may reach no workspace crate over any kind of edge, the test-only
/// crates included (a dev edge to the testkit is otherwise legal).
pub const INDEPENDENT: &[&str] = &["mayfly-faultlab", "mayfly-fake-tart", "mayfly-fake-github"];

/// A dependency kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// `[dependencies]`.
    Normal,
    /// `[dev-dependencies]`.
    Dev,
    /// `[build-dependencies]`.
    Build,
}

impl Kind {
    /// The word a diagnostic uses.
    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Dev => "dev",
            Self::Build => "build",
        }
    }
}

/// One package of the resolved graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// Package name.
    pub name: String,
    /// Whether it is a workspace member.
    pub member: bool,
}

/// The resolved graph, keyed by package id so two versions of one crate stay apart.
#[derive(Debug, Clone, Default)]
pub struct Graph {
    nodes: BTreeMap<String, Node>,
    edges: BTreeMap<String, BTreeSet<(String, Kind)>>,
}

impl Graph {
    /// Builds the graph from metadata that has a `resolve` section.
    pub fn from_metadata(metadata: &Metadata) -> Self {
        let mut graph = Self::default();
        let members: BTreeSet<_> = metadata.workspace_members.iter().collect();
        for package in &metadata.packages {
            graph.add_node(
                &package.id.repr,
                &package.name,
                members.contains(&package.id),
            );
        }
        for node in metadata.resolve.iter().flat_map(|r| &r.nodes) {
            for dep in &node.deps {
                for info in &dep.dep_kinds {
                    let kind = match info.kind {
                        DependencyKind::Development => Kind::Dev,
                        DependencyKind::Build => Kind::Build,
                        _ => Kind::Normal,
                    };
                    graph.add_edge(&node.id.repr, &dep.pkg.repr, kind);
                }
            }
        }
        graph
    }

    /// Adds a package.
    pub fn add_node(&mut self, id: &str, name: &str, member: bool) {
        self.nodes.insert(
            id.to_owned(),
            Node {
                name: name.to_owned(),
                member,
            },
        );
    }

    /// Adds an edge between two package ids.
    pub fn add_edge(&mut self, from: &str, to: &str, kind: Kind) {
        self.edges
            .entry(from.to_owned())
            .or_default()
            .insert((to.to_owned(), kind));
    }

    /// Every package, by id.
    pub fn nodes(&self) -> &BTreeMap<String, Node> {
        &self.nodes
    }

    /// The edges leaving `id`.
    pub fn edges_from(&self, id: &str) -> impl Iterator<Item = &(String, Kind)> {
        self.edges.get(id).into_iter().flatten()
    }

    /// The package name of `id`, or `id` itself when unknown.
    pub fn name_of<'a>(&'a self, id: &'a str) -> &'a str {
        self.nodes.get(id).map_or(id, |n| n.name.as_str())
    }

    fn is_member(&self, id: &str) -> bool {
        self.nodes.get(id).is_some_and(|n| n.member)
    }

    /// The shortest path from `start` to every package reachable over `kinds`, as ids.
    pub fn shortest_paths(&self, start: &str, kinds: &[Kind]) -> BTreeMap<String, Vec<String>> {
        let mut paths = BTreeMap::from([(start.to_owned(), vec![start.to_owned()])]);
        let mut queue = VecDeque::from([start.to_owned()]);
        while let Some(id) = queue.pop_front() {
            let path = paths[&id].clone();
            for (to, kind) in self.edges_from(&id) {
                if kinds.contains(kind) && !paths.contains_key(to) {
                    let mut next = path.clone();
                    next.push(to.clone());
                    paths.insert(to.clone(), next);
                    queue.push_back(to.clone());
                }
            }
        }
        paths
    }
}

/// Renders a dependency path as `a -> b -> c`.
pub fn render_path<S: AsRef<str>>(names: &[S]) -> String {
    names
        .iter()
        .map(AsRef::as_ref)
        .collect::<Vec<_>>()
        .join(" -> ")
}

/// Checks workspace edges against the policy. `manifests` maps a crate name to its manifest
/// path, which locates each diagnostic.
pub fn check(
    policy: &Policy,
    graph: &Graph,
    manifests: &BTreeMap<String, String>,
) -> Vec<Diagnostic> {
    let mut out = check_policy_cycles(policy);
    let category = categories(policy);
    let at = |name: &str| {
        manifests
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.to_owned())
    };
    let empty = Vec::new();
    for (id, node) in graph.nodes() {
        if !node.member {
            continue;
        }
        let from = node.name.as_str();
        let normal = policy.dependencies.normal.get(from).unwrap_or(&empty);
        let build = policy.dependencies.build.get(from).unwrap_or(&empty);
        for (to_id, kind) in graph.edges_from(id) {
            if !graph.is_member(to_id) {
                continue;
            }
            let to = graph.name_of(to_id);
            let path = render_path(&[from, to]);
            if INDEPENDENT.contains(&from) {
                out.push(Diagnostic::new(
                    Rule::IndependentOracle,
                    format!(
                        "{} dependency {path}: {from} is an independent oracle and reaches no workspace crate (D11)",
                        kind.label()
                    ),
                    at(from),
                ));
            }
            let allowed = |list: &Vec<String>| list.iter().any(|d| d == to);
            match kind {
                Kind::Normal if !allowed(normal) => out.push(Diagnostic::new(
                    Rule::ForbiddenEdge,
                    format!("normal dependency {path} is not in `[dependencies.normal] {from}`"),
                    at(from),
                )),
                Kind::Build if !allowed(build) => out.push(Diagnostic::new(
                    Rule::UndeclaredBuildDep,
                    format!("build dependency {path} is not in `[dependencies.build] {from}`"),
                    at(from),
                )),
                Kind::Dev if !allowed(normal) && category.get(to) != Some(&Category::TestOnly) => {
                    out.push(Diagnostic::new(
                        Rule::ForbiddenDevEdge,
                        format!(
                            "dev dependency {path} targets neither a test_only crate nor a crate in `[dependencies.normal] {from}`"
                        ),
                        at(from),
                    ));
                }
                _ => {}
            }
        }
        if category.get(from) == Some(&Category::Production) {
            out.extend(check_closure(graph, id, &category, &at));
        }
    }
    out
}

/// A production crate must not reach a test_only or tool_only crate over normal or build
/// edges, through any intermediate package.
fn check_closure(
    graph: &Graph,
    start: &str,
    category: &BTreeMap<&str, Category>,
    at: &dyn Fn(&str) -> String,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let from = graph.name_of(start);
    for (id, path) in graph.shortest_paths(start, &[Kind::Normal, Kind::Build]) {
        if !graph.is_member(&id) {
            continue;
        }
        let to = graph.name_of(&id);
        let Some(forbidden @ (Category::TestOnly | Category::ToolOnly)) = category.get(to) else {
            continue;
        };
        let names: Vec<&str> = path.iter().map(|p| graph.name_of(p)).collect();
        out.push(Diagnostic::new(
            Rule::ProductionClosure,
            format!(
                "production crate `{from}` reaches {} crate `{to}`: {}",
                forbidden.key(),
                render_path(&names)
            ),
            at(from),
        ));
    }
    out
}

/// The allowlist itself must be acyclic over normal and build entries. Reports one shortest
/// cycle per strongly connected component, through its alphabetically first crate.
fn check_policy_cycles(policy: &Policy) -> Vec<Diagnostic> {
    let mut adjacency: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (_, table) in policy.dependencies.tables() {
        for (from, deps) in table {
            adjacency
                .entry(from)
                .or_default()
                .extend(deps.iter().map(String::as_str));
        }
    }
    let reach: BTreeMap<&str, BTreeMap<&str, Vec<&str>>> = adjacency
        .keys()
        .map(|&start| (start, policy_paths(&adjacency, start)))
        .collect();
    let mut reported: BTreeSet<&str> = BTreeSet::new();
    let mut out = Vec::new();
    for (&start, paths) in &reach {
        // BFS reaches `start` again first along its shortest cycle.
        let Some(cycle) = paths.get(start) else {
            continue;
        };
        if reported.contains(start) {
            continue;
        }
        reported.extend(
            paths
                .keys()
                .filter(|n| reach.get(*n).is_some_and(|r| r.contains_key(start))),
        );
        out.push(Diagnostic::new(
            Rule::PolicyCycle,
            format!("`[dependencies]` allows a cycle: {}", render_path(cycle)),
            POLICY_FILE,
        ));
    }
    out
}

/// Shortest allowlist path from `start` to every crate it reaches, each beginning with
/// `start`; `start` itself is present only when a path returns to it.
fn policy_paths<'a>(
    adjacency: &BTreeMap<&'a str, BTreeSet<&'a str>>,
    start: &'a str,
) -> BTreeMap<&'a str, Vec<&'a str>> {
    let mut paths: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut queue = VecDeque::from([vec![start]]);
    while let Some(path) = queue.pop_front() {
        let last = path[path.len() - 1];
        for &next in adjacency.get(last).into_iter().flatten() {
            if !paths.contains_key(next) {
                let mut longer = path.clone();
                longer.push(next);
                paths.insert(next, longer.clone());
                queue.push_back(longer);
            }
        }
    }
    paths
}

fn categories(policy: &Policy) -> BTreeMap<&str, Category> {
    policy
        .workspace
        .lists()
        .into_iter()
        .flat_map(|(category, list)| {
            list.into_iter()
                .flatten()
                .map(move |n| (n.as_str(), category))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::architecture::policy;

    const POLICY: &str = r#"schema = 1
[workspace]
production = ["domain", "app", "cli"]
test_only = ["testkit"]
tool_only = ["tool"]
[dependencies.normal]
domain = []
app = ["domain"]
cli = ["app"]
testkit = ["domain"]
tool = []
"#;

    fn graph(edges: &[(&str, &str, Kind)]) -> Graph {
        let mut graph = Graph::default();
        for name in ["domain", "app", "cli", "testkit", "tool"] {
            graph.add_node(name, name, true);
        }
        graph.add_node("ext", "ext", false);
        for (from, to, kind) in edges {
            graph.add_edge(from, to, *kind);
        }
        graph
    }

    fn run(policy_text: &str, edges: &[(&str, &str, Kind)]) -> Vec<Diagnostic> {
        let policy = policy::parse(policy_text).unwrap();
        let mut out = check(&policy, &graph(edges), &BTreeMap::new());
        out.sort();
        out
    }

    #[test]
    fn should_render_arrows_when_path_has_three_crates() {
        // given
        let names = ["mayfly-cli", "mayfly-composition", "mayfly-testkit"];

        // when
        let rendered = render_path(&names);

        // then
        assert_eq!(
            rendered,
            "mayfly-cli -> mayfly-composition -> mayfly-testkit"
        );
    }

    #[test]
    fn should_pass_when_edges_follow_the_allowlist() {
        // given
        let edges = [
            ("app", "domain", Kind::Normal),
            ("cli", "app", Kind::Normal),
            ("app", "testkit", Kind::Dev),
            ("cli", "app", Kind::Dev),
        ];

        // when
        let diagnostics = run(POLICY, &edges);

        // then
        assert_eq!(diagnostics, []);
    }

    #[test]
    fn should_report_forbidden_edge_when_normal_edge_is_reversed() {
        // given
        let edges = [("domain", "app", Kind::Normal)];

        // when
        let diagnostics = run(POLICY, &edges);

        // then
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].rule, Rule::ForbiddenEdge);
        assert!(diagnostics[0].message.contains("domain -> app"));
    }

    #[test]
    fn should_report_full_path_when_test_only_crate_is_reached_through_external() {
        // given
        let edges = [
            ("cli", "ext", Kind::Normal),
            ("ext", "testkit", Kind::Normal),
        ];

        // when
        let diagnostics = run(POLICY, &edges);

        // then
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].rule, Rule::ProductionClosure);
        assert!(
            diagnostics[0].message.ends_with("cli -> ext -> testkit"),
            "{}",
            diagnostics[0].message
        );
    }

    #[test]
    fn should_report_closure_when_build_edge_reaches_tool_crate() {
        // given
        let text = format!("{POLICY}[dependencies.build]\napp = [\"tool\"]\n");
        let edges = [("app", "tool", Kind::Build)];

        // when
        let diagnostics = run(&text, &edges);

        // then
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].rule, Rule::ProductionClosure);
        assert!(diagnostics[0].message.contains("tool_only crate `tool`"));
    }

    #[test]
    fn should_report_undeclared_build_dep_when_build_table_is_absent() {
        // given
        let edges = [("app", "domain", Kind::Build)];

        // when
        let diagnostics = run(POLICY, &edges);

        // then
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].rule, Rule::UndeclaredBuildDep);
    }

    #[test]
    fn should_report_forbidden_dev_edge_when_target_is_tool_crate() {
        // given
        let edges = [("app", "tool", Kind::Dev)];

        // when
        let diagnostics = run(POLICY, &edges);

        // then
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].rule, Rule::ForbiddenDevEdge);
    }

    #[test]
    fn should_report_self_loop_when_crate_allows_itself() {
        // given
        let text = POLICY.replace("domain = []", "domain = [\"domain\"]");

        // when
        let diagnostics = run(&text, &[]);

        // then
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].message.ends_with("domain -> domain"));
    }

    #[test]
    fn should_report_each_cycle_once_when_allowlist_is_cyclic() {
        // given
        let text = POLICY.replace("domain = []", "domain = [\"cli\"]");

        // when
        let diagnostics = run(&text, &[]);

        // then
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].rule, Rule::PolicyCycle);
        assert!(
            diagnostics[0]
                .message
                .contains("app -> domain -> cli -> app"),
            "{}",
            diagnostics[0].message
        );
    }
}
