//! `ARCH-pure-clippy-config`: the semantic layer of the pure-crate rules (foundation.md
//! section 2.3). The syntactic rules read paths as written, so `type T = SystemTime;
//! T::now()` through an associated type, `Self::now()` in `impl Now for SystemTime`, or
//! `path.exists()` on a value escape them. Clippy resolves every call and type after macro
//! expansion, so each `[purity] pure` crate carries a `clippy.toml` whose `disallowed-methods`
//! and `disallowed-types` hold at least [`REQUIRED_METHODS`] and [`REQUIRED_TYPES`]; the
//! workspace's `clippy::all = "deny"` makes every hit an error in the `clippy` and
//! `cross-check` stages. This rule keeps the layer from being switched off silently:
//!
//! - the file is missing, or a `.clippy.toml` beside it shadows it (Clippy reads that name
//!   first);
//! - a required entry is missing, or an entry sets `allow-invalid`;
//! - a `.cargo/config.toml` `[env]` sets `CLIPPY_CONF_DIR`, which would point Clippy at
//!   another file for every crate (`cargo xtask` refuses the variable in its own environment).
//!
//! Clippy reads the nearest `clippy.toml` at or above the crate's manifest, so a pure crate's
//! file applies to it alone. Pinned by
//! `should_fail_clippy_stage_when_a_pure_crate_reaches_the_clock_through_names_the_checker_cannot_resolve`,
//! which runs the `clippy` stage's own command over each pure crate's file.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use cargo_metadata::Metadata;

use super::super::policy::Policy;
use super::super::report::{Diagnostic, Rule};
use super::relative;

/// The `disallowed-methods` every pure crate's `clippy.toml` holds.
pub const REQUIRED_METHODS: &[&str] = &[
    "std::time::SystemTime::now",
    "std::time::SystemTime::elapsed",
    "std::time::Instant::now",
    "std::time::Instant::elapsed",
    "std::fs::read",
    "std::fs::read_to_string",
    "std::fs::write",
    "std::fs::read_dir",
    "std::fs::metadata",
    "std::fs::symlink_metadata",
    "std::fs::canonicalize",
    "std::fs::exists",
    "std::fs::read_link",
    "std::fs::create_dir",
    "std::fs::create_dir_all",
    "std::fs::remove_file",
    "std::fs::remove_dir",
    "std::fs::remove_dir_all",
    "std::fs::rename",
    "std::fs::copy",
    "std::fs::hard_link",
    "std::fs::set_permissions",
    "std::fs::File::open",
    "std::fs::File::create",
    "std::fs::OpenOptions::open",
    "std::path::Path::exists",
    "std::path::Path::try_exists",
    "std::path::Path::is_file",
    "std::path::Path::is_dir",
    "std::path::Path::is_symlink",
    "std::path::Path::metadata",
    "std::path::Path::symlink_metadata",
    "std::path::Path::read_dir",
    "std::path::Path::read_link",
    "std::path::Path::canonicalize",
    "std::path::absolute",
    "std::env::var",
    "std::env::var_os",
    "std::env::vars",
    "std::env::vars_os",
    "std::env::args",
    "std::env::args_os",
    "std::env::current_dir",
    "std::env::set_current_dir",
    "std::env::current_exe",
    "std::env::temp_dir",
    "std::env::home_dir",
    "std::env::set_var",
    "std::env::remove_var",
    "std::process::Command::new",
    "std::process::exit",
    "std::process::abort",
    "std::process::id",
    "std::thread::spawn",
    "std::thread::scope",
    "std::thread::current",
    "std::thread::sleep",
    "std::thread::park",
    "std::thread::park_timeout",
    "std::thread::yield_now",
    "std::thread::available_parallelism",
    "std::sync::mpsc::channel",
    "std::sync::mpsc::sync_channel",
    "std::panic::set_hook",
    "std::panic::take_hook",
    "std::io::Error::last_os_error",
    "std::io::stdin",
    "std::io::stdout",
    "std::io::stderr",
    "std::backtrace::Backtrace::capture",
    "std::backtrace::Backtrace::force_capture",
];
/// The `disallowed-types` every pure crate's `clippy.toml` holds.
pub const REQUIRED_TYPES: &[&str] = &[
    "std::collections::HashMap",
    "std::collections::HashSet",
    "std::hash::RandomState",
    "std::fs::File",
    "std::fs::OpenOptions",
    "std::fs::DirEntry",
    "std::fs::ReadDir",
    "std::net::TcpStream",
    "std::net::TcpListener",
    "std::net::UdpSocket",
    "std::process::Command",
    "std::process::Child",
    "std::thread::Builder",
    "std::thread::JoinHandle",
    "std::time::Instant",
    "std::backtrace::Backtrace",
    "std::sync::Mutex",
    "std::sync::RwLock",
    "std::sync::Condvar",
    "std::sync::Once",
    "std::sync::OnceLock",
    "std::sync::LazyLock",
    "std::sync::Barrier",
    "std::sync::atomic::AtomicBool",
    "std::sync::atomic::AtomicI8",
    "std::sync::atomic::AtomicI16",
    "std::sync::atomic::AtomicI32",
    "std::sync::atomic::AtomicI64",
    "std::sync::atomic::AtomicIsize",
    "std::sync::atomic::AtomicU8",
    "std::sync::atomic::AtomicU16",
    "std::sync::atomic::AtomicU32",
    "std::sync::atomic::AtomicU64",
    "std::sync::atomic::AtomicUsize",
    "std::sync::atomic::AtomicPtr",
];

