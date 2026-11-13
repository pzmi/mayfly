//! Fixture: the secret wrapper and the one real holder that serialises it, beside a private
//! holder and a test-only holder whose names other types share.

pub struct Secret<T>(T);

#[derive(serde::Serialize)]
pub struct Launch {
    pub jit: Secret<String>,
}

struct Held {
    config: Secret<String>,
}

#[cfg(test)]
mod tests {
    pub struct World {
        pub key: super::Secret<String>,
    }
}
