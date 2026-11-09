//! The action-pinning check of `cargo xtask security` (foundation.md section 8, D15):
//! every `uses:` in a workflow or local action must name a full commit SHA, so a moved tag
//! cannot change what CI runs. Local actions (`./path`) and digest-pinned `docker://` images
//! pass.
//!
//! Each file is parsed as YAML and the check walks the three places GitHub reads `uses`:
//! `jobs.<id>.uses` (a reusable workflow), `jobs.<id>.steps[*].uses` and a composite action's
//! `runs.steps[*].uses`. Matching lines by text was rejected: flow mappings, quoted keys and a
//! space before the colon all hid references from it. The check also fails closed: a line
//! that mentions the token `uses` more often than the parsed structure accounts for is an
//! error, so a reference in a shape the walk does not know is reported, never skipped.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use yaml_rust2::parser::{Event, MarkedEventReceiver, Parser};
use yaml_rust2::scanner::Marker;

use crate::repo;

/// The directory GitHub reads, relative to the repository root. While a repository has none
/// (mayfly before FND-04), GitHub runs nothing from it and the check has nothing to verify;
/// once it exists, [`WORKFLOWS`] must exist and hold a workflow.
const GITHUB: &str = ".github";
/// The workflows directory, relative to the repository root. Once [`GITHUB`] exists it must
/// exist and hold at least one workflow, so a wrong root cannot pass with nothing checked.
const WORKFLOWS: &str = ".github/workflows";
/// Local actions, relative to the repository root; optional.
const ACTIONS: &str = ".github/actions";

/// One problem in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// 1-based line number.
    pub line: usize,
    /// What is wrong.
    pub message: String,
}

/// A parsed YAML node; scalars keep their 1-based line and 0-based column.
#[derive(Debug, Clone)]
enum Node {
    Scalar {
        value: String,
        line: usize,
        col: usize,
    },
    Seq(Vec<Node>),
    Map(Vec<(Node, Node)>),
}

enum Frame {
    Seq(Vec<Node>, usize),
    Map(Vec<(Node, Node)>, Option<Node>, usize),
}

/// Builds [`Node`] trees from parser events. Mappings keep every entry in order, duplicate
/// keys included, so a second `uses` in one step is checked too. Aliases are expanded.
#[derive(Default)]
struct Builder {
    stack: Vec<Frame>,
    documents: Vec<Node>,
    anchors: HashMap<usize, Node>,
    error: Option<Finding>,
}

impl Builder {
    fn push(&mut self, node: Node, anchor: usize) {
        if anchor > 0 {
            self.anchors.insert(anchor, node.clone());
        }
        match self.stack.last_mut() {
            None => self.documents.push(node),
            Some(Frame::Seq(items, _)) => items.push(node),
            Some(Frame::Map(entries, key, _)) => match key.take() {
                None => *key = Some(node),
                Some(k) => entries.push((k, node)),
            },
        }
    }
}

impl MarkedEventReceiver for Builder {
    fn on_event(&mut self, event: Event, mark: Marker) {
        match event {
            Event::Scalar(value, _, anchor, _) => self.push(
                Node::Scalar {
                    value,
                    line: mark.line(),
                    col: mark.col(),
                },
                anchor,
            ),
            Event::SequenceStart(anchor, _) => self.stack.push(Frame::Seq(Vec::new(), anchor)),
            Event::MappingStart(anchor, _) => {
                self.stack.push(Frame::Map(Vec::new(), None, anchor));
            }
            Event::SequenceEnd | Event::MappingEnd => {
                let (node, anchor) = match self.stack.pop() {
                    Some(Frame::Seq(items, anchor)) => (Node::Seq(items), anchor),
                    Some(Frame::Map(entries, _, anchor)) => (Node::Map(entries), anchor),
                    None => return,
                };
                self.push(node, anchor);
            }
            Event::Alias(id) => match self.anchors.get(&id).cloned() {
                Some(node) => self.push(node, 0),
                None => {
                    self.error.get_or_insert(Finding {
                        line: mark.line(),
                        message: "alias to an unknown anchor".into(),
                    });
                }
            },
            _ => {}
        }
    }
}