/// The diagnostics for the workspace at `root`: every pure member's `clippy.toml`, and the
/// `.cargo/config.toml` files of the root and of every member (`manifests`, relative to it).
pub fn check(
    root: &Path,
    metadata: &Metadata,
    policy: &Policy,
    manifests: &[String],
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for package in metadata.workspace_packages() {
        if !policy
            .purity
            .pure
            .iter()
            .any(|p| *p == package.name.as_str())
        {
            continue;
        }
        let Some(dir) = package.manifest_path.parent() else {
            continue;
        };
        out.extend(crate_config(root, dir.as_std_path(), package.name.as_str()));
    }
    out.extend(conf_dir_overrides(root, manifests));
    out
}

/// A `.cargo/config.toml` (or `.cargo/config`) of the workspace root or of a member
/// (`manifests`, relative to `root`) whose `[env]` sets `CLIPPY_CONF_DIR`.
fn conf_dir_overrides(root: &Path, manifests: &[String]) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let mut dirs: Vec<String> = vec![String::new()];
    dirs.extend(
        manifests
            .iter()
            .filter_map(|m| m.rsplit_once('/').map(|(dir, _)| format!("{dir}/"))),
    );
    for dir in &dirs {
        for name in [".cargo/config.toml", ".cargo/config"] {
            let rel = format!("{dir}{name}");
            let sets = fs::read_to_string(root.join(&rel))
                .ok()
                .and_then(|text| text.parse::<toml::Table>().ok())
                .and_then(|config| config.get("env").cloned())
                .and_then(|env| env.as_table().map(|t| t.contains_key("CLIPPY_CONF_DIR")));
            if sets == Some(true) {
                out.push(Diagnostic::new(
                    Rule::PureClippyConfig,
                    "`[env] CLIPPY_CONF_DIR` points Clippy past the pure crates' clippy.toml",
                    rel,
                ));
            }
        }
    }
    out
}

