//! Fixture: a data-plane test names the SQLite adapter (checkpoint finding M1): tests are held
//! to the plane split too.

pub fn ready() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use mayfly_adapter_sqlite::Store;

    #[test]
    fn should_persist_when_the_executor_runs() {
        // given
        let store = Store::open();

        // when
        let ok = super::ready();

        // then
        assert!(ok && store.is_empty());
    }
}
