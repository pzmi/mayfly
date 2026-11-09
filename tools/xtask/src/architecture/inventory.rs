//! Crate inventory (foundation.md section 3.2, rules 1 and 7): the policy, the workspace
//! members Cargo reports and the crates on disk must describe the same set of crates.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use super::POLICY_FILE;
use super::policy::{Category, Policy};
use super::report::{Diagnostic, Rule};

/// Directories whose children are scanned for crates, so a crate that is not a workspace
/// member is still found.
pub const SOURCE_DIRS: [&str; 2] = ["crates", "tools"];

/// A crate and its manifest, relative to the workspace root with `/` separators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crate {
    /// Package name.
    pub name: String,
    /// Manifest path relative to the workspace root.
    pub manifest: String,
}

/// Category lists that are empty, and crates listed more than once.
pub fn check_categories(policy: &Policy) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let mut seen: BTreeMap<&str, Vec<Category>> = BTreeMap::new();
    for (category, list) in policy.workspace.lists() {
        let Some(list) = list else { continue };
        if list.is_empty() {
            out.push(Diagnostic::new(
                Rule::EmptySelector,
                format!(
                    "`[workspace] {}` is empty; list its crates or remove the key",
                    category.key()
                ),
                POLICY_FILE,
            ));
        }
        for name in list {
            seen.entry(name).or_default().push(category);
        }
    }
    for (name, categories) in seen {
        if categories.len() > 1 {
            let keys: Vec<_> = categories.iter().map(|c| c.key()).collect();
            out.push(Diagnostic::new(
                Rule::DuplicateCategory,
                format!(
                    "crate `{name}` is listed {} times: {}",
                    keys.len(),
                    keys.join(", ")
                ),
                POLICY_FILE,
            ));
        }
    }
    out
}

/// Compares the policy, the workspace members and the crates found on disk.
pub fn check_crates(policy: &Policy, members: &[Crate], disk: &[Crate]) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let member_manifests: BTreeSet<&str> = members.iter().map(|c| c.manifest.as_str()).collect();
    for krate in disk {
        if !member_manifests.contains(krate.manifest.as_str()) {
            out.push(Diagnostic::new(
                Rule::NotAMember,
                format!(
                    "crate `{}` is not in `[workspace] members` of Cargo.toml",
                    krate.name
                ),
                &krate.manifest,
            ));
        }
    }
    let categorized: BTreeSet<&str> = policy
        .workspace
        .lists()
        .into_iter()
        .flat_map(|(_, list)| list.into_iter().flatten())
        .map(String::as_str)
        .collect();
    for krate in members {
        if !categorized.contains(krate.name.as_str()) {
            out.push(Diagnostic::new(
                Rule::UnknownCrate,
                format!(
                    "workspace crate `{}` is in no `[workspace]` category of {POLICY_FILE}",
                    krate.name
                ),
                &krate.manifest,
            ));
        }
    }
    let member_names: BTreeSet<&str> = members.iter().map(|c| c.name.as_str()).collect();
    for (place, name) in policy_names(policy) {
        if !member_names.contains(name) {
            out.push(Diagnostic::new(
                Rule::MissingCrate,
                format!("`{place}` names `{name}`, which is not a workspace crate"),
                POLICY_FILE,
            ));
        }
    }
    out
}

