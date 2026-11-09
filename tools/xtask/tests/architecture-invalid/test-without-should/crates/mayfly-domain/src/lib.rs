//! Fixture: a test whose name says nothing.

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        // given
        let input = 1;

        // when
        let output = input + 1;

        // then
        assert_eq!(output, 2);
    }
}
