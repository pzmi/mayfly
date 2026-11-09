//! `cargo xtask land <ref>... (--push <remote> <branch> [--also <branch>]... | --no-push)`:
//! cherry-pick lane commits onto `main`, gate them, and push only a green tree (D4;
//! site-preview ADR 0081).
//!
//! It replaces an untracked orchestrator script that resolved every conflict by deleting
//! conflict markers and pushed to a defaulted branch. Here:
//!
//! - it starts only from a clean `main` checkout equal to the remote's `main` after a fetch,
//!   with no merge, cherry-pick or rebase in progress;
//! - the picks happen on a detached `HEAD`, so `main` moves (fast-forward) only after the full
//!   `cargo xtask check` is green;
//! - a conflict is resolved automatically only in `Cargo.lock` (the integration side, then
//!   `cargo update -w` re-resolves it) and in the ADR index (`adr index --fix`, rows derived
//!   from the files); any other conflict aborts the pick, leaves `main` where it was and names
//!   the files;
//! - push targets are flags, never defaulted; a push is never forced and is retried only on
//!   what looks like a network failure, after 2, 4, 8 and 16 seconds;
//! - every external program runs through `Command` with an argument list, never a shell.

use std::fmt::Write as _;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

use crate::adr;
use crate::adr_files::ADR_INDEX;
use crate::repo;

const LOCKFILE: &str = "Cargo.lock";
const PUSH_DELAYS: [u64; 4] = [2, 4, 8, 16];

/// Where the landed `main` goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Push `branch` to `remote`, then `branch` to each of `also` on the same remote.
    Push {
        /// The remote, such as `origin`.
        remote: String,
        /// The branch landed on, such as `main`; also the branch the checkout must be on.
        branch: String,
        /// Further remote branches set to the same commit.
        also: Vec<String>,
    },
    /// Land on the local `main` only; the base is `origin/main`.
    NoPush,
}

impl Target {
    fn base(&self) -> (&str, &str) {
        match self {
            Self::Push { remote, branch, .. } => (remote, branch),
            Self::NoPush => ("origin", "main"),
        }
    }
}

/// The side effects `land` needs besides git: re-resolving the lockfile, the gate and the
/// pause between push attempts. Tests substitute a fake.
pub trait Toolbox {
    /// Re-resolves `Cargo.lock` in `dir` for the merged manifests.
    fn refresh_lockfile(&self, dir: &Path) -> Result<(), String>;
    /// Runs the full gate in `dir`; true when green.
    fn gate(&self, dir: &Path) -> bool;
    /// Waits before the next push attempt.
    fn pause(&self, delay: Duration);
}

/// The real toolbox: `cargo update -w`, `cargo xtask check`, a thread sleep.
struct Cargo {
    log: Option<PathBuf>,
}

impl Toolbox for Cargo {
    fn refresh_lockfile(&self, dir: &Path) -> Result<(), String> {
        let status = Command::new("cargo")
            .args(["update", "--workspace"])
            .current_dir(dir)
            .status()
            .map_err(|e| format!("cannot run cargo update: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err("cargo update --workspace failed".to_owned())
        }
    }

    fn gate(&self, dir: &Path) -> bool {
        let mut command = Command::new("cargo");
        command.args(["xtask", "check"]).current_dir(dir);
        if let Some(log) = &self.log {
            let files = File::create(log).and_then(|f| Ok((f.try_clone()?, f)));
            match files {
                Ok((out, err)) => {
                    println!("land: cargo xtask check, output in {}", log.display());
                    command.stdout(Stdio::from(out)).stderr(Stdio::from(err));
                }
                Err(e) => {
                    eprintln!("land: cannot create {}: {e}", log.display());
                    return false;
                }
            }
        }
        command.status().is_ok_and(|s| s.success())
    }

    fn pause(&self, delay: Duration) {
        sleep(delay);
    }
}

/// `cargo xtask land`.
pub fn run(refs: &[String], target: &Target, log: Option<PathBuf>) -> i32 {
    report(land(&repo::root(), refs, target, &Cargo { log }))
}

/// Prints the outcome and returns the exit code.
fn report(result: Result<String, String>) -> i32 {
    match result {
        Ok(message) => {
            println!("land: {message}");
            0
        }
        Err(err) => {
            eprintln!("land: {err}");
            1
        }
    }
}

/// The state of the checkout that decides whether landing may start.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoState {
    /// The current branch, `None` when `HEAD` is detached.
    pub branch: Option<String>,
    /// A merge, cherry-pick, revert or rebase in progress.
    pub operation: Option<String>,
    /// `git status --porcelain` lines: changed, staged or untracked (not ignored) files.
    pub dirty: Vec<String>,
    /// `HEAD`'s commit.
    pub head: String,
    /// The remote-tracking branch's commit after the fetch, `None` when absent.
    pub upstream: Option<String>,
}

