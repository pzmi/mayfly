//! Fixture: a macro 2.0 item, whose body `syn` keeps as unparsed tokens, so neither the
//! macro-export rule nor the token scan of `macro_rules!` bodies read it (third checkpoint
//! review M2).

pub macro clock() {
    0
}
