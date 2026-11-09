//! Kernel behaviour through the public API: names, ids, fences, digests, time, serde validation.

use mayfly_domain::{
    AttemptId, CommandId, Deadline, DirectiveId, Epoch, EventId, Fence, FenceError, Generation,
    HostId, ImageDigest, Millis, ObservationId, OwnedVmName, PoolName, ProviderInstanceId,
    Resources, Sample, SecretHandleId, Slot, VmNameClass, VmNameError, classify_vm_name,
};
use proptest::prelude::*;
use serde::Deserialize;
use serde::de::IntoDeserializer;
use serde::de::value::{Error as DeError, StrDeserializer, U32Deserializer};

const GOOD: &str = "mf-pool1-host1-s0-g1-0123abcd";

fn name_strategy() -> impl Strategy<Value = String> {
    "[a-z0-9]{1,16}"
}

proptest! {
    #[test]
    fn should_round_trip_when_any_owned_name_is_formatted_and_parsed(
        pool in name_strategy(),
        host in name_strategy(),
        slot in any::<u32>(),
        generation in 1..=u32::MAX,
        attempt in any::<u128>(),
    ) {
        // given
        let name = OwnedVmName::new(
            PoolName::new(&pool).unwrap(),
            HostId::new(&host).unwrap(),
            Slot::new(slot),
            Generation::new(generation).unwrap(),
            AttemptId::from_u128(attempt),
        );
        // when
        let text = name.to_string();
        let parsed = OwnedVmName::parse(&text).unwrap();
        // then
        prop_assert_eq!(&parsed, &name);
        prop_assert_eq!(parsed.to_string(), text);
        prop_assert_eq!(parsed.generation(), Generation::new(generation).unwrap());
        prop_assert_eq!(parsed.slot(), Slot::new(slot));
    }

    #[test]
    fn should_not_parse_foreign_names_as_owned_when_they_are_arbitrary_or_near_miss_text(
        prefix in prop_oneof![
            Just(String::new()),
            Just("mf".to_owned()),
            Just("mf_".to_owned()),
            Just("mf.".to_owned()),
            Just("MF-".to_owned()),
            Just("Mf-".to_owned()),
            Just("m".to_owned()),
            Just(" mf-".to_owned()),
            Just("xmf-".to_owned()),
            Just("\u{ff4d}f-".to_owned()),
        ],
        rest in ".{0,60}",
    ) {
        // given a string that does not start with the managed prefix, often only just
        let text = format!("{prefix}{rest}");
        prop_assume!(!text.starts_with("mf-"));
        // when
        // then
        prop_assert_eq!(OwnedVmName::parse(&text), Err(VmNameError::NotManaged));
        prop_assert_eq!(classify_vm_name(&text), VmNameClass::Foreign);
    }

    #[test]
    fn should_only_accept_the_canonical_spelling_when_a_canonical_name_is_mutated(
        pool in name_strategy(),
        host in name_strategy(),
        slot in any::<u32>(),
        generation in 1..=u32::MAX,
        attempt in any::<u128>(),
        mutation in 0..14_usize,
        at in any::<usize>(),
    ) {
        // given a canonical name built from components
        let canonical = OwnedVmName::new(
            PoolName::new(&pool).unwrap(),
            HostId::new(&host).unwrap(),
            Slot::new(slot),
            Generation::new(generation).unwrap(),
            AttemptId::from_u128(attempt),
        )
        .to_string();
        let slot_part = format!("-s{slot}-");
        let generation_part = format!("-g{generation}-");
        let short = AttemptId::from_u128(attempt).short();
        // when one spelling rule is broken
        let mutant = match mutation {
            0 => canonical.replace(&slot_part, &format!("-s0{slot}-")),
            1 => canonical.replace(&generation_part, &format!("-g0{generation}-")),
            2 => canonical.to_uppercase(),
            3 => canonical.replace(&short, &short.to_uppercase()),
            4 => format!(" {canonical}"),
            5 => format!("{canonical} "),
            6 => format!("{canonical}-"),
            7 => {
                let mut chars: Vec<char> = canonical.chars().collect();
                chars.insert(3 + at % (chars.len() - 3), '-');
                chars.into_iter().collect()
            }
            8 => {
                let dashes: Vec<usize> = canonical.match_indices('-').map(|(i, _)| i).collect();
                let mut text = canonical.clone();
                text.remove(dashes[at % dashes.len()]);
                text
            }
            9 => format!("{canonical}0"),
            10 => canonical[..canonical.len() - 1].to_owned(),
            11 => canonical.replace(&slot_part, "-s4294967296-"),
            12 => canonical.replace(&generation_part, "-g99999999999-"),
            _ => canonical.replace(&slot_part, "-s\u{663}-"),
        };
        // then it never parses to a name that prints differently: it fails, or reprints exactly
        match OwnedVmName::parse(&mutant) {
            Err(_) => {}
            Ok(name) => prop_assert_eq!(name.to_string(), mutant),
        }
    }

    #[test]
    fn should_only_accept_names_that_reformat_identically_when_parsing_arbitrary_text(
        text in "mf-[a-z0-9-]{0,40}",
    ) {
        // given arbitrary text under the managed prefix
        // when
        let parsed = OwnedVmName::parse(&text);
        // then a parse succeeds only for the canonical spelling
        if let Ok(name) = parsed {
            prop_assert_eq!(name.to_string(), text);
        }
    }

    #[test]
    fn should_round_trip_when_a_hex_id_is_displayed_and_parsed(value in any::<u128>()) {
        // given
        let id = DirectiveIdAlias::from_u128(value);
        // when
        let parsed: DirectiveIdAlias = id.to_string().parse().unwrap();
        // then
        prop_assert_eq!(parsed, id);
        prop_assert_eq!(id.to_string().len(), 32);
    }

    #[test]
    fn should_never_wrap_when_deadline_is_computed(now in any::<u64>(), timeout in any::<u64>()) {
        // given
        // when
        let deadline = Deadline::after(Millis::new(now), Millis::new(timeout));
        // then
        prop_assert_eq!(deadline.instant().get(), now.saturating_add(timeout));
    }
}

