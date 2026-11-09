//! Fixture: the data plane writes the control plane's journal, over a forbidden edge.

use mayfly_control::Journal;

pub fn record(journal: &mut impl Journal) {
    journal.append(1);
}
