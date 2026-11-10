//! Replays a whole manifest: each trace through the driver of its model.

use std::fmt;

use mayfly_control_domain::attempt::Attempt;

use crate::itf;
use crate::manifest::{Entry, Manifest};
use crate::replay::{ReplayError, Replayed, check_length};

/// The models with a driver.
pub const DRIVEN: &[&str] = &["lifecycle"];

/// What happened to one trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Replayed with no divergence.
    Passed(Replayed),
    /// Decoded and checked, but its model has no driver yet.
    NoDriver {
        /// States decoded.
        states: usize,
    },
    /// Ended in a known divergence ([`crate::lifecycle::KNOWN_DIVERGENCES`]): reported, not
    /// failed; the rest of the trace is not compared.
    Known {
        /// The divergence's id.
        id: &'static str,
        /// The state it was found at.
        step: usize,
    },
    /// A divergence, a driver error or a trace error.
    Failed(Box<ReplayError>),
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Passed(r) => write!(f, "pass: {} states compared", r.states),
            Self::NoDriver { states } => {
                write!(f, "decoded ({states} states); no driver for this model yet")
            }
            Self::Known { id, step } => write!(f, "KNOWN divergence {id} at step {step}"),
            Self::Failed(e) => write!(f, "FAIL: {e}"),
        }
    }
}

impl Verdict {
    /// Whether the trace failed.
    #[must_use]
    pub const fn failed(&self) -> bool {
        matches!(self, Self::Failed(..))
    }
}

/// Reads, decodes and replays one entry of `manifest`.
#[must_use]
pub fn trace(manifest: &Manifest, entry: &Entry) -> Verdict {
    let failed = |error| {
        Verdict::Failed(Box::new(ReplayError::Trace {
            trace: entry.id().to_owned(),
            error,
        }))
    };
    let text = match std::fs::read_to_string(manifest.path_of(entry)) {
        Ok(text) => text,
        Err(e) => return failed(itf::TraceError::Shape(format!("cannot read: {e}"))),
    };
    let decoded = match itf::parse(&text) {
        Ok(trace) => trace,
        Err(error) => return failed(error),
    };
    match entry.model_name() {
        "lifecycle" => match crate::lifecycle::replay::<Attempt>(entry, &decoded) {
            Ok(replayed) => Verdict::Passed(replayed),
            Err(error) => match error.divergence().and_then(crate::lifecycle::known) {
                Some(known) => Verdict::Known {
                    id: known.id,
                    step: error.divergence().map_or(0, |d| d.step),
                },
                None => Verdict::Failed(Box::new(error)),
            },
        },
        _ => match check_length(&decoded, entry) {
            Ok(()) => Verdict::NoDriver {
                states: decoded.states.len(),
            },
            Err(error) => failed(error),
        },
    }
}

/// Every entry of `manifest` with its verdict.
#[must_use]
pub fn all(manifest: &Manifest) -> Vec<(Entry, Verdict)> {
    manifest
        .entries
        .iter()
        .map(|entry| (entry.clone(), trace(manifest, entry)))
        .collect()
}

/// Replays every trace of each manifest at `paths`, writing one line per trace to `out`, then
/// the first divergence of each failed one in full to `err`, and a summary line. Returns 0 when
/// no trace failed, 1 otherwise, 2 on a usage or manifest error. Pinned by
/// `tests/run.rs::should_exit_by_verdict_when_manifests_are_replayed`.
pub fn report(
    paths: &[String],
    out: &mut impl std::io::Write,
    err: &mut impl std::io::Write,
) -> u8 {
    if paths.is_empty() {
        let _ = writeln!(err, "usage: mayfly-conformance <manifest.toml>...");
        return 2;
    }
    let (mut passed, mut known, mut undriven, mut failed) = (0_usize, 0_usize, 0_usize, 0_usize);
    for path in paths {
        let manifest = match crate::manifest::load(std::path::Path::new(path)) {
            Ok(m) => m,
            Err(e) => {
                let _ = writeln!(err, "{e}");
                return 2;
            }
        };
        for (entry, verdict) in all(&manifest) {
            let _ = writeln!(
                out,
                "{} [{} --step {} --seed {}]: {}",
                entry.file,
                entry.model_name(),
                entry.step,
                entry.seed,
                verdict.to_string().lines().next().unwrap_or_default()
            );
            match verdict {
                Verdict::Passed(..) => passed += 1,
                Verdict::NoDriver { .. } => undriven += 1,
                Verdict::Known { .. } => known += 1,
                Verdict::Failed(error) => {
                    failed += 1;
                    let _ = writeln!(err, "{error}");
                }
            }
        }
    }
    let _ = writeln!(
        out,
        "conformance: {passed} replayed, {known} ended in a known divergence, {undriven} decoded without a driver, {failed} failed"
    );
    u8::from(failed > 0)
}
