//! Fixture: a pure crate's test reads the build environment beyond its own manifest
//! directory, so its result depends on the machine (third checkpoint review M3).

pub fn value() -> u8 {
    1
}

#[cfg(test)]
mod tests {
    #[test]
    fn should_read_home_when_built() {
        // given
        let manifest = env!("CARGO_MANIFEST_DIR");

        // when
        let home = option_env!("HOME");

        // then
        assert!(!manifest.is_empty() && home.is_some());
    }
}
