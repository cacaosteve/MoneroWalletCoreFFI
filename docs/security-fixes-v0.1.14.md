# WalletCore v0.1.14 security fixes

## Published v0.1.15 follow-up

- Key-image spend scalars/offsets and subaddress view-key derivation buffers use wiping owners.
- Rust API seed C strings and output buffers use wiping owners. Swift mnemonic/open/unseal
  wrappers wipe owned UTF-8/output arrays on success and error via non-elidable platform routines.
- Secret C strings reserve their terminator before copying and reject interior NULs before
  allocation, avoiding CString constructor/error buffers and growth reallocations.
- No ABI/cache-schema/amount/fee change. The v0.1.15 XCFramework/native release distributes
  these changes; the v0.1.14 asset remains unchanged.
- Ownership wiping is defense-in-depth, not a promise that managed strings, registers, stack
  moves or third-party framework copies leave no recoverable data. Never use live funds in fixtures.

Local verification: 92 Rust tests pass (6 external/diagnostic fixtures ignored), 13 Swift wrapper
tests pass against the new native binary, all seven Apple library targets build into an XCFramework,
and arm64-v8a/x86_64 Android release libraries build with 16 KB alignment. Advisory checks pass.

Native artifacts were built from `3d0a97e87ae67d627099c9353104a239c1adeac3` in
[release CI](https://github.com/cacaosteve/MoneroWalletCoreFFI/actions/runs/38350931561).
The final release commit only updates this documentation and the matching SwiftPM URL/checksum;
the native crate and Swift wrapper source trees are identical to the build commit.

## Published v0.1.14 fixes

- Retain each owned output's one-time public key and accept it only once, across transaction
  IDs and output indices. Keep spent keys too. This applies to spend-capable and sealed/view-only
  scans; a reorg drops only keys belonging to outputs removed with the old fork.
- Reject duplicate keys in imported snapshots before mutating an open wallet.
- Cache schema is **v5**. Earlier snapshots cannot supply the output-key invariant and are
  incompatible. Hosts must quarantine them and rescan from the wallet's original restore height,
  not from its last scanned height. Seed storage and the C/JSON ABI are unchanged.
- Validate every decoded range block's height and internal parent link, including short
  responses, before output-index lookup, scanning, or checkpoint publication. This is structural
  RPC validation, not independent proof-of-work/consensus verification.
- Pin the oxide fork to `34201e58bf008f5ae66a2393d164837ec54b3405`, restoring zeroization
  of temporary shared-derivation buffers while retaining the optimized view-tag scan path.
- Swift lifecycle errors preserve the native return code. `-31` means the operation is still
  busy and **keys have not necessarily been sealed/closed**. Mobile managers retain pending
  cleanup, retry, and block unlock/replacement until it succeeds. OS process termination still
  destroys process memory; background scheduling is not guaranteed by the operating system.
- Apple and Android source build scripts require the committed Cargo lockfile (`--locked`).

Regression coverage includes a synthetic scanner replay with distinct TXIDs and one owned key,
sealed/spent/reorg deduplication, cache round-trip and rejection without mutation, wrong/missing/
reordered/repeated block heights, broken parent links, and a local slow-RPC lifecycle timeout.
No funded send or user-device rescan is performed by these tests.