/// The problems of the pure crate `name` in `dir`.
fn crate_config(root: &Path, dir: &Path, name: &str) -> Vec<Diagnostic> {
    let rel = relative(root, &dir.join("clippy.toml"));
    let mut out = Vec::new();
    if dir.join(".clippy.toml").is_file() {
        out.push(Diagnostic::new(
            Rule::PureClippyConfig,
            format!(
                "{name} has a `.clippy.toml`, which Clippy reads instead of its `clippy.toml`; remove it"
            ),
            relative(root, &dir.join(".clippy.toml")),
        ));
    }
    let table = match fs::read_to_string(dir.join("clippy.toml")) {
        Ok(text) => match text.parse::<toml::Table>() {
            Ok(table) => table,
            Err(err) => {
                out.push(Diagnostic::new(
                    Rule::PureClippyConfig,
                    format!("{name}'s clippy.toml does not parse: {err}"),
                    rel,
                ));
                return out;
            }
        },
        Err(_) => {
            out.push(Diagnostic::new(
                Rule::PureClippyConfig,
                format!(
                    "pure crate {name} has no clippy.toml: the semantic layer of the purity rules (disallowed-methods and disallowed-types, foundation.md section 2.3) is off"
                ),
                rel,
            ));
            return out;
        }
    };
    for (key, required) in [
        ("disallowed-methods", REQUIRED_METHODS),
        ("disallowed-types", REQUIRED_TYPES),
    ] {
        let (listed, invalid) = entries(table.get(key));
        let missing: Vec<&str> = required
            .iter()
            .copied()
            .filter(|path| !listed.contains(*path))
            .collect();
        if !missing.is_empty() {
            out.push(Diagnostic::new(
                Rule::PureClippyConfig,
                format!(
                    "{name}'s clippy.toml `{key}` lacks {}",
                    missing
                        .iter()
                        .map(|p| format!("`{p}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                rel.clone(),
            ));
        }
        for path in invalid {
            out.push(Diagnostic::new(
                Rule::PureClippyConfig,
                format!(
                    "{name}'s clippy.toml `{key}` entry `{path}` sets `allow-invalid`, so a path that resolves to nothing passes silently"
                ),
                rel.clone(),
            ));
        }
    }
    out
}

/// The paths a `disallowed-*` list names (a string or a `{ path = .. }` table), and the paths
/// of entries that set `allow-invalid`.
fn entries(value: Option<&toml::Value>) -> (BTreeSet<String>, Vec<String>) {
    let mut listed = BTreeSet::new();
    let mut invalid = Vec::new();
    for entry in value.and_then(toml::Value::as_array).into_iter().flatten() {
        let path = match entry {
            toml::Value::String(path) => Some(path.clone()),
            toml::Value::Table(table) => {
                let path = table
                    .get("path")
                    .and_then(toml::Value::as_str)
                    .map(str::to_owned);
                if table.get("allow-invalid") == Some(&toml::Value::Boolean(true)) {
                    invalid.push(path.clone().unwrap_or_default());
                }
                path
            }
            _ => None,
        };
        listed.extend(path);
    }
    (listed, invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, text: &str) {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn complete() -> String {
        let quoted = |list: &[&str]| {
            list.iter()
                .map(|p| format!("\"{p}\""))
                .collect::<Vec<_>>()
                .join(", ")
        };
        format!(
            "disallowed-methods = [{}]\ndisallowed-types = [{}]\n",
            quoted(REQUIRED_METHODS),
            quoted(REQUIRED_TYPES)
        )
    }

    #[test]
    fn should_pass_when_pure_crate_lists_every_required_entry() {
        // given
        let dir = tempfile::TempDir::new().unwrap();
        write(dir.path(), "crates/p/clippy.toml", &complete());

        // when
        let got = crate_config(dir.path(), &dir.path().join("crates/p"), "p");

        // then
        assert_eq!(got, []);
    }

    #[test]
    fn should_report_when_config_is_missing_shadowed_incomplete_or_lenient() {
        // given
        let dir = tempfile::TempDir::new().unwrap();
        let lenient = complete()
            .replace("\"std::time::SystemTime::now\", ", "")
            .replace(
                "\"std::fs::read\"",
                "{ path = \"std::fs::read\", allow-invalid = true }",
            );
        write(dir.path(), "crates/a/clippy.toml", &lenient);
        write(dir.path(), "crates/b/clippy.toml", &complete());
        write(dir.path(), "crates/b/.clippy.toml", "");
        fs::create_dir_all(dir.path().join("crates/c")).unwrap();

        // when
        let got: Vec<String> = ["a", "b", "c"]
            .iter()
            .flat_map(|c| crate_config(dir.path(), &dir.path().join("crates").join(c), c))
            .map(|d| d.to_string())
            .collect();

        // then
        assert_eq!(got.len(), 4, "{got:?}");
        assert!(
            got[0].contains("lacks `std::time::SystemTime::now`"),
            "{got:?}"
        );
        assert!(
            got[1].contains("`std::fs::read` sets `allow-invalid`"),
            "{got:?}"
        );
        assert!(got[2].ends_with("(at crates/b/.clippy.toml)"), "{got:?}");
        assert!(
            got[3].contains("pure crate c has no clippy.toml"),
            "{got:?}"
        );
    }

    /// Code a pure crate must not hold that the syntactic rules cannot see: each line marked
    /// `// bypass` reaches the clock, the file system, a process-global hook or a hash map
    /// through a type alias, `Self`, an associated type or a method call.
    const BYPASSES: &str = r#"//! Reaches what a pure crate must not, through names the checker does not resolve.
use std::time::SystemTime;
/// An alias of the clock.
pub type T = SystemTime;
/// Through a type alias.
pub fn alias() -> T {
    T::now() // bypass
}
/// A clock trait.
pub trait Now {
    /// The time.
    fn n() -> Self;
}
impl Now for SystemTime {
    fn n() -> Self {
        Self::now() // bypass
    }
}
/// A trait with an associated type.
pub trait K {
    /// The type.
    type C;
}
/// A type.
pub struct Z;
impl K for Z {
    type C = SystemTime;
}
/// Through an associated type.
pub fn associated() -> SystemTime {
    <Z as K>::C::now() // bypass
}
/// The file system through a method.
pub fn exists(path: &std::path::Path) -> bool {
    path.exists() // bypass
}
/// A process-global hook and the OS error.
pub fn hooks() -> std::io::Error {
    drop(std::panic::take_hook()); // bypass
    std::io::Error::last_os_error() // bypass
}
/// A hash map through an alias.
pub type Map = std::collections::HashMap<u8, u8>; // bypass
"#;

    /// Runs `program args` in `dir` with `target` as its target directory and none of the
    /// outer build's wrappers, flags or Clippy configuration directory.
    fn run_in(dir: &Path, target: &Path, program: &str, args: &[String]) -> std::process::Output {
        let mut command = std::process::Command::new(program);
        command
            .args(args)
            .current_dir(dir)
            .env("CARGO_TARGET_DIR", target);
        for name in [
            "CLIPPY_CONF_DIR",
            "RUSTC",
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
            "RUSTFLAGS",
            "CARGO_ENCODED_RUSTFLAGS",
            "CARGO_BUILD_RUSTFLAGS",
            "RUSTDOCFLAGS",
            "LLVM_PROFILE_FILE",
            "CARGO_LLVM_COV",
            "CARGO_LLVM_COV_TARGET_DIR",
        ] {
            command.env_remove(name);
        }
        command.output().expect("cargo runs")
    }

    /// A one-crate workspace under `parent` named `name`, holding [`BYPASSES`] and, when
    /// given, `clippy` as the crate's clippy.toml; the repository's toolchain file and a
    /// lockfile, so the stage's `--locked` holds.
    fn workspace(parent: &Path, name: &str, clippy: Option<&Path>) -> std::path::PathBuf {
        let dir = parent.join(name);
        write(
            &dir,
            "Cargo.toml",
            &format!("[workspace]\nresolver = \"3\"\nmembers = [\"crates/{name}\"]\n"),
        );
        fs::copy(
            crate::repo::root().join("rust-toolchain.toml"),
            dir.join("rust-toolchain.toml"),
        )
        .unwrap();
        write(
            &dir,
            &format!("crates/{name}/Cargo.toml"),
            &format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
        );
        write(&dir, &format!("crates/{name}/src/lib.rs"), BYPASSES);
        if let Some(clippy) = clippy {
            fs::copy(clippy, dir.join(format!("crates/{name}/clippy.toml"))).unwrap();
        }
        let lock = run_in(
            &dir,
            &parent.join("target"),
            "cargo",
            &["generate-lockfile".to_owned(), "--offline".to_owned()],
        );
        assert!(lock.status.success(), "{lock:?}");
        dir
    }

    #[test]
    fn should_fail_clippy_stage_when_a_pure_crate_reaches_the_clock_through_names_the_checker_cannot_resolve()
     {
        // given: each pure crate's own clippy.toml beside code that reaches the clock, the file
        // system, a hook and a hash map through names the syntactic rules do not resolve, and
        // the same code without a clippy.toml, each linted by the `clippy` stage's command
        let repo = crate::repo::root();
        let parent = tempfile::TempDir::new().unwrap();
        let stage = crate::quick::stages()
            .into_iter()
            .find(|s| s.name == "clippy")
            .expect("the clippy stage");
        let crate::process::Action::Command { program, args, .. } = stage.action else {
            panic!("the clippy stage runs a command");
        };
        let pure = [
            "mayfly-domain",
            "mayfly-protocol",
            "mayfly-control-domain",
            "mayfly-data-domain",
        ];
        let lines: Vec<usize> = BYPASSES
            .lines()
            .enumerate()
            .filter(|(_, l)| l.ends_with("// bypass"))
            .map(|(i, _)| i + 1)
            .collect();

        // when
        let linted: Vec<(&str, std::process::Output)> = pure
            .iter()
            .map(|name| {
                let clippy = repo.join("crates").join(name).join("clippy.toml");
                let dir = workspace(parent.path(), name, Some(&clippy));
                (
                    *name,
                    run_in(&dir, &parent.path().join("target"), &program, &args),
                )
            })
            .collect();
        let unchecked = workspace(parent.path(), "unchecked", None);
        let control = run_in(&unchecked, &parent.path().join("target"), &program, &args);

        // then
        assert_eq!(lines.len(), 7);
        for (name, output) in &linted {
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(!output.status.success(), "{name}: {stderr}");
            let missed: Vec<String> = lines
                .iter()
                .map(|line| format!("crates/{name}/src/lib.rs:{line}:"))
                .filter(|at| !stderr.contains(at.as_str()))
                .collect();
            assert!(
                missed.is_empty(),
                "{name}: not reported: {missed:?}\n{stderr}"
            );
            assert!(
                !stderr.contains("allow-invalid"),
                "{name}: a clippy.toml path resolves to nothing: {stderr}"
            );
        }
        assert!(
            control.status.success(),
            "{}",
            String::from_utf8_lossy(&control.stderr)
        );
    }

    #[test]
    fn should_report_when_cargo_config_points_clippy_elsewhere() {
        // given
        let dir = tempfile::TempDir::new().unwrap();
        write(
            dir.path(),
            ".cargo/config.toml",
            "[env]\nCLIPPY_CONF_DIR = { value = \"lint\", relative = true }\n",
        );
        write(
            dir.path(),
            "crates/a/.cargo/config",
            "[env]\nCLIPPY_CONF_DIR = \"/elsewhere\"\n",
        );
        write(
            dir.path(),
            "crates/b/.cargo/config.toml",
            "[env]\nOTHER = \"1\"\n",
        );

        // when
        let got: Vec<String> = conf_dir_overrides(
            dir.path(),
            &[
                "crates/a/Cargo.toml".to_owned(),
                "crates/b/Cargo.toml".to_owned(),
            ],
        )
        .iter()
        .map(|d| d.at.clone())
        .collect();

        // then
        assert_eq!(got, [".cargo/config.toml", "crates/a/.cargo/config"]);
    }

    #[test]
    fn should_list_every_required_entry_when_repository_config_is_read() {
        // given
        let root = crate::repo::root();

        // when
        let got: Vec<Diagnostic> = [
            "mayfly-domain",
            "mayfly-protocol",
            "mayfly-control-domain",
            "mayfly-data-domain",
        ]
        .iter()
        .flat_map(|c| crate_config(&root, &root.join("crates").join(c), c))
        .collect();

        // then
        assert_eq!(got, []);
    }
}
