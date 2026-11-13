//! Fixture: secret-free types named like the holders in `mayfly-domain` (a production
//! `Launch`, a private `Held`, a test-only `World`) serialise freely: a holder is known by its
//! path, not by its simple name.

#[derive(serde::Serialize)]
pub struct Launch {
    pub name: String,
}

#[derive(serde::Serialize)]
pub struct Held {
    pub name: String,
}

#[derive(serde::Serialize)]
pub struct World {
    pub hosts: u32,
}

#[derive(serde::Serialize)]
pub struct Envelope {
    pub launch: Launch,
    pub held: Held,
    pub world: World,
}