/// Every workspace crate name the policy mentions, with where it mentions it. Deduplicated,
/// so one stale name in one place yields one diagnostic.
fn policy_names(policy: &Policy) -> BTreeSet<(String, &str)> {
    let mut out = BTreeSet::new();
    let fuzz = fuzz_names(policy);
    for (category, list) in policy.workspace.lists() {
        if category == Category::FuzzOnly {
            continue;
        }
        for name in list.into_iter().flatten() {
            out.insert((format!("[workspace] {}", category.key()), name.as_str()));
        }
    }
    for (kind, table) in policy.dependencies.tables() {
        for (krate, deps) in table {
            if !fuzz.contains(krate.as_str()) {
                out.insert((format!("[dependencies.{kind}]"), krate.as_str()));
            }
            for dep in deps {
                out.insert((format!("[dependencies.{kind}] {krate}"), dep.as_str()));
            }
        }
    }
    for (kind, table) in policy.external.tables() {
        for krate in table.keys().filter(|k| !fuzz.contains(k.as_str())) {
            out.insert((format!("[external.{kind}]"), krate.as_str()));
        }
    }
    for (plane, list) in policy.planes.lists() {
        for name in list.iter().filter(|n| !fuzz.contains(n.as_str())) {
            out.insert((format!("[planes] {}", plane.key()), name.as_str()));
        }
    }
    for (key, list) in policy.purity.lists() {
        for name in list {
            out.insert((format!("[purity] {key}"), name.as_str()));
        }
    }
    out
}

/// The fuzz crates the policy lists.
fn fuzz_names(policy: &Policy) -> BTreeSet<&str> {
    policy
        .workspace
        .fuzz_only
        .iter()
        .flatten()
        .map(String::as_str)
        .collect()
}

/// Where the one fuzz crate lives (the cargo-fuzz layout), relative to the workspace root.
pub const FUZZ_MANIFEST: &str = "fuzz/Cargo.toml";

/// The `fuzz_only` rules: the fuzz crate at [`FUZZ_MANIFEST`] is listed, is no
/// workspace member, is its own workspace root, and depends only on the workspace crates
/// `[dependencies.normal]` and the third-party crates `[external.normal]` allow it.
pub fn check_fuzz(policy: &Policy, root: &Path, members: &[Crate]) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let listed = fuzz_names(policy);
    let manifest = if root.join(FUZZ_MANIFEST).is_file() {
        match read_toml(root, FUZZ_MANIFEST) {
            Ok(table) => Some(table),
            Err(diagnostic) => return vec![diagnostic],
        }
    } else {
        None
    };
    let name = manifest
        .as_ref()
        .and_then(|m| m.get("package"))
        .and_then(|p| p.get("name"))
        .and_then(toml::Value::as_str);
    for krate in members.iter().filter(|c| listed.contains(c.name.as_str())) {
        out.push(Diagnostic::new(
            Rule::FuzzCrate,
            format!(
                "fuzz crate `{}` is a workspace member; it must be its own workspace root",
                krate.name
            ),
            &krate.manifest,
        ));
    }
    for listed_name in &listed {
        if name != Some(*listed_name) && !members.iter().any(|c| c.name == *listed_name) {
            out.push(Diagnostic::new(
                Rule::FuzzCrate,
                format!(
                    "`[workspace] fuzz_only` names `{listed_name}`, which is not the package of {FUZZ_MANIFEST}"
                ),
                POLICY_FILE,
            ));
        }
    }
    let Some(manifest) = manifest.as_ref() else {
        return out;
    };
    let Some(name) = name else {
        out.push(Diagnostic::new(
            Rule::FuzzCrate,
            format!("{FUZZ_MANIFEST} has no [package] name; the fuzz crate must be a package"),
            FUZZ_MANIFEST,
        ));
        return out;
    };
    if !listed.contains(name) {
        out.push(Diagnostic::new(
            Rule::FuzzCrate,
            format!("fuzz crate `{name}` is not in `[workspace] fuzz_only` of {POLICY_FILE}"),
            FUZZ_MANIFEST,
        ));
        return out;
    }
    if manifest.get("workspace").is_none() {
        out.push(Diagnostic::new(
            Rule::FuzzCrate,
            format!("fuzz crate `{name}` has no `[workspace]` table of its own"),
            FUZZ_MANIFEST,
        ));
    }
    out.extend(check_fuzz_dependencies(policy, manifest, name));
    out
}

