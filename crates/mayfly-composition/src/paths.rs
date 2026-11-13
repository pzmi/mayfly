//! Where `mayflyd` keeps its state when the configuration names no place (ADR 0091 decision 5,
//! the owner's "reasonable defaults out of the box", 2026-11-12). The configuration's
//! `state_dir` and `[provider] tart_home` always win.
//!
//! | | macOS (terminal mode) | Linux (terminal mode) |
//! |:--|:--|:--|
//! | state: journal, host id, base registry, shims | `~/Library/Application Support/mayfly` | `$XDG_STATE_HOME/mayfly` (`~/.local/state/mayfly`) |
//! | mayfly's own `TART_HOME`, the epoch floor in it | `<state>/tart` | `$XDG_DATA_HOME/mayfly/tart` (`~/.local/share/mayfly/tart`) |
//!
//! On Linux the VM disks and image bases go to the data directory, as XDG asks of large,
//! user-owned files, and the bookkeeping to the state directory. The epoch floor stays in
//! `TART_HOME` (ADR 0026: beside the VMs it fences, so it travels with them), which the split
//! does not complicate: it reads one file in whichever `TART_HOME` is in use. Nothing goes in
//! a cache directory, which the system may purge. Logs are RT-04's. The operator socket
//! ([`socket`], RT-03) is `<state_dir>/mayfly.sock` when the configuration names `state_dir`;
//! otherwise `$XDG_RUNTIME_DIR/mayfly.sock` on Linux when that variable is absolute (the
//! per-user runtime directory XDG provides for sockets), else `<state>/mayfly.sock`, and
//! `<state>/mayfly.sock` on macOS (ADR 0091 decision 5). Service mode under launchd or systemd configures `/Library/Application
//! Support/mayfly` or `/var/lib/mayfly` explicitly (OPS-01).
//!
//! An XDG variable is used only when absolute (the XDG specification: a relative one is
//! invalid and ignored); `HOME` likewise. With no usable `HOME` there is no default and the
//! daemon asks for `state_dir` in the configuration.
//!
//! The rules are pure functions of a [`PlatformEnv`] read once from the process
//! ([`PlatformEnv::from_process`]), so tests inject the environment instead of mutating the
//! process's. Rejected alternative: the `directories` crate's `ProjectDirs`, which reads the
//! process environment itself (a test could only set it, process-wide) and would be a
//! dependency for the dozen lines below.

use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};

use mayfly_adapter_tart::verify::{UnsafeLayout, check_path, check_private};

/// The application's directory name.
const APP: &str = "mayfly";

/// The variables the defaults read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlatformEnv {
    /// `HOME`.
    pub home: Option<PathBuf>,
    /// `XDG_STATE_HOME`.
    pub xdg_state_home: Option<PathBuf>,
    /// `XDG_DATA_HOME`.
    pub xdg_data_home: Option<PathBuf>,
    /// `XDG_RUNTIME_DIR`: where the operator socket goes on Linux when the configuration names
    /// no state directory ([`socket`]).
    pub xdg_runtime_dir: Option<PathBuf>,
}

impl PlatformEnv {
    /// Read from this process's environment: called once, by the `mayflyd` and `mayfly`
    /// binaries at their entry points (`mayfly-cli`'s `bin/`); everything below takes the value
    /// it read, so the library never reads the process environment for its places.
    #[must_use]
    pub fn from_process() -> Self {
        let var = |name| std::env::var_os(name).map(PathBuf::from);
        Self {
            home: var("HOME"),
            xdg_state_home: var("XDG_STATE_HOME"),
            xdg_data_home: var("XDG_DATA_HOME"),
            xdg_runtime_dir: var("XDG_RUNTIME_DIR"),
        }
    }
}

/// Where the daemon keeps its state and mayfly's own `TART_HOME`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Places {
    /// The state directory.
    pub state_dir: PathBuf,
    /// mayfly's `TART_HOME`.
    pub tart_home: PathBuf,
}

fn absolute(path: Option<&PathBuf>) -> Option<&Path> {
    path.map(PathBuf::as_path)
        .filter(|p| p.is_absolute() && !p.as_os_str().is_empty())
}

