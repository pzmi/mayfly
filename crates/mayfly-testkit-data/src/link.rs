//! An in-memory link for the executor's tests: bounded channels of protocol values, no
//! encoding (the transports and their contract suite are DP-02's).

use std::sync::{Arc, Mutex, PoisonError};

use mayfly_protocol::{
    Directive, DirectiveSource, HandshakeInitiator, Hello, LinkError, Observation, ObservationSink,
    Welcome,
};
use tokio::sync::mpsc;

/// The control plane's end of the directive channel; dropping it closes the source.
#[derive(Debug, Clone)]
pub struct DirectiveFeed(mpsc::Sender<Directive>);

impl DirectiveFeed {
    /// Sends `directive`, waiting while the channel is full.
    ///
    /// # Panics
    /// When the executor dropped its source.
    pub async fn send(&self, directive: Directive) {
        assert!(
            self.0.send(directive).await.is_ok(),
            "the executor dropped its directive source"
        );
    }
}

/// The executor's end of the directive channel.
#[derive(Debug)]
pub struct FeedSource(mpsc::Receiver<Directive>);

impl DirectiveSource for FeedSource {
    async fn recv(&mut self) -> Result<Directive, LinkError> {
        self.0.recv().await.ok_or(LinkError::Closed)
    }
}

/// A directive channel holding `capacity` directives.
#[must_use]
pub fn directives(capacity: usize) -> (DirectiveFeed, FeedSource) {
    let (tx, rx) = mpsc::channel(capacity.max(1));
    (DirectiveFeed(tx), FeedSource(rx))
}

/// The executor's end of the observation channel.
#[derive(Debug, Clone)]
pub struct TapSink(mpsc::Sender<Observation>);

impl ObservationSink for TapSink {
    async fn send(&self, observation: Observation) -> Result<(), LinkError> {
        self.0
            .send(observation)
            .await
            .map_err(|_| LinkError::Closed)
    }
}

/// The control plane's end of the observation channel: it keeps every observation it read.
#[derive(Debug)]
pub struct Tap {
    rx: mpsc::Receiver<Observation>,
    seen: Vec<Observation>,
}

impl Tap {
    /// Reads until an observation satisfies `wanted`, and returns it.
    ///
    /// # Panics
    /// When the channel closes first.
    pub async fn wait_for(&mut self, wanted: impl Fn(&Observation) -> bool) -> Observation {
        loop {
            let Some(observation) = self.rx.recv().await else {
                panic!("the observation channel closed before the awaited observation");
            };
            self.seen.push(observation.clone());
            if wanted(&observation) {
                return observation;
            }
        }
    }

    /// Reads what is queued without waiting, then returns everything read so far.
    pub fn drain(&mut self) -> Vec<Observation> {
        while let Ok(observation) = self.rx.try_recv() {
            self.seen.push(observation);
        }
        self.seen.clone()
    }
}

/// An observation channel holding `capacity` observations.
#[must_use]
pub fn observations(capacity: usize) -> (TapSink, Tap) {
    let (tx, rx) = mpsc::channel(capacity.max(1));
    (
        TapSink(tx),
        Tap {
            rx,
            seen: Vec::new(),
        },
    )
}

/// A handshake that answers every hello with one answer and keeps the hellos.
#[derive(Debug, Clone)]
pub struct StaticWelcome {
    answer: Result<Welcome, LinkError>,
    hellos: Arc<Mutex<Vec<Hello>>>,
}

impl StaticWelcome {
    /// Answers `answer`.
    #[must_use]
    pub fn new(answer: Result<Welcome, LinkError>) -> Self {
        Self {
            answer,
            hellos: Arc::default(),
        }
    }

    /// The hellos received.
    #[must_use]
    pub fn hellos(&self) -> Vec<Hello> {
        self.hellos
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl HandshakeInitiator for StaticWelcome {
    async fn hello(&mut self, hello: Hello) -> Result<Welcome, LinkError> {
        self.hellos
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(hello);
        self.answer.clone()
    }
}
