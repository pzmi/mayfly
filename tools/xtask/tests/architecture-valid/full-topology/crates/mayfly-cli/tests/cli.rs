//! Black-box tests may build sites and run processes.

use std::process::Command;

#[test]
fn should_build_site_when_test_prepares_it() {
    // given
    let dir = std::env::temp_dir().join("site");

    // when
    let created = std::fs::create_dir_all(&dir).and_then(|()| Command::new("true").status());

    // then
    assert!(created.is_ok());
}
