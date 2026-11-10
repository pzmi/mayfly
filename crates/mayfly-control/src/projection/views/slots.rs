//! `slots`: the leased slots of each host (`CapacityBoard`'s allocation side,
//! `doc/domain-model.md` §8), kept inline from each host stream.

use mayfly_control_domain::EventKind;
use mayfly_control_domain::attempt::GuestOs;
use mayfly_control_domain::host::{
    Holds, Host, HostEvent, HostState, LeaseActivated, LeasePhase, LeaseQuarantined, SlotReleased,
    SlotReserved,
};
use mayfly_domain::{AttemptId, Generation, PoolName, Resources};
use serde::{Deserialize, Serialize};

use crate::projection::spec::{EventMeta, ProjectionSpec, RowChange, Rows, sync_rows};

/// One leased slot of a host. Key: `<host stream>/<slot>`; deleted when the slot is released.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotRow {
    /// The attempt holding the slot.
    pub attempt_id: AttemptId,
    /// The generation it holds the slot under.
    pub generation: Generation,
    /// Its pool.
    pub pool: PoolName,
    /// Its guest OS.
    pub os: GuestOs,
    /// What its VM needs.
    pub resources: Resources,
    /// The lease's phase: `reserved`, `activated` or `quarantined`.
    pub phase: String,
    /// What a quarantined lease still holds: `full` or `disk_and_slot`.
    pub holds: Option<String>,
}

/// The `slots` projection: one row per live lease of a host. Counts across hosts are a query
/// over these rows ([`crate::queries::leases_by_host`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct Slots;

impl ProjectionSpec for Slots {
    const NAME: &'static str = "slots";
    const VERSION: u32 = 1;
    /// Every kind that adds, changes or removes a lease (`evolve`).
    const SUBSCRIBES: &'static [EventKind] = &[
        EventKind::of::<SlotReserved>(),
        EventKind::of::<LeaseActivated>(),
        EventKind::of::<LeaseQuarantined>(),
        EventKind::of::<SlotReleased>(),
    ];
    type Source = Host;
    type Row = SlotRow;

    fn apply(
        rows: &Rows<SlotRow>,
        _event: &HostEvent,
        after: &HostState,
        _at: EventMeta,
    ) -> Vec<RowChange<SlotRow>> {
        let desired = after
            .leases
            .iter()
            .map(|(attempt_id, lease)| {
                let (phase, holds) = match lease.phase {
                    LeasePhase::Reserved => ("reserved", None),
                    LeasePhase::Activated => ("activated", None),
                    LeasePhase::Quarantined { holds } => (
                        "quarantined",
                        Some(match holds {
                            Holds::Full => "full",
                            Holds::DiskAndSlot => "disk_and_slot",
                        }),
                    ),
                };
                (
                    lease.slot.get().to_string(),
                    SlotRow {
                        attempt_id: *attempt_id,
                        generation: lease.generation,
                        pool: lease.pool.clone(),
                        os: lease.os,
                        resources: lease.resources,
                        phase: phase.to_owned(),
                        holds: holds.map(str::to_owned),
                    },
                )
            })
            .collect();
        sync_rows(rows, desired)
    }
}
