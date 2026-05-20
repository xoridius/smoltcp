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

Run all of these before any commit lands on `main`. Each must finish with zero
failures.

```
cargo test --release --lib
cargo test --release --lib --features socket-tcp-cubic
cargo test --release --lib --features socket-tcp-reno

cargo test --release --lib --no-default-features \
  --features "std,medium-ethernet,proto-ipv4,proto-ipv4-fragmentation,socket-raw,socket-dns"

cargo test --release --lib --no-default-features \
  --features "std,medium-ethernet,proto-ipv6,socket-tcp,socket-udp"

cargo test --release --lib --no-default-features \
  --features "alloc,medium-ethernet,proto-ipv4,proto-ipv6,socket-raw,socket-udp,socket-tcp,socket-icmp,proto-ipv6-slaac"

cargo clippy --release --lib --tests
cargo +nightly bench --bench bench
```

The feature-gated rows guard against build-failure regressions in code paths
that only some consumers enable. If clippy newly fires on a path you didn't
touch, fix it rather than allowlisting.

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

### 4.2.1 Dynamic-buffer / multi-thread shapes

Three shapes that require `--features socket-tcp-dynamic-buffer`. They
exercise the pool-backed dynamic-buffer paths (§14) under workloads
that the legacy `many_tcp` / `many_udp` shapes don't cover.

```
# Multi-Interface pool contention: N threads, M flows each, shared MemoryPool.
cargo run --release --example profile_loopback --features socket-tcp-dynamic-buffer \
  -- multi_tcp <seconds> <n_threads> <flows_per_thread>

# Connection churn: open/close at the target rate; verifies pool refund
# accounting under high lifecycle pressure.
cargo run --release --example profile_loopback --features socket-tcp-dynamic-buffer \
  -- churn <seconds> <conn_per_sec>

# Mixed idle + active: many idle sockets + few hot ones.
cargo run --release --example profile_loopback --features socket-tcp-dynamic-buffer \
  -- idle_hot <seconds> <n_idle> <n_active>
```

What each catches:

| Shape | What's measured | What a regression looks like |
|---|---|---|
| `multi_tcp` | aggregate throughput scaling, per-thread Jain across `MemoryPool` contention | `Jain < 0.95` or aggregate throughput drops by >10 % at N=4 threads relative to N=1 |
| `churn` | open+close rate sustained, `pool used` returns to 0, `net heap delta` bounded | `pool used (end) > 0` (leaked reservations); `net heap delta` growing with rate (allocator-on-hot-path) |
| `idle_hot` | `pool used post-create == 0` for idle flows; steady-state pool = N_active × 2 × MAX_BUF | non-zero charge from idle sockets (lazy alloc broken); active flows can't reach max (grow policy broken) |

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

### 6.3.1 dhat — per-callstack heap attribution

The harness has a build-time switch that swaps the global allocator for
`dhat::Alloc`. It writes `dhat-heap.json` on exit; load it in
`https://nnethercote.github.io/dh_view/dh_view.html` for an interactive view.

```
cargo run --release --example profile_loopback --features dhat-heap -- many_tcp 3 100
# inspect quickly without the GUI:
python3 -c "import json; d=json.load(open('dhat-heap.json')); \
  pps=d['pps']; ftbl=d['ftbl']; import re; g={}
[g.__setitem__(m.group(1), g.get(m.group(1),0)+p['tb']) \
  for p in pps for f in [next((x for x in (ftbl[i] for i in p['fs']) \
  if re.search(r'(profile_loopback|smoltcp)::',x)), '')] \
  for m in [re.search(r'((profile_loopback|smoltcp)::\w+(?:::\w+)*)',f)] if m]
[print(f'{b:>12}  {s}') for s,b in sorted(g.items(),key=lambda x:-x[1])[:12]]"
```

Use this when the harness's `net heap delta` flags growth and you need to
know *which* callsite, not just how much. Stricter than CountingAlloc;
slower than baseline (~2× overhead). Don't ship CI on it — run on demand.

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

### 7.1 Coverage-guided fuzzing (`fuzz/`)

The `fuzz/` directory carries libFuzzer harnesses for the wire parsers.
Coverage-guided fuzzing finds the kinds of input-validation bugs that
property tests miss — the codex audit PRs that landed (6LoWPAN, IPsec AH,
IPv6 loopback) were the exact bug class this catches.

```
cargo install cargo-fuzz   # one-time
cargo +nightly fuzz run wire_parsers   -- -max_total_time=120 -max_len=2000
cargo +nightly fuzz run wire_roundtrip -- -max_total_time=60  -max_len=2000
cargo +nightly fuzz run dhcp_header    -- -max_total_time=60  -max_len=2000
cargo +nightly fuzz run ieee802154_header -- -max_total_time=60 -max_len=2000
cargo +nightly fuzz run packet_parser  -- -max_total_time=60  -max_len=2000
```