type DirectiveIdAlias = mayfly_domain::DirectiveId;

#[test]
fn should_refuse_vm_name_when_it_is_malformed() {
    // given
    let table: &[(&str, VmNameError)] = &[
        ("", VmNameError::NotManaged),
        ("mf", VmNameError::NotManaged),
        ("mf-", VmNameError::WrongShape),
        ("mf-pool-host-s0-g1", VmNameError::WrongShape),
        ("mf-pool-host-s0-g1-0123abcd-extra", VmNameError::WrongShape),
        ("mf--host-s0-g1-0123abcd", VmNameError::Pool),
        ("mf-POOL-host-s0-g1-0123abcd", VmNameError::Pool),
        ("mf-pool_x-host-s0-g1-0123abcd", VmNameError::Pool),
        (
            "mf-aaaaaaaaaaaaaaaaa-host-s0-g1-0123abcd",
            VmNameError::Pool,
        ),
        ("mf-pool--s0-g1-0123abcd", VmNameError::Host),
        ("mf-pool-Host-s0-g1-0123abcd", VmNameError::Host),
        ("mf-pool-host-0-g1-0123abcd", VmNameError::Slot),
        ("mf-pool-host-s-g1-0123abcd", VmNameError::Slot),
        ("mf-pool-host-s01-g1-0123abcd", VmNameError::Slot),
        ("mf-pool-host-s+1-g1-0123abcd", VmNameError::Slot),
        ("mf-pool-host-s-1-g1-0123abcd", VmNameError::WrongShape),
        ("mf-pool-host-s4294967296-g1-0123abcd", VmNameError::Slot),
        ("mf-pool-host-s0-1-0123abcd", VmNameError::Generation),
        ("mf-pool-host-s0-g-0123abcd", VmNameError::Generation),
        ("mf-pool-host-s0-g01-0123abcd", VmNameError::Generation),
        ("mf-pool-host-S0-g1-0123abcd", VmNameError::Slot),
        ("mf-pool-host-s0-G1-0123abcd", VmNameError::Generation),
        ("mf-pool-host-g1-s0-0123abcd", VmNameError::Slot),
        ("mf-pool-host-s\u{663}-g1-0123abcd", VmNameError::Slot),
        ("mf-pool-host-s0-g\u{663}-0123abcd", VmNameError::Generation),
        (
            "mf-pool-host-s0-g4294967296-0123abcd",
            VmNameError::Generation,
        ),
        ("mf-pool-host-s0-g0-0123abcd", VmNameError::Generation),
        ("mf-pool-host-s0-g1-", VmNameError::AttemptShort),
        ("mf-pool-host-s0-g1-0123ABCD", VmNameError::AttemptShort),
        ("mf-pool-host-s0-g1-0123abc", VmNameError::AttemptShort),
        ("mf-pool-host-s0-g1-0123abcde", VmNameError::AttemptShort),
        ("mf-pool-host-s0-g1-0123abcg", VmNameError::AttemptShort),
        (" mf-pool1-host1-s0-g1-0123abcd", VmNameError::NotManaged),
        ("mf-pool1-host1-s0-g1-0123abcd ", VmNameError::AttemptShort),
        ("mf-pool1-host1-s0-g1-0123abcd\n", VmNameError::AttemptShort),
    ];
    for (text, expected) in table {
        // when
        let result = OwnedVmName::parse(text);
        // then
        assert_eq!(result, Err(expected.clone()), "{text:?}");
    }
    assert!(OwnedVmName::parse(GOOD).is_ok());
}

