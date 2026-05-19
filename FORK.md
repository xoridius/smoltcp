# FORK.md

Operational guide for maintaining this downstream of `smoltcp-rs/smoltcp`.

## 1. Relationship

This is a downstream of `https://github.com/smoltcp-rs/smoltcp`. It carries a
small set of additive changes — RFC-compliance fixes, host-side wire-layer
performance, Darwin/BSD phy hardening, and an in-process profiling harness.
There is no architectural divergence from upstream; do not introduce one.

Configure the upstream remote on a fresh clone:

```
git remote add upstream https://github.com/smoltcp-rs/smoltcp.git
git fetch upstream
```

## 2. Sync from upstream

```
git fetch upstream main
git log --oneline HEAD..upstream/main
git merge --ff-only upstream/main || git merge upstream/main
# resolve conflicts (most likely in src/socket/tcp.rs and
# src/socket/tcp/congestion*.rs — these have the most local edits)
# run the test matrix in §3
git push origin main
```

Prefer fast-forward when possible. Never rebase published commits. Never
force-push `main`.

After every sync, re-check `Cargo.toml`'s feature list against the test matrix
in §3 — upstream occasionally adds or renames features.

## 3. Test matrix

Run all of these before any commit lands on `main`. Expected pass counts let
you spot regressions at a glance.

```
cargo test --release --lib                                            # 663
cargo test --release --lib --features socket-tcp-cubic                # 673
cargo test --release --lib --features socket-tcp-reno                 # 668

cargo test --release --lib --no-default-features \
  --features "std,medium-ethernet,proto-ipv4,proto-ipv4-fragmentation,socket-raw,socket-dns"     # 172

cargo test --release --lib --no-default-features \
  --features "std,medium-ethernet,proto-ipv6,socket-tcp,socket-udp"                              # 198

cargo test --release --lib --no-default-features \
  --features "alloc,medium-ethernet,proto-ipv4,proto-ipv6,socket-raw,socket-udp,socket-tcp,socket-icmp,proto-ipv6-slaac"     # 284

cargo clippy --release --lib --tests   # clean except a pre-existing ieee802154 warning
cargo +nightly bench --bench bench     # 11 measurements, no errors
```

If a number changes, re-derive expectations and update this file in the same
commit. The numbers move only when a test is added or removed.

## 4. Load-test workflows

### 4.1 Wire-level microbench

```
cargo +nightly bench --bench bench
```

Measures ns/iter for `checksum::data` across packet sizes (64, 576, 1500,
1501, 9000, 65535), the four `bench_emit_*` benches at 400 B payload, and
`bench_parse_verify_tcp` (1480 B segment, full RX path).

Headline reads:
- `bench_checksum_1500` — sustained checksum throughput in MB/s on the right
  margin. Auto-vectorized; expect ~40 GB/s on modern x86_64 with
  `-C target-cpu=native`, ~30 GB/s on aarch64 NEON, lower on baseline targets.
- `bench_parse_verify_tcp` — closest single number to "RX hot path cost per
  packet."
- `bench_emit_ipv4` minus `bench_emit_ipv6` ≈ cost of the IPv4 header
  checksum step (IPv6 has no header checksum).

Variance: ±5% run-to-run on a quiet host is normal. Thermal throttling, noisy
neighbors in shared VMs, or perf governor changes can move it 20%+. For
comparisons that matter, pin the CPU governor to `performance`, disable
hyperthreading siblings of the bench CPU, and re-run three times.

### 4.2 End-to-end shapes via the harness

```
cargo build --release --example profile_loopback
cargo run --release --example profile_loopback -- <shape> <seconds> [offload]
```

Single-flow shapes (saturated one connection):

| Shape | What it stresses |
|---|---|
| `udp` | Pure UDP forwarding at MTU. Tunnel analogue. The Mpps headline. |
| `small` | Many small TCP segments. State-machine overhead independent of payload size. |
| `pingpong` | 128 B TCP request/response. Latency-bound. Verifies Nagle / delayed-ACK config. |
| `firehose` | TCP bulk transfer. Both peers are smoltcp so cwnd dynamics dominate; useful only for relative comparisons. |
| `all` | Runs `udp` + `small` + `pingpong` back-to-back. |

Multi-flow shapes (fairness + scaling + memory at large flow counts):

```
cargo run --release --example profile_loopback -- many_tcp <seconds> <N>
cargo run --release --example profile_loopback -- many_udp <seconds> <N>
```

Sweep N to characterize scaling:

