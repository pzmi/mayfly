//! Integration tests may use the filesystem.

#[test]
fn should_see_temp_dir_when_test_runs() {
    // given
    let dir = std::env::temp_dir();

    // when
    let exists = std::fs::metadata(&dir).is_ok();

    // then
    assert!(exists);
}
