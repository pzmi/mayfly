//! The configuration schema through the public API (plan §11): golden valid and invalid
//! examples under `tests/fixtures/config/`, one invalid example per refusal, and the rules
//! the module documents.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use mayfly_composition::config::{
    CapacityPolicy, Config, ConfigErrorKind, GithubScope, GuestOs, HostConfig, LoadError,
    ProviderKind, Tier, TimeOfDay, plan_policy,
};
use mayfly_control_domain::capacity;
use mayfly_domain::SecretRef;

fn fixtures(family: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/config")
        .join(family)
}

fn read(family: &str, name: &str) -> String {
    fs::read_to_string(fixtures(family).join(name)).unwrap()
}

fn names(family: &str, extension: &str) -> Vec<String> {
    let mut found: Vec<String> = fs::read_dir(fixtures(family))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.ends_with(extension))
        .collect();
    found.sort();
    found
}

fn minimal() -> String {
    read("valid", "minimal.toml")
}

fn errors_of(text: &str) -> Vec<(String, ConfigErrorKind)> {
    Config::parse(text)
        .unwrap_err()
        .0
        .into_iter()
        .map(|error| (error.field, error.kind))
        .collect()
}

#[test]
fn should_accept_the_input_when_a_fixture_is_valid() {
    // given
    let valid = names("valid", ".toml");
    // when
    let results: Vec<_> = valid
        .iter()
        .map(|name| (name, Config::parse(&read("valid", name))))
        .collect();
    // then
    assert!(valid.len() >= 3);
    for (name, result) in results {
        assert!(result.is_ok(), "{name}: {:?}", result.err());
    }
}

#[test]
fn should_refuse_the_input_when_a_fixture_is_invalid() {
    // given
    let invalid = names("invalid", ".toml");
    // when
    let reports: Vec<_> = invalid
        .iter()
        .map(|name| {
            let report = Config::parse(&read("invalid", name))
                .unwrap_err()
                .to_string();
            (name, report)
        })
        .collect();
    // then
    assert!(invalid.len() >= 15);
    for (name, report) in reports {
        let golden = read("invalid", &name.replace(".toml", ".errors.txt"));
        assert_eq!(report, golden.trim_end_matches('\n'), "{name}");
    }
}

#[test]
fn should_pair_every_invalid_fixture_with_its_golden_report_when_the_directory_is_listed() {
    // given
    let inputs = names("invalid", ".toml");
    let goldens = names("invalid", ".errors.txt");
    // when
    let expected: Vec<String> = inputs
        .iter()
        .map(|name| name.replace(".toml", ".errors.txt"))
        .collect();
    // then
    assert_eq!(goldens, expected);
}

#[test]
fn should_name_the_reason_when_a_refused_key_is_present() {
    // given
    let cases = [
        (
            "refused_allow_untrusted_prs.toml",
            "pools[0].allow_untrusted_prs",
            "ADR 0027",
        ),
        ("refused_classic_mode.toml", "github.mode", "ADR 0028"),
    ];
    for (fixture, field, adr) in cases {
        // when
        let errors = Config::parse(&read("invalid", fixture)).unwrap_err().0;
        // then
        assert_eq!(errors.len(), 1, "{fixture}");
        assert_eq!(errors[0].field, field);
        assert_eq!(errors[0].kind, ConfigErrorKind::Refused);
        assert!(errors[0].problem.contains(adr), "{}", errors[0]);
    }
}

#[test]
fn should_refuse_an_unknown_key_when_it_sits_at_any_level() {
    // given
    let cases = [
        ("", "extra = 1\n", "extra"),
        ("[github]", "extra = 1", "github.extra"),
        ("[provider]", "extra = 1", "provider.extra"),
        ("[[pools]]", "extra = 1", "pools[0].extra"),
    ];
    for (anchor, line, field) in cases {
        // when
        let text = if anchor.is_empty() {
            format!("{line}{}", minimal())
        } else {
            minimal().replacen(anchor, &format!("{anchor}\n{line}"), 1)
        };
        // then
        assert_eq!(
            errors_of(&text),
            [(field.to_owned(), ConfigErrorKind::Unknown)],
            "{field}"
        );
    }
}

