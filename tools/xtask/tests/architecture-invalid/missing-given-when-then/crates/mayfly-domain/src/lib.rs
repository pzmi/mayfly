//! Fixture: a test without given/when/then structure.

#[cfg(test)]
mod tests {
    #[test]
    fn should_add_when_inputs_are_small() {
        assert_eq!(1 + 1, 2);
    }
}
