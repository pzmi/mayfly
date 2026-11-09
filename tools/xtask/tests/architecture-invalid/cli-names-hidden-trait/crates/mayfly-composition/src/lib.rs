pub struct Server;

impl Server {
    pub fn start() -> Self {
        Server
    }
}

#[doc(hidden)]
pub trait PipelineCoordinator {
    fn settle(&mut self) -> bool;
}
