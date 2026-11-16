//! The runner watch (DP-09): which runners are read, and what an answer reports.

use mayfly_data_domain::{Inventory, Ledger, RunnerStatus, RunnerWatch};
use mayfly_domain::Epoch;
use mayfly_protocol::{InventoryGap, ObservationKind};

use super::support::{EPOCH, ledger, listed_vm, listing, roomy, vm};

/// A ledger that applied `inventory`.
fn applied(inventory: &Inventory) -> Ledger {
    let mut counted = ledger(roomy());
    counted.apply_inventory(inventory);
    counted
}

/// A complete listing of `vm(n)` for each `(n, running)`.
fn listed(vms: &[(u128, bool)]) -> Inventory {
    listing(
        vms.iter()
            .map(|(n, running)| listed_vm(&vm(*n).to_string(), None, *running))
            .collect(),
    )
}

fn exited(n: u128, code: i32) -> ObservationKind {
    ObservationKind::RunnerExited {
        vm_name: vm(n),
        code,
    }
}

#[test]
fn should_adopt_only_the_running_vms_when_the_first_complete_inventory_arrives() {
    // given: an incomplete listing first, then a complete one, then another with a new VM
    let mut watch = RunnerWatch::new();
    let incomplete = Inventory::Incomplete {
        reason: InventoryGap::Deadline,
    };
    let first = listed(&[(1, true), (2, false)]);
    let later = listed(&[(1, true), (2, false), (3, true)]);

    // when
    watch.observed(&incomplete, &applied(&incomplete).vms);
    let before = watch.adopting;
    watch.observed(&first, &applied(&first).vms);
    let ledger = applied(&later);
    watch.observed(&later, &ledger.vms);

    // then: the incomplete one adopted nothing; the first complete one adopted the running VM;
    // a VM started later is read only once a launch reached it
    assert!(before);
    assert!(!watch.adopting);
    assert_eq!(watch.due(&ledger.vms), [vm(1)]);
}

#[test]
fn should_read_a_vm_when_a_launch_reached_it() {
    // given: no VM when the process started; one started since
    let mut watch = RunnerWatch::new();
    let empty = listed(&[]);
    watch.observed(&empty, &applied(&empty).vms);
    let ledger = applied(&listed(&[(1, true), (2, true)]));

    // when
    let before = watch.due(&ledger.vms);
    watch.launched(&vm(2));

    // then
    assert!(before.is_empty());
    assert_eq!(watch.due(&ledger.vms), [vm(2)]);
}

#[test]
fn should_read_no_vm_when_the_ledger_counts_it_stopped() {
    // given: a launched VM that stopped
    let mut watch = RunnerWatch::new();
    let stopped = listed(&[(1, false)]);
    let ledger = applied(&stopped);
    watch.observed(&stopped, &ledger.vms);
    watch.launched(&vm(1));

    // when
    let due = watch.due(&ledger.vms);

    // then: a stopped guest cannot answer
    assert!(due.is_empty());
}

#[test]
fn should_report_an_exit_once_when_the_guest_says_exited() {
    // given
    let mut watch = RunnerWatch::new();
    let running = listed(&[(1, true)]);
    let ledger = applied(&running);
    watch.observed(&running, &ledger.vms);
    watch.launched(&vm(1));

    // when
    let first = watch.answered(&vm(1), Some(RunnerStatus::Exited { code: 3 }), EPOCH);
    let again = watch.answered(&vm(1), Some(RunnerStatus::Exited { code: 3 }), EPOCH);

    // then: reported once, and not read again
    assert_eq!(first, Some(exited(1, 3)));
    assert_eq!(again, None);
    assert!(watch.due(&ledger.vms).is_empty());
}

#[test]
fn should_record_nothing_when_the_status_cannot_be_read() {
    // given
    let mut watch = RunnerWatch::new();
    let running = listed(&[(1, true)]);
    let ledger = applied(&running);
    watch.observed(&running, &ledger.vms);
    watch.launched(&vm(1));

    // when
    let unread = watch.answered(&vm(1), None, EPOCH);
    let still = watch.answered(&vm(1), Some(RunnerStatus::Running), EPOCH);

    // then: nothing is reported, and the VM is read again (plan §6: never an exit)
    assert_eq!((unread, still), (None, None));
    assert_eq!(watch.due(&ledger.vms), [vm(1)]);
}

#[test]
fn should_drop_an_adopted_vm_when_no_launch_reached_it() {
    // given: two VMs adopted after a restart
    let mut watch = RunnerWatch::new();
    let running = listed(&[(1, true), (2, true)]);
    let ledger = applied(&running);
    watch.observed(&running, &ledger.vms);
    watch.launched(&vm(2));

    // when: both guests say no launch reached them
    let one = watch.answered(&vm(1), Some(RunnerStatus::NotLaunched), EPOCH);
    let two = watch.answered(&vm(2), Some(RunnerStatus::NotLaunched), EPOCH);

    // then: the adopted one is no longer read; the one this process launched still is
    assert_eq!((one, two), (None, None));
    assert_eq!(watch.due(&ledger.vms), [vm(2)]);
}

#[test]
fn should_forget_a_vm_when_a_complete_inventory_no_longer_lists_it() {
    // given: an exit reported for vm 1
    let mut watch = RunnerWatch::new();
    let running = listed(&[(1, true)]);
    watch.observed(&running, &applied(&running).vms);
    let _reported = watch.answered(&vm(1), Some(RunnerStatus::Exited { code: 0 }), EPOCH);

    // when: an incomplete listing, then a complete one without it
    let incomplete = Inventory::Incomplete {
        reason: InventoryGap::Deadline,
    };
    let kept = applied(&running);
    watch.observed(&incomplete, &kept.vms);
    let after_incomplete = watch.watched.contains_key(&vm(1));
    let gone = listed(&[]);
    let ledger = applied(&gone);
    watch.observed(&gone, &ledger.vms);

    // then: nothing is forgotten on an incomplete listing (I25); a gone VM is
    assert!(after_incomplete);
    assert!(watch.watched.is_empty());
}

#[test]
fn should_send_a_reported_exit_again_once_when_a_new_session_opens() {
    // given: an exit reported in the session of EPOCH
    let mut watch = RunnerWatch::new();
    let running = listed(&[(1, true)]);
    let ledger = applied(&running);
    watch.observed(&running, &ledger.vms);
    let _reported = watch.answered(&vm(1), Some(RunnerStatus::Exited { code: 2 }), EPOCH);
    let next = Epoch::new(EPOCH.get() + 1);

    // when
    let same = watch.resend(EPOCH, &ledger.vms);
    let opened = watch.resend(next, &ledger.vms);
    let twice = watch.resend(next, &ledger.vms);

    // then: sent again in the new session, once; never in the session that reported it
    assert!(same.is_empty());
    assert_eq!(opened, [exited(1, 2)]);
    assert!(twice.is_empty());
}