/// Every dependency of the fuzz crate, of any kind: a path dependency must name a workspace
/// crate in `[dependencies.normal] <fuzz crate>`, any other must be in
/// `[external.normal] <fuzz crate>` (features are not checked: the fuzz crate has its own
/// lockfile and is built on the nightly only). The target-specific tables
/// (`[target.'cfg(...)'.dependencies]` and the like) count as well.
fn check_fuzz_dependencies(policy: &Policy, manifest: &toml::Table, name: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let workspace_allowed = policy.dependencies.normal.get(name);
    let external_allowed = policy.external.normal.get(name);
    let targets = manifest
        .get("target")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flat_map(|t| t.values().filter_map(toml::Value::as_table));
    let scopes: Vec<&toml::Table> = std::iter::once(manifest).chain(targets).collect();
    let tables = scopes.iter().flat_map(|scope| {
        ["dependencies", "dev-dependencies", "build-dependencies"]
            .into_iter()
            .filter_map(|kind| scope.get(kind).and_then(toml::Value::as_table))
    });
    for table in tables {
        for (key, spec) in table {
            let package = spec
                .get("package")
                .and_then(toml::Value::as_str)
                .unwrap_or(key);
            if spec.get("path").is_some() {
                if !workspace_allowed.is_some_and(|list| list.iter().any(|d| d == package)) {
                    out.push(Diagnostic::new(
                        Rule::FuzzCrate,
                        format!(
                            "fuzz crate `{name}` depends on `{package}`, which is not in `[dependencies.normal] {name}`"
                        ),
                        FUZZ_MANIFEST,
                    ));
                }
            } else if !external_allowed.is_some_and(|list| list.contains_key(package)) {
                out.push(Diagnostic::new(
                    Rule::FuzzCrate,
                    format!(
                        "fuzz crate `{name}` depends on `{package}`, which is not in `[external.normal] {name}`"
                    ),
                    FUZZ_MANIFEST,
                ));
            }
        }
    }
    out
}