#[test]
fn should_not_parse_foreign_names_as_owned_when_a_provider_reports_them() {
    // given names a provider might report that are not ours, or look like ours
    let foreign = [
        "ubuntu-base",
        "macos-15-xcode",
        "MF-pool-host-s0-g1-0123abcd",
        "xmf-a-b-s0-g1-0123abcd",
    ];
    for text in foreign {
        // when
        // then
        assert!(OwnedVmName::parse(text).is_err(), "{text}");
        assert_eq!(classify_vm_name(text), VmNameClass::Foreign, "{text}");
    }
}

#[test]
fn should_classify_shape_when_inventory_lists_names() {
    // given
    // when
    // then
    assert!(matches!(classify_vm_name(GOOD), VmNameClass::WellFormed(_)));
    assert_eq!(
        classify_vm_name("mf-legacy-vm"),
        VmNameClass::MalformedManaged
    );
    assert_eq!(classify_vm_name("mf-"), VmNameClass::MalformedManaged);
    assert_eq!(classify_vm_name("base-image"), VmNameClass::Foreign);
}

#[test]
fn should_take_the_short_part_from_the_attempt_id_when_a_name_is_built() {
    // given
    let attempt = AttemptId::from_u128(0xffff_u128 << 96 | 0x0123_abcd);
    // when
    let name = OwnedVmName::new(
        PoolName::new("p").unwrap(),
        HostId::new("h").unwrap(),
        Slot::new(2),
        Generation::new(7).unwrap(),
        attempt,
    );
    // then
    assert_eq!(name.to_string(), "mf-p-h-s2-g7-0123abcd");
    assert_eq!(name.attempt_short(), attempt.short());
}

#[test]
fn should_differ_in_short_when_v7_shaped_ids_are_a_minute_apart() {
    // given UUIDv7-shaped ids: a 48-bit millisecond timestamp on top, random bits below
    let v7 = |millis: u128, random: u128| (millis << 80) | (0x7_u128 << 76) | random;
    let first = AttemptId::from_u128(v7(
        1_800_000_000_000,
        0x0123_4567_89ab_cdef_0123_4567_89ab_cdef_u128 & ((1 << 76) - 1),
    ));
    let later = AttemptId::from_u128(v7(
        1_800_000_060_000,
        0x0fed_cba9_8765_4321_0fed_cba9_8765_4321_u128 & ((1 << 76) - 1),
    ));
    // when
    let (a, b) = (first.short(), later.short());
    // then the short follows the random low bits, not the timestamp
    assert_ne!(a, b);
    assert_eq!(a, "89abcdef");
    assert_eq!(b, "87654321");
}

#[test]
fn should_check_consistency_when_name_is_compared_with_an_attempt() {
    // given
    let attempt = AttemptId::from_u128(0xdead_beef);
    let name = OwnedVmName::new(
        PoolName::new("p").unwrap(),
        HostId::new("h").unwrap(),
        Slot::new(0),
        Generation::new(3).unwrap(),
        attempt,
    );
    // when
    // then
    assert!(name.is_for(&attempt, Generation::new(3).unwrap()));
    assert!(!name.is_for(&attempt, Generation::new(4).unwrap()));
    assert!(!name.is_for(
        &AttemptId::from_u128(0xdead_bef0),
        Generation::new(3).unwrap()
    ));
}

#[test]
fn should_refuse_pool_name_when_it_is_malformed() {
    // given
    // when
    // then
    for bad in [
        "",
        "A",
        "a-b",
        "a_b",
        "a b",
        "aaaaaaaaaaaaaaaaa",
        "é",
        "a\n",
    ] {
        assert!(PoolName::new(bad).is_err(), "{bad:?}");
    }
    assert!(PoolName::new("a").is_ok());
    assert!(PoolName::new("0123456789abcdef").is_ok());
}