Target map:

| Target | What it fuzzes |
|---|---|
| `wire_parsers` | IPv4/IPv6, TCP, UDP, IPsec AH, 6LoWPAN ExtHeader, ICMPv4/v6. Parse-only. Discriminates by `data[0] & 0x07`. |
| `wire_roundtrip` | Differential round-trip: parse → emit → re-parse → assert equal. Catches "accepts but emits malformed" drift. |
| `packet_parser` | `PrettyPrinter<EthernetFrame>` end-to-end. Survival check for the pretty-printer. |
| `dhcp_header` | `DhcpPacket::new_checked` + `DhcpRepr::parse` + emit. |
| `ieee802154_header` | `Ieee802154Frame` parse/emit. |

Operational notes:

- Corpus is committed under `fuzz/corpus/<target>/`. Don't let it grow
  unbounded; trim with `cargo fuzz cmin <target>` if it gets large.
- A crash drops a reproducer at `fuzz/artifacts/<target>/crash-<sha>`.
  Convert to a unit test: `cat fuzz/artifacts/.../crash-... | xxd` and
  pin the bytes in the corresponding `wire::*::test` module.
- Run for tens of minutes per parser before treating the result as
  meaningful. New-units-added stalling near zero is the signal that
  coverage has plateaued; bumping `-max_len` or expanding the
  discriminator usually re-opens it.

### 7.2 MIRI

```
cargo +nightly miri test --lib wire
cargo +nightly miri test --lib socket::tcp
```

Detects UB, aliasing violations, out-of-bounds reads, and uninitialised
memory access — the things release builds compile away silently. Runs
~30-50× slower than a normal test; restrict to the `wire` and `socket::tcp`
modules in regular use. Add to a "deep" CI lane, not the default one.

Smoltcp uses very little `unsafe`, so MIRI's value here is mostly
catching slice-arithmetic mistakes in the wire parsers and TCP option
walking — same surface as the property tests, with a different shaped
detector.

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

## 13. Reference measurements

A one-time snapshot from a quiet x86_64 host (governor `performance`,
release mode, default features unless noted). Treat the **ratios** as
durable; absolute numbers will move with CPU, RAM, kernel, and toolchain
version. Use this as a sanity check after a sync or a refactor, not as a
pass/fail oracle.

### 13.1 Wire microbench (`cargo +nightly bench --bench bench`)

| Bench | ns/iter | Notes |
|---|---:|---|
| `bench_checksum_64` | ~3 | Per-call overhead dominates. |
| `bench_checksum_576` | ~15 | Vectorized loop warm. |
| `bench_checksum_1500` | ~36 | ≈40 GB/s. Top headline number. |
| `bench_checksum_1501` | ~37 | Odd-tail path engaged. |
| `bench_checksum_9000` | ~220 | Jumbo. |
| `bench_checksum_65535` | ~1600 | Worst-case single buffer. |
| `bench_emit_ipv4` | ~42 | Includes header checksum. |
| `bench_emit_ipv6` | ~38 | No header checksum. |
| `bench_emit_tcp` | ~38 | Pseudo-header + body in one pass. |
| `bench_emit_udp` | ~36 | Same. |
| `bench_parse_verify_tcp` | ~47 | 1480 B segment, full RX verify. |

Big-ticket regressions to look for: any `checksum_*` row jumping >2× on
`-C target-cpu=native` means the auto-vectorizer stopped engaging
(verify via §5.3). `bench_emit_tcp` rising while `bench_checksum_*`
holds means the pseudo-header consolidation regressed.

### 13.2 End-to-end shapes (`profile_loopback`)

10-second runs on the same host:

| Shape | Throughput (app) | Per-packet | Notes |
|---|---:|---:|---|
| `udp` | ~20–26 Gbps | ~80 ns | Software checksum. |
| `udp offload` | ~31–33 Gbps | ~63 ns | `ChecksumCapabilities::ignored()`. Loopback-only; see §4.3. |
| `small` | ~3 Mpps | ~330 ns | State-machine bound. |
| `pingpong` | ~1.33 M-rtt/s | n/a | Latency-bound. |
| `firehose` | varies | n/a | Cwnd dynamics on both peers; ratios only. |

Cubic and Reno raise the `pingpong` RTT/s count on short handshakes
because of the IW10 first-RTT ramp; bulk shapes barely move.

### 13.3 Scaling and fairness (`many_tcp` / `many_udp`)

