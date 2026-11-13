//! `AggregateStore` over the in-memory journal (component level, ver §2): the write path's
//! steps, receipts, epochs, retries, unknown outcomes and notifications; the codec's goldens;
//! and durable delivery over it: the delivery worker, kicks, the directive relay and its sink
//! over the link, and the attempt saga; the inline projections, their rebuild and queries; and
//! the start of an attempt (P2), the image catalog's pin (OPS-02), the observation inbox (P8),
//! startup recovery (P1) and the application of a recovery plan (P20); the terms policy (P4); and
//! the registration manager and the reaper (P9 to P14 but P12, CP-05); the reconciler's
//! activation grant (P9, CP-09); and the operator's commands (RT-03).

mod activation;
mod attempts;
mod codec;
mod counter;
mod delivery;
mod images;
mod inbox;
mod link_sink;
mod operator;
mod placement;
mod projections;
mod reconciler;
mod recovery;
mod recovery_kit;
mod registration;
mod relay;
mod repair;
mod saga;
mod save;
mod scripted;
mod start;
mod terms;
