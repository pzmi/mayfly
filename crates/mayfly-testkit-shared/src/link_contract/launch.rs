//! LK-17 to LK-19: the JIT config of a launch on the link (protocol 1.6, LNK-02; plan §3.2,
//! ADR 0014, ADR 0082; `formal/link.qnt`'s `configSentOnce`, `configDeliveredOnce` and
//! `configOnlyInItsEpoch`).
//!
//! The suite plays both stores: [`SuiteBroker`] stands for the control plane's single-take
//! broker, [`SuiteStore`] for the data plane's `LaunchStore`. What the cases check is the
//! transport's part: that the control plane's end asks the broker once per send and carries
//! what it answers in that frame only, and that the data plane's end puts what it reads only
//! for a launch of its own session. Every config is built at run time, never a literal.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use mayfly_domain::{
    AttemptId, Deadline, DirectiveId, Epoch, Generation, HostId, Millis, OwnedVmName, PoolName,
    ProviderInstanceId, Sample, Secret, SecretHandleId, Slot,
};
use mayfly_protocol::{
    AttemptRef, CURRENT, Directive, DirectiveKind, DirectiveSink as _, Frame, IMAGE_LISTING,
    LaunchConfigSink, LaunchConfigSource, LaunchTarget, Message, ProtocolVersion, PutRefused,
    SUPPORTED, SecretHandle,
};
use serde_json::Value;

use super::{
    Check, LinkFactory, RawWire, SESSION_EPOCH, Session, at_least,
    control_plane_session_against_raw_over, data_plane_session_against_raw_over, directive,
    directive_key, drain_after, eq, frame_bytes, join2, ok, open_session_over, read_frame, set,
    step, step_ok, until_closed,
};
use crate::link_contract::DiscardCounts;

/// The member a launch's frame carries its config in (protocol 1.6).
const CONFIG_MEMBER: &str = "launch_config";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A config the broker holds: the attempt and the epoch it was minted for, and the config.
type Minted = (AttemptId, Epoch, Secret<String>);

/// The suite's control-plane broker: configs held by handle for an attempt and the epoch they
/// were minted under, each taken at most once, as `mayfly_control::registration::JitBroker`
/// holds them. A take for another attempt or epoch discards the config.
#[derive(Default)]
pub struct SuiteBroker {
    held: Mutex<BTreeMap<SecretHandleId, Minted>>,
}

impl SuiteBroker {
    /// Holds `config` behind `handle` for `attempt` and `epoch`.
    pub fn hold(&self, handle: SecretHandle, attempt: AttemptId, epoch: Epoch, config: String) {
        lock(&self.held).insert(handle.handle_id, (attempt, epoch, Secret::new(config)));
    }

    /// Whether a config is held behind `handle`.
    #[must_use]
    pub fn holds(&self, handle: SecretHandle) -> bool {
        lock(&self.held).contains_key(&handle.handle_id)
    }
}

impl LaunchConfigSource for SuiteBroker {
    fn take(
        &self,
        handle: SecretHandle,
        attempt: AttemptId,
        epoch: Epoch,
    ) -> Option<Secret<String>> {
        let (held_for, minted, config) = lock(&self.held).remove(&handle.handle_id)?;
        (held_for == attempt && minted == epoch).then_some(config)
    }
}

#[derive(Default)]
struct Stored {
    held: Vec<(SecretHandle, LaunchTarget, Secret<String>)>,
    used: BTreeSet<SecretHandleId>,
}

/// The suite's data-plane store: every config put, in put order; a second put for a handle is
/// refused, as `mayfly_data::LaunchStore` refuses it.
#[derive(Default)]
pub struct SuiteStore {
    stored: Mutex<Stored>,
}

impl SuiteStore {
    /// What it holds: each handle, its target and its config's text, in put order.
    #[must_use]
    pub fn held(&self) -> Vec<(SecretHandle, LaunchTarget, String)> {
        lock(&self.stored)
            .held
            .iter()
            .map(|(handle, target, config)| (*handle, target.clone(), config.expose().clone()))
            .collect()
    }
}

impl LaunchConfigSink for SuiteStore {
    fn put(
        &self,
        handle: SecretHandle,
        target: LaunchTarget,
        config: Secret<String>,
    ) -> Result<(), PutRefused> {
        let mut stored = lock(&self.stored);
        if !stored.used.insert(handle.handle_id) {
            return Err(PutRefused);
        }
        stored.held.push((handle, target, config));
        Ok(())
    }

