//! Fixture: the secret wrapper derives Debug, which prints the secret.

#[derive(Clone, Debug)]
pub struct Secret<T>(T);
