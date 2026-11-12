//! What the adapter promises about every `tart` call, read from the fake's invocation log: the
//! environment (`TART_HOME` and `TART_NO_AUTO_PRUNE` on every call, plan §8.2), argv shapes
//! (clone never `--overwrite`, clone only from a local base then `set --random-mac`, with
//! `--random-serial` for a macOS base), and the version probe.

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
    fake.seed_base(base().as_str());
    let seeding = fake.events("invoke").len();
    let provider = fake.provider();
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
    let calls: Vec<Value> = fake.events("invoke").into_iter().skip(seeding).collect();
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
    for linux in [false, true] {
        // given
        let fake = Fake::new();
        if linux {
            fake.seed_linux_base(base().as_str());
        } else {
            fake.seed_base(base().as_str());
        }
        let seeding = fake.events("invoke").len();
        let provider = fake.provider();

        // when
        let id = runtime()
            .block_on(provider.clone_vm(&base(), &vm(1)))
            .expect("clone");

        // then: a macOS clone also gets its own serial; a Linux one has none
        let calls: Vec<Vec<String>> = fake
            .events("invoke")
            .iter()
            .skip(seeding)
            .map(argv)
            .collect();
        let name = vm(1).to_string();
        let set = if linux {
            vec!["set", name.as_str(), "--random-mac"]
        } else {
            vec!["set", name.as_str(), "--random-mac", "--random-serial"]
        };
        assert_eq!(
            calls,
            vec![
                vec!["get", base().as_str(), "--format", "json"],
                vec!["clone", base().as_str(), name.as_str()],
                set,
                vec!["get", name.as_str(), "--format", "json"],
            ]
            .into_iter()
            .map(|args| args.into_iter().map(str::to_owned).collect::<Vec<_>>())
            .collect::<Vec<_>>(),
            "linux: {linux}"
        );
        let info: Value =
            serde_json::from_slice(&fake.tart(&["get", &name, "--format", "json"]).stdout)
                .expect("get json");
        assert_eq!(info["MACAddress"].as_str(), Some(id.as_str()));
    }
}

#[test]
fn should_refuse_to_clone_an_oci_image_when_it_is_not_a_local_base() {
    // given
    let fake = Fake::new();
    assert!(fake.tart(&["pull", base().as_str()]).status.success());
    let provider = fake.provider();

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
    let provider = fake.provider();

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
    let provider = fake.provider();
    let runtime = runtime();
    let pinned = runtime.block_on(provider.probe());

    // when
    fake.faults("[[rule]]\ncommand = \"--version\"\nversion = \"9.9.9\"\n");
    let other = runtime.block_on(provider.probe());
    fake.faults("[[rule]]\ncommand = \"--version\"\nversion = \"2.28.0\"\n");
    let older = runtime.block_on(provider.probe());

    // then
    assert_eq!(pinned, Ok("2.41.1".to_owned()));
    assert_eq!(other, Err(ProbeError::Unsupported("9.9.9".to_owned())));
    assert_eq!(older, Err(ProbeError::Unsupported("2.28.0".to_owned())));
}

