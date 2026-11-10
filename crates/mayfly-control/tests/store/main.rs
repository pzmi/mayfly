//! `AggregateStore` over the in-memory journal (component level, ver §2): the write path's
//! steps, receipts, epochs, retries, unknown outcomes and notifications; the codec's goldens;
//! and durable delivery over it: the delivery worker, kicks and the directive relay.

mod codec;
mod counter;
mod delivery;
mod relay;
mod save;
mod scripted;
