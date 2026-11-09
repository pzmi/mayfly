//! `AggregateStore` over the in-memory journal (component level, ver §2): the write path's
//! steps, receipts, epochs, retries, unknown outcomes and notifications; and the codec's goldens.

mod codec;
mod counter;
mod save;
mod scripted;