/// Why landing must not start from `state` onto `remote`/`branch`; empty when it may.
pub fn refusals(state: &RepoState, remote: &str, branch: &str) -> Vec<String> {
    let mut out = Vec::new();
    match &state.branch {
        Some(b) if b == branch => {}
        Some(b) => out.push(format!("the checkout is on `{b}`, not `{branch}`")),
        None => out.push(format!("HEAD is detached; check out `{branch}`")),
    }
    if let Some(op) = &state.operation {
        out.push(format!("a {op} is in progress; finish or abort it"));
    }
    if !state.dirty.is_empty() {
        out.push(format!(
            "the working tree is not clean: {}",
            state.dirty.join(", ")
        ));
    }
    match &state.upstream {
        None => out.push(format!("there is no `{remote}/{branch}` to land on")),
        Some(up) if *up != state.head => out.push(format!(
            "`{branch}` ({}) is not `{remote}/{branch}` ({}); bring it up to date first",
            short(&state.head),
            short(up)
        )),
        Some(_) => {}
    }
    out
}

/// A branch name given as a push target: no option-like, forcing or refspec syntax.
pub fn valid_branch(name: &str) -> Result<(), String> {
    let bad = name.is_empty()
        || name.starts_with(['-', '+'])
        || name.contains([':', ' ', '\\', '~', '^', '?', '*', '['])
        || name.contains("..");
    if bad {
        Err(format!("`{name}` is not a branch name land pushes to"))
    } else {
        Ok(())
    }
}

/// How the conflicted files of one cherry-pick are handled.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Conflicts {
    /// `Cargo.lock` conflicted: take the integration side, then `cargo update -w`.
    pub lockfile: bool,
    /// The ADR index conflicted: `adr index --fix`.
    pub adr_index: bool,
    /// Everything else: the pick is aborted and these are reported.
    pub manual: Vec<String>,
}

/// Sorts conflicted paths into the two automatic resolutions and the rest.
pub fn classify(paths: &[String]) -> Conflicts {
    let mut out = Conflicts::default();
    for path in paths {
        match path.as_str() {
            LOCKFILE => out.lockfile = true,
            ADR_INDEX => out.adr_index = true,
            _ => out.manual.push(path.clone()),
        }
    }
    out
}

/// What a failed push said.
#[derive(Debug, PartialEq, Eq)]
pub enum PushFailure {
    /// Worth another attempt (a network or server error).
    Transient(String),
    /// Retrying cannot help (rejected, not a fast-forward, denied).
    Final(String),
}

/// Classifies a failed push by its stderr.
pub fn push_failure(stderr: &str) -> PushFailure {
    const FINAL: &[&str] = &[
        "[rejected]",
        "[remote rejected]",
        "non-fast-forward",
        "fetch first",
        "Permission denied",
        "permission denied",
        "protected branch",
        "hook declined",
        "does not match any",
        "403",
    ];
    if FINAL.iter().any(|needle| stderr.contains(needle)) {
        PushFailure::Final(stderr.trim().to_owned())
    } else {
        PushFailure::Transient(stderr.trim().to_owned())
    }
}

/// Runs `attempt` until it succeeds, a failure is final, or the delays are used up, pausing
/// for each delay in turn between attempts.
pub fn with_retries(
    delays: &[u64],
    attempt: &mut dyn FnMut() -> Result<(), PushFailure>,
    pause: &mut dyn FnMut(Duration),
) -> Result<(), String> {
    let mut delays = delays.iter();
    loop {
        match attempt() {
            Ok(()) => return Ok(()),
            Err(PushFailure::Final(err)) => return Err(err),
            Err(PushFailure::Transient(err)) => match delays.next() {
                Some(secs) => {
                    eprintln!("land: transient failure, retrying in {secs}s: {err}");
                    pause(Duration::from_secs(*secs));
                }
                None => return Err(format!("gave up after retries: {err}")),
            },
        }
    }
}