    fn discard_held(&self) {
        lock(&self.stored).held.clear();
    }
}

// ---------------------------------------------------------------------------------------------
// Sample launches

/// A config, built at run time.
fn config(tag: &str) -> String {
    ["suite", "launch", tag, "config"].join("-")
}

fn handle(n: u128) -> SecretHandle {
    SecretHandle {
        handle_id: SecretHandleId::from_u128(n),
    }
}

fn vm(n: u128) -> OwnedVmName {
    OwnedVmName::new(
        PoolName::sample(),
        HostId::sample(),
        Slot::new(1),
        Generation::FIRST,
        AttemptId::from_u128(n),
    )
}

fn instance() -> Result<ProviderInstanceId, String> {
    ok(
        "an instance id",
        ProviderInstanceId::new("7e:0b:14:52:9a:c1"),
    )
}

fn deadline() -> Deadline {
    Deadline::at(Millis::new(1_700_000_060_000))
}

/// The launch `n` of attempt `n` in `epoch`, its config behind handle `n`; with the instance id
/// `VmCloned` reported, or without it (`checked: false`).
fn launch(n: u128, epoch: Epoch, checked: bool) -> Result<Directive, String> {
    Ok(Directive {
        directive_id: DirectiveId::from_u128(n),
        epoch,
        attempt: Some(AttemptRef {
            attempt_id: AttemptId::from_u128(n),
            generation: Generation::FIRST,
        }),
        deadline: deadline(),
        kind: DirectiveKind::LaunchRunner {
            vm_name: vm(n),
            launch: handle(n),
            instance_id: if checked { Some(instance()?) } else { None },
        },
    })
}

/// What a config of launch `n` is put for.
fn target(n: u128) -> Result<LaunchTarget, String> {
    Ok(LaunchTarget {
        vm_name: vm(n),
        instance_id: instance()?,
        deadline: deadline(),
    })
}

/// The frame of `directive` in `version`, written by the raw peer carrying `text`.
fn carrying(
    version: ProtocolVersion,
    directive: &Directive,
    text: &str,
) -> Result<Vec<u8>, String> {
    let plain = frame_bytes(version, Message::Directive(directive.clone()))?;
    let mut value: Value = ok("the peer's frame as JSON", serde_json::from_slice(&plain))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| "the peer's frame is not a JSON object".to_owned())?;
    object.insert(CONFIG_MEMBER.to_owned(), Value::from(text));
    ok("encoding the peer's frame", serde_json::to_vec(&value))
}

/// Whether `text` occurs anywhere in `bytes`.
fn names(bytes: &[u8], text: &str) -> bool {
    bytes.windows(text.len()).any(|w| w == text.as_bytes())
}

/// The plain JSON of `directive`'s frame in `version`, as a value to compare with.
fn plain_value(version: ProtocolVersion, directive: &Directive) -> Result<Value, String> {
    let bytes = frame_bytes(version, Message::Directive(directive.clone()))?;
    ok("the plain frame as JSON", serde_json::from_slice(&bytes))
}

const fn config_counts(configs: u64) -> DiscardCounts {
    DiscardCounts {
        version: 0,
        malformed: 0,
        unexpected: 0,
        configs,
    }
}

// ---------------------------------------------------------------------------------------------
// The cases

/// LK-17.
pub(super) async fn config_delivered_once<F: LinkFactory>(f: &F) -> Check {
    let broker = Arc::new(SuiteBroker::default());
    let store = Arc::new(SuiteStore::default());
    let sent = launch(0xb1, SESSION_EPOCH, true)?;
    broker.hold(
        handle(0xb1),
        AttemptId::from_u128(0xb1),
        SESSION_EPOCH,
        config("b1"),
    );
    let links = step(
        "connecting with configs",
        f.connect_with_configs(SUPPORTED, SUPPORTED, broker.clone(), store.clone()),
    )
    .await?;
    let (session, welcomed) = open_session_over(f, SUPPORTED, SUPPORTED, links).await?;
    eq("the session version", &welcomed.accepted_version, &CURRENT)?;
    let Session {
        mut dp_directives,
        dp_observations,
        cp_directives,
        cp_observations,
    } = session;
    let resends = async {
        for _ in 0..3 {
            ok("sending the launch", cp_directives.send(sent.clone()).await)?;
        }
        f.close_control_plane(cp_directives, cp_observations).await;
        Ok::<(), String>(())
    };
    let (sent_all, drained) = step(
        "the launch and its resends",
        join2(
            resends,
            until_closed(
                "every copy of the launch",
                &mut dp_directives,
                &set([directive_key(&sent)]),
                directive_key,
            ),
        ),
    )
    .await?;
    sent_all?;
    drained?;
    drop(dp_observations);
    eq(
        "what the data plane's store holds",
        &store.held(),
        &vec![(handle(0xb1), target(0xb1)?, config("b1"))],
    )?;
    eq(
        "whether the broker still holds the config",
        &broker.holds(handle(0xb1)),
        &false,
    )
}