#[test]
fn should_refuse_host_id_when_it_is_malformed() {
    // given
    // when
    // then
    for bad in ["", "H", "h-1", "aaaaaaaaaaaaaaaaa"] {
        assert!(HostId::new(bad).is_err(), "{bad:?}");
    }
    assert!("mac1".parse::<HostId>().is_ok());
}

#[test]
fn should_refuse_hex_id_when_it_is_not_32_lowercase_digits() {
    // given
    // when
    // then
    for bad in [
        "",
        "abc",
        &"A".repeat(32),
        &"g".repeat(32),
        &"0".repeat(31),
        &"0".repeat(33),
    ] {
        assert!(bad.parse::<AttemptId>().is_err(), "{bad:?}");
    }
    assert_eq!(&"0".repeat(32).parse::<AttemptId>().unwrap().as_u128(), &0);
}

#[test]
fn should_admit_the_fence_only_when_host_and_epoch_match() {
    // given
    let host = HostId::new("mac1").unwrap();
    let current = Fence::new(host.clone(), Epoch::new(5));
    // when
    let same = current.admit(&Fence::new(host.clone(), Epoch::new(5)));
    // then
    assert_eq!(same, Ok(()));
    assert_eq!(
        current.admit(&Fence::new(host.clone(), Epoch::new(4))),
        Err(FenceError::Stale)
    );
    assert_eq!(
        current.admit(&Fence::new(host, Epoch::new(6))),
        Err(FenceError::Ahead)
    );
    assert_eq!(
        current.admit(&Fence::new(HostId::new("mac2").unwrap(), Epoch::new(5))),
        Err(FenceError::WrongHost)
    );
    // the host is checked first: host and epoch both different is WrongHost, not Stale or Ahead
    for epoch in [4, 6] {
        assert_eq!(
            current.admit(&Fence::new(HostId::new("mac2").unwrap(), Epoch::new(epoch))),
            Err(FenceError::WrongHost)
        );
    }
}

#[test]
fn should_not_wrap_when_generation_or_epoch_reaches_its_limit() {
    // given
    // when
    // then
    assert_eq!(Generation::FIRST.next(), Generation::new(2));
    assert_eq!(Generation::new(u32::MAX).unwrap().next(), None);
    assert_eq!(Generation::new(0), None);
    assert_eq!(Generation::FIRST.get(), 1);
    assert_eq!(Epoch::FIRST.next(), Some(Epoch::new(2)));
    assert_eq!(Epoch::new(u64::MAX).next(), None);
}

#[test]
fn should_accept_only_canonical_digests_when_one_is_parsed() {
    // given
    let good = format!("sha256:{}", "ab01".repeat(16));
    // when
    // then
    assert_eq!(ImageDigest::new(&good).unwrap().as_str(), good);
    for bad in [
        "".to_owned(),
        "latest".to_owned(),
        "sha256:".to_owned(),
        format!("sha256:{}", "AB01".repeat(16)),
        format!("sha256:{}", "ab01".repeat(15)),
        format!("sha512:{}", "ab01".repeat(16)),
        format!(" {good}"),
    ] {
        assert!(ImageDigest::new(&bad).is_err(), "{bad:?}");
    }
}

#[test]
fn should_do_checked_arithmetic_when_resources_are_combined() {
    // given
    let a = Resources::new(8, 100);
    let b = Resources::new(2, 30);
    // when
    // then
    assert_eq!(a.checked_sub(b), Some(Resources::new(6, 70)));
    assert_eq!(b.checked_sub(a), None);
    assert_eq!(a.checked_add(b), Some(Resources::new(10, 130)));
    assert_eq!(
        Resources::new(u64::MAX, 0).checked_add(Resources::new(1, 0)),
        None
    );
    assert!(b.fits_within(a));
    assert!(!Resources::new(9, 1).fits_within(a));
}

#[test]
fn should_expire_a_deadline_when_now_reaches_it() {
    // given
    let deadline = Deadline::after(Millis::new(1_000), Millis::new(500));
    // when
    // then
    assert!(!deadline.is_expired(Millis::new(1_499)));
    assert!(deadline.is_expired(Millis::new(1_500)));
    assert_eq!(deadline.remaining(Millis::new(1_200)), Millis::new(300));
    assert_eq!(deadline.remaining(Millis::new(2_000)), Millis::new(0));
}

