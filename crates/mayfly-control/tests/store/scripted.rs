//! `ScriptedJournal`: a `MemoryJournal` that misbehaves once, on purpose, at the moment a test
//! names: a competitor commits between the store's load and its append, or a commit's reply is
//! lost in a way the plain fault injection cannot stage.

use std::sync::{Arc, Mutex, PoisonError};

use mayfly_control::journal::{
    AggregateName, AppendError, Appended, CommandReceipt, EventLocation, ExpectedRevision,
    InboxKey, Journal, NewEvent, Payload, ProjectionName, RecordedEvent, StorageError, StoredKind,
    StreamHead, StreamId, StreamRevision, SupportedEpochs, UnitOfWork,
};
use mayfly_domain::{CommandId, EventId, Millis};
use mayfly_testkit::journal::MemoryJournal;

/// What the next appends do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Script {
    /// Behave.
    Plain,
    /// The identical unit commits first, as a twin run of the same command would: the store's
    /// own append is then answered by the twin's receipt.
    Twin,
    /// Before each of the next `n` appends, a competitor appends one event to `stream`.
    Race { stream: StreamId, n: u32 },
    /// The next append commits without its receipt, then its reply is lost.
    LoseReplyWithoutReceipt,
    /// The next append does not commit; its first event's id is recorded on `elsewhere` instead,
    /// and the reply is lost.
    LoseReplyAfterPlanting { elsewhere: StreamId },
}

/// See the module docs. Clones share the script and the store.
#[derive(Debug, Clone)]
pub struct ScriptedJournal {
    pub inner: MemoryJournal,
    script: Arc<Mutex<(Script, u128)>>,
}

impl ScriptedJournal {
    pub fn new(inner: MemoryJournal) -> Self {
        Self {
            inner,
            script: Arc::new(Mutex::new((Script::Plain, 0))),
        }
    }

    pub fn play(&self, script: Script) {
        self.script.lock().unwrap_or_else(PoisonError::into_inner).0 = script;
    }

    /// The script step for this append, consuming it.
    fn step(&self) -> (Script, u128) {
        let mut script = self.script.lock().unwrap_or_else(PoisonError::into_inner);
        script.1 += 1;
        let now = script.0.clone();
        script.0 = match &script.0 {
            Script::Race { stream, n } if *n > 1 => Script::Race {
                stream: stream.clone(),
                n: n - 1,
            },
            _ => Script::Plain,
        };
        (now, script.1)
    }
}

fn counter() -> AggregateName {
    AggregateName::new("counter").unwrap_or_else(|e| panic!("{e}"))
}

fn raw_unit(stream: &StreamId, expected: ExpectedRevision, id: EventId) -> UnitOfWork {
    UnitOfWork::new(Millis::new(1), SupportedEpochs::new().with(counter(), 1)).with_append(
        stream.clone(),
        counter(),
        expected,
        vec![NewEvent {
            event_id: id,
            kind: StoredKind {
                event_type: "counter_incremented".to_owned(),
                schema_version: 1,
            },
            required_epoch: 1,
            payload: Payload::new("{\"by\":1}"),
        }],
    )
}

fn lost() -> AppendError {
    AppendError::OutcomeUnknown(StorageError::new("scripted_lost_reply"))
}

impl Journal for ScriptedJournal {
    async fn append(&self, unit: UnitOfWork) -> Result<Appended, AppendError> {
        let (script, call) = self.step();
        match script {
            Script::Plain => self.inner.append(unit).await,
            Script::Twin => {
                self.inner.append(unit.clone()).await?;
                self.inner.append(unit).await
            }
            Script::Race { stream, .. } => {
                let head = self
                    .inner
                    .head(&stream)
                    .await
                    .map_err(AppendError::Storage)?;
                let competitor = raw_unit(
                    &stream,
                    ExpectedRevision::of(head.map(|h| h.revision)),
                    EventId::from_u128(0xC0FF_EE00 + call),
                );
                self.inner.append(competitor).await?;
                self.inner.append(unit).await
            }
            Script::LoseReplyWithoutReceipt => {
                let mut bare = unit;
                bare.receipt = None;
                self.inner.append(bare).await?;
                Err(lost())
            }
            Script::LoseReplyAfterPlanting { elsewhere } => {
                if let Some(id) = unit.event_ids().next() {
                    self.inner
                        .append(raw_unit(&elsewhere, ExpectedRevision::NoStream, id))
                        .await?;
                }
                Err(lost())
            }
        }
    }

    async fn head(&self, stream: &StreamId) -> Result<Option<StreamHead>, StorageError> {
        self.inner.head(stream).await
    }

    async fn read_stream(
        &self,
        stream: &StreamId,
        after: Option<StreamRevision>,
        through: Option<StreamRevision>,
    ) -> Result<Vec<RecordedEvent>, StorageError> {
        self.inner.read_stream(stream, after, through).await
    }

    async fn lookup_event(&self, event_id: EventId) -> Result<Option<EventLocation>, StorageError> {
        self.inner.lookup_event(event_id).await
    }

    async fn receipt(&self, command_id: CommandId) -> Result<Option<CommandReceipt>, StorageError> {
        self.inner.receipt(command_id).await
    }

    async fn inbox_contains(&self, key: &InboxKey) -> Result<bool, StorageError> {
        self.inner.inbox_contains(key).await
    }

    async fn streams(
        &self,
        after: Option<&StreamId>,
        limit: u32,
    ) -> Result<Vec<StreamHead>, StorageError> {
        self.inner.streams(after, limit).await
    }

    async fn stored_kinds(&self) -> Result<Vec<StoredKind>, StorageError> {
        self.inner.stored_kinds().await
    }

    async fn projection(
        &self,
        projection: &ProjectionName,
        key: &str,
    ) -> Result<Option<Payload>, StorageError> {
        self.inner.projection(projection, key).await
    }
}
