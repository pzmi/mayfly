//! The local proof a base must give before it is cloned from or deleted (ADR 0087): the registry
//! entry, the VM's file identity and fingerprint, its MAC as a secondary check, and its state.
//!
//! CloneAllowed ⇒ Owned ∧ Ready ∧ IntegrityValid: [`ready_for_clone`].
//! DeleteAllowed ⇒ Owned ∧ Stopped ∧ NoUnresolvedDependents: [`identity_holds`], plus the
//! stopped and dependents checks the image store makes. The model is `formal/image_store.qnt`.

use std::path::{Path, PathBuf};

use crate::bases::{BaseEntry, BaseRecords, BaseState, RECIPE_VERSION};
use crate::parse::VmInfo;
use crate::verify;

/// A VM's directory in `home` (`<TART_HOME>/vms/<name>`).
#[must_use]
pub fn vm_dir(home: &Path, name: &str) -> PathBuf {
    home.join("vms").join(name)
}

/// Whether the VM Tart lists as `info` under `name` is the one `entry` recorded: the same
/// directory and disk (file identity) and the same MAC. Owned.
#[must_use]
pub fn identity_holds(entry: &BaseEntry, info: &VmInfo, home: &Path, name: &str) -> bool {
    entry.vm_mac.as_deref() == Some(info.instance_text().as_str())
        && entry.vm_file_identity.is_some()
        && verify::identity(&vm_dir(home, name)) == entry.vm_file_identity
}

/// Why a base may not be cloned from now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotCloneable {
    /// It is not `Ready`, or of another build recipe: refused, nothing recorded.
    NotReady,
    /// It runs, its identity or its fingerprint changed: it is quarantined.
    Changed(&'static str),
}

/// CloneAllowed: `Ready` by the current recipe, stopped, the same MAC and file identity, and the
/// fingerprint recorded when it became `Ready`.
///
/// # Errors
/// [`NotCloneable`].
pub fn ready_for_clone(
    entry: &BaseEntry,
    info: &VmInfo,
    home: &Path,
    name: &str,
) -> Result<(), NotCloneable> {
    if entry.state != BaseState::Ready || entry.build_recipe_version != RECIPE_VERSION {
        return Err(NotCloneable::NotReady);
    }
    if info.running {
        return Err(NotCloneable::Changed("running"));
    }
    if !identity_holds(entry, info, home, name) {
        return Err(NotCloneable::Changed("identity_changed"));
    }
    if entry.final_fingerprint.is_none()
        || verify::fingerprint(&vm_dir(home, name)) != entry.final_fingerprint
    {
        return Err(NotCloneable::Changed("integrity_changed"));
    }
    Ok(())
}

/// Marks `name` quarantined for `reason`; a record that cannot be written leaves it as it was,
/// which is refused all the same.
pub fn quarantine(records: &BaseRecords, name: &str, reason: &str) {
    let _written = records.update(|record| {
        if let Some(entry) = record.bases.get_mut(name) {
            entry.state = BaseState::Quarantined;
            entry.quarantine = Some(reason.to_owned());
        }
    });
}
