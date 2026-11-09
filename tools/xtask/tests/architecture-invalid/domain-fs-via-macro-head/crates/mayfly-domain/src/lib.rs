//! Fixture: the crate reaches the domain only as a macro argument.

macro_rules! via {
    ($k:ident) => {
        $k::fs::read("index.html")
    };
}

pub fn load() -> Vec<u8> {
    via!(std).unwrap_or_default()
}
