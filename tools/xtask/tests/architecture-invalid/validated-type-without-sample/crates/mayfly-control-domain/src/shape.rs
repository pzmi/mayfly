//! Fixture: the tracer's list of value types, which names `PoolName` and `ImageDigest`.

pub fn texts() -> Vec<String> {
    let mut texts = vec![String::new()];
    for text in [
        String::from(PoolName::sample()),
        String::from(ImageDigest::sample()),
    ] {
        texts.push(text);
    }
    texts
}