fn parse(text: &str) -> Result<Vec<Node>, Finding> {
    let mut builder = Builder::default();
    Parser::new_from_str(text)
        .load(&mut builder, true)
        .map_err(|err| Finding {
            line: err.marker().line(),
            message: format!("cannot parse YAML: {}", err.info()),
        })?;
    match builder.error {
        Some(finding) => Err(finding),
        None => Ok(builder.documents),
    }
}

fn is_key(node: &Node, key: &str) -> bool {
    matches!(node, Node::Scalar { value, .. } if value == key)
}

/// Every entry under `key` in `node` when it is a mapping: all of them, so a duplicate key
/// cannot shadow a checked one.
fn entries<'a>(node: &'a Node, key: &'a str) -> impl Iterator<Item = &'a (Node, Node)> {
    let all: &[(Node, Node)] = match node {
        Node::Map(all) => all,
        Node::Scalar { .. } | Node::Seq(_) => &[],
    };
    all.iter().filter(move |(k, _)| is_key(k, key))
}

fn items(node: &Node) -> &[Node] {
    match node {
        Node::Seq(items) => items,
        Node::Scalar { .. } | Node::Map(_) => &[],
    }
}

/// One `uses` found on a walked path: the key's position and the value.
struct Uses<'a> {
    key_line: usize,
    key_col: usize,
    value: &'a Node,
}

fn uses_of<'a>(node: &'a Node, out: &mut Vec<Uses<'a>>) {
    for (key, value) in entries(node, "uses") {
        if let Node::Scalar { line, col, .. } = key {
            out.push(Uses {
                key_line: *line,
                key_col: *col,
                value,
            });
        }
    }
}

fn steps_of<'a>(node: &'a Node, out: &mut Vec<Uses<'a>>) {
    for (_, steps) in entries(node, "steps") {
        for step in items(steps) {
            uses_of(step, out);
        }
    }
}

/// The `uses` entries GitHub reads: `jobs.*.uses`, `jobs.*.steps[*].uses` and
/// `runs.steps[*].uses`.
fn walk(document: &Node) -> Vec<Uses<'_>> {
    let mut out = Vec::new();
    for (_, jobs) in entries(document, "jobs") {
        if let Node::Map(jobs) = jobs {
            for (_, job) in jobs {
                uses_of(job, &mut out);
                steps_of(job, &mut out);
            }
        }
    }
    for (_, runs) in entries(document, "runs") {
        steps_of(runs, &mut out);
    }
    out
}

/// Merge keys are refused outright: the parser does not apply them, so a step assembled by
/// one would not be seen as the step GitHub runs.
fn merge_keys(node: &Node, out: &mut Vec<Finding>) {
    match node {
        Node::Scalar { .. } => {}
        Node::Seq(items) => items.iter().for_each(|n| merge_keys(n, out)),
        Node::Map(all) => {
            for (key, value) in all {
                if let Node::Scalar { value: k, line, .. } = key
                    && k == "<<"
                {
                    out.push(Finding {
                        line: *line,
                        message: "YAML merge keys (`<<`) are not supported here".into(),
                    });
                }
                merge_keys(key, out);
                merge_keys(value, out);
            }
        }
    }
}

/// How often the token `uses` occurs in `text`: not touching a letter, digit, `_` or `-`.
fn token_count(text: &str) -> usize {
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    text.match_indices("uses")
        .filter(|(at, _)| {
            let before = text[..*at].chars().next_back();
            let after = text[at + 4..].chars().next();
            !before.is_some_and(word) && !after.is_some_and(word)
        })
        .count()
}

fn is_hex(text: &str, len: usize) -> bool {
    text.len() == len
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `owner/repo[/path]@<40 lowercase hex>`, a local `./path`, or `docker://image@sha256:<64 hex>`.
/// The ref is everything after the first `@`, so `name@v4#@<sha>` is not a pin.
fn is_pinned(reference: &str) -> bool {
    if reference.starts_with("./") {
        return true;
    }
    if let Some(image) = reference.strip_prefix("docker://") {
        return image.split_once("@sha256:").is_some_and(|(name, digest)| {
            !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"./:_-".contains(&b))
                && is_hex(digest, 64)
        });
    }
    reference.split_once('@').is_some_and(|(name, sha)| {
        let mut parts = name.split('/');
        let owner = parts.next().unwrap_or("");
        let repository = parts.next().unwrap_or("");
        !owner.is_empty()
            && !repository.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"./_-".contains(&b))
            && is_hex(sha, 40)
    })
}

