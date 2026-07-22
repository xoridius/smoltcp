# Fork maintenance guide

This repository is a downstream of
[`smoltcp-rs/smoltcp`](https://github.com/smoltcp-rs/smoltcp), based on the
upstream `v0.13.1` tag. It keeps upstream's public interface where practical,
but carries deliberate TCP, memory-management, platform, and validation
changes for memory-constrained packet tunnels.

## Relationship to upstream

Fork commits are `v0.13.1..HEAD`. Upstream commits after that tag are reviewed
one pull request at a time and recorded in the ledger below. Prefer a
cherry-pick or a small hand-port over merging `upstream/main`: the fork's TCP
state machine has enough intentional changes that a wholesale merge obscures
which behavior won each conflict.

The main maintained differences are:

| Area | Fork behavior | Main conflict surface |
|---|---|---|
| TCP send and loss recovery | Transmitted-sequence validation, SACK-based selective retransmission, RecoveryPoint handling, exact IW10, per-connection controller reset, and PAWS expiry | `src/socket/tcp.rs`, `src/socket/tcp/sack.rs`, `src/socket/tcp/congestion/` |
| TCP memory | Optional lazy, pool-accounted receive and transmit buffers | `src/socket/tcp.rs`, `src/socket/tcp/dynbuf.rs`, `src/storage/ring_buffer.rs` |
| Wire and interface validation | Checked length, checksum, address, multicast, and security-metadata handling on audited paths | `src/wire/`, `src/iface/` |
| Hosted platforms | Monotonic hosted time, batched BPF reads, and explicit TUN/TAP descriptor ownership | `src/time.rs`, `src/phy/sys/`, `src/phy/tuntap_interface.rs` |
| Production evidence | Socket-size, constrained-memory, traffic, Apple cross-build, netsim, fuzz, and profiling lanes | `ci.sh`, `ci/`, `examples/profile_loopback*`, `tests/`, `docs/perf/` |

Keep upstream-shaped state transitions and dispatch ordering in
`src/socket/tcp.rs`. Fork-owned data structures belong in private modules; do
not wrap the state machine in a parallel abstraction.

## Using the fork

Consumers must pin an immutable commit with Cargo's `rev` field. Do not depend
on `branch = "main"`; it makes dependency updates implicit.

The fork adds these public interfaces while retaining the upstream fixed-buffer
constructor:

- `tcp::DynamicBufferConfig`, `tcp::MemoryPool`, and
  `tcp::Socket::new_dynamic` for optional pool-backed buffers;
- `tcp::Socket::recv_with`, the closure-based receive form that also performs
  terminal-buffer reclamation;
- `TunTapInterface::from_owned_fd` for ownership transfer, while `from_fd`
  remains the compatibility form that duplicates a borrowed descriptor;
- unconditional `core::error::Error` implementations and the public
  `wire::checksum` re-export.

## Runtime invariants

### TCP sequence and recovery state

- `local_seq_no` is SND.UNA. `local_seq_next` is RFC SND.NXT, the monotonic
  high-water mark after bytes actually transmitted. `remote_last_seq` is a
  rewindable dispatch/recovery cursor. ACK and SACK admission must never use
  queued-but-unsent bytes as their upper bound.
- Sender SACK ranges are offsets from SND.UNA, bounded by SND.NXT and buffered
  data. Cumulative ACKs rebase the scoreboard. The transmit cursor never rests
  inside a SACKed range, and retransmitted segments stop at the next SACKed
  range.
- RecoveryPoint is SND.NXT when fast recovery or RTO recovery begins. Partial
  ACKs remain in recovery below it. RTO clears stale SACK state and prioritizes
  the oldest missing data before unsent data.
- Reno and CUBIC are state per TCB. A new connection resets controller and RTT
  state and initializes the congestion window with RFC 6928 IW10 after MSS is
  known.
- Timestamp negotiation is state per TCB. TSval zero is valid; an explicit
  validity bit distinguishes missing PAWS state. Stored TS.Recent expires after
  more than 24 days as required by RFC 7323.

`SackScoreboard` owns only interval validation, rebasing, cursor advancement,
segment clamping, and SACKed-byte counting. Recovery policy, congestion
notifications, timers, and dispatch order remain owned by `Socket`.

The SACK flight estimate is intentionally not a complete RFC 6675 pipe
estimator: it subtracts confirmed ranges below the cursor but does not account
for all outstanding bytes above it. `NoControl` may make one bounded redundant
pass to recover a lost cumulative ACK; Reno and CUBIC do not.

### Dynamic TCP buffers

The `socket-tcp-dynamic-buffer` feature is off by default. When off, the state,
hooks, and resizable ring-buffer operations are not compiled. When on,
`Socket::new` remains fixed-buffer behavior and `Socket::new_dynamic` opts in.

- `rx_max` and `tx_max` are capacity limits, not reservations. Each is clamped
  to the RFC 7323 window limit. Window scaling is fixed during the handshake
  from the receive maximum.
- `MemoryPool` charges logical buffer capacity shared by its sockets. Charge is
  reserved before allocation or growth and refunded on allocation failure,
  safe terminal release, reset, or drop. Arithmetic overflow refuses the
  transaction.
- Growth is fallible. Refusal preserves accepted bytes and becomes TCP
  backpressure; it never advertises receive capacity without backing storage.
  Above 75% pool use, growth is linear rather than geometric to preserve shared
  headroom.
- Initial receive and transmit allocation is one transaction: failure in
  either direction rolls back both allocations and the complete pool charge.
- Unread receive data survives terminal transitions until application dequeue
  no longer needs it. In particular, peer RST cannot prematurely refund or
  discard unread data. `recv_slice` and `recv_with` reclaim terminal storage
  after dequeue. The borrowing `recv` API cannot reclaim storage before its
  returned value stops borrowing the receive buffer; reset, drop, or a later
  non-borrowing receive completes reclamation.
- Pool charge must return to zero after all sharing sockets are torn down.

A zero-initial configuration admits idle sockets without buffer capacity:

```rust
let pool = tcp::MemoryPool::new(24 * 1024 * 1024);
let config = tcp::DynamicBufferConfig {
    rx_initial: 0,
    rx_max: 64 * 1024,
    tx_initial: 0,
    tx_max: 64 * 1024,
    grow_chunk: 4 * 1024,
};
let socket = tcp::Socket::new_dynamic(config, Some(pool.clone()));
```

The 24 MiB pool is a starting point, not an iOS memory guarantee. It excludes
the interface, device queues, other sockets, the embedding application, and
Network framework overhead.

### Wire and hosted-platform behavior

| Path | Maintained contract |
|---|---|
| Checksums | Wide checksum accumulation is endian-correct; IPv6 UDP zero checksums are rejected; compressed 6LoWPAN UDP checksums are preserved and validated. |
| IPv4 and IPv6 | Fragment bounds are checked before arithmetic; SLAAC prefixes, NDISC NA/DAD fields, MLD router alerts, and MLDv2 response codes are validated. |
| IEEE 802.15.4 | Security-level metadata determines and validates the MIC length before payload slicing. |
| BPF | One device read may contain multiple aligned records; records are returned individually. A malformed boundary discards the untrustworthy suffix. |
| TUN/TAP descriptors | `from_owned_fd` consumes ownership; compatibility `from_fd` duplicates the caller's descriptor. |
| Hosted time | Conversion from `std::time::Instant` is monotonic and does not use a process-age subtraction. |

## Known limits

- Growing a ring buffer copies its live contents and may transiently hold old
  and new allocations. Pool accounting bounds steady logical capacity, not
  allocator peak RSS or Apple jetsam footprint.
- Dynamic growth is pressure-driven, not a per-RTT bandwidth-delay-product
  autotuner. Per-flow maxima cannot be changed after construction.
- The SACK implementation is selective recovery, not RACK-TLP, pacing, or a
  full RFC 6675 pipe estimator.
- On native macOS, `apple_phys_footprint` comes from `proc_pid_rusage`. Like
  host-side Linux RSS, it is a useful signal, not an iOS Network Extension
  proof. iOS targets are cross-compiled; final integration still needs
  on-device footprint and jetsam validation.
- `Interface` remains single-thread owned. Consumers that need parallelism
  shard interfaces; the fork does not make `Interface` synchronized.
- `poll()` retains upstream's drain-until-empty behavior. Use the existing
  single-ingress and split egress/maintenance methods when a driver needs a
  bounded unit of work.

## Verification

`./ci.sh help` is the command reference. Traffic commands live in
`ci/ios-full-gate-static.txt` and `ci/ios-full-gate-dynamic.txt`; do not copy
those matrices into prose. Apple harnesses use paired in-process `Medium::Ip`
devices and require no elevated privileges.

Before a production release, run:

```text
./ci.sh all
./ci.sh docs
TRACE=0 ./ci.sh ios-full-gate
cargo +nightly bench --bench bench
./ci.sh fuzz-build
./ci.sh fuzz-smoke 30
tools/upstream-delta.sh
```

`./ci.sh all` is the portable core matrix: MSRV, stable, and nightly tests and
checks, Clippy, the 16-bit build, coverage, and serialized NoControl/Reno/CUBIC
netsim. It deliberately excludes Apple cross-builds, performance, fuzz, Miri,
and documentation.

For changes to unsafe code, parsing, borrowing, or dynamic buffer lifetime,
also run the relevant Tree Borrows proofs:

```text
MIRIFLAGS="-Zmiri-tree-borrows" cargo +nightly miri test --lib socket::tcp
MIRIFLAGS="-Zmiri-tree-borrows" cargo +nightly miri test --lib test_deconstruct
MIRIFLAGS="-Zmiri-tree-borrows" cargo +nightly miri test --lib \
  --features socket-tcp-dynamic-buffer dyn \
  -- --skip socket::tcp::test::dyn_buf::pool_capacity_floor_300_sockets_24mib_budget
```

The footprint gates are 536 bytes for the default TCP `Socket` and 592 bytes
for the constrained-Apple dynamic/CUBIC feature set. A landing throughput
regression blocks at worse than -2% in matched runs. An RSS increase blocks
only when its paired median exceeds both one host page and 1% and occurs in at
least three of five matched pairs. Dynamic traffic must retain Jain fairness
of at least 0.95, no starvation, no fallback allocation, bounded memory traces,
and zero pool charge after teardown.

Dated benchmark summaries live in `docs/perf/`. New evidence must identify the
exact source, toolchain, feature set, commands, and matched-run method.

## Upstream sync

Configure and inspect upstream with:

```text
git remote add upstream https://github.com/smoltcp-rs/smoltcp.git
git fetch upstream
tools/upstream-delta.sh
```

The tool fixes the comparison base at `v0.13.1`, verifies ancestry, validates
the structured ledger, and fails if upstream contains an unclassified pull
request, an unrecognized first-parent merge, or a direct-to-main commit.

For each new upstream pull request:

1. Read the complete upstream change and its tests; classify semantic overlap,
   not just patch identity.
2. Cherry-pick clean changes. Hand-port conflicts only where fork invariants
   require it. Do not accept conflict resolutions that replace fork behavior
   silently.
3. Run focused tests, then the applicable production lanes above. TCP data-path
   changes require netsim, socket-size, static/dynamic traffic, allocation,
   pool, RSS, and throughput evidence.
4. Add one ledger row with the pull request number, one allowed outcome, and a
   durable explanation. The tool treats the title as display text only.

Allowed outcomes are:

- `integrated`: upstream behavior is present without a meaningful fork-specific
  replacement;
- `adapted`: upstream behavior is present but reconciled with fork-owned code;
- `superseded`: independent fork behavior covers or exceeds the upstream
  change, so its patch is not applied;
- `skipped`: the change is deliberately outside this fork's product or
  repository scope.

<!-- upstream-ledger:start -->
| PR | Outcome | Note |
|---:|---|---|
| #1143 | skipped | The fork retains its audited fuzz targets and seeded smoke lane. |
| #1150 | integrated | PacketBuffer reserves metadata before invoking the payload closure. |
| #1152 | integrated | Logging macros carry collapse_debuginfo. |
| #1153 | skipped | Upstream netsim fingerprints do not represent the fork recovery stack. |
| #1154 | adapted | Controller behavior uses the fork's static AnyController dispatch. |
| #1155 | adapted | RTT fixes are present; selective recovery replaces the upstream retransmit mechanism. |
| #1156 | adapted | Reno behavior is reconciled with per-TCB reset and exact IW10. |
| #1157 | adapted | CUBIC behavior is reconciled with per-TCB reset and exact IW10. |
| #1159 | integrated | Generated configuration ordering uses BTreeMap. |
| #1161 | adapted | Effective MSS accounts for options while preserving fork SACK bounds. |
| #1162 | integrated | Out-of-window data ACKs are not rate limited. |
| #1164 | integrated | Out-of-window RST handling follows the upstream correction. |
| #1169 | skipped | Upstream repository contribution policy is not a library backport. |
| #1170 | skipped | This typo follow-up applies only to the skipped policy document. |
| #1172 | superseded | The checksum re-export was implemented independently. |
| #1173 | integrated | Broken rustdoc links were corrected. |
| #1175 | adapted | core::error::Error covers all fork error types including no_std. |
| #1176 | superseded | The fork keeps its measured endian-correct wide checksum path. |
| #1177 | superseded | Hosted Instant conversion was independently fixed with ordering coverage. |
<!-- upstream-ledger:end -->