/// macOS's standard places: Application Support, `TART_HOME` inside it. Pinned by
/// `tests::should_use_application_support_when_on_macos`.
#[must_use]
pub fn macos_defaults(env: &PlatformEnv) -> Option<Places> {
    let state_dir = absolute(env.home.as_ref())?
        .join("Library")
        .join("Application Support")
        .join(APP);
    Some(Places {
        tart_home: state_dir.join("tart"),
        state_dir,
    })
}

/// XDG's standard places: state in `$XDG_STATE_HOME`, `TART_HOME` in `$XDG_DATA_HOME`, each
/// falling back to its `~/.local` default. Pinned by
/// `tests::should_follow_xdg_when_its_variables_are_set_or_unset`.
#[must_use]
pub fn xdg_defaults(env: &PlatformEnv) -> Option<Places> {
    let home = absolute(env.home.as_ref());
    let state = absolute(env.xdg_state_home.as_ref())
        .map(Path::to_path_buf)
        .or_else(|| home.map(|h| h.join(".local").join("state")))?;
    let data = absolute(env.xdg_data_home.as_ref())
        .map(Path::to_path_buf)
        .or_else(|| home.map(|h| h.join(".local").join("share")))?;
    Some(Places {
        state_dir: state.join(APP),
        tart_home: data.join(APP).join("tart"),
    })
}

/// This platform's standard places.
#[cfg(target_os = "macos")]
#[must_use]
pub fn defaults(env: &PlatformEnv) -> Option<Places> {
    macos_defaults(env)
}

/// This platform's standard places.
#[cfg(not(target_os = "macos"))]
#[must_use]
pub fn defaults(env: &PlatformEnv) -> Option<Places> {
    xdg_defaults(env)
}

/// The places to use: the configuration's where it names one, else the platform's. A
/// configured `state_dir` without a configured `tart_home` keeps `TART_HOME` at
/// `<state_dir>/tart` (service mode's `/var/lib/mayfly/tart`), so one setting moves everything.
/// `None` when the configuration leaves the state out and the platform has no default (no usable
/// `HOME`). Pinned by `tests::should_prefer_the_configuration_when_it_names_a_place`.
#[must_use]
pub fn resolve(
    state_dir: Option<&Path>,
    tart_home: Option<&Path>,
    platform: Option<Places>,
) -> Option<Places> {
    let chosen = match state_dir {
        Some(state_dir) => Places {
            state_dir: state_dir.to_path_buf(),
            tart_home: state_dir.join("tart"),
        },
        None => platform?,
    };
    Some(Places {
        tart_home: tart_home.map_or(chosen.tart_home, Path::to_path_buf),
        state_dir: chosen.state_dir,
    })
}

/// The operator socket's file name.
pub const SOCKET: &str = "mayfly.sock";

/// The operator socket's place (RT-03, ADR 0091 decision 5): `<state_dir>/mayfly.sock` when the
/// configuration names `state_dir` (`configured`), so one setting moves everything and two
/// daemons of one user with their own state directories never share a socket; otherwise, on
/// Linux, `$XDG_RUNTIME_DIR/mayfly.sock` when that variable is absolute, else
/// `<state>/mayfly.sock` in the places in use (`places`); on macOS always
/// `<state>/mayfly.sock`. `mayflyd` and `mayfly` both use this rule, the CLI with the
/// configuration it is given or none. Pinned by
/// `tests::should_put_the_socket_in_the_configured_state_dir_when_one_is_named` and
/// `tests::should_put_the_socket_in_the_runtime_dir_when_linux_names_one`.
#[must_use]
pub fn socket(configured: Option<&Path>, places: &Places, env: &PlatformEnv) -> PathBuf {
    if let Some(state_dir) = configured {
        return state_dir.join(SOCKET);
    }
    runtime_socket(env).unwrap_or_else(|| places.state_dir.join(SOCKET))
}

/// The socket in the runtime directory, where the platform has one for it.
#[cfg(target_os = "macos")]
const fn runtime_socket(_env: &PlatformEnv) -> Option<PathBuf> {
    None
}

