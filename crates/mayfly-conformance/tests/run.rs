//! Replaying whole manifests: the verdict of each trace and the replay binary's report and exit
//! code (`mayfly_conformance::run`).

use std::path::{Path, PathBuf};

use mayfly_conformance::manifest::{self, Entry, Manifest};
use mayfly_conformance::replay::ReplayError;
use mayfly_conformance::run::{self, Verdict};

fn regression_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../formal/traces/regression/manifest.toml")
}

fn entry(file: &str, model: &str, max_steps: usize) -> Entry {
    Entry {
        file: file.to_owned(),
        model: model.to_owned(),
        main: "m".to_owned(),
        init: "init".to_owned(),
        step: "step".to_owned(),
        max_steps,
        seed: "0x1".to_owned(),
        invariant: "allInvariants".to_owned(),
    }
}

fn report(paths: &[String]) -> (u8, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run::report(paths, &mut out, &mut err);
    (
        code,
        String::from_utf8_lossy(&out).into_owned(),
        String::from_utf8_lossy(&err).into_owned(),
    )
}

/// A manifest in this test's own scratch directory, naming traces of the regression directory.
fn scratch_manifest(name: &str, entries: &[Entry]) -> String {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
    let regression = regression_path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let mut text = String::new();
    for e in entries {
        text.push_str(&format!(
            "[[trace]]\nfile = \"{}\"\nmodel = \"{}\"\nmain = \"{}\"\ninit = \"{}\"\nstep = \"{}\"\nmax_steps = {}\nseed = \"{}\"\ninvariant = \"{}\"\n\n",
            regression.join(&e.file).display(),
            e.model,
            e.main,
            e.init,
            e.step,
            e.max_steps,
            e.seed,
            e.invariant
        ));
    }
    let path = dir.join("manifest.toml");
    std::fs::write(&path, text).unwrap_or_else(|e| panic!("{e}"));
    path.display().to_string()
}

#[test]
fn should_give_each_trace_its_verdict_when_the_regression_manifest_is_replayed() {
    // given
    let manifest = manifest::load(&regression_path()).unwrap_or_else(|e| panic!("{e}"));

    // when
    let verdicts = run::all(&manifest);

    // then
    let count = |f: fn(&Verdict) -> bool| verdicts.iter().filter(|(_, v)| f(v)).count();
    assert_eq!(count(|v| matches!(v, Verdict::Known { .. })), 4);
    assert!(count(|v| matches!(v, Verdict::Passed(..))) >= 12);
    assert!(count(|v| matches!(v, Verdict::NoDriver { .. })) >= 30);
    assert_eq!(count(Verdict::failed), 0);
    assert!(verdicts.iter().all(|(_, v)| !v.to_string().is_empty()));
}

#[test]
fn should_fail_a_trace_when_it_cannot_be_read_decoded_or_replayed() {
    // given: a missing file, an event-store trace read as a lifecycle one, and one too long
    let dir = regression_path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let manifest = Manifest {
        dir,
        entries: vec![
            entry("absent.itf.json", "lifecycle.qnt", 10),
            entry("event_store-lostRace.itf.json", "lifecycle.qnt", 10),
            entry("event_store-lostRace.itf.json", "event_store.qnt", 1),
            entry("manifest.toml", "event_store.qnt", 1),
        ],
    };

    // when
    let verdicts: Vec<Verdict> = run::all(&manifest).into_iter().map(|(_, v)| v).collect();

    // then
    assert!(verdicts.iter().all(Verdict::failed), "{verdicts:?}");
    assert!(
        matches!(&verdicts[1], Verdict::Failed(e) if matches!(**e, ReplayError::Driver { .. }))
    );
    let shown: Vec<String> = verdicts.iter().map(ToString::to_string).collect();
    assert!(
        shown.iter().all(|s| s.starts_with("FAIL: trace ")),
        "{shown:?}"
    );
}

#[test]
fn should_exit_by_verdict_when_manifests_are_replayed() {
    // given
    let passing = regression_path().display().to_string();
    let failing = scratch_manifest(
        "failing",
        &[entry("event_store-lostRace.itf.json", "lifecycle.qnt", 10)],
    );

    // when
    let none = report(&[]);
    let missing = report(&["no-such-manifest.toml".to_owned()]);
    let green = report(&[passing]);
    let red = report(&[failing]);

    // then
    assert_eq!(none.0, 2);
    assert_eq!(missing.0, 2);
    assert_eq!(green.0, 0, "{}", green.2);
    assert!(green.1.contains("KNOWN divergence"), "{}", green.1);
    assert!(green.1.contains("no driver for this model yet"));
    assert!(green.1.contains(", 0 failed"));
    assert_eq!(red.0, 1);
    assert!(red.1.contains(", 1 failed"), "{}", red.1);
    assert!(!red.2.is_empty());
}
