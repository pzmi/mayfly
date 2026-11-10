//! A command handler that writes another stream's projection row itself.

pub struct RowWrite {
    pub key: String,
    pub row: Option<String>,
}

pub struct ProjectionWrite {
    pub stream: String,
    pub rows: Vec<RowWrite>,
}

pub fn count_for_pool(pool: &str) -> ProjectionWrite {
    ProjectionWrite {
        stream: format!("pool-{pool}"),
        rows: Vec::new(),
    }
}