/// `[workspace] members` entries of the root manifest that have no `Cargo.toml`. Checked
/// before `cargo metadata`, which cannot load such a workspace at all.
pub fn check_members_on_disk(root: &Path) -> Result<Vec<Diagnostic>, Diagnostic> {
    let manifest = read_toml(root, "Cargo.toml")?;
    let members = manifest
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(toml::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for member in &members {
        let Some(path) = member.as_str() else {
            continue;
        };
        if !root.join(path).join("Cargo.toml").is_file() {
            out.push(Diagnostic::new(
                Rule::MissingMember,
                format!("workspace member `{path}` has no Cargo.toml (members are explicit paths)"),
                "Cargo.toml",
            ));
        }
    }
    Ok(out)
}

/// Every crate in a direct child of [`SOURCE_DIRS`], whether or not it is a member.
pub fn scan_source_tree(root: &Path) -> Result<Vec<Crate>, Diagnostic> {
    let mut out = Vec::new();
    for dir in SOURCE_DIRS {
        let Ok(entries) = fs::read_dir(root.join(dir)) else {
            continue;
        };
        let mut children: Vec<String> = entries
            .filter_map(Result::ok)
            .filter(|e| e.path().join("Cargo.toml").is_file())
            .filter_map(|e| e.file_name().to_str().map(str::to_owned))
            .collect();
        children.sort();
        for child in children {
            let manifest = format!("{dir}/{child}/Cargo.toml");
            let table = read_toml(root, &manifest)?;
            if let Some(name) = table
                .get("package")
                .and_then(|p| p.get("name"))
                .and_then(toml::Value::as_str)
            {
                out.push(Crate {
                    name: name.to_owned(),
                    manifest,
                });
            }
        }
    }
    Ok(out)
}

fn read_toml(root: &Path, relative: &str) -> Result<toml::Table, Diagnostic> {
    let text = fs::read_to_string(root.join(relative))
        .map_err(|e| Diagnostic::new(Rule::Metadata, format!("cannot read: {e}"), relative))?;
    text.parse().map_err(|e: toml::de::Error| {
        Diagnostic::new(
            Rule::Metadata,
            format!("not valid TOML: {}", e.message()),
            relative,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::architecture::policy;

    fn policy(text: &str) -> Policy {
        policy::parse(text).unwrap()
    }

    fn krate(name: &str, dir: &str) -> Crate {
        Crate {
            name: name.to_owned(),
            manifest: format!("{dir}/{name}/Cargo.toml"),
        }
    }

    const TWO: &str = "schema = 1\n[workspace]\nproduction = [\"a\"]\ntool_only = [\"t\"]\n[dependencies.normal]\na = []\nt = []\n";

    #[test]
    fn should_pass_when_policy_members_and_disk_agree() {
        // given
        let crates = [krate("a", "crates"), krate("t", "tools")];

        // when
        let mut diagnostics = check_categories(&policy(TWO));
        diagnostics.extend(check_crates(&policy(TWO), &crates, &crates));

        // then
        assert_eq!(diagnostics, []);
    }

    #[test]
    fn should_report_crate_once_when_listed_in_two_categories() {
        // given
        let text = TWO.replace("tool_only = [\"t\"]", "tool_only = [\"t\", \"a\"]");

        // when
        let diagnostics = check_categories(&policy(&text));

        // then
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].rule, Rule::DuplicateCategory);
        assert!(diagnostics[0].message.contains("production, tool_only"));
    }

    #[test]
    fn should_report_empty_selector_when_category_list_is_empty() {
        // given
        let text = TWO.replace("tool_only = [\"t\"]", "tool_only = []");

        // when
        let diagnostics = check_categories(&policy(&text));

        // then
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].rule, Rule::EmptySelector);
    }

    #[test]
    fn should_report_not_a_member_when_disk_crate_is_not_in_members() {
        // given
        let members = [krate("a", "crates"), krate("t", "tools")];
        let disk = [
            krate("a", "crates"),
            krate("t", "tools"),
            krate("x", "crates"),
        ];

        // when
        let diagnostics = check_crates(&policy(TWO), &members, &disk);

        // then
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].rule, Rule::NotAMember);
        assert_eq!(diagnostics[0].at, "crates/x/Cargo.toml");
    }

    #[test]
    fn should_report_missing_crate_when_dependency_table_names_a_stale_crate() {
        // given
        let text = TWO.replace("a = []", "a = [\"gone\"]");
        let crates = [krate("a", "crates"), krate("t", "tools")];

        // when
        let diagnostics = check_crates(&policy(&text), &crates, &crates);

        // then
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].rule, Rule::MissingCrate);
        assert!(diagnostics[0].message.contains("[dependencies.normal] a"));
    }

    #[test]
    fn should_report_missing_crate_when_external_table_names_a_stale_crate() {
        // given
        let text = format!("{TWO}[external.dev]\ngone = {{ x = \"*\" }}\n");
        let crates = [krate("a", "crates"), krate("t", "tools")];

        // when
        let diagnostics = check_crates(&policy(&text), &crates, &crates);

        // then
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].message.contains("[external.dev]"));
    }

    const FUZZ_POLICY: &str = "schema = 1\n[workspace]\nproduction = [\"a\"]\nfuzz_only = [\"fz\"]\n[dependencies.normal]\na = []\nfz = [\"a\"]\n[external.normal]\nfz = { libfuzzer-sys = [] }\n";

    fn fuzz_root(manifest: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("fuzz")).unwrap();
        fs::write(dir.path().join(FUZZ_MANIFEST), manifest).unwrap();
        dir
    }

    const FUZZ_OK: &str = "[package]\nname = \"fz\"\n[dependencies]\nlibfuzzer-sys = \"0.4\"\na = { path = \"../crates/a\" }\n[workspace]\n";

    #[test]
    fn should_pass_when_fuzz_crate_is_listed_and_self_contained() {
        // given
        let root = fuzz_root(FUZZ_OK);
        let members = [krate("a", "crates")];

        // when
        let diagnostics = check_fuzz(&policy(FUZZ_POLICY), root.path(), &members);
        let names = check_crates(&policy(FUZZ_POLICY), &members, &members);

        // then
        assert_eq!(diagnostics, []);
        assert_eq!(names, []);
    }

    #[test]
    fn should_report_fuzz_crate_when_it_is_not_in_fuzz_only() {
        // given
        let root = fuzz_root(FUZZ_OK);
        let text = "schema = 1\n[workspace]\nproduction = [\"a\"]\n[dependencies.normal]\na = []\n";

        // when
        let diagnostics = check_fuzz(&policy(text), root.path(), &[krate("a", "crates")]);

        // then
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].rule, Rule::FuzzCrate);
        assert!(
            diagnostics[0]
                .message
                .contains("not in `[workspace] fuzz_only`")
        );
    }

    #[test]
    fn should_report_fuzz_crate_when_fuzz_only_names_a_missing_package() {
        // given
        let dir = tempfile::tempdir().unwrap();

        // when
        let diagnostics = check_fuzz(&policy(FUZZ_POLICY), dir.path(), &[krate("a", "crates")]);

        // then
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].message.contains("names `fz`"));
    }

    #[test]
    fn should_report_fuzz_crate_when_it_is_a_workspace_member() {
        // given
        let root = fuzz_root(FUZZ_OK);
        let members = [krate("a", "crates"), krate("fz", "fuzz")];

        // when
        let diagnostics = check_fuzz(&policy(FUZZ_POLICY), root.path(), &members);

        // then
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].message.contains("workspace member"));
    }

    #[test]
    fn should_report_fuzz_crate_when_it_has_no_workspace_table() {
        // given
        let root = fuzz_root(&FUZZ_OK.replace("[workspace]\n", ""));

        // when
        let diagnostics = check_fuzz(&policy(FUZZ_POLICY), root.path(), &[krate("a", "crates")]);

        // then
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].message.contains("no `[workspace]` table"));
    }

    #[test]
    fn should_report_fuzz_crate_when_it_depends_on_unlisted_crates() {
        // given
        let root = fuzz_root(
            "[package]\nname = \"fz\"\n[dependencies]\nrand = \"0.9\"\nb = { path = \"../crates/b\" }\n[workspace]\n",
        );

        // when
        let diagnostics = check_fuzz(&policy(FUZZ_POLICY), root.path(), &[krate("a", "crates")]);

        // then
        assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
        let text: Vec<_> = diagnostics.iter().map(|d| d.message.as_str()).collect();
        assert!(
            text.iter()
                .any(|m| m.contains("`b`") && m.contains("[dependencies.normal] fz"))
        );
        assert!(
            text.iter()
                .any(|m| m.contains("`rand`") && m.contains("[external.normal] fz"))
        );
    }

    #[test]
    fn should_report_fuzz_crate_when_a_target_specific_dependency_is_unlisted() {
        // given
        let root = fuzz_root(
            "[package]\nname = \"fz\"\n[dependencies]\nlibfuzzer-sys = \"0.4\"\n[target.'cfg(unix)'.dependencies]\nlibc = \"0.2\"\n[target.'cfg(unix)'.dev-dependencies]\nb = { path = \"../crates/b\" }\n[workspace]\n",
        );

        // when
        let diagnostics = check_fuzz(&policy(FUZZ_POLICY), root.path(), &[krate("a", "crates")]);

        // then
        let text: Vec<_> = diagnostics.iter().map(|d| d.message.as_str()).collect();
        assert_eq!(text.len(), 2, "{text:?}");
        assert!(
            text.iter()
                .any(|m| m.contains("`libc`") && m.contains("[external.normal] fz"))
        );
        assert!(
            text.iter()
                .any(|m| m.contains("`b`") && m.contains("[dependencies.normal] fz"))
        );
    }
}
