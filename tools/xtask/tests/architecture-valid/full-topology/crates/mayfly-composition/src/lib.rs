//! Positive fixture: composition is the one crate that sees both planes and the adapters; it
//! may spawn processes (plan section 3.4).

mod bootstrap;

pub fn run(root: &std::path::Path) -> u8 {
    match bootstrap::open_root(root) {
        Ok(site) => serve(site),
        Err(_) => 1,
    }
}

fn serve(site: bootstrap::Site) -> u8 {
    u8::from(site.entries == 0)
}

pub fn start_data_plane(current: &mayfly_domain::Revision) -> bool {
    let directive = mayfly_protocol::Directive {
        revision: mayfly_domain::Revision(current.0),
    };
    let spawned = std::process::Command::new("true").env_clear().status().is_ok();
    spawned
        && mayfly_data::accept(current, &directive)
        && mayfly_adapter_tart::clone_vm("runner-1").unwrap_or(false)
}