/// Every problem in one file's text, in line order.
pub fn findings(text: &str) -> Vec<Finding> {
    let documents = match parse(text) {
        Ok(documents) => documents,
        Err(finding) => return vec![finding],
    };
    let mut out = Vec::new();
    // (line, column) of each walked key or value -> occurrences of the token it accounts for.
    // Keyed by position so an alias, which repeats a node, does not account for it twice.
    let mut accounted: BTreeMap<(usize, usize), usize> = BTreeMap::new();
    for document in &documents {
        merge_keys(document, &mut out);
        for uses in walk(document) {
            accounted.insert((uses.key_line, uses.key_col), 1);
            match uses.value {
                Node::Scalar { value, line, col } => {
                    accounted.insert((*line, *col), token_count(value));
                    if !is_pinned(value) {
                        out.push(Finding {
                            line: uses.key_line,
                            message: format!("'{value}' is not pinned to a full commit SHA"),
                        });
                    }
                }
                Node::Seq(_) | Node::Map(_) => out.push(Finding {
                    line: uses.key_line,
                    message: "'uses' is not a string".into(),
                }),
            }
        }
    }
    let mut per_line: BTreeMap<usize, usize> = BTreeMap::new();
    for ((line, _), count) in accounted {
        *per_line.entry(line).or_default() += count;
    }
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        if token_count(line) > per_line.get(&number).copied().unwrap_or(0) {
            out.push(Finding {
                line: number,
                message: "mentions `uses` outside jobs.<id>.uses, jobs.<id>.steps[*].uses or \
                          runs.steps[*].uses, so the pin check cannot verify it; reword the \
                          comment or script, or restructure the reference"
                    .into(),
            });
        }
    }
    out.sort_by_key(|f| f.line);
    out
}

fn yaml_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let listing = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in listing {
        let path = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
        if path.is_dir() {
            yaml_files(&path, out)?;
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("yml" | "yaml")
        ) {
            out.push(path);
        }
    }
    Ok(())
}

/// Checks the files under `root`; returns one message per problem. A missing or empty
/// workflows directory under an existing `.github`, or an unreadable file, is an error:
/// nothing checked is not a pass. A root without `.github` has nothing GitHub would run, so it
/// has nothing to check (pinned by `should_pass_when_the_repository_has_no_github_directory`
/// and `should_fail_when_workflows_directory_is_missing`).
pub fn check(root: &Path) -> Result<Vec<String>, String> {
    match fs::metadata(root.join(GITHUB)) {
        Ok(_) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {
            eprintln!("xtask: action pins: no {GITHUB} directory, so no workflow to check");
            return Ok(Vec::new());
        }
        Err(err) => return Err(format!("{}: {err}", root.join(GITHUB).display())),
    }
    let mut files = Vec::new();
    yaml_files(&root.join(WORKFLOWS), &mut files)?;
    if files.is_empty() {
        return Err(format!(
            "no workflow files under {WORKFLOWS} in {}",
            root.display()
        ));
    }
    let actions = root.join(ACTIONS);
    match fs::metadata(&actions) {
        Ok(_) => yaml_files(&actions, &mut files)?,
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(format!("{}: {err}", actions.display())),
    }
    files.sort();
    let mut problems = Vec::new();
    for file in files {
        let text = fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
        let shown = file
            .strip_prefix(root)
            .unwrap_or(&file)
            .display()
            .to_string();
        for f in findings(&text) {
            problems.push(format!("{shown}:{}: {}", f.line, f.message));
        }
    }
    Ok(problems)
}