/// LK-18.
pub(super) async fn config_only_in_its_frame<F: LinkFactory>(f: &F) -> Check {
    // A session of 1.6: launch c1 is held for its attempt, c2 for another attempt.
    let broker = Arc::new(SuiteBroker::default());
    broker.hold(
        handle(0xc1),
        AttemptId::from_u128(0xc1),
        SESSION_EPOCH,
        config("c1"),
    );
    broker.hold(
        handle(0xc2),
        AttemptId::from_u128(0xc9),
        SESSION_EPOCH,
        config("c2"),
    );
    let links = step(
        "connecting with configs",
        f.control_plane_facing_raw_with_configs(SUPPORTED, broker.clone()),
    )
    .await?;
    let (directives, _observations, mut raw) =
        control_plane_session_against_raw_over(f, CURRENT, links).await?;
    let first = launch(0xc1, SESSION_EPOCH, true)?;
    let other_attempt = launch(0xc2, SESSION_EPOCH, true)?;
    // c6 and c7 are held for their own attempt and the session's epoch, but the data plane
    // would put neither (no instance id; stamped with another epoch): nothing is taken.
    for n in [0xc6, 0xc7] {
        broker.hold(
            handle(n),
            AttemptId::from_u128(n),
            SESSION_EPOCH,
            config(&format!("{n:x}")),
        );
    }
    let unchecked = launch(0xc6, SESSION_EPOCH, false)?;
    let other_epoch = launch(0xc7, Epoch::new(4), true)?;
    let sends = [
        ("the launch's first send", first.clone(), true),
        ("its resend", first, false),
        ("another directive", directive(0xc3, SESSION_EPOCH), false),
        ("a launch of another attempt", other_attempt, false),
        ("a launch without an instance id", unchecked, false),
        ("a launch of another epoch", other_epoch, false),
    ];
    for (what, sent, carries) in sends {
        step_ok(what, directives.send(sent.clone())).await?;
        let bytes = step(what, raw.recv())
            .await?
            .ok_or_else(|| format!("{what}: the control plane closed"))?;
        let mut value: Value = ok(what, serde_json::from_slice(&bytes))?;
        let member = value
            .as_object_mut()
            .and_then(|object| object.remove(CONFIG_MEMBER));
        if carries {
            eq(
                &format!("{what}: the config member"),
                &member,
                &Some(Value::from(config("c1"))),
            )?;
        } else if member.is_some()
            || ["c1", "c2", "c6", "c7"]
                .iter()
                .any(|tag| names(&bytes, &config(tag)))
        {
            return Err(format!(
                "{what}: the frame carries a config, which it may not"
            ));
        }
        eq(
            &format!("{what}: the frame beside the member"),
            &value,
            &plain_value(CURRENT, &sent)?,
        )?;
    }
    eq(
        "whether the broker holds c1",
        &broker.holds(handle(0xc1)),
        &false,
    )?;
    eq(
        "whether the broker holds c2",
        &broker.holds(handle(0xc2)),
        &false,
    )?;
    eq(
        "whether the broker holds c6 and c7",
        &(broker.holds(handle(0xc6)), broker.holds(handle(0xc7))),
        &(true, true),
    )?;

    // A session of 1.5: nothing is taken, nothing carried.
    let old = launch(0xc4, SESSION_EPOCH, true)?;
    broker.hold(
        handle(0xc4),
        AttemptId::from_u128(0xc4),
        SESSION_EPOCH,
        config("c4"),
    );
    let links = step(
        "connecting with configs",
        f.control_plane_facing_raw_with_configs(SUPPORTED, broker.clone()),
    )
    .await?;
    let (directives, _observations, mut raw) =
        control_plane_session_against_raw_over(f, IMAGE_LISTING, links).await?;
    step_ok("a launch in a 1.5 session", directives.send(old.clone())).await?;
    let bytes = step("a launch in a 1.5 session", raw.recv())
        .await?
        .ok_or_else(|| "a launch in a 1.5 session: the control plane closed".to_owned())?;
    if names(&bytes, CONFIG_MEMBER) || names(&bytes, &config("c4")) {
        return Err("a launch in a 1.5 session carries a config, which it may not".to_owned());
    }
    eq(
        "a launch in a 1.5 session",
        &read_frame("a launch in a 1.5 session", Some(bytes))?,
        &Frame {
            protocol_version: IMAGE_LISTING,
            message: Message::Directive(old),
        },
    )?;
    eq(
        "whether the broker holds c4",
        &broker.holds(handle(0xc4)),
        &true,
    )?;

    // A send that fails names no byte of the config it took.
    let lost = launch(0xc5, SESSION_EPOCH, true)?;
    broker.hold(
        handle(0xc5),
        AttemptId::from_u128(0xc5),
        SESSION_EPOCH,
        config("c5"),
    );
    let links = step(
        "connecting with configs",
        f.control_plane_facing_raw_with_configs(SUPPORTED, broker.clone()),
    )
    .await?;
    let (directives, _observations, raw) =
        control_plane_session_against_raw_over(f, CURRENT, links).await?;
    drop(raw);
    if let Err(error) = step("a send to a peer that left", directives.send(lost)).await? {
        let shown = format!("{error} {error:?}");
        if shown.contains(&config("c5")) {
            return Err(format!("a failed send names its config: {shown}"));
        }
    }
    Ok(())
}

