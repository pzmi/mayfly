//! The provider's inventory, complete or not (I25).

use mayfly_domain::OwnedVmName;
use mayfly_protocol::{InventoryGap, ObservationKind, VmObservation};

use crate::supervisor::Seen;

/// One reading of the provider's VM list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inventory {
    /// Exactly what the provider lists, foreign VMs included.
    Complete {
        /// Every VM listed.
        vms: Vec<VmObservation>,
    },
    /// The listing failed, did not parse or did not finish: it says nothing about any VM, in
    /// particular not that one is absent (I25).
    Incomplete {
        /// Why.
        reason: InventoryGap,
    },
}

impl Inventory {
    /// The observation that reports it.
    #[must_use]
    pub fn observation(&self) -> ObservationKind {
        match self {
            Self::Complete { vms } => ObservationKind::Inventory { vms: vms.clone() },
            Self::Incomplete { reason } => ObservationKind::InventoryIncomplete { reason: *reason },
        }
    }

    /// What it says about `name`: listed running or stopped, or absent; `None` when it is
    /// incomplete, which never means absent (I25). Pinned by
    /// `tests/data_plane/supervisor.rs::should_never_read_absence_when_the_inventory_is_incomplete`.
    #[must_use]
    pub fn seen(&self, name: &OwnedVmName) -> Option<Seen> {
        match self {
            Self::Complete { vms } => {
                let text = name.to_string();
                Some(
                    vms.iter()
                        .find(|vm| vm.name == text)
                        .map_or(Seen::Absent, |vm| {
                            if vm.running {
                                Seen::Running
                            } else {
                                Seen::Stopped
                            }
                        }),
                )
            }
            Self::Incomplete { .. } => None,
        }
    }
}
