//! Fixture: the domain names tokio only inside a macro body.

macro_rules! spawn_detached {
    ($task:expr) => {
        tokio::spawn($task)
    };
}

pub fn start() {
    spawn_detached!(async {});
}