/// Git in one directory.
struct Git<'a> {
    dir: &'a Path,
}

impl Git<'_> {
    fn output(&self, args: &[&str]) -> Result<std::process::Output, String> {
        Command::new("git")
            .args(args)
            .current_dir(self.dir)
            .env("GIT_EDITOR", "true")
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("cannot run git: {e}"))
    }

    /// Stdout of a successful run, trimmed; the error carries stderr.
    fn run(&self, args: &[&str]) -> Result<String, String> {
        let out = self.output(args)?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
        } else {
            Err(format!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    fn lines(&self, args: &[&str]) -> Result<Vec<String>, String> {
        Ok(self
            .run(args)?
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect())
    }
}

/// Reads the checkout's state, fetching `remote`/`branch` first (with retries).
fn read_state(
    git: &Git<'_>,
    remote: &str,
    branch: &str,
    tools: &dyn Toolbox,
) -> Result<RepoState, String> {
    let branch_now = git
        .run(&["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok();
    let mut operation = None;
    for (marker, name) in [
        ("MERGE_HEAD", "merge"),
        ("CHERRY_PICK_HEAD", "cherry-pick"),
        ("REVERT_HEAD", "revert"),
        ("rebase-merge", "rebase"),
        ("rebase-apply", "rebase"),
    ] {
        let path = git.run(&["rev-parse", "--git-path", marker])?;
        if git.dir.join(path).exists() {
            operation = Some(name.to_owned());
            break;
        }
    }
    let dirty = git.lines(&["status", "--porcelain"])?;
    let refspec = format!("+refs/heads/{branch}:refs/remotes/{remote}/{branch}");
    with_retries(
        &PUSH_DELAYS,
        &mut || {
            git.run(&["fetch", "--quiet", remote, &refspec])
                .map(|_| ())
                .map_err(|e| push_failure(&e))
        },
        &mut |d| tools.pause(d),
    )
    .map_err(|e| format!("cannot fetch {remote}/{branch}: {e}"))?;
    let head = git.run(&["rev-parse", "HEAD"])?;
    let tracking = format!("refs/remotes/{remote}/{branch}");
    let upstream = git
        .run(&["rev-parse", "--verify", "--quiet", &tracking])
        .ok();
    Ok(RepoState {
        branch: branch_now,
        operation,
        dirty,
        head,
        upstream,
    })
}

/// Expands each ref (a commit, or a range `a..b` in order) into commit ids.
fn commits(git: &Git<'_>, refs: &[String]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for r in refs {
        if r.starts_with('-') {
            return Err(format!("`{r}` is not a commit"));
        }
        if r.contains("..") {
            let listed = git.lines(&["rev-list", "--reverse", r])?;
            if listed.is_empty() {
                return Err(format!("`{r}` names no commits"));
            }
            out.extend(listed);
        } else {
            out.push(
                git.run(&[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{r}^{{commit}}"),
                ])
                .map_err(|_| format!("`{r}` is not a commit"))?,
            );
        }
    }
    Ok(out)
}

/// Lands `refs` on the checkout at `root`. `Ok` carries a summary; on `Err`, `main` is where
/// it was (unless only a push failed, which the message says).
pub fn land(
    root: &Path,
    refs: &[String],
    target: &Target,
    tools: &dyn Toolbox,
) -> Result<String, String> {
    let git = Git { dir: root };
    let (remote, branch) = target.base();
    valid_branch(branch)?;
    if let Target::Push { also, .. } = target {
        also.iter().try_for_each(|b| valid_branch(b))?;
    }
    if refs.is_empty() {
        return Err("name at least one commit to land".to_owned());
    }
    let state = read_state(&git, remote, branch, tools)?;
    let refused = refusals(&state, remote, branch);
    if !refused.is_empty() {
        return Err(format!("refusing to land:\n  {}", refused.join("\n  ")));
    }
    let picks = commits(&git, refs)?;
    git.run(&["checkout", "--quiet", "--detach"])?;
    let landed = pick_all(&git, &picks, tools).and_then(|()| {
        tools.refresh_lockfile(root)?;
        if !git
            .lines(&["status", "--porcelain", "--", LOCKFILE])?
            .is_empty()
        {
            git.run(&["add", "--", LOCKFILE])?;
            git.run(&["commit", "--quiet", "--amend", "--no-edit"])?;
        }
        let sha = git.run(&["rev-parse", "HEAD"])?;
        if tools.gate(root) {
            Ok(sha)
        } else {
            Err(format!(
                "cargo xtask check failed on the landed tree {}; `{branch}` is unchanged",
                short(&sha)
            ))
        }
    });
    if landed.is_err() {
        // Only the detached work can be dirty here (the checkout was clean): drop it, so the
        // branch is checked out exactly as it was.
        let _ = git.run(&["reset", "--quiet", "--hard"]);
    }
    git.run(&["checkout", "--quiet", branch])?;
    let sha = landed?;
    git.run(&["merge", "--quiet", "--ff-only", &sha])?;
    let mut summary = format!("{branch} is at {} ({} commit(s))", short(&sha), picks.len());
    if let Target::Push {
        remote,
        branch,
        also,
    } = target
    {
        let mut refspecs = vec![format!("{branch}:refs/heads/{branch}")];
        refspecs.extend(also.iter().map(|a| format!("{branch}:refs/heads/{a}")));
        for refspec in &refspecs {
            push(&git, remote, refspec, tools)
                .map_err(|e| format!("{summary} locally, but pushing {refspec} failed: {e}"))?;
            let _ = write!(summary, "; pushed {refspec}");
        }
        let remote_sha = git.run(&["ls-remote", remote, &format!("refs/heads/{branch}")])?;
        if !remote_sha.starts_with(&sha) {
            return Err(format!("{summary}, but {remote} reports `{remote_sha}`"));
        }
    }
    Ok(summary)
}

/// Cherry-picks each commit, resolving only the lockfile and ADR index; on any other conflict
/// or failure the pick is aborted and the error names the files.
fn pick_all(git: &Git<'_>, picks: &[String], tools: &dyn Toolbox) -> Result<(), String> {
    for sha in picks {
        let Err(err) = git.run(&["cherry-pick", sha]) else {
            continue;
        };
        let unmerged = git.lines(&["diff", "--name-only", "--diff-filter=U"])?;
        let plan = classify(&unmerged);
        if unmerged.is_empty() || !plan.manual.is_empty() {
            let _ = git.run(&["cherry-pick", "--abort"]);
            return Err(if unmerged.is_empty() {
                format!("cherry-pick of {} failed: {err}", short(sha))
            } else {
                format!(
                    "cherry-pick of {} conflicts in files land does not resolve: {}",
                    short(sha),
                    plan.manual.join(", ")
                )
            });
        }
        if let Err(e) = resolve(git, &plan, tools) {
            let _ = git.run(&["cherry-pick", "--abort"]);
            return Err(format!(
                "cannot resolve the conflict of {}: {e}",
                short(sha)
            ));
        }
        if let Err(e) = git.run(&["cherry-pick", "--continue"]) {
            let _ = git.run(&["cherry-pick", "--abort"]);
            return Err(e);
        }
        println!("land: {} resolved automatically", short(sha));
    }
    Ok(())
}

fn resolve(git: &Git<'_>, plan: &Conflicts, tools: &dyn Toolbox) -> Result<(), String> {
    if plan.lockfile {
        git.run(&["checkout", "--ours", "--", LOCKFILE])?;
        tools.refresh_lockfile(git.dir)?;
        git.run(&["add", "--", LOCKFILE])?;
    }
    if plan.adr_index {
        adr::fix_index(git.dir)?;
        git.run(&["add", "--", ADR_INDEX])?;
    }
    Ok(())
}

fn push(git: &Git<'_>, remote: &str, refspec: &str, tools: &dyn Toolbox) -> Result<(), String> {
    with_retries(
        &PUSH_DELAYS,
        &mut || match git.output(&["push", "--porcelain", remote, refspec]) {
            Ok(out) if out.status.success() => Ok(()),
            Ok(out) => Err(push_failure(&format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ))),
            Err(e) => Err(PushFailure::Final(e)),
        },
        &mut |d| tools.pause(d),
    )
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::fs;

    // ---- pure decisions ------------------------------------------------------------

    fn ready() -> RepoState {
        RepoState {
            branch: Some("main".to_owned()),
            operation: None,
            dirty: Vec::new(),
            head: "a".repeat(40),
            upstream: Some("a".repeat(40)),
        }
    }

    #[test]
    fn should_allow_landing_when_main_is_clean_and_current() {
        // given
        let state = ready();

        // when
        let refused = refusals(&state, "origin", "main");

        // then
        assert!(refused.is_empty(), "{refused:?}");
    }

    #[test]
    fn should_refuse_when_any_precondition_fails() {
        // given
        let cases: Vec<(RepoState, &str)> = vec![
            (
                RepoState {
                    branch: Some("lane".to_owned()),
                    ..ready()
                },
                "is on `lane`, not `main`",
            ),
            (
                RepoState {
                    branch: None,
                    ..ready()
                },
                "HEAD is detached",
            ),
            (
                RepoState {
                    operation: Some("cherry-pick".to_owned()),
                    ..ready()
                },
                "a cherry-pick is in progress",
            ),
            (
                RepoState {
                    dirty: vec![" M src/a.rs".to_owned()],
                    ..ready()
                },
                "not clean:  M src/a.rs",
            ),
            (
                RepoState {
                    upstream: None,
                    ..ready()
                },
                "no `origin/main`",
            ),
            (
                RepoState {
                    upstream: Some("b".repeat(40)),
                    ..ready()
                },
                "is not `origin/main` (bbbbbbbbbbbb)",
            ),
        ];

        // when
        let refused: Vec<Vec<String>> = cases
            .iter()
            .map(|(state, _)| refusals(state, "origin", "main"))
            .collect();

        // then
        for ((_, want), got) in cases.iter().zip(&refused) {
            assert_eq!(got.len(), 1, "{got:?}");
            assert!(got[0].contains(want), "{got:?} lacks {want}");
        }
    }

    #[test]
    fn should_resolve_only_the_lockfile_and_index_when_conflicts_are_classified() {
        // given
        let paths: Vec<String> = [
            "Cargo.lock",
            "doc/adr/README.md",
            "src/a.rs",
            "fuzz/Cargo.lock",
            "Cargo.toml",
        ]
        .map(str::to_owned)
        .to_vec();

        // when
        let plan = classify(&paths);

        // then
        assert_eq!(
            plan,
            Conflicts {
                lockfile: true,
                adr_index: true,
                manual: ["src/a.rs", "fuzz/Cargo.lock", "Cargo.toml"]
                    .map(str::to_owned)
                    .to_vec(),
            }
        );
        assert_eq!(classify(&[]), Conflicts::default());
    }

    #[test]
    fn should_reject_a_branch_name_when_it_could_force_or_inject() {
        // given
        let names = [
            "main",
            "lane-c",
            "claude/fix-1",
            "+main",
            "-f",
            "a:b",
            "",
            "a..b",
            "x y",
        ];

        // when
        let valid: Vec<bool> = names.iter().map(|n| valid_branch(n).is_ok()).collect();

        // then
        assert_eq!(
            valid,
            [true, true, true, false, false, false, false, false, false]
        );
    }

    #[test]
    fn should_retry_only_transient_push_failures_when_classified() {
        // given
        let outputs = [
            "fatal: unable to access 'https://x/': Could not resolve host",
            "error: RPC failed; HTTP 502",
            " ! [rejected]        main -> main (fetch first)",
            "! [remote rejected] main -> main (protected branch hook declined)",
            "remote: Permission to x denied. HTTP 403",
        ];

        // when
        let kinds: Vec<bool> = outputs
            .iter()
            .map(|o| matches!(push_failure(o), PushFailure::Transient(_)))
            .collect();

        // then
        assert_eq!(kinds, [true, true, false, false, false]);
    }

    #[test]
    fn should_back_off_and_succeed_when_a_transient_failure_clears() {
        // given
        let mut left = 2;
        let mut pauses = Vec::new();

        // when
        let result = with_retries(
            &PUSH_DELAYS,
            &mut || {
                if left == 0 {
                    return Ok(());
                }
                left -= 1;
                Err(PushFailure::Transient("timeout".to_owned()))
            },
            &mut |d| pauses.push(d.as_secs()),
        );

        // then
        assert_eq!(result, Ok(()));
        assert_eq!(pauses, [2, 4]);
    }

    #[test]
    fn should_give_up_when_retries_are_exhausted_or_the_failure_is_final() {
        // given
        let mut attempts = 0;
        let mut pauses = Vec::new();
        let mut final_pauses = Vec::new();

        // when
        let exhausted = with_retries(
            &PUSH_DELAYS,
            &mut || {
                attempts += 1;
                Err(PushFailure::Transient("timeout".to_owned()))
            },
            &mut |d| pauses.push(d.as_secs()),
        );
        let rejected = with_retries(
            &PUSH_DELAYS,
            &mut || Err(PushFailure::Final("rejected".to_owned())),
            &mut |d| final_pauses.push(d.as_secs()),
        );

        // then
        assert_eq!(exhausted, Err("gave up after retries: timeout".to_owned()));
        assert_eq!((attempts, pauses), (5, vec![2, 4, 8, 16]));
        assert_eq!(rejected, Err("rejected".to_owned()));
        assert!(final_pauses.is_empty());
    }

    // ---- a real git repository with a local bare remote --------------------------------

    struct Fake {
        green: bool,
        lockfile: Option<&'static str>,
        gated: RefCell<u32>,
    }

    impl Fake {
        fn new(green: bool) -> Self {
            Self {
                green,
                lockfile: None,
                gated: RefCell::new(0),
            }
        }
    }

    impl Toolbox for Fake {
        fn refresh_lockfile(&self, dir: &Path) -> Result<(), String> {
            if let Some(text) = self.lockfile {
                fs::write(dir.join(LOCKFILE), text).map_err(|e| e.to_string())?;
            }
            Ok(())
        }

        fn gate(&self, _dir: &Path) -> bool {
            *self.gated.borrow_mut() += 1;
            self.green
        }

        fn pause(&self, _delay: Duration) {}
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        Git { dir }.run(args).unwrap()
    }

    fn write(dir: &Path, rel: &str, text: &str) {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn commit(dir: &Path, message: &str) -> String {
        git(dir, &["add", "--all"]);
        git(dir, &["commit", "--quiet", "-m", message]);
        git(dir, &["rev-parse", "HEAD"])
    }

    const INDEX_HEAD: &str =
        "# ADRs\n\n| ADR | Title | Status |\n|---|---|---|\n| [0001](0001-a.md) | A | Proposed |\n";

    fn adr_text(n: &str, title: &str) -> String {
        format!("# {n}. {title}\n\nDate: 2026-10-14\n\n## Status\n\nProposed\n")
    }

    /// A work repository on `main`, pushed to a bare `origin`, plus a branch `lane` from the
    /// same base. Returns (tempdir, work dir, origin dir).
    fn repository() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let origin = temp.path().join("origin.git");
        let work = temp.path().join("work");
        fs::create_dir_all(&origin).unwrap();
        fs::create_dir_all(&work).unwrap();
        git(
            &origin,
            &["init", "--quiet", "--bare", "--initial-branch=main"],
        );
        git(&work, &["init", "--quiet", "--initial-branch=main"]);
        for (key, value) in [
            ("user.name", "fixture"),
            ("user.email", "fixture@example.invalid"),
            ("commit.gpgsign", "false"),
        ] {
            git(&work, &["config", key, value]);
        }
        write(&work, "src/a.rs", "fn a() {}\n");
        write(&work, LOCKFILE, "base\n");
        write(&work, "doc/adr/0001-a.md", &adr_text("0001", "A"));
        write(&work, ADR_INDEX, INDEX_HEAD);
        commit(&work, "base");
        git(
            &work,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(&work, &["push", "--quiet", "origin", "main"]);
        git(&work, &["branch", "lane"]);
        (temp, work, origin)
    }

    /// Commits `files` on `lane` and returns to `main`; returns the commit.
    fn on_lane(work: &Path, files: &[(&str, &str)], message: &str) -> String {
        git(work, &["checkout", "--quiet", "lane"]);
        for (path, text) in files {
            write(work, path, text);
        }
        let sha = commit(work, message);
        git(work, &["checkout", "--quiet", "main"]);
        sha
    }

    /// Commits `files` on `main` and pushes it.
    fn on_main(work: &Path, files: &[(&str, &str)], message: &str) -> String {
        for (path, text) in files {
            write(work, path, text);
        }
        let sha = commit(work, message);
        git(work, &["push", "--quiet", "origin", "main"]);
        sha
    }

    fn push_to(also: &[&str]) -> Target {
        Target::Push {
            remote: "origin".to_owned(),
            branch: "main".to_owned(),
            also: also.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn should_fast_forward_main_without_pushing_when_the_pick_is_green() {
        // given
        let (_temp, work, origin) = repository();
        let lane = on_lane(&work, &[("src/b.rs", "fn b() {}\n")], "add b");
        let before = git(&origin, &["rev-parse", "main"]);
        let tools = Fake::new(true);

        // when
        let result = land(&work, &[lane], &Target::NoPush, &tools);

        // then
        assert_eq!(report(result), 0);
        assert_eq!(git(&work, &["symbolic-ref", "--short", "HEAD"]), "main");
        assert_eq!(git(&work, &["log", "-1", "--format=%s"]), "add b");
        assert_eq!(git(&origin, &["rev-parse", "main"]), before);
        assert_eq!(*tools.gated.borrow(), 1);
    }

    #[test]
    fn should_push_main_and_the_also_branch_when_the_check_is_green() {
        // given
        let (_temp, work, origin) = repository();
        let lane = on_lane(&work, &[("src/b.rs", "fn b() {}\n")], "add b");

        // when
        let result = land(&work, &[lane], &push_to(&["lane-c"]), &Fake::new(true));

        // then
        let head = git(&work, &["rev-parse", "main"]);
        assert!(result.unwrap().contains("pushed main:refs/heads/lane-c"));
        assert_eq!(git(&origin, &["rev-parse", "main"]), head);
        assert_eq!(git(&origin, &["rev-parse", "lane-c"]), head);
    }

    #[test]
    fn should_leave_main_untouched_when_a_source_file_conflicts() {
        // given
        let (_temp, work, _origin) = repository();
        let first = on_lane(&work, &[("src/c.rs", "fn c() {}\n")], "add c");
        let second = on_lane(&work, &[("src/a.rs", "fn a() { lane(); }\n")], "lane a");
        let main = on_main(&work, &[("src/a.rs", "fn a() { main(); }\n")], "main a");
        let tools = Fake::new(true);

        // when
        let result = land(&work, &[first, second], &Target::NoPush, &tools);

        // then
        let err = result.unwrap_err();
        assert!(
            err.contains("conflicts in files land does not resolve: src/a.rs"),
            "{err}"
        );
        assert_eq!(git(&work, &["rev-parse", "HEAD"]), main);
        assert_eq!(git(&work, &["symbolic-ref", "--short", "HEAD"]), "main");
        assert_eq!(git(&work, &["status", "--porcelain"]), "");
        assert_eq!(*tools.gated.borrow(), 0);
    }

    #[test]
    fn should_resolve_the_index_and_lockfile_when_only_they_conflict() {
        // given
        let (_temp, work, _origin) = repository();
        let lane = on_lane(
            &work,
            &[
                ("doc/adr/0003-c.md", &adr_text("0003", "C")),
                (
                    ADR_INDEX,
                    &format!("{INDEX_HEAD}| [0003](0003-c.md) | C | Proposed |\n"),
                ),
                (LOCKFILE, "lane\n"),
            ],
            "add c",
        );
        on_main(
            &work,
            &[
                ("doc/adr/0002-b.md", &adr_text("0002", "B")),
                (
                    ADR_INDEX,
                    &format!("{INDEX_HEAD}| [0002](0002-b.md) | B | Proposed |\n"),
                ),
                (LOCKFILE, "main\n"),
            ],
            "add b",
        );
        let tools = Fake {
            lockfile: Some("resolved\n"),
            ..Fake::new(true)
        };

        // when
        let result = land(&work, &[lane], &Target::NoPush, &tools);

        // then
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            fs::read_to_string(work.join(ADR_INDEX)).unwrap(),
            format!(
                "{INDEX_HEAD}| [0002](0002-b.md) | B | Proposed |\n| [0003](0003-c.md) | C | Proposed |\n"
            )
        );
        assert_eq!(
            fs::read_to_string(work.join(LOCKFILE)).unwrap(),
            "resolved\n"
        );
        assert_eq!(git(&work, &["log", "-1", "--format=%s"]), "add c");
        assert_eq!(git(&work, &["status", "--porcelain"]), "");
    }

    #[test]
    fn should_report_the_failed_push_when_the_also_branch_is_not_a_fast_forward() {
        // given
        let (_temp, work, origin) = repository();
        let other = on_lane(&work, &[("src/x.rs", "fn x() {}\n")], "x");
        git(
            &work,
            &[
                "push",
                "--quiet",
                "origin",
                &format!("{other}:refs/heads/lane-c"),
            ],
        );
        let lane = on_lane(&work, &[("src/b.rs", "fn b() {}\n")], "add b");

        // when
        let result = land(&work, &[lane], &push_to(&["lane-c"]), &Fake::new(true));

        // then
        let err = result.unwrap_err();
        assert!(
            err.contains("pushing main:refs/heads/lane-c failed"),
            "{err}"
        );
        assert_eq!(
            git(&origin, &["rev-parse", "main"]),
            git(&work, &["rev-parse", "main"])
        );
        assert_eq!(git(&origin, &["rev-parse", "lane-c"]), other);
    }

    #[test]
    fn should_fail_closed_when_the_real_tools_cannot_run() {
        // given
        let dir = tempfile::tempdir().unwrap();
        let tools = Cargo {
            log: Some(dir.path().join("missing/check.log")),
        };

        // when
        let gate = tools.gate(dir.path());
        let lockfile = tools.refresh_lockfile(dir.path());

        // then
        assert!(!gate);
        assert_eq!(lockfile, Err("cargo update --workspace failed".to_owned()));
    }

    #[test]
    fn should_keep_main_when_the_check_is_red() {
        // given
        let (_temp, work, origin) = repository();
        let lane = on_lane(&work, &[("src/b.rs", "fn b() {}\n")], "add b");
        let before = git(&work, &["rev-parse", "main"]);

        // when
        let result = land(&work, &[lane], &push_to(&[]), &Fake::new(false));

        // then
        assert!(result.unwrap_err().contains("cargo xtask check failed"));
        assert_eq!(git(&work, &["rev-parse", "main"]), before);
        assert_eq!(git(&origin, &["rev-parse", "main"]), before);
        assert_eq!(git(&work, &["symbolic-ref", "--short", "HEAD"]), "main");
    }

    #[test]
    fn should_refuse_when_the_checkout_is_dirty_or_behind() {
        // given
        let (_temp, work, _origin) = repository();
        let lane = on_lane(&work, &[("src/b.rs", "fn b() {}\n")], "add b");
        write(&work, "notes.txt", "scratch\n");
        let dirty = land(
            &work,
            std::slice::from_ref(&lane),
            &Target::NoPush,
            &Fake::new(true),
        );
        fs::remove_file(work.join("notes.txt")).unwrap();
        on_main(&work, &[("src/d.rs", "fn d() {}\n")], "d");
        git(&work, &["reset", "--quiet", "--hard", "HEAD~1"]);

        // when
        let behind = land(&work, &[lane], &Target::NoPush, &Fake::new(true));

        // then
        assert!(dirty.unwrap_err().contains("?? notes.txt"));
        assert!(behind.unwrap_err().contains("is not `origin/main`"));
    }

    #[test]
    fn should_refuse_without_moving_when_a_ref_or_target_is_bad() {
        // given
        let (_temp, work, _origin) = repository();

        // when
        let results = [
            land(
                &work,
                &["no-such-ref".to_owned()],
                &Target::NoPush,
                &Fake::new(true),
            ),
            land(
                &work,
                &["--force".to_owned()],
                &Target::NoPush,
                &Fake::new(true),
            ),
            land(&work, &[], &Target::NoPush, &Fake::new(true)),
            land(
                &work,
                &["main".to_owned()],
                &push_to(&["+main"]),
                &Fake::new(true),
            ),
            land(
                &work,
                &["main..main".to_owned()],
                &Target::NoPush,
                &Fake::new(true),
            ),
        ];

        // then
        assert!(results.iter().all(Result::is_err), "{results:?}");
        assert_eq!(git(&work, &["symbolic-ref", "--short", "HEAD"]), "main");
    }
}
