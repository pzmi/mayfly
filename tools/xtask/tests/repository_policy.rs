//! Every registry dependency names a full `major.minor.patch` version, as `cargo add` writes
//! it, so a manifest says which release was reviewed.

use std::fs;
use std::path::{Path, PathBuf};

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("xtask lives two levels below the repository root")
        .to_path_buf()
}

fn is_full_version(text: &str) -> bool {
    let parts: Vec<_> = text.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// `(table, dependency, requirement)` for every requirement in one manifest's dependency
/// tables that is not a full version.
fn partial_versions(manifest: &toml::Table) -> Vec<(String, String, String)> {
    let mut tables: Vec<(String, &toml::Table)> = Vec::new();
    for kind in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(t) = manifest.get(kind).and_then(toml::Value::as_table) {
            tables.push((kind.to_owned(), t));
        }
    }
    if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
        for (target, value) in targets {
            for kind in ["dependencies", "dev-dependencies", "build-dependencies"] {
                if let Some(t) = value.get(kind).and_then(toml::Value::as_table) {
                    tables.push((format!("target.{target}.{kind}"), t));
                }
            }
        }
    }
    let mut out = Vec::new();
    for (table, deps) in tables {
        for (name, spec) in deps {
            let requirement = match spec {
                toml::Value::String(v) => Some(v.as_str()),
                toml::Value::Table(t) => t.get("version").and_then(toml::Value::as_str),
                _ => None,
            };
            if let Some(req) = requirement
                && !is_full_version(req)
            {
                out.push((table.clone(), name.clone(), req.to_owned()));
            }
        }
    }
    out
}

fn manifests(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for parent in ["crates", "tools"] {
        let listing = fs::read_dir(root.join(parent)).expect("list crates");
        for entry in listing {
            let manifest = entry.expect("entry").path().join("Cargo.toml");
            if manifest.is_file() {
                out.push(manifest);
            }
        }
    }
    out.sort();
    out
}

#[test]
fn should_name_full_versions_when_crates_declare_registry_dependencies() {
    // given
    let root = repository_root();
    let files = manifests(&root);

    // when
    let partial: Vec<_> = files
        .iter()
        .flat_map(|path| {
            let text = fs::read_to_string(path).expect("read manifest");
            let manifest: toml::Table = text.parse().expect("parse manifest");
            partial_versions(&manifest)
                .into_iter()
                .map(|p| (path.strip_prefix(&root).unwrap_or(path).to_owned(), p))
                .collect::<Vec<_>>()
        })
        .collect();

    // then
    assert!(files.len() > 1, "{files:?}");
    assert!(partial.is_empty(), "{partial:?}");
}

#[test]
fn should_report_requirement_when_it_omits_the_patch_version() {
    // given
    let manifest: toml::Table =
        "[dependencies]\naxum = { version = \"0.8\" }\nfoo = \"1.2.3\"\nbar = \"^1.2.3\"\n"
            .parse()
            .expect("parse");

    // when
    let partial = partial_versions(&manifest);

    // then
    let names: Vec<_> = partial.iter().map(|(_, n, _)| n.as_str()).collect();
    assert_eq!(names, ["axum", "bar"]);
}
