//! The supervisor's inputs: an inventory is complete or says nothing (I25).

use mayfly_data_domain::{Inventory, Seen, VmFact, VmFacts, VmState, supervisor};
use mayfly_protocol::{InventoryGap, ObservationKind, VmObservation};

use super::support::vm;

#[test]
fn should_never_read_absence_when_the_inventory_is_incomplete() {
    // given
    let incomplete = Inventory::Incomplete {
        reason: InventoryGap::Deadline,
    };
    let running = supervisor::step(VmFacts::UNOBSERVED, VmFact::Inventory(Seen::Stopped));

    // when
    let seen = incomplete.seen(&vm(1));
    let after = supervisor::step(running, VmFact::InventoryIncomplete);

    // then
    assert_eq!(seen, None);
    assert_eq!(after, running);
    assert_eq!(supervisor::state(after), VmState::Present);
    assert_eq!(
        incomplete.observation(),
        ObservationKind::InventoryIncomplete {
            reason: InventoryGap::Deadline
        }
    );
}

#[test]
fn should_say_listed_or_absent_when_the_inventory_is_complete() {
    // given
    let vms = vec![
        VmObservation {
            name: vm(1).to_string(),
            instance_id: None,
            running: true,
            guest: None,
            memory_bytes: None,
            disk_bytes: None,
        },
        VmObservation {
            name: vm(2).to_string(),
            instance_id: None,
            running: false,
            guest: None,
            memory_bytes: None,
            disk_bytes: None,
        },
    ];
    let complete = Inventory::Complete { vms: vms.clone() };

    // when
    let seen = [vm(1), vm(2), vm(3)].map(|name| complete.seen(&name));

    // then
    assert_eq!(
        seen,
        [Some(Seen::Running), Some(Seen::Stopped), Some(Seen::Absent)]
    );
    assert_eq!(complete.observation(), ObservationKind::Inventory { vms });
}
