//! Fixture: a test that never runs.

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "flaky"]
    fn should_decide_when_asked() {
        // given
        let input = 1;

        // when
        let output = input + 1;

        // then
        assert_eq!(output, 2);
    }
}