/// The in-process stage body: 0 when every reference is pinned. Paths come from the
/// repository root, never the current directory.
pub fn run() -> i32 {
    match check(&repo::root()) {
        Ok(problems) if problems.is_empty() => 0,
        Ok(problems) => {
            for p in problems {
                eprintln!("xtask: {p}");
            }
            1
        }
        Err(err) => {
            eprintln!("xtask: action pins: {err}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "3d3c42e5aac5ba805825da76410c181273ba90b1";

    fn lines(found: &[Finding]) -> Vec<usize> {
        found.iter().map(|f| f.line).collect()
    }

    /// A workflow whose single job's steps are `steps`; the first step is on line 6.
    fn workflow(steps: &str) -> String {
        format!("on: push\njobs:\n  j:\n    runs-on: x\n    steps:\n{steps}\n")
    }

    #[test]
    fn should_report_reference_when_action_uses_a_tag() {
        // given
        let text = workflow("      - uses: actions/checkout@v4");

        // when
        let found = findings(&text);

        // then
        assert_eq!(lines(&found), [6], "{found:?}");
        assert!(
            found[0].message.contains("'actions/checkout@v4'"),
            "{found:?}"
        );
    }

    #[test]
    fn should_pass_when_action_is_pinned_to_a_full_sha_with_comment() {
        // given
        let text = workflow(&format!("      - uses: actions/checkout@{SHA} # v7.0.1"));

        // when
        let found = findings(&text);

        // then
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn should_report_reference_when_sha_is_abbreviated_or_missing() {
        // given
        let text = workflow("      - uses: actions/cache@3d3c42e\n      - uses: actions/cache");

        // when
        let found = findings(&text);

        // then
        assert_eq!(lines(&found), [6, 7], "{found:?}");
    }

    #[test]
    fn should_report_reference_when_sha_is_uppercase() {
        // given
        let text = workflow(&format!(
            "      - uses: actions/cache@{}",
            SHA.to_uppercase()
        ));

        // when
        let found = findings(&text);

        // then
        assert_eq!(lines(&found), [6], "{found:?}");
    }

    #[test]
    fn should_pass_when_action_is_local_or_digest_pinned_image() {
        // given
        let digest = "a".repeat(64);
        let text = workflow(&format!(
            "      - uses: ./.github/actions/x\n      - uses: docker://alpine@sha256:{digest}"
        ));

        // when
        let found = findings(&text);

        // then
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn should_report_docker_image_when_it_is_only_tagged() {
        // given
        let text = workflow("      - uses: docker://alpine:3.20");

        // when
        let found = findings(&text);

        // then
        assert_eq!(lines(&found), [6], "{found:?}");
    }

    #[test]
    fn should_report_reference_when_step_is_a_flow_mapping() {
        // given
        let text = workflow("      - {uses: actions/checkout@v4}");

        // when
        let found = findings(&text);

        // then
        assert_eq!(lines(&found), [6], "{found:?}");
        assert!(found[0].message.contains("not pinned"), "{found:?}");
    }

    #[test]
    fn should_report_reference_when_key_is_quoted() {
        // given
        let text = workflow("      - \"uses\": actions/setup-node@v4");

        // when
        let found = findings(&text);

        // then
        assert_eq!(lines(&found), [6], "{found:?}");
        assert!(found[0].message.contains("not pinned"), "{found:?}");
    }

    #[test]
    fn should_report_reference_when_space_precedes_the_colon() {
        // given
        let text = workflow("      - uses : actions/cache@v4");

        // when
        let found = findings(&text);

        // then
        assert_eq!(lines(&found), [6], "{found:?}");
        assert!(found[0].message.contains("not pinned"), "{found:?}");
    }

    #[test]
    fn should_report_reusable_workflow_when_job_uses_a_branch() {
        // given
        let text = "on: push\njobs:\n  call:\n    uses : octo-org/wf/.github/workflows/reusable.yml@main\n";

        // when
        let found = findings(text);

        // then
        assert_eq!(lines(&found), [4], "{found:?}");
        assert!(found[0].message.contains("not pinned"), "{found:?}");
    }

    #[test]
    fn should_pass_when_reusable_workflow_is_pinned() {
        // given
        let text = format!(
            "on: push\njobs:\n  call:\n    uses: octo-org/wf/.github/workflows/r.yml@{SHA}\n"
        );

        // when
        let found = findings(&text);

        // then
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn should_report_reference_when_a_sha_follows_a_fragment() {
        // given
        let text = workflow(&format!("      - uses: owner/repo@v4#@{SHA}"));

        // when
        let found = findings(&text);

        // then
        assert_eq!(lines(&found), [6], "{found:?}");
    }

    #[test]
    fn should_report_reference_when_composite_action_step_is_unpinned() {
        // given
        let text = "name: x\nruns:\n  using: composite\n  steps:\n    - uses: actions/cache@v4\n";

        // when
        let found = findings(text);

        // then
        assert_eq!(lines(&found), [5], "{found:?}");
    }

    #[test]
    fn should_report_line_when_uses_appears_outside_a_walked_path() {
        // given
        let text = workflow(&format!(
            "      - uses: actions/checkout@{SHA}\n        with:\n          uses: a/b@v4"
        ));

        // when
        let found = findings(&text);

        // then
        assert_eq!(lines(&found), [8], "{found:?}");
        assert!(found[0].message.contains("outside"), "{found:?}");
    }

    #[test]
    fn should_report_line_when_a_comment_mentions_uses() {
        // given
        let text = workflow(&format!(
            "      # uses: actions/checkout@v4\n      - uses: actions/checkout@{SHA}"
        ));

        // when
        let found = findings(&text);

        // then
        assert_eq!(lines(&found), [6], "{found:?}");
    }

    #[test]
    fn should_report_line_when_flow_mapping_nests_a_second_uses() {
        // given
        let text = workflow(&format!(
            "      - {{uses: actions/checkout@{SHA}, with: {{uses: a/b@v4}}}}"
        ));

        // when
        let found = findings(&text);

        // then
        assert_eq!(lines(&found), [6], "{found:?}");
        assert!(found[0].message.contains("outside"), "{found:?}");
    }

    #[test]
    fn should_report_second_when_a_step_repeats_the_uses_key() {
        // given
        let text = workflow(&format!(
            "      - uses: actions/checkout@{SHA}\n        uses: actions/checkout@v4"
        ));

        // when
        let found = findings(&text);

        // then
        assert_eq!(lines(&found), [7], "{found:?}");
    }

    #[test]
    fn should_report_line_when_an_aliased_step_hides_a_second_uses() {
        // given
        let text = workflow(&format!(
            "      - &s {{uses: actions/checkout@{SHA}, with: {{uses: a/b@v4}}}}\n      - *s"
        ));

        // when
        let found = findings(&text);

        // then
        assert_eq!(lines(&found), [6], "{found:?}");
        assert!(found[0].message.contains("outside"), "{found:?}");
    }

    #[test]
    fn should_report_merge_key_when_a_step_merges_an_anchor() {
        // given
        let text = format!(
            "x: &c {{name: n}}\njobs:\n  j:\n    steps:\n      - <<: *c\n        uses: a/b@{SHA}\n"
        );

        // when
        let found = findings(&text);

        // then
        assert_eq!(lines(&found), [5], "{found:?}");
        assert!(found[0].message.contains("merge"), "{found:?}");
    }

    #[test]
    fn should_report_parse_error_when_yaml_is_invalid() {
        // given
        let text = "jobs: [unclosed\n";

        // when
        let found = findings(text);

        // then
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].message.contains("cannot parse"), "{found:?}");
    }

    #[test]
    fn should_report_file_and_line_when_workflow_on_disk_is_unpinned() {
        // given
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().join(WORKFLOWS);
        fs::create_dir_all(&dir).expect("create dir");
        fs::write(
            dir.join("a.yml"),
            "jobs:\n  j:\n    steps:\n      - uses: actions/checkout@v4\n",
        )
        .expect("write");

        // when
        let problems = check(temp.path()).expect("scan");

        // then
        assert_eq!(problems.len(), 1);
        assert!(
            problems[0].contains(".github/workflows/a.yml:4:"),
            "{problems:?}"
        );
    }

    #[test]
    fn should_pass_when_the_repository_has_no_github_directory() {
        // given
        let temp = tempfile::tempdir().expect("tempdir");

        // when
        let result = check(temp.path());

        // then
        assert_eq!(result, Ok(Vec::new()));
    }

    #[test]
    fn should_fail_when_workflows_directory_is_missing() {
        // given
        let temp = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(temp.path().join(ACTIONS)).expect("create dir");

        // when
        let result = check(temp.path());

        // then
        assert!(result.is_err(), "{result:?}");
    }

    #[test]
    fn should_fail_when_workflows_directory_holds_no_workflow() {
        // given
        let temp = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(temp.path().join(WORKFLOWS)).expect("create dir");

        // when
        let result = check(temp.path());

        // then
        let err = result.expect_err("an empty directory checks nothing");
        assert!(err.contains("no workflow files"), "{err}");
    }

    #[test]
    fn should_pass_when_repository_workflows_are_pinned() {
        // given
        let root = repo::root();

        // when
        let problems = check(&root).expect("scan");

        // then
        assert!(problems.is_empty(), "{problems:?}");
    }
}