```
for n in 50 100 200 500 1000 2000; do
  cargo run --release --example profile_loopback -- many_tcp 5 $n 2>&1 | \
    grep -E "throughput \(app|Jain|verdict|RSS verdict"
done
```

Report fields to read:

| Field | Meaning |
|---|---|
| `throughput (app)` | Aggregate app-visible Gbps / MB/s. |
| `per-packet` | ns + estimated cycles per packet at the harness's reference frequency. |
| `poll-cycle latency: p50 / p99` | Tail latency of a single `Interface::poll` invocation. |
| `Jain` | Per-flow fairness index. ≥ 0.95 = FAIR; below 0.95 needs investigation. |
| `verdict` | Single-line pass/fail style summary for fairness + starvation. |
| `RSS verdict` | `bounded` or `GROWTH`. GROWTH means the median RSS over the run is materially smaller than the final RSS — leak suspect. |
| `net heap delta` | Should be a small constant (each periodic `/proc/self/status` read accounts for it). Non-constant values mean smoltcp itself allocated on the hot path → bug. |

### 4.3 Configuration variants worth measuring

**Checksum offload:**

```
cargo run --release --example profile_loopback -- udp 5            # software checksums (default)
cargo run --release --example profile_loopback -- udp 5 offload    # device claims hardware offload
```

The delta is the all-in checksum cost. Useful as a ceiling number. `offload`
mode is only safe when both peers ignore checksums (e.g., a loopback
benchmark). Real deployments whose peer is a kernel TCP stack must NOT
enable this — kernel strict-checksum validation will drop every reply.

**Congestion control:**

```
cargo build --release --example profile_loopback                                    # NoControl (default features)
cargo build --release --example profile_loopback --features socket-tcp-cubic        # Cubic + RFC 6928 IW10
cargo build --release --example profile_loopback --features socket-tcp-reno         # Reno + RFC 6928 IW10
```

Run `pingpong` with each: Cubic and Reno finish more round-trips in the same
wall time on short connections because of the IW10 first-RTT ramp.

**Feature gating:**

Build the harness with `--no-default-features --features ...` matching the
feature set of the consumer you're targeting. This catches build-failure
regressions where a perf-relevant code path is gated behind a feature that
the consumer doesn't enable.

### 4.4 Reproducing canonical numbers

Run on a quiet host, governor pinned to `performance`:

```
# Wire microbench, multiple runs for variance
for i in 1 2 3; do cargo +nightly bench --bench bench; done

# Single-flow shapes, 10 sec each for stable averages
cargo run --release --example profile_loopback -- all 10
cargo run --release --example profile_loopback -- all 10 offload

# Scaling sweep
for n in 100 500 1000; do
  cargo run --release --example profile_loopback -- many_tcp 10 $n
  cargo run --release --example profile_loopback -- many_udp 10 $n
done
```

## 5. CPU profiling

### 5.1 perf

```
perf record -F 999 --call-graph dwarf -o /tmp/prof.data \
  target/release/examples/profile_loopback udp 5
perf report -i /tmp/prof.data --no-children --stdio --percent-limit 1
```

Symbols to expect in the top 10 on any reasonable workload:

- `wire::ip::checksum::data` — vectorized but still touches every byte; routinely top-3.
- `__memmove*` — payload copies between socket buffers and wire packets; structurally unavoidable; top-5.
- `socket::{tcp,udp}::Socket::{process,dispatch,recv_slice,send_slice}` — protocol state machine + socket-buffer plumbing.
- `iface::interface::{Interface::poll,InterfaceInner::process_ipv4,dispatch_ip}` — packet dispatch layer.

Diagnostics:

- `__memmove` over ~25% of profile → allocator-bound or excessive copies; check `net heap delta` and confirm steady state.
- `checksum::data` over ~35% → auto-vectorizer didn't engage; verify codegen (§5.3).
- A tokio/runtime symbol or `clock_gettime`-related vDSO entry in the top 10 → you're profiling harness overhead, not smoltcp. Re-check the sample rate vs the workload's iteration rate.

`perf stat -e cycles,instructions,cache-misses,branch-misses` requires hardware
PMU access. Containers and locked-down VMs typically disallow it; `perf record`
still works (falls back to `task-clock` software sampling).

### 5.2 samply

```
samply record target/release/examples/profile_loopback udp 5
```

Opens the Firefox-profiler UI in a browser. Better for interactive call-stack
navigation than `perf report --stdio`. Works on macOS and Linux.

### 5.3 Cross-target codegen verification