/// The socket in the runtime directory, where the platform has one for it.
#[cfg(not(target_os = "macos"))]
fn runtime_socket(env: &PlatformEnv) -> Option<PathBuf> {
    absolute(env.xdg_runtime_dir.as_ref()).map(|dir| dir.join(SOCKET))
}

/// Makes `dir` a private directory of the daemon's user, safely (ADR 0087's layout rule, the
/// one `TART_HOME` is held to, applied to every directory the daemon writes in):
///
/// 1. the path is checked before anything is created ([`check_path`]: every ancestor owned by
///    root or the daemon's user and not writable by others unless sticky, a symlink on the way
///    followed only when root or the daemon's user owns it, `dir` itself never one);
/// 2. each missing component is created alone, `0700`, by `mkdir`, which never follows a
///    symlink in the final component (an existing one makes it fail), and checked private right
///    after ([`check_private`]);
/// 3. `dir` is checked private, whether it was created or already there.
///
/// Rejected alternative: `DirBuilder::recursive`, which follows whatever it meets and checks
/// nothing. Pinned by `tests::should_create_each_missing_level_private_when_preparing_a_place`
/// and `tests/daemon.rs::should_refuse_before_writing_when_an_ancestor_is_open_to_others`.
///
/// # Errors
/// [`UnsafeLayout`], naming the path refused; nothing below it was created.
pub fn prepare_private(dir: &Path) -> Result<(), UnsafeLayout> {
    check_path(dir)?;
    let mut path = PathBuf::from("/");
    for component in dir.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        path.push(name);
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::DirBuilder::new()
                    .mode(0o700)
                    .create(&path)
                    .map_err(|_| UnsafeLayout::NotADirectory(path.clone()))?;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                    .map_err(|_| UnsafeLayout::NotADirectory(path.clone()))?;
                check_private(&path)?;
            }
            Err(_) => return Err(UnsafeLayout::NotADirectory(path)),
        }
    }
    check_private(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(home: Option<&str>, state: Option<&str>, data: Option<&str>) -> PlatformEnv {
        PlatformEnv {
            home: home.map(PathBuf::from),
            xdg_state_home: state.map(PathBuf::from),
            xdg_data_home: data.map(PathBuf::from),
            xdg_runtime_dir: None,
        }
    }

    #[test]
    fn should_follow_xdg_when_its_variables_are_set_or_unset() {
        // given
        let unset = env(Some("/home/u"), None, None);
        let set = env(Some("/home/u"), Some("/s"), Some("/d"));
        let relative = env(Some("/home/u"), Some("state"), Some("data"));

        // when
        let (unset, set, relative) = (
            xdg_defaults(&unset),
            xdg_defaults(&set),
            xdg_defaults(&relative),
        );

        // then
        let expected = Places {
            state_dir: "/home/u/.local/state/mayfly".into(),
            tart_home: "/home/u/.local/share/mayfly/tart".into(),
        };
        assert_eq!(unset.as_ref(), Some(&expected));
        assert_eq!(
            set,
            Some(Places {
                state_dir: "/s/mayfly".into(),
                tart_home: "/d/mayfly/tart".into(),
            })
        );
        assert_eq!(relative, Some(expected));
        assert_eq!(xdg_defaults(&env(None, None, None)), None);
        assert_eq!(xdg_defaults(&env(Some("home"), None, None)), None);
        assert!(xdg_defaults(&env(None, Some("/s"), Some("/d"))).is_some());
    }

    #[test]
    fn should_use_application_support_when_on_macos() {
        // given
        let env = env(Some("/Users/u"), Some("/s"), Some("/d"));

        // when
        let places = macos_defaults(&env);

        // then
        assert_eq!(
            places,
            Some(Places {
                state_dir: "/Users/u/Library/Application Support/mayfly".into(),
                tart_home: "/Users/u/Library/Application Support/mayfly/tart".into(),
            })
        );
        assert_eq!(macos_defaults(&PlatformEnv::default()), None);
    }

    #[test]
    fn should_prefer_the_configuration_when_it_names_a_place() {
        // given
        let platform = xdg_defaults(&env(Some("/home/u"), None, None));

        // when
        let both = resolve(
            Some(Path::new("/var/lib/mayfly")),
            Some(Path::new("/srv/tart")),
            platform.clone(),
        );
        let state_only = resolve(Some(Path::new("/var/lib/mayfly")), None, platform.clone());
        let home_only = resolve(None, Some(Path::new("/srv/tart")), platform);
        let nothing = resolve(None, None, None);

        // then
        assert_eq!(
            both,
            Some(Places {
                state_dir: "/var/lib/mayfly".into(),
                tart_home: "/srv/tart".into(),
            })
        );
        assert_eq!(
            state_only,
            Some(Places {
                state_dir: "/var/lib/mayfly".into(),
                tart_home: "/var/lib/mayfly/tart".into(),
            })
        );
        assert_eq!(
            home_only,
            Some(Places {
                state_dir: "/home/u/.local/state/mayfly".into(),
                tart_home: "/srv/tart".into(),
            })
        );
        assert_eq!(nothing, None);
    }

    #[test]
    fn should_create_each_missing_level_private_when_preparing_a_place() {
        // given
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let place = dir.path().join("a").join("b").join("mayfly");

        // when
        let prepared = prepare_private(&place);
        let again = prepare_private(&place);

        // then
        assert_eq!(prepared, Ok(()));
        assert_eq!(again, Ok(()));
        for level in [dir.path().join("a"), dir.path().join("a/b"), place] {
            let mode = std::fs::metadata(&level)
                .unwrap_or_else(|e| panic!("{e}"))
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700, "{}", level.display());
        }
    }

    #[test]
    fn should_refuse_and_create_nothing_when_an_ancestor_is_open_to_others() {
        // given
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let open = dir.path().join("open");
        std::fs::create_dir(&open).unwrap_or_else(|e| panic!("{e}"));
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777))
            .unwrap_or_else(|e| panic!("{e}"));

        // when
        let prepared = prepare_private(&open.join("state"));

        // then
        assert_eq!(prepared, Err(UnsafeLayout::UnsafeAncestor(open.clone())));
        assert!(!open.join("state").exists());
    }

    #[test]
    fn should_refuse_when_the_place_is_a_symlink() {
        // given
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap_or_else(|e| panic!("{e}"));
        let link = dir.path().join("state");
        std::os::unix::fs::symlink(&target, &link).unwrap_or_else(|e| panic!("{e}"));

        // when
        let prepared = prepare_private(&link);

        // then
        assert_eq!(prepared, Err(UnsafeLayout::Symlink(link)));
    }

    #[test]
    fn should_put_the_socket_in_the_configured_state_dir_when_one_is_named() {
        // given: a runtime directory, and a configuration that names the state directory
        let env = PlatformEnv {
            xdg_runtime_dir: Some("/run/user/501".into()),
            ..env(Some("/home/u"), None, None)
        };
        let places = Places {
            state_dir: "/var/lib/mayfly".into(),
            tart_home: "/var/lib/mayfly/tart".into(),
        };

        // when
        let configured = socket(Some(Path::new("/var/lib/mayfly")), &places, &env);

        // then
        assert_eq!(configured, PathBuf::from("/var/lib/mayfly/mayfly.sock"));
    }

    #[test]
    fn should_put_the_socket_in_the_runtime_dir_when_linux_names_one() {
        // given
        let places = Places {
            state_dir: "/home/u/.local/state/mayfly".into(),
            tart_home: "/home/u/.local/share/mayfly/tart".into(),
        };
        let with = PlatformEnv {
            xdg_runtime_dir: Some("/run/user/501".into()),
            ..env(Some("/home/u"), None, None)
        };
        let relative = PlatformEnv {
            xdg_runtime_dir: Some("run".into()),
            ..env(Some("/home/u"), None, None)
        };

        // when
        let (with, relative) = (
            socket(None, &places, &with),
            socket(None, &places, &relative),
        );

        // then: Linux uses an absolute runtime directory; macOS keeps it in the state directory
        let in_state = PathBuf::from("/home/u/.local/state/mayfly/mayfly.sock");
        #[cfg(not(target_os = "macos"))]
        assert_eq!(with, PathBuf::from("/run/user/501/mayfly.sock"));
        #[cfg(target_os = "macos")]
        assert_eq!(with, in_state);
        assert_eq!(relative, in_state);
    }
}
