//! Fixture: a decision iterates a std HashMap, whose order is randomised per process, so the
//! same command decided twice can emit events in a different order.

use std::collections::HashMap;

pub fn decide(slots: &HashMap<String, u32>) -> Vec<String> {
    slots.keys().cloned().collect()
}
