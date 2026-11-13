//! Fixture: the secret wrapper and a type that holds it.

pub struct Secret<T>(T);

pub struct Jit {
    config: Secret<String>,
}
