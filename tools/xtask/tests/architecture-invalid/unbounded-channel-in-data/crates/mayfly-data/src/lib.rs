//! Fixture: the data plane queues observations on an unbounded channel, so a stalled control
//! plane grows it without limit.

pub fn link() {
    let (_tx, _rx) = tokio::sync::mpsc::unbounded_channel::<u8>();
}
