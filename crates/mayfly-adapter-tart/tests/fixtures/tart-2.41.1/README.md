# Tart 2.41.1 output, derived from its source

None of these files was captured from a running binary: no Mac runs in the agents' sandbox. Each
is written from the source of the 2.41.1 release (github.com/openai/tart, tag `2.41.1`) with
synthetic names, sizes, MACs and dates. HW-01 replaces the derivation with a capture on real Tart.

| File | Derived from | Synthetic |
|:--|:--|:--|
| `version.txt` | `Root.swift` (`version: CI.version`), `CI/CI.swift` (the release tag) | nothing |
| `list.json` | `Commands/List.swift` (`Source`, `Name`, `Disk`, `Size`, `Accessed`, `Running`, `State`; local entries, then OCI, each sorted by name; `Disk` `null` whenever the size read fails, as for a running VM's ASIF disk; `Accessed` in ISO 8601), `VMStorageOCI.list` (a requested digest that Tart linked to another manifest's entry is listed `<repo>:<digest>`, the entry itself `<repo>@<digest>`) | names, sizes, dates, digests |
| `get-*.json` | `Commands/Get.swift` (`OS`, `CPU`, `Memory` in MiB, `Disk` in whole decimal GB, `null` when the read fails, `DiskFormat`, `Size` as a three-decimal string, `Display` with its optional `pt`/`px` unit, `MACAddress`, `Running`, `State`) | values; the MAC's letter case (the adapter lowercases it) |
| `run-hypervisor-limit.stderr` | `Commands/Run.swift` and `VMStorageHelper.swift` (`RuntimeError.VirtualMachineLimitExceeded`, printed bare by `tart run`) | the VM names |

Not known from the source, so not pinned: the key order and whitespace Foundation's
`JSONEncoder` writes (the fixtures use declaration order and its pretty-printed `" : "`), and the
letter case of `VZMACAddress.string`. The adapter reads none of them.