| Shape | N | Jain | RSS verdict | Net heap delta |
|---|---:|---:|---|---|
| `many_tcp` | 100 | ≥0.99 | bounded | small constant |
| `many_tcp` | 500 | ≥0.98 | bounded | small constant |
| `many_tcp` | 1000 | ≥0.97 | bounded | small constant |
| `many_udp` | any | 1.00 | bounded | small constant |

Jain < 0.95 at any flow count is a regression: scheduling or socket-pump
ordering. RSS verdict ≠ `bounded` is a leak or unbounded buffer growth.

### 13.3.1 Dynamic-buffer / multi-thread shapes

Reference numbers from a containerized x86_64 host (high noise floor;
ratios more durable than absolute Gbps).

`multi_tcp` aggregate Gbps and Jain across threads:

| Threads × flows/thread | Total flows | Aggregate Gbps | Jain |
|---|---:|---:|---:|
| 2 × 30 | 60 | ~7.5 | ≥0.99 |
| 4 × 20 | 80 | ~11.7 | ≥0.98 |

Sub-linear scaling at 4 threads is expected (each thread is fully
CPU-bound). Jain < 0.95 across threads suggests `MemoryPool` cache-
line contention regressed (verify §14 `#[repr(align(64))]` is intact).

`churn` sustained connections/sec and pool balance:

| Target rate | Achieved | Pool end | Net heap delta |
|---:|---:|---:|---:|
| 500 conn/s | 500 conn/s | 0 KiB | ~256 B |
| 1000 conn/s | 1000 conn/s | 0 KiB | ~256 B |
| 2000 conn/s | 2000 conn/s | 0 KiB | ~256 B |

`pool end != 0` means the refund path leaked (regression to debug at
§14 release sites). `net heap delta` proportional to rate means an
allocator-on-hot-path slipped in (use `dhat` to localize).

`idle_hot` per-flow accounting:

| n_idle + n_active | Pool post-create | Pool steady | Idle/flow |
|---|---:|---:|---:|
| 200 + 10 | 0 KiB | 640 KiB | 0 B |
| 1000 + 0 | 0 KiB | 0 KiB | 0 B |

Non-zero `Pool post-create` is the canary: lazy allocation broken.
`Pool steady` should equal `n_active × 2 × MAX_BUF` exactly.

### 13.4 Struct footprint (`sizecheck`)

`size_of::<tcp::Socket>` on a 64-bit host:

| Feature set | Size |
|---:|---:|
| default | ~464 B |
| `socket-tcp-reno` | ~488 B |
| `socket-tcp-cubic` | ~512 B |

These move on any field-type or congestion-controller field change.
Record the new values in the commit that moves them.

### 13.5 Allocator state in steady state

Across every shape:

- `net heap delta`: small constant, dominated by the harness's periodic
  `/proc/self/status` reads (~1.5 KiB visible). Materially larger →
  smoltcp itself is allocating on the hot path.
- `allocation count`: scales with `MemTrace` sample count (one per
  ~250 ms), not with packet count.

If allocation count tracks packet count instead of wall time, a
`Vec::with_capacity` or boxing has crept into the hot path.

## 14. Dynamic-buffer TCP sockets (`socket-tcp-dynamic-buffer`)

Pool-backed, lazy, resizable rx/tx buffers for TCP. Opt-in. Designed
for memory-constrained hosts that admit many concurrent flows — the
iOS NetworkExtension case (~50 MiB jetsam ceiling, hundreds of TCP
flows). Disabled by default; the legacy `Socket::new(rx_buf, tx_buf)`
API is bit-for-bit unchanged.

### 14.1 What it adds

- `tcp::MemoryPool` — shared `AtomicUsize`-tracked byte budget, the
  smoltcp analogue of Linux `tcp_memory_allocated` against `tcp_mem`.
- `tcp::DynamicBufferConfig` — per-flow `{rx, tx} × {initial, max}` +
  `grow_chunk`. Analogue of `tcp_rmem`/`tcp_wmem`.
- `tcp::Socket::new_dynamic(config, Option<MemoryPool>)` — alternate
  constructor. Buffers start at `initial`, grow geometrically on
  pressure (mirrors Linux `tcp_rcv_space_adjust`/`tcp_sndbuf_expand`,
  XNU `tcp_sbrcv_grow`), and release on `Closed`/`reset`/`Drop`.

### 14.2 Canonical patterns mirrored

