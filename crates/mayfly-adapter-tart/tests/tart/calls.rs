//! What the adapter promises about every `tart` call, read from the fake's invocation log: the
//! environment (`TART_HOME` and `TART_NO_AUTO_PRUNE` on every call, plan §8.2), argv shapes
//! (clone never `--overwrite`, clone only from a local base then `set --random-mac`), and the
//! version probe.

use mayfly_adapter_tart::{ProbeError, TartProvider};
use mayfly_data::{Inventory, InventoryGap, VmProvider};
use mayfly_domain::Millis;
use mayfly_testkit_data::provider_contract::{START_MEMORY, base, proof, vm};
use serde_json::Value;

use crate::support::Fake;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
}

fn argv(record: &Value) -> Vec<String> {
    record["argv"]
        .as_array()
        .map(|args| {
            args.iter()
                .filter_map(|a| a.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn should_set_tart_home_and_no_auto_prune_on_every_call_when_the_adapter_runs_tart() {
    // given
    let fake = Fake::new();
    assert!(fake.tart(&["create", base().as_str()]).status.success());
    let provider = TartProvider::new(fake.config());
    let runtime = runtime();
    let options = mayfly_data::RunOptions {
        resources: mayfly_domain::Resources::new(START_MEMORY, 0),
    };

    // when
    runtime.block_on(async {
        let id = provider.clone_vm(&base(), &vm(1)).await.expect("clone");
        provider.start(&vm(1), &options).await.expect("start");
        let _listed = provider.inventory().await;
        provider
            .stop(&proof(1, &id), Millis::new(5_000))
            .await
            .expect("stop");
        provider.delete(&proof(1, &id)).await.expect("delete");
        provider.probe().await.expect("probe");
    });

    // then
    let calls: Vec<Value> = fake
        .events("invoke")
        .into_iter()
        .filter(|record| record["argv"][0] != "create")
        .collect();
    let commands: Vec<String> = calls.iter().map(|c| argv(c)[0].clone()).collect();
    for command in [
        "get",
        "clone",
        "set",
        "run",
        "list",
        "stop",
        "delete",
        "--version",
    ] {
        assert!(
            commands.iter().any(|c| c == command),
            "{command} in {commands:?}"
        );
    }
    for call in &calls {
        let env = call["env"].as_object().expect("env");
        assert!(env.contains_key("TART_NO_AUTO_PRUNE"), "{call}");
        assert_eq!(
            env["TART_HOME"].as_str(),
            Some(fake.home().to_str().expect("utf-8")),
            "{call}"
        );
    }
}

#[test]
fn should_clone_from_the_local_base_then_draw_a_random_mac_when_cloning() {
    // given
    let fake = Fake::new();
    assert!(fake.tart(&["create", base().as_str()]).status.success());
    let provider = TartProvider::new(fake.config());

    // when
    let id = runtime()
        .block_on(provider.clone_vm(&base(), &vm(1)))
        .expect("clone");

    // then
    let calls: Vec<Vec<String>> = fake
        .events("invoke")
        .iter()
        .map(argv)
        .filter(|args| args[0] != "create")
        .collect();
    let name = vm(1).to_string();
    assert_eq!(
        calls,
        vec![
            vec!["get", base().as_str(), "--format", "json"],
            vec!["clone", base().as_str(), name.as_str()],
            vec!["set", name.as_str(), "--random-mac"],
            vec!["get", name.as_str(), "--format", "json"],
        ]
        .into_iter()
        .map(|args| args.into_iter().map(str::to_owned).collect::<Vec<_>>())
        .collect::<Vec<_>>()
    );
    let info: Value =
        serde_json::from_slice(&fake.tart(&["get", &name, "--format", "json"]).stdout)
            .expect("get json");
    assert_eq!(info["MACAddress"].as_str(), Some(id.as_str()));
}

#[test]
fn should_refuse_to_clone_an_oci_image_when_it_is_not_a_local_base() {
    // given
    let fake = Fake::new();
    assert!(fake.tart(&["pull", base().as_str()]).status.success());
    let provider = TartProvider::new(fake.config());

    // when
    let cloned = runtime().block_on(provider.clone_vm(&base(), &vm(1)));

    // then
    assert_eq!(cloned, Err(mayfly_data::VmError::Definite));
    assert!(
        fake.events("invoke")
            .iter()
            .all(|r| r["argv"][0] != "clone")
    );
}

#[test]
fn should_report_incomplete_when_a_vm_description_does_not_parse() {
    // given
    let fake = Fake::new();
    assert!(fake.tart(&["create", "foreign-vm"]).status.success());
    fake.faults(
        "[[rule]]\ncommand = \"get\"\naction = \"corrupt_output\"\ncorrupt = \"truncate\"\n",
    );
    let provider = TartProvider::new(fake.config());

    // when
    let inventory = runtime().block_on(provider.inventory());

    // then
    assert_eq!(
        inventory,
        Inventory::Incomplete {
            reason: InventoryGap::Unparseable
        }
    );
}

#[test]
fn should_accept_the_pinned_version_and_refuse_another_when_probing() {
    // given
    let fake = Fake::new();
    let provider = TartProvider::new(fake.config());
    let runtime = runtime();
    let pinned = runtime.block_on(provider.probe());

    // when
    fake.faults("[[rule]]\ncommand = \"--version\"\nversion = \"9.9.9\"\n");
    let other = runtime.block_on(provider.probe());

    // then
    assert_eq!(pinned, Ok("2.28.0".to_owned()));
    assert_eq!(other, Err(ProbeError::Unsupported("9.9.9".to_owned())));
}
