pub fn run(coordinator: &mut dyn mayfly_composition::PipelineCoordinator) -> bool {
    coordinator.settle()
}
