//! `mayflyd`'s start-up trust checks on GitHub (GH-08; ADR 0090 points 1 and 4, its 2026-11-12
//! amendment, ADR 0045; `doc/github-scaleset.md` §6.2, §13) and the desk `mayfly doctor` reads
//! them from (`trust`).
//!
//! - **The App's permissions** ([`needed_permissions`], [`compare_permissions`]), read first:
//!   what the configuration's options need, against what the installation was granted; a
//!   missing one is logged at WARN (`app_permission_missing`) and failed by `doctor`, one beyond
//!   the need is a `doctor` warning (ADR 0045 §3).
//! - **The fork-PR setting** (`[github] check_fork_pr_setting`, default off): read once a run
//!   recovered, before any listener starts ([`fork_state`]). When the organisation runs
//!   workflows from fork pull requests in private repositories, no listener starts for the run:
//!   the desk says `refused`, and it is logged at ERROR (`pools_refused_fork_pr_setting`). A read
//!   that fails is asked again after a backoff while the listeners wait (fail closed: nothing is
//!   served on a setting mayfly could not read): `unread` when it answered nothing definite,
//!   `denied` when GitHub answered 403 or 404 (the App lacks "Administration: Read", or the
//!   organisation is not the one installed), which `doctor` fails, since every pool waits on
//!   it. The per-job check already refuses a fork's run; this is defence in depth. Pinned by
//!   `tests::should_keep_the_listeners_waiting_when_the_fork_setting_is_not_read` and
//!   `tests/daemon.rs::github::should_serve_no_pool_and_fail_doctor_when_the_fork_setting_is_denied`.
//! - **The runner-group report**: once a pool's scale set is ensured, its runner group is read,
//!   off the listener's path; when the group admits no public repository while the pool lists
//!   allowed repositories, the pool is reported (desk and a WARN
//!   `runner_group_admits_no_public_repository`): a public one among them would be routed
//!   nothing. mayfly does not read a repository's visibility (ADR 0045: "Metadata" is not
//!   used), so this is a possible problem, never a finding about a repository, and never a
//!   refusal: the runner group is not the boundary.
//!
//! Every outcome is the desk's [`Trust`] report, kept for the process; nothing here is
//! journaled.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use mayfly_adapter_github::{Access, GrantedPermissions};
use mayfly_control::github::{Failure, GitHubCallError};
use tokio::sync::watch;

use crate::config::{GithubConfig, PoolConfig};
use crate::operator::wire::Trust;

/// Whether the run's pools may be served, as the fork-PR setting check decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// Not decided yet: the listeners wait.
    Pending,
    /// The pools are served.
    Open,
    /// The pools are refused for this run.
    Refused,
}

/// What the trust checks found, for `doctor`, and the gate the listeners wait on.
#[derive(Debug)]
pub struct TrustDesk {
    report: Mutex<Trust>,
    gate: watch::Sender<Gate>,
}

impl Default for TrustDesk {
    fn default() -> Self {
        Self {
            report: Mutex::new(Trust {
                fork_pr_setting: "pending".to_owned(),
                ..Trust::default()
            }),
            gate: watch::channel(Gate::Pending).0,
        }
    }
}

impl TrustDesk {
    fn lock(&self) -> MutexGuard<'_, Trust> {
        self.report.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What the desk holds now.
    #[must_use]
    pub fn report(&self) -> Trust {
        self.lock().clone()
    }

    /// A new run starts: the whole report is the run's, so it starts afresh; with
    /// `check_fork_pr_setting` the setting is read again before its pools are served
    /// (`pending`); without it the gate is open at once (`off`), so what `doctor` shows of it
    /// never depends on how far the run got. Pinned by
    /// `tests::should_forget_the_last_runs_report_when_a_run_starts`.
    pub fn run_started(&self, check_fork_pr_setting: bool) {
        *self.lock() = Trust::default();
        self.fork_pr_setting(if check_fork_pr_setting {
            "pending"
        } else {
            "off"
        });
    }

    /// The fork-PR setting's state (`off`, `passed`, `refused`, `unread`, `denied`), and the
    /// gate it sets: `off` and `passed` open it, `refused` refuses the pools, any other keeps
    /// them waiting.
    pub fn fork_pr_setting(&self, state: &str) {
        self.lock().fork_pr_setting = state.to_owned();
        let gate = match state {
            "off" | "passed" => Gate::Open,
            "refused" => Gate::Refused,
            _ => Gate::Pending,
        };
        self.gate.send_replace(gate);
    }

    /// The gate as it stands.
    #[must_use]
    pub fn gate(&self) -> Gate {
        *self.gate.borrow()
    }

    /// Waits until the gate is decided and answers it.
    pub async fn decided(&self) -> Gate {
        let mut gate = self.gate.subscribe();
        gate.wait_for(|g| *g != Gate::Pending)
            .await
            .map_or(Gate::Pending, |g| *g)
    }