#[test]
fn should_refuse_a_host_key_when_it_is_unknown() {
    // given
    let text = format!("{}\n[host]\nreserve_ram = 1\n", minimal());
    // when
    let errors = errors_of(&text);
    // then
    assert_eq!(
        errors,
        [("host.reserve_ram".to_owned(), ConfigErrorKind::Unknown)]
    );
}

#[test]
fn should_accept_every_key_when_the_full_fixture_is_parsed() {
    // given
    let text = read("valid", "full.toml");
    // when
    let config = Config::parse(&text).unwrap();
    // then
    assert_eq!(config.state_dir, Some(PathBuf::from("/var/lib/mayfly")));
    assert_eq!(
        config.provider.tart_home,
        Some(PathBuf::from("/var/lib/mayfly/tart"))
    );
    assert_eq!(
        config.github.scope,
        GithubScope::Repository {
            organisation: "example-org".to_owned(),
            repository: "example-repo".to_owned()
        }
    );
    assert_eq!(config.github.installation_id, 12_345_678);
    assert_eq!(config.github.private_key.scheme(), "keychain");
    assert_eq!(config.host.reserve_memory_mib, Some(8192));
    assert_eq!(config.host.reserve_disk_bytes(), 100 * 1024 * 1024 * 1024);
    assert_eq!(config.host.reserve_cpus, Some(3));
    assert_eq!(
        (config.host.vm_slots, config.host.macos_guests),
        (Some(4), Some(1))
    );
    assert_eq!(
        (config.host.image_disk_gib, config.host.clone_disk_gib),
        (Some(80), Some(90))
    );
    assert_eq!(config.provider.kind, ProviderKind::Tart);
    let images = config.images.as_ref().unwrap();
    assert_eq!(images.repository, "ghcr.io/example-org/runner-macos");
    assert_eq!(images.realm_hosts, ["auth.example.com"]);
    assert_eq!(images.runners.keys().collect::<Vec<_>>(), ["2.330.0"]);
    assert_eq!(config.pools.len(), 2);
    let mac = &config.pools[0];
    assert_eq!(mac.name.as_str(), "mac");
    assert_eq!((mac.tier, mac.guest_os), (Tier::Trusted, GuestOs::Macos));
    assert_eq!((mac.cpu, mac.memory_mib), (4, 8192));
    assert_eq!((mac.min_prepared, mac.max_running), (1, 2));
    assert_eq!(mac.max_prepared_age, Duration::from_secs(7200));
    assert_eq!(mac.breaker.failures, 5);
    assert_eq!(mac.breaker.window, Duration::from_secs(3600));
    assert_eq!(
        mac.runner_refresh_at,
        Some(TimeOfDay {
            hour: 3,
            minute: 30
        })
    );
    assert_eq!(mac.capacity_policy, CapacityPolicy::Demand);
    assert_eq!(config.pools[1].guest_os, GuestOs::Linux);
    assert_eq!(config.shutdown.busy_wait, Duration::from_secs(45 * 60));
}

#[test]
fn should_read_runner_pins_per_platform_when_a_version_names_them() {
    // given: one version pinned per platform, one with the first form (every platform)
    let (a, b, c) = ("a".repeat(64), "b".repeat(64), "c".repeat(64));
    let text = format!(
        "{}\n[images]\nrepository = \"ghcr.io/example-org/runner\"\n[images.runners]\n\
         \"2.330.0\" = {{ linux-arm64 = \"{a}\", osx-arm64 = \"{b}\" }}\n\"2.331.0\" = \"{c}\"\n",
        minimal()
    );
    let unknown = format!(
        "{}\n[images]\nrepository = \"ghcr.io/example-org/runner\"\n[images.runners]\n\
         \"2.330.0\" = {{ windows-x64 = \"{a}\" }}\n",
        minimal()
    );

    // when
    let config = Config::parse(&text).unwrap();
    let refused = errors_of(&unknown);

    // then
    let runners = &config.images.as_ref().unwrap().runners;
    assert_eq!(runners["2.330.0"]["linux-arm64"], a);
    assert_eq!(runners["2.330.0"]["osx-arm64"], b);
    assert_eq!(runners["2.331.0"]["linux-arm64"], c);
    assert_eq!(runners["2.331.0"]["osx-arm64"], c);
    assert!(
        refused.iter().any(|(field, _)| field.contains("runners")),
        "{refused:?}"
    );
}

