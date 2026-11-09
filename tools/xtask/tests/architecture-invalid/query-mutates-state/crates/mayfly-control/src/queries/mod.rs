//! Fixture: a query mutates the pool state while it answers a read.

use crate::state::Pools;

pub struct Read;

impl Read {
    pub fn drained(&self, pools: &mut Pools) -> bool {
        pools.draining = true;
        pools.draining
    }
}
