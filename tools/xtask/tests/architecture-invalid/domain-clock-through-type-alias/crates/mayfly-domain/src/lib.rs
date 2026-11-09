//! Fixture: a pure crate reads the clock through a type alias, which the name resolver
//! bound as a local item (second checkpoint review H2).

use std::time::SystemTime;

type Wall = SystemTime;

pub fn stamp() -> Wall {
    Wall::now()
}