| Pattern | Kernel | Here |
|---|---|---|
| Limit, not reservation | XNU `sbreserve` (no alloc); Linux `sk_rcvbuf` cap | `rx_max`/`tx_max` |
| Global accounting | Linux `tcp_mem` low/pressure/high | `MemoryPool.budget` |
| Lazy alloc on pressure | Linux `tcp_data_queue` charges on arrival; XNU mbuf chain | `try_grow_rx` at dispatch |
| Pressure → window collapse | Linux `__tcp_select_window` zero | grow refuses → backpressure |
| Pressure-tier autotune throttle | Linux `tcp_under_memory_pressure(sk)` gates `tcp_rcv_space_adjust` | `MemoryPool::under_pressure` (75% threshold) forces linear growth |
| Geometric grow | Linux `copied << 1`; XNU ×2/×4 | `max(cur+chunk, cur×2)` |
| Release on close | Linux `tcp_done` returns `sk_forward_alloc` | `set_state(Closed)` releases |
| Fallible alloc | n/a (kernel context) | `Vec::try_reserve_exact` |

### 14.3 Cost when feature is **off**

Zero. The `dyn_state` field, the new module, all hooks — all
`#[cfg(feature = "socket-tcp-dynamic-buffer")]`-gated.

### 14.4 Cost when feature is **on** but not used (legacy API)

- `tcp::Socket` grows by **8 bytes** (a `Option<Box<DynBufState>>`):
  472 → 480.
- The hot dispatch path gains a single null-pointer check (≤ 3 cycles)
  before the conditional growth code (which is `#[cold]`).
- Net measured cost: **~2 % UDP throughput** on the `udp` shape from
  binary-layout shift (Socket enum size bound), TCP perf neutral.
  Cannot reduce below ~2 % without separate compilation units.

### 14.5 Cost when feature is **on** and used (`new_dynamic`)

- Per-flow steady state: `Vec<u8>` per buffer sized to current
  capacity (between `initial` and `max`).
- Growth path: amortized O(rx_max) total memcpy across O(log(rx_max))
  steps. Geometric.
- Atomic CAS on each grow attempt (pool charge) and on each refund.
  Single-thread per Interface; multi-Interface contention rare.

### 14.6 Memory savings (idle flows)

Per the `dynbuf_memcompare` example, 32 KiB rx + 32 KiB tx per flow:

| N | Legacy fixed (KiB / flow) | Dynamic idle (KiB / flow) |
|---:|---:|---:|
| 100 | 55.0 | 0.0 |
| 1000 | 55.0 | 0.4 |
| 4000 | 55.0 | 0.5 |

### 14.7 Test matrix additions

Run alongside §3:

```
cargo test --release --lib --features socket-tcp-dynamic-buffer
cargo test --release --lib --no-default-features \
  --features "alloc,medium-ethernet,proto-ipv4,proto-ipv6,socket-raw,socket-udp,socket-tcp,socket-icmp,proto-ipv6-slaac,socket-tcp-dynamic-buffer"
cargo +nightly miri test --lib --features socket-tcp-dynamic-buffer socket::tcp::test::dyn_buf
cargo run --release --example dynbuf_memcompare --features socket-tcp-dynamic-buffer -- 1000
```

### 14.8 Upstream-sync surface

Touched files:

- `Cargo.toml` — feature decl, example registration.
- `src/storage/ring_buffer.rs` — `try_grow` + `release_owned` (alloc-gated,
  appended).
- `src/socket/tcp.rs` — module decl, struct field, `new_dynamic`,
  grow/release helpers, hooks in `dispatch`/`send_impl`/`set_state`/
  `reset`. All `#[cfg(feature = "socket-tcp-dynamic-buffer")]`-gated.
- `src/socket/tcp/dynbuf.rs` — new file, no conflict.
- `examples/dynbuf_memcompare.rs` — new file.

Conflict surface on `git merge upstream/main` is the cfg-gated additions
in `tcp.rs`. Each hook is 4–6 lines, easy to re-apply if upstream
restructures the surrounding code.

### 14.9 Limitations

- O(len) memcpy on each grow step (amortized to O(rx_max) by geometric
  growth). True zero-copy chunked storage would require restructuring
  `RingBuffer`; out of scope.
- No per-RTT BDP autotuner (Linux `tcp_rcv_space_adjust` measures
  bytes copied per RTT and grows proportionally). We use a simpler
  "near-full → grow" trigger. Adequate for the iOS use case.
- No OOO `Assembler` collapse / drop under pressure (Linux
  `tcp_prune_ofo_queue`). `Assembler` is fixed-size, so unbounded
  growth there is impossible anyway.
- Per-flow `rx_max`/`tx_max` is fixed at `new_dynamic`. No setter
  yet for runtime adjustment (analogous to `setsockopt(SO_RCVBUF)`).