Run after any change to `src/wire/ip.rs::checksum`. Confirms vectorization
survives on each supported target:

```
cargo rustc --release --lib --target aarch64-unknown-linux-gnu -- --emit=asm
cargo rustc --release --lib --target powerpc-unknown-linux-gnu -- --emit=asm
cargo rustc --release --lib -- --emit=asm -C target-cpu=native
```

Look in the emitted `.s` files at the `_ZN.*checksum.*data.*` symbol:

| Target | What to find |
|---|---|
| aarch64 | `add v.\.2d`, `cmhi v.\.2d`, `ldp q.+, q.+` (NEON vector add + carry) |
| powerpc (big-endian) | scalar `addc`/`adde`/`addze` chain; **no byte-swap at function tail** — the BE fix removed it; reappearance is a regression |
| x86_64 + `target-cpu=native` | `vpaddq`, `vpcmpltuq`, `vpmovm2q` (AVX-512) |
| x86_64 default | scalar two-chain `addq`/`adcq` (what most consumers compile to) |

### 5.4 Flamegraph

```
cargo install flamegraph
cargo flamegraph --example profile_loopback -- udp 5
# Outputs flamegraph.svg
```

Useful for showing call chains at a glance in a single image.

## 6. Memory profiling

### 6.1 Harness built-in metrics

Every shape's report already includes the steady-state allocator state and
RSS bookends:

```
steady-state allocations:
  bytes allocated:        N
  bytes freed:            N
  net heap delta:         small constant
  allocation count:       N

process memory:
  rss start:              ...
  rss end:                ...
```

`many_*` shapes additionally print an RSS trajectory sampled every ~250 ms
with a verdict line.

Interpretation rules:

- **`net heap delta` should be a small constant** dominated by the harness's
  own periodic `/proc/self/status` reads. Anything else means smoltcp itself
  allocated on the hot path — a regression to investigate.
- **`RSS verdict: bounded`** when the final RSS is within ~1.5× the median.
  `GROWTH` flags a possible leak; drop into massif/heaptrack to confirm.
- **`bytes allocated ≈ bytes freed`** in steady state. A persistent imbalance
  means a buffer that isn't returning to its pool, a held reference, or a
  growing data structure.

### 6.2 massif

```
valgrind --tool=massif --pages-as-heap=no \
  --massif-out-file=/tmp/massif.out \
  target/release/examples/profile_loopback udp 2
ms_print /tmp/massif.out | less
```

Per-allocation-site heap trajectory. Use when the harness's `RSS verdict`
flags growth and you need to identify the source.

### 6.3 heaptrack

```
heaptrack target/release/examples/profile_loopback udp 3
heaptrack_gui heaptrack.profile_loopback.*.zst
```

Faster than massif, lower runtime overhead, has a real UI. Default choice for
deep allocation analysis.

### 6.4 Sizecheck — struct footprint diagnostic

```
cargo test --release --test sizecheck -- --nocapture
cargo test --release --test sizecheck --features socket-tcp-cubic -- --nocapture
cargo test --release --test sizecheck --features socket-tcp-reno -- --nocapture
```

Prints `size_of` for the TCP/UDP/ICMP/Raw `Socket` types, `RingBuffer`,
`Assembler`, `IpRepr`, and `TcpRepr`. The test never asserts; it is diagnostic
only. Run after any field-type change and record the current values in the
commit message — future field-layout changes will move them, which is the
catch.

### 6.5 Verifying the alloc-free hot path

The canonical check:

```
cargo run --release --example profile_loopback -- many_tcp 5 1000 2>&1 | \
  grep -E "net heap delta|allocation count"
```

Expect a small constant `net heap delta` and an `allocation count` whose
magnitude tracks the number of `/proc/self/status` reads (one per
`MemTrace::maybe_sample`, roughly every 250 ms). Materially higher values
indicate something is allocating per packet — usually a `Vec::with_capacity`
or `Bytes::from(Vec)` introduced in the hot path.

## 7. Property tests as regression gates

Run before every commit; these catch the classes of bug that throughput
numbers will not surface:

```
cargo test --release --lib checksum_matches_reference     # full size×pattern cross-check vs slow reference
cargo test --release --lib checksum_self_inverse          # RFC 1071 identity (sum + complement = !0)
cargo test --release --lib checksum_odd_byte_is_padded_zero
cargo test --release --lib checksum_pinned_values         # platform-independent pinned values; BE regression catch
cargo test --release --lib test_paws_rejects_older_tsval
cargo test --release --lib test_paws_accepts_newer_tsval
```

