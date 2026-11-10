//! Trace manifests (ver §6.2 item 1): the committed `formal/traces/regression/manifest.toml`
//! and the generated manifest `cargo xtask conformance` writes beside the traces it makes. Each
//! `[[trace]]` names its file (relative to the manifest), the model file, the main module, the
//! init and step relations, `max_steps`, the seed and the invariant, which is everything
//! needed to regenerate it with the locked Quint.

use std::fmt;
use std::path::{Path, PathBuf};

/// One trace of a manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The trace file, relative to the manifest's directory.
    pub file: String,
    /// The model file in `formal/` (`lifecycle.qnt`).
    pub model: String,
    /// The main module.
    pub main: String,
    /// The init relation.
    pub init: String,
    /// The step relation (or a scenario script of a traces module).
    pub step: String,
    /// The bound on the steps: a trace has at most `max_steps + 1` states.
    pub max_steps: usize,
    /// The seed Quint ran with (`0x…` or decimal, as Quint prints it).
    pub seed: String,
    /// The invariant checked while it was made.
    pub invariant: String,
}

impl Entry {
    /// The model's name: the file without `.qnt`.
    #[must_use]
    pub fn model_name(&self) -> &str {
        self.model.strip_suffix(".qnt").unwrap_or(&self.model)
    }

    /// A short id for reports: the file name.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.file
    }
}

/// A manifest, with the directory its relative file names resolve against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// The directory holding the manifest.
    pub dir: PathBuf,
    /// Its traces, in file order.
    pub entries: Vec<Entry>,
}

impl Manifest {
    /// The path of `entry`'s trace.
    #[must_use]
    pub fn path_of(&self, entry: &Entry) -> PathBuf {
        self.dir.join(&entry.file)
    }
}

/// Why a manifest cannot be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestError(pub String);

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "trace manifest: {}", self.0)
    }
}

impl std::error::Error for ManifestError {}

/// Reads the manifest at `path`.
///
/// # Errors
/// [`ManifestError`] when it cannot be read or parsed.
pub fn load(path: &Path) -> Result<Manifest, ManifestError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| ManifestError(format!("cannot read {}: {e}", path.display())))?;
    let dir = path.parent().map_or_else(PathBuf::new, Path::to_path_buf);
    parse(&text, dir)
}

/// Parses manifest `text` whose traces live in `dir`.
///
/// # Errors
/// [`ManifestError`] for TOML errors, a missing or mistyped key, or no `[[trace]]` at all.
pub fn parse(text: &str, dir: PathBuf) -> Result<Manifest, ManifestError> {
    let table: toml::Table = toml::from_str(text).map_err(|e| ManifestError(e.to_string()))?;
    let traces = table
        .get("trace")
        .and_then(toml::Value::as_array)
        .ok_or_else(|| ManifestError("no [[trace]] entry".to_owned()))?;
    let entries = traces
        .iter()
        .enumerate()
        .map(|(i, raw)| entry(i, raw))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Manifest { dir, entries })
}

fn entry(index: usize, raw: &toml::Value) -> Result<Entry, ManifestError> {
    let text = |key: &str| {
        raw.get(key)
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| ManifestError(format!("trace {index} has no string {key:?}")))
    };
    let max_steps = raw
        .get("max_steps")
        .and_then(toml::Value::as_integer)
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| ManifestError(format!("trace {index} has no max_steps")))?;
    Ok(Entry {
        file: text("file")?,
        model: text("model")?,
        main: text("main")?,
        init: text("init")?,
        step: text("step")?,
        max_steps,
        seed: text("seed")?,
        invariant: text("invariant")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_read_every_key_when_a_manifest_lists_a_trace() {
        // given
        let text = "[[trace]]\nfile = \"a.itf.json\"\nmodel = \"lifecycle.qnt\"\nmain = \"lifecycle\"\ninit = \"init\"\nstep = \"step\"\nmax_steps = 4\nseed = \"0x1\"\ninvariant = \"allInvariants\"\n";

        // when
        let manifest = parse(text, PathBuf::from("dir")).unwrap_or_else(|e| panic!("{e}"));

        // then
        let entry = &manifest.entries[0];
        assert_eq!(entry.model_name(), "lifecycle");
        assert_eq!(entry.max_steps, 4);
        assert_eq!(manifest.path_of(entry), PathBuf::from("dir/a.itf.json"));
    }

    #[test]
    fn should_refuse_a_manifest_when_a_key_is_missing_or_there_is_no_trace() {
        // given
        let missing = "[[trace]]\nfile = \"a\"\n";

        // when
        let results = (parse(missing, PathBuf::new()), parse("", PathBuf::new()));

        // then
        assert!(results.0.is_err());
        assert!(results.1.is_err());
    }
}
