//! Projections (`doc/event-store.md` §3.6, plan §5.5): read models kept inline by the units
//! that change them, stream by stream, rebuilt by enumerating streams.
//!
//! - [`ProjectionSpec`]: one read model as a pure fold over one stream (its rows, an event and
//!   the aggregate's state after it), with a name, a version and the exact kinds it reads.
//! - [`Projection`], [`Projections`]: specs with their types erased, the set a store keeps.
//!   `AggregateStore::with_projections` computes each unit's rows **outside** the transaction,
//!   from the events its decision was made on plus its batch ([`Projection::inline`]), and the
//!   journal puts them **inside** the unit's transaction (ES-I9) in place of the stream's rows
//!   in the active generation (the whole set, so a missed write heals), with the stream's new
//!   head as the revision applied.
//! - [`ProjectionStore`] (reads of the active generation) and [`ProjectionRebuild`] (a new
//!   generation, stream by stream, then a cutover in one transaction): two ports, so a query
//!   takes the read port only.
//! - [`prepare`]: the offline rebuild at start-up of every projection whose stored version or
//!   build differs, or with a stream behind its head (ES-I11; ES-12 makes it online with dual
//!   writes and a cutover fence). The cutover is refused while a stream moved since it was
//!   folded, and a generation belongs to the rebuild that claimed it.
//! - [`views`]: `attempts`, `pending_effects`, `slots`, `pools`, `recent_failures`, `hosts`,
//!   `scale_sets`, `images`, `placements` and `open_registrations`.
//!
//! **Locality (ES-I10).** A spec sees one stream and returns suffixes; the fold keys them under
//! that stream; the journal refuses a write for a stream the unit does not append to, or a key
//! outside its stream, `Malformed(ProjectionOutsideUnit)` (PC-05); `ARCH-projection-locality`
//! refuses the code that could build one elsewhere.

mod fold;
mod ports;
mod rebuild;
mod spec;
pub mod views;

pub use fold::{EncodedRows, Projection, ProjectionError, Projections};
pub use ports::{
    BuildId, Cutover, Generation, InvalidBuildId, ProjectionMeta, ProjectionRebuild,
    ProjectionStore, StreamRows,
};
pub use rebuild::{
    Prepared, REBUILD_PASSES, RebuildError, needs_rebuild, prepare, rebuild_projection,
};
pub use spec::{EventMeta, ProjectionSpec, RowChange, Rows, SpecEvent, SpecState, sync_rows};

/// The control plane's projections: [`views::Attempts`], [`views::PendingEffects`],
/// [`views::Slots`], [`views::Pools`], [`views::RecentFailures`], [`views::Hosts`],
/// [`views::ScaleSets`], [`views::Images`], [`views::Placements`] and
/// [`views::OpenRegistrations`].
///
/// # Errors
/// [`ProjectionError::InvalidSpec`] if a spec's name is not a journal name (a build-time
/// mistake: pinned by `should_build_the_control_projections_when_asked`).
pub fn control() -> Result<Projections, ProjectionError> {
    Projections::new(vec![
        Projection::of::<views::Attempts>()?,
        Projection::of::<views::PendingEffects>()?,
        Projection::of::<views::Slots>()?,
        Projection::of::<views::Pools>()?,
        Projection::of::<views::RecentFailures>()?,
        Projection::of::<views::Hosts>()?,
        Projection::of::<views::ScaleSets>()?,
        Projection::of::<views::Images>()?,
        Projection::of::<views::Placements>()?,
        Projection::of::<views::OpenRegistrations>()?,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_build_the_control_projections_when_asked() {
        // given
        let names = [
            "attempts",
            "pending_effects",
            "slots",
            "pools",
            "recent_failures",
            "hosts",
            "scale_sets",
            "images",
            "placements",
            "open_registrations",
        ];

        // when
        let projections = control();

        // then
        let projections = projections.unwrap_or_else(|e| panic!("{e}"));
        let built: Vec<&str> = projections
            .all()
            .iter()
            .map(|p| p.name().as_str())
            .collect();
        assert_eq!(built, names);
    }
}