    /// Whether `pool`, which lists allowed repositories, has a runner group that admits public
    /// repositories.
    pub fn group_admits_public(&self, pool: &str, admits: bool) {
        let mut report = self.lock();
        report.unreported.remove(pool);
        report.public_not_admitted.remove(pool);
        if !admits {
            report.public_not_admitted.insert(pool.to_owned());
        }
    }

    /// The report of `pool` could not be made, by the failure's code.
    pub fn unreported(&self, pool: &str, code: &str) {
        self.lock()
            .unreported
            .insert(pool.to_owned(), code.to_owned());
    }

    /// The comparison of the installation's permissions with the options' need.
    pub fn permissions(&self, compared: &PermissionGap) {
        let mut report = self.lock();
        report.permissions_read = true;
        report.missing_permissions.clone_from(&compared.missing);
        report.excess_permissions.clone_from(&compared.excess);
    }
}

/// The fork-PR setting's state ([`TrustDesk::fork_pr_setting`]) for the read's answer: `passed`
/// (fork workflows do not run), `refused` (they do), `denied` (a definite 403 or 404) or
/// `unread` (anything else: no definite answer). Only `passed` serves the pools. Pinned by
/// `tests::should_keep_the_listeners_waiting_when_the_fork_setting_is_not_read`.
#[must_use]
pub const fn fork_state(answer: &Result<bool, GitHubCallError>) -> &'static str {
    match answer {
        Ok(false) => "passed",
        Ok(true) => "refused",
        Err(GitHubCallError {
            failure: Failure::Refused { status: 403 | 404 },
            ..
        }) => "denied",
        Err(_) => "unread",
    }
}

/// What the configuration's options need of the App (ADR 0045, spec §13), by GitHub's names:
/// "Self-hosted runners: Read and write" always, "Actions: Read" for the run lookups ("Write"
/// with any pool's `cancel_refused_runs`), and "Administration: Read" with
/// `check_fork_pr_setting`. "Metadata: Read-only" is not needed ([`IMPLIED`]). Pinned by
/// `tests::should_need_write_and_administration_only_when_the_options_ask_for_them`.
#[must_use]
pub fn needed_permissions(
    github: &GithubConfig,
    pools: &[PoolConfig],
) -> BTreeMap<&'static str, Access> {
    let cancels = pools.iter().any(|p| p.cancel_refused_runs);
    let mut needed = BTreeMap::from([
        ("organization_self_hosted_runners", Access::Write),
        (
            "actions",
            if cancels { Access::Write } else { Access::Read },
        ),
    ]);
    if github.check_fork_pr_setting {
        needed.insert("organization_administration", Access::Read);
    }
    needed
}

/// What GitHub grants with any repository permission, which mayfly does not use (ADR 0045):
/// never needed, and never counted beyond the need either, since no App holding "Actions" can
/// be without it.
pub const IMPLIED: &[(&str, Access)] = &[("metadata", Access::Read)];

/// The difference between what is needed and what is granted, as `name: level` lines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PermissionGap {
    /// Needed and not granted at the level needed.
    pub missing: Vec<String>,
    /// Granted beyond the need (a level this build does not know counts as beyond).
    pub excess: Vec<String>,
}

const fn access_name(access: Access) -> &'static str {
    match access {
        Access::Read => "read",
        Access::Write => "write",
        Access::Admin => "admin",
    }
}