/// LK-19.
pub(super) async fn config_put_only_for_its_session<F: LinkFactory>(f: &F) -> Check {
    // A config held before the session opens was put for none of its launches: dropped.
    let store = Arc::new(SuiteStore::default());
    store
        .put(handle(0xd0), target(0xd0)?, Secret::new(config("d0")))
        .map_err(|PutRefused| "a config held before the session: refused".to_owned())?;
    let links = step(
        "connecting with configs",
        f.data_plane_facing_raw_with_configs(SUPPORTED, store.clone()),
    )
    .await?;
    let (mut directives, _observations, mut raw) =
        data_plane_session_against_raw_over(f, SUPPORTED, CURRENT, links).await?;
    eq(
        "what the store holds once the session opened",
        &store.held(),
        &Vec::new(),
    )?;
    let own = launch(0xd1, SESSION_EPOCH, true)?;
    let stale = launch(0xd2, Epoch::new(4), true)?;
    let other = directive(0xd3, SESSION_EPOCH);
    let unchecked = launch(0xd4, SESSION_EPOCH, false)?;
    let frames = vec![
        carrying(CURRENT, &own, &config("d1"))?,
        carrying(CURRENT, &stale, &config("d2"))?,
        carrying(CURRENT, &other, &config("d3"))?,
        carrying(CURRENT, &unchecked, &config("d4"))?,
        carrying(CURRENT, &own, &config("d1"))?,
    ];
    drain_after(
        "launches carrying configs",
        &mut raw,
        frames,
        &mut directives,
        &set([&own, &stale, &other, &unchecked].map(directive_key)),
        directive_key,
    )
    .await?;
    eq(
        "what the data plane's store holds",
        &store.held(),
        &vec![(handle(0xd1), target(0xd1)?, config("d1"))],
    )?;
    at_least(
        "the configs the data plane dropped",
        f.data_plane_session_discards(&directives),
        config_counts(4),
    )?;

    // A session of 1.5 puts nothing.
    let old_store = Arc::new(SuiteStore::default());
    let links = step(
        "connecting with configs",
        f.data_plane_facing_raw_with_configs(SUPPORTED, old_store.clone()),
    )
    .await?;
    let (mut directives, _observations, mut raw) =
        data_plane_session_against_raw_over(f, SUPPORTED, IMAGE_LISTING, links).await?;
    let old = launch(0xd5, SESSION_EPOCH, true)?;
    drain_after(
        "a launch carrying a config in a 1.5 session",
        &mut raw,
        vec![carrying(IMAGE_LISTING, &old, &config("d5"))?],
        &mut directives,
        &set([directive_key(&old)]),
        directive_key,
    )
    .await?;
    eq(
        "what a 1.5 session's store holds",
        &old_store.held(),
        &Vec::new(),
    )?;
    at_least(
        "the configs a 1.5 session dropped",
        f.data_plane_session_discards(&directives),
        config_counts(1),
    )
}