What each guards:

- `checksum_pinned_values` — the canary for the `cfg(target_endian)` split in
  `wire::ip::checksum::data`. Will fail on a big-endian host immediately if
  the swap_bytes block is broken. Tested numerical values are
  platform-independent by definition.
- `checksum_matches_reference` — every size in a representative sweep, every
  pattern in a representative set, cross-checked against a slow obvious
  big-endian u16-sum reference. Caught a real carry bug during initial
  development.
- `checksum_self_inverse` — closes the loop: build a packet, append its
  checksum, verify that the total checksum of the whole thing is `!0`.
- `checksum_odd_byte_is_padded_zero` — `data(&[b]) == data(&[b, 0])` for all
  byte values. Guards the odd-tail handling path.
- PAWS tests — guard the segment-acceptance check in `Socket::process`
  against future refactors that would re-introduce silent acceptance of
  replayed/wrapped segments.

Do not delete these without an explicit justification in the same commit.

## 8. Harness tuning knobs

Constants in `examples/profile_loopback.rs` that change what's measured. Edit,
rebuild, rerun:

- `LAT_SAMPLE_EVERY` — latency sampling rate. Lower → more samples but more
  `clock_gettime` cost per loop iteration; higher → sparser histogram but
  cleaner throughput.
- `REF_CPU_GHZ` — reference frequency for the "estimated cycles" column.
  Adjust to match the host you're measuring if that column matters.
- `Histo::SUBBUCKET_BITS` — log-linear histogram resolution. More
  sub-buckets → finer percentile granularity, more memory.
- The per-shape `BUF` constant — per-socket rx/tx buffer size. Sweep to study
  the per-flow memory-cost curve.
- The `Lane::new(mtu, depth)` `depth` argument — paired-device queue depth.
  Too small → starvation; too large → hides back-pressure problems.

The harness has no runtime config; these are the dials.

## 9. Consumer pinning policy

Consumers should pin via `rev = "<sha>"` or `tag = "<tag>"`, never
`branch = "main"`. The Cargo manifest is the version-of-record; `branch =`
hides the actual commit in `Cargo.lock`, where it silently drifts on
`cargo update`.

## 10. Bug routing

- Reproduces on upstream → file at upstream's issue tracker. When the fix
  lands, drop the local copy on the next sync.
- Reproduces only on this fork → file in this repo's tracker. Bisect to a
  specific commit.
- Profiling harness or sizecheck-test bug → file in this repo.

## 11. Out of scope

Things deliberately not addressed in this fork. Don't sink time here without
re-litigating scope:

- **Explicit AVX-512/NEON intrinsics for checksum.** Auto-vectorization is
  sufficient on every target tested; explicit intrinsics add
  runtime-feature-detection complexity for marginal further gain.
- **Zero-copy RX/TX.** Would require breaking the `RxToken::consume(&[u8])`
  signature; that's an upstream redesign, not a fork-scope change.
- **Multi-core sharding.** The `Interface` type is `!Sync` by design; sharding
  is the consumer's responsibility.
- **RFC 6528 ISN generation.** Security hardening, not behavioural
  correctness. Consumers can plug in a stronger `rand` source via
  `Config::random_seed`.
- **Async `Device` trait.** The sync trait composes cleanly with
  consumer-side async drivers; an async trait would fragment the ecosystem.
- **Asymmetric `ChecksumCapabilities` (TX-only / RX-only).** Useful for
  hardware-checksum-offload scenarios; would require a new variant on the
  enum. PR upstream rather than carry locally.

## 12. Upstreaming policy

Some commits are PR candidates for upstream; others are not. Rough category
map for triage:

| Category | Upstreamability |
|---|---|
| Wire-layer perf (vectorized checksum, single-buffer pseudo-header) | Yes; low-controversy, file as perf PRs. |
| Bug fixes (big-endian checksum, IPv6 zero-csum reject, BPF length) | Yes; file as bug-fix PRs with the failing case. |
| RFC compliance (PAWS, IW10, rwnd-shrink) | Yes; file with RFC citation. |
| Phy hardening (panic → log+drop) | Likely; brief design discussion. |
| Field-type shrink (`usize → u32`) | Low-controversy; easy PR. |
| Static-dispatch wrappers on `AnyController` | Touches trait surface; needs maintainer buy-in. |
| Profiling harness | Maintainer-discretion; scope discussion. |
| Sizecheck diagnostic | Skip — diagnostic, not a behavioral test. |

When a commit is accepted upstream, drop it from this fork on the next sync.