#[test]
fn should_map_the_capacity_policy_when_a_pool_is_planned() {
    // given
    let text = read("valid", "full.toml");
    let config = Config::parse(&text).unwrap();
    let demand = config
        .pools
        .iter()
        .find(|p| p.capacity_policy == CapacityPolicy::Demand)
        .unwrap();

    // when
    let planned = plan_policy(demand.capacity_policy, 3);
    let exact = plan_policy(CapacityPolicy::Exact, 3);

    // then
    assert_eq!(planned, capacity::CapacityPolicy::Demand { waiting: 3 });
    assert_eq!(exact, capacity::CapacityPolicy::Exact);
}

#[test]
fn should_apply_the_defaults_when_the_optional_keys_are_absent() {
    // given
    let text = minimal();
    // when
    let config = Config::parse(&text).unwrap();
    // then
    let pool = &config.pools[0];
    assert_eq!(pool.min_prepared, 0);
    assert_eq!(pool.max_prepared_age, Duration::from_secs(3600));
    assert_eq!(
        (pool.breaker.failures, pool.breaker.window),
        (3, Duration::from_secs(1800))
    );
    assert_eq!(pool.runner_refresh_at, None);
    assert_eq!(pool.capacity_policy, CapacityPolicy::Exact);
    assert_eq!(config.host.reserve_memory_mib, None, "measured when absent");
    assert_eq!(config.host.reserve_disk_gib, 50);
    assert_eq!(
        (config.host.reserve_cpus, config.host.reserve_cpus()),
        (None, 2)
    );
    assert_eq!(config.host.macos_guests(), 2, "Apple's limit when absent");
    assert_eq!(
        (config.host.vm_slots, config.host.macos_guests),
        (None, None)
    );
    assert_eq!(
        (config.host.image_disk_gib, config.host.clone_disk_gib),
        (None, None)
    );
    assert_eq!(config.images, None);
    assert_eq!(config.provider.kind, ProviderKind::Tart);
    assert_eq!(
        config.shutdown.busy_wait,
        Duration::from_secs(30 * 60),
        "terminal mode waits 30 minutes for busy jobs when absent (owner, 2026-11-12)"
    );
    assert!(matches!(
        config.github.scope,
        GithubScope::Organisation { .. }
    ));
}

#[test]
fn should_parse_each_secret_scheme_when_the_private_key_names_one() {
    // given
    let cases = [
        ("file:/etc/mayfly/app-key.pem", "file"),
        ("keychain:mayfly/app-key", "keychain"),
        ("env:MAYFLY_APP_KEY", "env"),
        ("exec:/usr/local/bin/print-app-key", "exec"),
    ];
    for (reference, scheme) in cases {
        // when
        let text = minimal().replace("file:/etc/mayfly/app-key.pem", reference);
        let config = Config::parse(&text).unwrap();
        // then
        assert_eq!(config.github.private_key.scheme(), scheme);
        assert_eq!(
            config.github.private_key,
            reference.parse::<SecretRef>().unwrap()
        );
    }
}

#[test]
fn should_not_echo_a_pasted_secret_when_the_reference_is_refused() {
    // given
    let pasted = [
        "SYNTHETIC-NOT-A-KEY",
        "file:SYNTHETIC-NOT-A-KEY",
        "-----BEGIN SYNTHETIC-----",
    ];
    for value in pasted {
        let text = minimal().replace("file:/etc/mayfly/app-key.pem", value);
        // when
        let report = Config::parse(&text).unwrap_err().to_string();
        // then
        assert!(report.contains("github.private_key"), "{report}");
        assert!(!report.contains("SYNTHETIC"), "{report}");
    }
}

