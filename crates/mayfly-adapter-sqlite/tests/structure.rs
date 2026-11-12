//! Integration level: start-up's structural verification (`StructureCheck`, plan §10) over a
//! real SQLite file: a sound journal verifies, a file whose pages were damaged does not.

use mayfly_adapter_sqlite::{SqliteJournal, SqliteOptions};
use mayfly_control::recovery::StructureCheck as _;
use mayfly_domain::Millis;
use mayfly_testkit::block_on;

#[test]
fn should_verify_the_structure_when_the_journal_is_sound() {
    // given
    let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let journal = SqliteJournal::open(
        &dir.path().join("journal.db"),
        SqliteOptions::new(Millis::new(1)),
    )
    .unwrap_or_else(|e| panic!("{e}"));

    // when
    let verified = block_on(journal.verify());

    // then
    assert_eq!(verified, Ok(true));
}

#[test]
fn should_not_verify_the_structure_when_a_page_is_damaged() {
    // given
    let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let path = dir.path().join("journal.db");
    drop(
        SqliteJournal::open(&path, SqliteOptions::new(Millis::new(1)))
            .unwrap_or_else(|e| panic!("{e}")),
    );
    let mut bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{e}"));
    let page = 4096;
    assert!(
        bytes.len() > 2 * page,
        "the migrated file has several pages"
    );
    for byte in &mut bytes[page + 16..2 * page] {
        *byte = 0xa5;
    }
    std::fs::write(&path, &bytes).unwrap_or_else(|e| panic!("{e}"));

    // when
    let verified = SqliteJournal::open(&path, SqliteOptions::new(Millis::new(2)))
        .map(|journal| block_on(journal.verify()));

    // then
    assert!(
        !matches!(verified, Ok(Ok(true))),
        "a damaged file must not verify: {verified:?}"
    );
}