/// Compares `granted` with `needed`. Pinned by
/// `tests::should_name_each_missing_and_excess_permission_when_compared`.
#[must_use]
pub fn compare_permissions(
    needed: &BTreeMap<&'static str, Access>,
    granted: &GrantedPermissions,
) -> PermissionGap {
    let missing = needed
        .iter()
        .filter(|(name, level)| {
            let held = granted.0.get(**name).and_then(|l| Access::parse(l));
            !held.is_some_and(|held| held >= **level)
        })
        .map(|(name, level)| format!("{name}: {}", access_name(*level)))
        .collect();
    let excess = granted
        .0
        .iter()
        .filter(|(name, level)| {
            let allowed = needed.get(name.as_str()).copied().or_else(|| {
                IMPLIED
                    .iter()
                    .find(|(implied, _)| implied == name)
                    .map(|(_, a)| *a)
            });
            !matches!((Access::parse(level), allowed), (Some(g), Some(a)) if g <= a)
        })
        .map(|(name, level)| format!("{name}: {level}"))
        .collect();
    PermissionGap { missing, excess }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    const BASE: &str = r#"
schema_version = 1
[github]
organisation = "example-org"
app_client_id = "Iv1.0123456789abcdef"
installation_id = 1
private_key = "file:/etc/mayfly/app-key.pem"
"#;

    fn config(github_extra: &str, pool_extra: &str) -> Config {
        let text = format!(
            "{BASE}{github_extra}\n[[pools]]\nname = \"mac\"\ntier = \"trusted\"\nimage_manifest = \"/etc/mayfly/images/mac.toml\"\nguest_os = \"macos\"\ncpu = 4\nmemory_mib = 8192\nmax_running = 2\nallowed_repositories = [\"example-org/app\"]\n{pool_extra}\n"
        );
        Config::parse(&text).unwrap_or_else(|e| panic!("{e}"))
    }

    fn granted(pairs: &[(&str, &str)]) -> GrantedPermissions {
        GrantedPermissions(
            pairs
                .iter()
                .map(|(n, l)| ((*n).to_owned(), (*l).to_owned()))
                .collect(),
        )
    }

    #[test]
    fn should_need_write_and_administration_only_when_the_options_ask_for_them() {
        // given
        let plain = config("", "");
        let both = config("check_fork_pr_setting = true", "cancel_refused_runs = true");

        // when
        let need = |c: &Config| needed_permissions(&c.github, &c.pools);
        let (plain, both) = (need(&plain), need(&both));

        // then
        assert_eq!(plain.get("actions"), Some(&Access::Read));
        assert_eq!(plain.get("organization_administration"), None);
        assert_eq!(plain.get("metadata"), None);
        assert_eq!(both.get("actions"), Some(&Access::Write));
        assert_eq!(both.get("organization_administration"), Some(&Access::Read));
        assert_eq!(
            both.get("organization_self_hosted_runners"),
            Some(&Access::Write)
        );
    }

    #[test]
    fn should_name_each_missing_and_excess_permission_when_compared() {
        // given: the cancel option on, the installation without Actions: Write but with
        // Contents: Read and an admin level on runners
        let cancelling = config("", "cancel_refused_runs = true");
        let needed = needed_permissions(&cancelling.github, &cancelling.pools);
        let held = granted(&[
            ("organization_self_hosted_runners", "admin"),
            ("actions", "read"),
            ("metadata", "read"),
            ("contents", "read"),
        ]);

        // when
        let gap = compare_permissions(&needed, &held);

        // then
        assert_eq!(gap.missing, ["actions: write"]);
        assert_eq!(
            gap.excess,
            ["contents: read", "organization_self_hosted_runners: admin"]
        );
        let exact = granted(&[
            ("organization_self_hosted_runners", "write"),
            ("actions", "write"),
            ("metadata", "read"),
        ]);
        assert_eq!(
            compare_permissions(&needed, &exact),
            PermissionGap::default()
        );
        let metadata_written = granted(&[
            ("organization_self_hosted_runners", "write"),
            ("actions", "write"),
            ("metadata", "write"),
        ]);
        assert_eq!(
            compare_permissions(&needed, &metadata_written).excess,
            ["metadata: write"]
        );
    }

    #[test]
    fn should_keep_the_listeners_waiting_when_the_fork_setting_is_not_read() {
        // given: a run that checks the setting, and each answer its read can get
        let failed = |failure| Err(GitHubCallError::new(failure, "synthetic"));
        let answers = [
            (failed(Failure::Unavailable), "unread"),
            (failed(Failure::Throttled), "unread"),
            (failed(Failure::Credentials), "unread"),
            (failed(Failure::Refused { status: 500 }), "unread"),
            (failed(Failure::Refused { status: 403 }), "denied"),
            (failed(Failure::Refused { status: 404 }), "denied"),
        ];

        for (answer, state) in answers {
            let desk = TrustDesk::default();
            desk.run_started(true);

            // when
            desk.fork_pr_setting(fork_state(&answer));

            // then: the listeners wait (fail closed), and doctor sees why
            assert_eq!(fork_state(&answer), state);
            assert_eq!(desk.gate(), Gate::Pending, "{state}");
            assert_eq!(desk.report().fork_pr_setting, state);
        }
        let desk = TrustDesk::default();
        desk.run_started(true);
        assert_eq!(desk.gate(), Gate::Pending, "unread before its first answer");
        desk.fork_pr_setting(fork_state(&Ok(false)));
        assert_eq!(desk.gate(), Gate::Open);
        desk.fork_pr_setting(fork_state(&Ok(true)));
        assert_eq!(desk.gate(), Gate::Refused);
    }

    #[test]
    fn should_forget_the_last_runs_report_when_a_run_starts() {
        // given: a run that found everything there is to find
        let desk = TrustDesk::default();
        desk.run_started(true);
        desk.fork_pr_setting("refused");
        desk.group_admits_public("mac", false);
        desk.unreported("lin", "unavailable");
        desk.permissions(&PermissionGap {
            missing: vec!["actions: write".to_owned()],
            excess: vec!["contents: read".to_owned()],
        });

        // when: the next run starts without the fork check
        desk.run_started(false);

        // then
        assert_eq!(
            desk.report(),
            Trust {
                fork_pr_setting: "off".to_owned(),
                ..Trust::default()
            }
        );
        assert_eq!(desk.gate(), Gate::Open);
    }
}