#[test]
fn should_not_echo_the_source_line_when_the_syntax_is_wrong() {
    // given
    let text = "schema_version = 1\nprivate_key = \"SYNTHETIC-NOT-A-KEY\" trailing\n";
    // when
    let report = Config::parse(text).unwrap_err().to_string();
    // then
    assert!(report.contains("line 2, column"), "{report}");
    assert!(!report.contains("SYNTHETIC"), "{report}");
}

#[test]
fn should_refuse_the_pools_when_the_array_has_the_wrong_shape() {
    // given
    let pools_string = format!(
        "pools = \"x\"\n{}",
        minimal().split("[[pools]]").next().unwrap()
    );
    let pools_empty = format!(
        "pools = []\n{}",
        minimal().split("[[pools]]").next().unwrap()
    );
    let pools_numbers = format!(
        "pools = [1]\n{}",
        minimal().split("[[pools]]").next().unwrap()
    );
    // when
    let got = [
        errors_of(&pools_string),
        errors_of(&pools_empty),
        errors_of(&pools_numbers),
    ];
    // then
    assert_eq!(got[0], [("pools".to_owned(), ConfigErrorKind::WrongType)]);
    assert_eq!(got[1], [("pools".to_owned(), ConfigErrorKind::Invalid)]);
    assert_eq!(
        got[2],
        [("pools[0]".to_owned(), ConfigErrorKind::WrongType)]
    );
}

#[test]
fn should_name_the_section_when_a_table_is_a_scalar() {
    // given
    let text = "schema_version = 1\nstate_dir = \"/var/lib/mayfly\"\ngithub = 1\nhost = \"x\"\nprovider = []\n";
    // when
    let errors = errors_of(text);
    // then
    assert_eq!(
        errors,
        [
            ("github".to_owned(), ConfigErrorKind::WrongType),
            ("host".to_owned(), ConfigErrorKind::WrongType),
            ("provider".to_owned(), ConfigErrorKind::WrongType),
            ("pools".to_owned(), ConfigErrorKind::Missing),
        ]
    );
}

#[test]
fn should_refuse_a_schema_version_that_is_not_an_integer_when_it_is_a_string() {
    // given
    let text = minimal().replace("schema_version = 1", "schema_version = \"1\"");
    // when
    let errors = Config::parse(&text).unwrap_err().0;
    // then
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].kind, ConfigErrorKind::SchemaVersion);
    assert!(
        errors[0].problem.contains("expected an integer"),
        "{}",
        errors[0]
    );
}

