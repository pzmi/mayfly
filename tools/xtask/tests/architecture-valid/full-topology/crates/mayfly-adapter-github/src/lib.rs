//! Positive fixture: the HTTP adapter uses application and domain types only, and carries
//! one lint suppression approved in [[exceptions.lints]].

use mayfly_control::publish;

#[allow(clippy::type_complexity)]
pub fn handler() -> fn(&tokio_free::Sender, &mayfly_domain::Revision) {
    publish
}

mod tokio_free {
    pub type Sender = ();
}

#[cfg(test)]
mod tests {
    fn pending() -> u8 {
        todo!()
    }

    #[test]
    fn should_keep_helper_when_test_code_is_unfinished() {
        // given
        let helper: fn() -> u8 = pending;

        // when
        let named = format!("{helper:p}");

        // then
        assert!(!named.is_empty());
    }
}
