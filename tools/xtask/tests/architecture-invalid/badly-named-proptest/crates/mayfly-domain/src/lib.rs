//! Fixture: a property test inside proptest! with a name that says nothing.

#[cfg(test)]
mod tests {
    proptest::proptest! {
        #[test]
        fn roundtrip(n in 0u8..4) {
            // given
            let input = n;

            // when
            let output = input;

            // then
            proptest::prop_assert_eq!(output, n);
        }
    }
}