#[test]
fn should_refuse_each_bad_value_when_it_is_outside_its_range_or_shape() {
    // given
    let cases = [
        ("name = \"mac\"", "name = \"\"", "pools[0].name"),
        ("cpu = 4", "cpu = 257", "pools[0].cpu"),
        (
            "memory_mib = 8192",
            "memory_mib = 512",
            "pools[0].memory_mib",
        ),
        ("max_running = 2", "max_running = 0", "pools[0].max_running"),
        (
            "max_running = 2",
            "max_running = -1",
            "pools[0].max_running",
        ),
        (
            "max_running = 2",
            "max_running = 2\nbreaker_failures = 0",
            "pools[0].breaker_failures",
        ),
        (
            "max_running = 2",
            "max_running = 2\nmax_prepared_age = \"8d\"",
            "pools[0].max_prepared_age",
        ),
        (
            "max_running = 2",
            "max_running = 2\nmax_prepared_age = \"1.5h\"",
            "pools[0].max_prepared_age",
        ),
        (
            "max_running = 2",
            "max_running = 2\nmax_prepared_age = \"\"",
            "pools[0].max_prepared_age",
        ),
        (
            "max_running = 2",
            "max_running = 2\nmax_prepared_age = \"5x\"",
            "pools[0].max_prepared_age",
        ),
        (
            "max_running = 2",
            "max_running = 2\nmax_prepared_age = \"99999999999999999999h\"",
            "pools[0].max_prepared_age",
        ),
        (
            "max_running = 2",
            "max_running = 2\nrunner_refresh_at = \"3:30\"",
            "pools[0].runner_refresh_at",
        ),
        (
            "max_running = 2",
            "max_running = 2\nrunner_refresh_at = \"03:60\"",
            "pools[0].runner_refresh_at",
        ),
        (
            "max_running = 2",
            "max_running = 2\nrunner_refresh_at = \"0a:30\"",
            "pools[0].runner_refresh_at",
        ),
        (
            "max_running = 2",
            "max_running = 2\nrunner_refresh_at = \"0330\"",
            "pools[0].runner_refresh_at",
        ),
        (
            "organisation = \"example-org\"",
            "organisation = \"example org\"",
            "github.organisation",
        ),
        (
            "organisation = \"example-org\"",
            "organisation = \"example-\"",
            "github.organisation",
        ),
        (
            "organisation = \"example-org\"",
            "organisation = \"\"",
            "github.organisation",
        ),
        ("Iv1.0123456789abcdef", "bad id", "github.app_client_id"),
        (
            "installation_id = 12345678",
            "installation_id = -5",
            "github.installation_id",
        ),
        (
            "tart_home = \"/var/lib/mayfly/tart\"",
            "tart_home = \"/var/lib/../tart\"",
            "provider.tart_home",
        ),
        (
            "tart_home = \"/var/lib/mayfly/tart\"",
            "tart_home = \"\"",
            "provider.tart_home",
        ),
        (
            "tart_home = \"/var/lib/mayfly/tart\"",
            "tart_home = \"~/tart\"",
            "provider.tart_home",
        ),
    ];
    for (from, to, field) in cases {
        // when
        let text = minimal().replacen(from, to, 1);
        let errors = errors_of(&text);
        // then
        assert_eq!(errors.len(), 1, "{to}: {errors:?}");
        assert_eq!(errors[0].0, field, "{to}");
        assert_eq!(errors[0].1, ConfigErrorKind::Invalid, "{to}");
    }
}

#[test]
fn should_read_the_busy_wait_when_shutdown_names_one_within_its_bounds() {
    // given: `[shutdown]` (RT-05a) at both bounds, outside them, not a duration, and a key it
    // does not know
    let with = |table: &str| format!("{}\n[shutdown]\n{table}\n", minimal());
    let (low, high) = (with("busy_wait = \"1s\""), with("busy_wait = \"7d\""));
    let refused = [
        with("busy_wait = \"0s\""),
        with("busy_wait = \"8d\""),
        with("busy_wait = \"soon\""),
        with("busy_wait = 30"),
    ];
    let unknown = with("grace = \"1m\"");

    // when
    let (low, high) = (Config::parse(&low).unwrap(), Config::parse(&high).unwrap());
    let refusals: Vec<_> = refused.iter().map(|text| errors_of(text)).collect();
    let unknown = errors_of(&unknown);

    // then
    assert_eq!(low.shutdown.busy_wait, Duration::from_secs(1));
    assert_eq!(high.shutdown.busy_wait, Duration::from_secs(7 * 86_400));
    for errors in refusals {
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].0, "shutdown.busy_wait", "{errors:?}");
    }
    assert_eq!(
        unknown,
        [("shutdown.grace".to_owned(), ConfigErrorKind::Unknown)]
    );
}

#[test]
fn should_refuse_a_repository_name_when_it_is_a_dot_or_too_long() {
    // given
    let long = "r".repeat(101);
    for name in [".", "..", long.as_str(), ""] {
        // when
        let text = minimal().replacen(
            "organisation = \"example-org\"",
            &format!("organisation = \"example-org\"\nrepository = \"{name}\""),
            1,
        );
        // then
        assert_eq!(
            errors_of(&text),
            [("github.repository".to_owned(), ConfigErrorKind::Invalid)],
            "{name:?}"
        );
    }
}

