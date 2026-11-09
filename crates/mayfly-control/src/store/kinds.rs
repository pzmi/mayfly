//! What this build supports, read from its aggregate registry: the epochs every unit carries
//! and the start-up kind check that feeds safe mode (`doc/event-store.md` §6.2, §6.3).

use std::collections::{BTreeMap, BTreeSet};

use mayfly_control_domain::{AggregateRegistry, AggregateVisitor, DeterminismCases, known_kinds};

use crate::journal::{
    AggregateName, Journal, JournalValueError, MAX_PAGE, StorageError, StoredKind, StreamId,
    SupportedEpochs,
};

/// Every aggregate of registry `R` with its `SUPPORTED_EPOCH`: what `AggregateStore` puts in
/// every unit, so an aggregate's supported epoch reaches the store only through the registry
/// composition hands it (the epoch half of `doc/event-store.md` §6.2; pinned by
/// `tests/store/save.rs::should_carry_every_registered_epoch_when_built_from_a_registry`).
///
/// # Errors
/// [`JournalValueError::InvalidAggregateName`] when an aggregate's `NAME` is not a journal name.
pub fn supported_epochs<R: AggregateRegistry>() -> Result<SupportedEpochs, JournalValueError> {
    struct Epochs(Result<SupportedEpochs, JournalValueError>);
    impl AggregateVisitor for Epochs {
        fn visit<A: DeterminismCases>(&mut self) {
            if let Ok(epochs) = &self.0 {
                self.0 = AggregateName::new(A::NAME)
                    .map(|name| epochs.clone().with(name, A::SUPPORTED_EPOCH));
            }
        }
    }
    let mut epochs = Epochs(Ok(SupportedEpochs::new()));
    R::accept(&mut epochs);
    epochs.0
}

/// A stored kind this build does not decode, with the first stream (in `StreamId` order) that
/// holds it, for the safe-mode reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownStoredKind {
    /// The kind.
    pub kind: StoredKind,
    /// The first stream holding it; `None` only if the manifest lists a kind no stream holds
    /// (a store that lost events, which safe mode is right to stop for).
    pub first_stream: Option<StreamId>,
}

/// The start-up comparison of the event-kind manifest with what this build decodes
/// (`doc/event-store.md` §6.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KindCheck {
    /// Every stored kind is known: a normal start.
    Known,
    /// Some stored kind is unknown (an unregistered type, or a known type at a newer version):
    /// the daemon starts in safe mode, naming each kind and its first stream.
    SafeMode {
        /// The unknown kinds, ascending.
        unknown: Vec<UnknownStoredKind>,
    },
}

/// Compares `journal.stored_kinds()` with the kinds registry `R` decodes, current and legacy;
/// run after migrations and before any writer. Only when a kind is unknown are streams
/// enumerated (by key, `Journal::streams`) to name the first one holding it, stopping once
/// every unknown kind is placed. Pinned by
/// `tests/store/save.rs::should_start_in_safe_mode_when_a_stored_kind_is_unknown`.
///
/// # Errors
/// [`StorageError`] when the store fails.
pub async fn check_stored_kinds<R: AggregateRegistry, J: Journal>(
    journal: &J,
) -> Result<KindCheck, StorageError> {
    let known: BTreeSet<(String, u32)> = known_kinds::<R>()
        .into_iter()
        .map(|k| (k.event_type.to_owned(), k.schema_version))
        .collect();
    let mut unknown: BTreeMap<StoredKind, Option<StreamId>> = journal
        .stored_kinds()
        .await?
        .into_iter()
        .filter(|k| !known.contains(&(k.event_type.clone(), k.schema_version)))
        .map(|k| (k, None))
        .collect();
    if unknown.is_empty() {
        return Ok(KindCheck::Known);
    }
    let mut after: Option<StreamId> = None;
    'pages: loop {
        let page = journal.streams(after.as_ref(), MAX_PAGE).await?;
        let Some(last) = page.last().map(|h| h.stream.clone()) else {
            break;
        };
        for head in page {
            for event in journal.read_stream(&head.stream, None, None).await? {
                if let Some(first @ None) = unknown.get_mut(&event.kind) {
                    *first = Some(head.stream.clone());
                }
            }
            if unknown.values().all(Option::is_some) {
                break 'pages;
            }
        }
        after = Some(last);
    }
    Ok(KindCheck::SafeMode {
        unknown: unknown
            .into_iter()
            .map(|(kind, first_stream)| UnknownStoredKind { kind, first_stream })
            .collect(),
    })
}
