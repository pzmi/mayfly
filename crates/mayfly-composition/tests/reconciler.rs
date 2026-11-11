//! The reconciler with each pool's `capacity_policy` mapped through the configuration (CP-03,
//! DEC-06F, ADR 0069), component level: the pass hands `capacity::plan` a `demand` pool's
//! `waiting`, so an idle `demand` pool keeps nothing ahead of a later pool, and one with assigned
//! jobs beyond its runners is carved for them; an `exact` pool keeps its whole headroom.

use mayfly_composition::config::{Config, GuestOs, reconciler_policies};
use mayfly_control::delivery::Kicks;
use mayfly_control::reconciler::{Reconciler, ReconcilerConfig};
use mayfly_control_domain::attempt;
use mayfly_domain::{HostId, PoolName};
use mayfly_testkit::block_on;
use mayfly_testkit::clock::FakeClock;
use mayfly_testkit::fleet::{EPOCH, PoolSpec, T0, World, report, store};
use mayfly_testkit::journal::MemoryJournal;

fn config(mac_policy: &str) -> Config {
    let text = format!(
        r#"schema_version = 1
state_dir = "/var/lib/mayfly"

[github]
organisation = "example-org"
app_client_id = "Iv1.0123456789abcdef"
installation_id = 12345678
private_key = "file:/etc/mayfly/app-key.pem"

[provider]
tart_home = "/var/lib/mayfly/tart"

[[pools]]
name = "amac"
tier = "trusted"
image_manifest = "/etc/mayfly/images/mac.toml"
guest_os = "macos"
cpu = 4
memory_mib = 8192
max_running = 2
capacity_policy = "{mac_policy}"

[[pools]]
name = "blin"
tier = "trusted"
image_manifest = "/etc/mayfly/images/lin.toml"
guest_os = "linux"
cpu = 2
memory_mib = 4096
max_running = 4
"#
    );
    Config::parse(&text).unwrap()
}

/// Runs one pass over a 13 GiB host serving the configured pools, `amac` with `mac_jobs`
/// assigned and `blin` with three; the leases per pool and `amac`'s reported max.
fn pass(mac_policy: &str, mac_jobs: u32) -> (usize, usize, u32) {
    let config = config(mac_policy);
    let journal = MemoryJournal::new();
    let clock = FakeClock::at(T0);
    let store = store(&journal, &clock);
    let mut world = World::new(&store);
    world.register_host("h1", report(13, 0, 8));
    for pool in &config.pools {
        world.configure_pool(PoolSpec {
            name: pool.name.clone(),
            os: match pool.guest_os {
                GuestOs::Macos => attempt::GuestOs::Macos,
                GuestOs::Linux => attempt::GuestOs::Linux,
            },
            memory_gib: pool.memory_mib / 1024,
            max_running: pool.max_running,
            min_prepared: pool.min_prepared,
            breaker: pool.breaker.failures,
        });
    }
    let mac = PoolName::new("amac").unwrap();
    let lin = PoolName::new("blin").unwrap();
    world.statistics(&mac, mac_jobs, 0, false);
    world.statistics(&lin, 3, 0, false);
    let mut reconciler_config = ReconcilerConfig::new(EPOCH);
    reconciler_config.policies = reconciler_policies(&config);
    let reconciler = Reconciler::new(&store, &journal, reconciler_config, Kicks::new(8));

    let report = block_on(reconciler.run_pass()).unwrap();

    assert!(report.refused.is_empty(), "{:?}", report.refused);
    let leases = world.host(&HostId::new("h1").unwrap()).leases;
    let of = |pool: &PoolName| leases.values().filter(|l| &l.pool == pool).count();
    (
        of(&mac),
        of(&lin),
        world.scale_set(&mac).capacity.reported_max,
    )
}

#[test]
fn should_keep_nothing_ahead_when_a_configured_demand_pool_waits_for_nothing() {
    // given: `amac` is `demand` with no job assigned
    let policy = "demand";

    // when
    let (mac, lin, told) = pass(policy, 0);

    // then: `blin` takes the 13 GiB: three 4 GiB VMs
    assert_eq!((mac, lin, told), (0, 3, 0));
}

#[test]
fn should_carve_a_demand_pool_only_for_its_assigned_jobs_when_configured() {
    // given: `amac` is `demand` with one job assigned
    let policy = "demand";

    // when
    let (mac, lin, told) = pass(policy, 1);

    // then: one 8 GiB macOS VM for it, the 5 GiB left for one `blin` VM
    assert_eq!((mac, lin, told), (1, 1, 1));
}

#[test]
fn should_keep_the_whole_headroom_when_the_pool_is_exact() {
    // given: the same idle pool under `exact`
    let policy = "exact";

    // when
    let (mac, lin, told) = pass(policy, 0);

    // then: it keeps room for its two VMs (16 GiB > 13 GiB), so `blin` gets nothing, and its
    // scale set is told the one VM that fits
    assert_eq!((mac, lin, told), (0, 0, 1));
}