#[test]
fn should_accept_the_bounds_when_values_sit_exactly_on_them() {
    // given
    let text = minimal().replacen(
        "max_running = 2",
        "max_running = 2\nmin_prepared = 2\nmax_prepared_age = \"1m\"\nbreaker_window = \"1d\"\nrunner_refresh_at = \"23:59\"",
        1,
    );
    // when
    let config = Config::parse(&text).unwrap();
    // then
    let pool = &config.pools[0];
    assert_eq!(pool.min_prepared, 2);
    assert_eq!(pool.max_prepared_age, Duration::from_secs(60));
    assert_eq!(pool.breaker.window, Duration::from_secs(86_400));
    assert_eq!(
        pool.runner_refresh_at,
        Some(TimeOfDay {
            hour: 23,
            minute: 59
        })
    );
}

#[test]
fn should_refuse_an_unknown_provider_when_the_kind_is_not_tart() {
    // given
    let text = minimal().replacen("[provider]", "[provider]\nkind = \"qemu\"", 1);
    // when
    let errors = Config::parse(&text).unwrap_err().0;
    // then
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].field, "provider.kind");
    assert!(errors[0].problem.contains("\"qemu\""), "{}", errors[0]);
}

#[test]
fn should_cut_a_long_enum_value_when_it_is_echoed() {
    // given
    let long = "x".repeat(200);
    let text = minimal().replacen("\"trusted\"", &format!("\"{long}\""), 1);
    // when
    let report = Config::parse(&text).unwrap_err().to_string();
    // then
    assert!(report.contains("pools[0].tier"), "{report}");
    assert!(!report.contains(&long), "{report}");
}

#[test]
fn should_report_every_problem_when_several_exist() {
    // given
    let text = read("invalid", "many_errors.toml");
    // when
    let errors = Config::parse(&text).unwrap_err();
    // then
    assert!(errors.0.len() >= 10);
    assert_eq!(
        errors.exit_code(),
        mayfly_composition::exit_code::CONFIG_INVALID
    );
}

#[test]
fn should_say_error_in_the_singular_when_one_problem_exists() {
    // given
    let text = minimal().replace("tier = \"trusted\"\n", "");
    // when
    let report = Config::parse(&text).unwrap_err().to_string();
    // then
    assert!(
        report.starts_with("invalid configuration: 1 error\n"),
        "{report}"
    );
}

#[test]
fn should_load_a_file_when_it_is_valid_and_refuse_it_when_it_is_not() {
    // given
    let dir = tempfile::tempdir().unwrap();
    let good = dir.path().join("good.toml");
    let bad = dir.path().join("bad.toml");
    let missing = dir.path().join("missing.toml");
    fs::write(&good, minimal()).unwrap();
    fs::write(&bad, "schema_version = 7\n").unwrap();
    // when
    let loaded = Config::load(&good);
    let invalid = Config::load(&bad).unwrap_err();
    let unreadable = Config::load(&missing).unwrap_err();
    // then
    assert!(loaded.is_ok());
    assert!(matches!(invalid, LoadError::Invalid(_)));
    assert!(invalid.to_string().contains("schema_version"), "{invalid}");
    assert!(matches!(unreadable, LoadError::Read { .. }));
    assert!(
        unreadable.to_string().contains("missing.toml"),
        "{unreadable}"
    );
    assert_eq!(
        unreadable.exit_code(),
        mayfly_composition::exit_code::CONFIG_INVALID
    );
    assert_eq!(
        invalid.exit_code(),
        mayfly_composition::exit_code::CONFIG_INVALID
    );
}

#[test]
fn should_leave_the_places_to_the_platform_when_the_file_names_none() {
    // given
    let text = read("valid", "defaults.toml");
    // when
    let config = Config::parse(&text).unwrap();
    // then
    assert_eq!(config.state_dir, None);
    assert_eq!(config.provider.tart_home, None);
    assert_eq!(config.provider.kind, ProviderKind::Tart);
}

#[test]
fn should_cap_macos_guests_at_apples_limit_when_more_are_configured() {
    // given
    let host = |n| HostConfig {
        macos_guests: Some(n),
        ..HostConfig::default()
    };

    // when
    let effective = [0, 1, 2, 3, 64].map(|n| host(n).macos_guests());

    // then
    assert_eq!(effective, [0, 1, 2, 2, 2]);
}