#[test]
fn should_count_the_disk_recorded_at_clone_time_when_tart_prints_none() {
    // given: two running clones of a base whose ASIF disk Tart cannot read while they run;
    // the second's MAC was changed behind the adapter's back after it was cloned
    let fake = Fake::new();
    fake.seed_asif_base(base().as_str());
    let provider = fake.provider();
    let options = mayfly_data::RunOptions {
        resources: mayfly_domain::Resources::new(START_MEMORY, 0),
    };
    let (first, second) = (vm(1).to_string(), vm(2).to_string());
    runtime().block_on(async {
        provider.clone_vm(&base(), &vm(1)).await.expect("clone");
        provider.clone_vm(&base(), &vm(2)).await.expect("clone");
    });
    assert!(
        fake.tart(&["set", &second, "--random-mac"])
            .status
            .success()
    );
    runtime().block_on(async {
        provider.start(&vm(1), &options).await.expect("start");
        provider.start(&vm(2), &options).await.expect("start");
    });
    let printed: Value =
        serde_json::from_slice(&fake.tart(&["get", &first, "--format", "json"]).stdout)
            .expect("get json");

    // when
    let inventory = runtime().block_on(provider.inventory());

    // then: the recorded ceiling for the incarnation it was read for, unknown for another
    assert_eq!(printed["Disk"], Value::Null);
    let Inventory::Complete { vms } = inventory else {
        panic!("a complete inventory: {inventory:?}");
    };
    let disk = |name: &str| vms.iter().find(|v| v.name == name).map(|v| v.disk_bytes);
    assert_eq!(disk(&first), Some(Some(50 * (1 << 30))));
    assert_eq!(disk(&second), Some(None));
    for name in [&first, &second] {
        assert!(fake.tart(&["stop", name]).status.success(), "stop {name}");
    }
}

fn started(fake: &Fake) -> (TartProvider, mayfly_domain::ProviderInstanceId) {
    fake.seed_base(base().as_str());
    let provider = fake.provider();
    let options = mayfly_data::RunOptions {
        resources: mayfly_domain::Resources::new(START_MEMORY, 0),
    };
    let id = runtime().block_on(async {
        let id = provider.clone_vm(&base(), &vm(1)).await.expect("clone");
        provider.start(&vm(1), &options).await.expect("start");
        id
    });
    (provider, id)
}

#[test]
fn should_answer_done_when_tart_stop_fails_after_the_vm_stopped() {
    // given
    let fake = Fake::new();
    let (provider, id) = started(&fake);
    fake.faults("[[rule]]\ncommand = \"stop\"\naction = \"fail_after_effect\"\nexit_code = 2\n");

    // when
    let stopped = runtime().block_on(provider.stop(&proof(1, &id), Millis::new(5_000)));

    // then
    assert_eq!(stopped, Ok(()));
    assert_eq!(fake.exists(&vm(1).to_string()), Some(false));
}

#[test]
fn should_refuse_definite_when_tart_stop_fails_and_the_vm_still_runs() {
    // given
    let fake = Fake::new();
    let (provider, id) = started(&fake);
    fake.faults("[[rule]]\ncommand = \"stop\"\naction = \"fail_before_effect\"\n");

    // when
    let stopped = runtime().block_on(provider.stop(&proof(1, &id), Millis::new(5_000)));

    // then
    assert_eq!(stopped, Err(mayfly_data::VmError::Definite));
    assert_eq!(fake.exists(&vm(1).to_string()), Some(true));
}

#[test]
fn should_answer_done_when_tart_delete_fails_after_removing_the_vm() {
    // given
    let fake = Fake::new();
    fake.seed_base(base().as_str());
    let provider = fake.provider();
    let runtime = runtime();
    let id = runtime
        .block_on(provider.clone_vm(&base(), &vm(1)))
        .expect("clone");
    fake.faults("[[rule]]\ncommand = \"delete\"\naction = \"fail_after_effect\"\n");

    // when
    let deleted = runtime.block_on(provider.delete(&proof(1, &id)));

    // then
    assert_eq!(deleted, Ok(()));
    assert_eq!(fake.exists(&vm(1).to_string()), None);
}

#[test]
fn should_refuse_definite_when_tart_delete_fails_and_the_vm_remains() {
    // given
    let fake = Fake::new();
    fake.seed_base(base().as_str());
    let provider = fake.provider();
    let runtime = runtime();
    let id = runtime
        .block_on(provider.clone_vm(&base(), &vm(1)))
        .expect("clone");
    fake.faults("[[rule]]\ncommand = \"delete\"\naction = \"fail_before_effect\"\n");

    // when
    let deleted = runtime.block_on(provider.delete(&proof(1, &id)));

    // then
    assert_eq!(deleted, Err(mayfly_data::VmError::Definite));
    assert_eq!(fake.exists(&vm(1).to_string()), Some(false));
}