fn decode<'de, T: Deserialize<'de>>(text: &'de str) -> Result<T, DeError> {
    let de: StrDeserializer<'de, DeError> = text.into_deserializer();
    T::deserialize(de)
}

#[test]
fn should_validate_when_a_value_is_deserialized() {
    // given the wire form is as strict as the constructor
    // when
    // then
    assert!(decode::<OwnedVmName>(GOOD).is_ok());
    assert!(decode::<OwnedVmName>("mf-pool-host-s01-g1-0123abcd").is_err());
    assert!(decode::<OwnedVmName>("ubuntu").is_err());
    assert!(decode::<PoolName>("ok1").is_ok());
    assert!(decode::<PoolName>("no-good").is_err());
    assert!(decode::<HostId>("").is_err());
    assert!(decode::<AttemptId>(&"a".repeat(32)).is_ok());
    assert!(decode::<AttemptId>("xyz").is_err());
    assert!(decode::<ImageDigest>("latest").is_err());
    let de: U32Deserializer<DeError> = 9_u32.into_deserializer();
    assert_eq!(
        Generation::deserialize(de).unwrap(),
        Generation::new(9).unwrap()
    );
    let zero: U32Deserializer<DeError> = 0_u32.into_deserializer();
    assert!(Generation::deserialize(zero).is_err());
}

/// `T`'s sample, decoded from its own text as an event payload would carry it.
fn decoded_sample<T>() -> Result<T, DeError>
where
    T: Sample + for<'de> Deserialize<'de>,
    String: From<T>,
{
    let text = String::from(T::sample());
    T::deserialize(StrDeserializer::<DeError>::new(&text))
}

#[test]
fn should_decode_its_own_text_when_a_sample_is_serialised() {
    // given
    let expected = (
        PoolName::sample(),
        HostId::sample(),
        AttemptId::sample(),
        CommandId::sample(),
        DirectiveId::sample(),
        EventId::sample(),
        ObservationId::sample(),
        ImageDigest::sample(),
        OwnedVmName::sample(),
    );
    // when
    let decoded = (
        decoded_sample::<PoolName>(),
        decoded_sample::<HostId>(),
        decoded_sample::<AttemptId>(),
        decoded_sample::<CommandId>(),
        decoded_sample::<DirectiveId>(),
        decoded_sample::<EventId>(),
        decoded_sample::<ObservationId>(),
        decoded_sample::<ImageDigest>(),
        decoded_sample::<OwnedVmName>(),
    );
    // then
    assert_eq!(decoded.0, Ok(expected.0));
    assert_eq!(decoded.1, Ok(expected.1));
    assert_eq!(decoded.2, Ok(expected.2));
    assert_eq!(decoded.3, Ok(expected.3));
    assert_eq!(decoded.4, Ok(expected.4));
    assert_eq!(decoded.5, Ok(expected.5));
    assert_eq!(decoded.6, Ok(expected.6));
    assert_eq!(decoded.7, Ok(expected.7));
    assert_eq!(decoded.8, Ok(expected.8));
    assert_eq!(
        String::from(OwnedVmName::sample()),
        "mf-pool0-host0-s0-g1-00000001"
    );
}

#[test]
fn should_refuse_instance_id_when_it_is_malformed() {
    // given
    let too_long = "a".repeat(65);
    // when
    let refused: Vec<&str> = ["", "AA:BB", "a b", "a/b", "a;b", "é", too_long.as_str()]
        .into_iter()
        .filter(|bad| ProviderInstanceId::new(bad).is_err())
        .collect();
    let decoded: Result<ProviderInstanceId, DeError> =
        ProviderInstanceId::deserialize(StrDeserializer::new("a$b"));
    // then
    assert_eq!(refused.len(), 7, "{refused:?}");
    assert!(decoded.is_err());
    assert!(ProviderInstanceId::new("0a:1b:2c:3d:4e:5f").is_ok());
    assert!(ProviderInstanceId::new(&"0".repeat(64)).is_ok());
}

#[test]
fn should_decode_its_own_text_when_an_attempt_sample_is_serialised() {
    // given
    let instance = ProviderInstanceId::sample();
    let handle = SecretHandleId::sample();
    // when
    let decoded_instance = decoded_sample::<ProviderInstanceId>();
    let decoded_handle = decoded_sample::<SecretHandleId>();
    // then
    assert_eq!(decoded_instance, Ok(instance));
    assert_eq!(decoded_handle, Ok(handle));
}
