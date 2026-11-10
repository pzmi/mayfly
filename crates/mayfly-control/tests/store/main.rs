//! `AggregateStore` over the in-memory journal (component level, ver §2): the write path's
//! steps, receipts, epochs, retries, unknown outcomes and notifications; the codec's goldens;
//! and durable delivery over it: the delivery worker, kicks, the directive relay and its sink
//! over the link, and the attempt saga; and the inline projections, their rebuild and queries.

mod attempts;
mod codec;
mod counter;
mod delivery;
mod link_sink;
mod projections;
mod relay;
mod saga;
mod save;
mod scripted;
