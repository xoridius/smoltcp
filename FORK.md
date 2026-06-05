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
cargo run --release --example profile_loopback -- --mode bench <shape> <seconds> [offload]
```

`--mode bench` is the default and prints steady-state benchmark metrics.
`--mode trace` keeps the workload shape stable for Instruments capture and
disables periodic RSS sampling so the trace is not polluted by polling.

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
cargo run --release --example profile_loopback -- --mode bench many_tcp <seconds> <N>
cargo run --release --example profile_loopback -- --mode bench many_tcp_fair <seconds> <N>
cargo run --release --example profile_loopback -- --mode bench many_udp <seconds> <N>
```

Sweep N to characterize scaling:

```
for n in 50 100 200 500 1000 2000; do
  cargo run --release --example profile_loopback -- --mode bench many_tcp 5 $n 2>&1 | \
    grep -E "throughput \(app|Jain|verdict|RSS verdict"
done
```

Report fields to read:

| Field | Meaning |
|---|---|
| `throughput (app)` | Aggregate app-visible Gbps / MB/s. |
| `per-packet` | ns + estimated cycles per packet at the harness's reference frequency. |
| `poll-cycle latency: p50 / p99` | Tail latency of a single `Interface::poll` invocation. |
| `Jain` | Per-flow fairness index. `many_tcp_fair` is the deterministic TCP fairness signal; `many_tcp` is a high-throughput stress shape. |
| `verdict` | Single-line pass/fail style summary for fairness + starvation. |
| `RSS verdict` | `bounded` or `GROWTH`. GROWTH means the median RSS over the run is materially smaller than the final RSS — leak suspect. |
| `net heap delta` | Should be a small constant. Non-constant values mean smoltcp itself allocated on the hot path → bug. |
| `lane stats` | Harness packet-pool health. Trace-mode performance claims require `fallback allocs == 0`. |

### 4.2.1 Dynamic-buffer / multi-thread shapes

Three shapes that require `--features socket-tcp-dynamic-buffer`. They
exercise the pool-backed dynamic-buffer paths (§14) under workloads
that the legacy `many_tcp` / `many_udp` shapes don't cover.

```
# Multi-Interface pool contention: N threads, M flows each, shared MemoryPool.
cargo run --release --example profile_loopback --features socket-tcp-dynamic-buffer \
  -- --mode bench multi_tcp <seconds> <n_threads> <flows_per_thread>

# One-way dynamic-buffer TCP sink. Uses `Socket::send` / `Socket::recv`
# closures to reduce app-side copy pressure relative to `multi_tcp`.
cargo run --release --example profile_loopback --features socket-tcp-dynamic-buffer \
  -- --mode bench multi_tcp_sink <seconds> <n_threads> <flows_per_thread>

# Connection churn: open/close at the target rate; verifies pool refund
# accounting under high lifecycle pressure.
cargo run --release --example profile_loopback --features socket-tcp-dynamic-buffer \
  -- --mode bench churn <seconds> <conn_per_sec>

# Mixed idle + active: many idle sockets + few hot ones.
cargo run --release --example profile_loopback --features socket-tcp-dynamic-buffer \
  -- --mode bench idle_hot <seconds> <n_idle> <n_active>
```

What each catches:

| Shape | What's measured | What a regression looks like |
|---|---|---|
| `multi_tcp` | copy-heavy dynamic-buffer TCP echo throughput, per-thread Jain across `MemoryPool` contention | `Jain < 0.95`, nonzero lane fallback allocations in trace mode, or a large host-local throughput drop versus the previous isolated baseline |
| `multi_tcp_sink` | one-way dynamic-buffer TCP throughput with direct send/recv closures | nonzero lane fallback allocations in trace mode, lower throughput than `multi_tcp` without a trace-backed explanation, or no reduction in copy/memmove pressure |
| `churn` | open+close rate sustained, `pool used` returns to 0, `net heap delta` bounded | `pool used (end) > 0` (leaked reservations); `net heap delta` growing with rate (allocator-on-hot-path) |
| `idle_hot` | `pool used post-create == 0` for idle flows; steady-state pool = N_active × 2 sockets × MAX_BUF | non-zero charge from idle sockets (lazy alloc broken); active flows can't reach max (grow policy broken) |

In `--mode trace`, the lane packet pool is strict: if a shape exhausts its
prebuilt lane packets the run fails instead of silently allocating fallback
packets. Treat trace evidence as usable only when the printed lane stats show
`fallback allocs: 0`.

### 4.3 Configuration variants worth measuring

**Checksum offload:**

```
cargo run --release --example profile_loopback -- --mode bench udp 5          # software checksums
cargo run --release --example profile_loopback -- --mode bench udp 5 offload  # offload
```

The delta is the all-in checksum cost. Useful as a ceiling number. `offload`
mode is only safe when both peers ignore checksums (e.g., a loopback
benchmark). Real deployments whose peer is a kernel TCP stack must NOT
enable this — kernel strict-checksum validation will drop every reply.

Dynamic-buffer readiness and trace comparisons use the default software
checksum path. Checksum/offload behavior is intentionally excluded from those
pass/fail claims unless a separate checksum-specific run is explicitly named.

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
cargo run --release --example profile_loopback -- --mode bench all 10
cargo run --release --example profile_loopback -- --mode bench all 10 offload

# Scaling sweep
for n in 100 500 1000; do
  cargo run --release --example profile_loopback -- --mode bench many_tcp 10 $n
  cargo run --release --example profile_loopback -- --mode bench many_udp 10 $n
done
```

## 5. CPU profiling

### 5.1 perf

```
PROFILE_DIR=$(mktemp -d)
perf record -F 999 --call-graph dwarf -o "$PROFILE_DIR/prof.data" \
  target/release/examples/profile_loopback udp 5
perf report -i "$PROFILE_DIR/prof.data" --no-children --stdio --percent-limit 1
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

### 5.1.1 macOS Instruments trace analysis

Use direct binary launch for capture, then analyze every `.trace` bundle with
Instruments' Summary, Call Tree, and System Trace tables. Do not use ad-hoc
`xctrace export --xpath`, XML parsing, or grep-based trace analysis.

Build once:

```
cargo build --release --example profile_loopback \
  --features socket-tcp-dynamic-buffer

BIN=target/release/examples/profile_loopback
```

CPU Profiler or Time Profiler for hot functions. Capture both the copy-heavy
echo workload and the one-way sink workload when evaluating copy pressure:

```
TRACE_DIR=$(mktemp -d)
TRACE="$TRACE_DIR/smoltcp-multi-tcp-cpu.trace"
xcrun xctrace record --template "CPU Profiler" --time-limit 15s \
  --output "$TRACE" --target-stdout - \
  --launch -- "$BIN" --mode trace multi_tcp 5 4 30

TRACE="$TRACE_DIR/smoltcp-multi-tcp-sink-cpu.trace"
xcrun xctrace record --template "CPU Profiler" --time-limit 15s \
  --output "$TRACE" --target-stdout - \
  --launch -- "$BIN" --mode trace multi_tcp_sink 5 4 30
```

After capture, record the trace summary, top function hotspots, and a call
tree or flamegraph view.

If a Time Profiler capture has samples but no symbolized function rows for a
raw CLI, capture a supplemental `"CPU Profiler"` trace with the same
`--mode trace` workload. Keep the Time Profiler bundle for the required
sample timeline, but quote function-level hotspots only from a symbolized
trace.

System Trace for scheduler/syscall/thread-state evidence:

```
TRACE_DIR=$(mktemp -d)
TRACE="$TRACE_DIR/smoltcp-multi-tcp-system.trace"
xcrun xctrace record --template "System Trace" --time-limit 12s \
  --output "$TRACE" --target-stdout - \
  --launch -- "$BIN" --mode trace multi_tcp 5 4 30

TRACE="$TRACE_DIR/smoltcp-multi-tcp-sink-system.trace"
xcrun xctrace record --template "System Trace" --time-limit 12s \
  --output "$TRACE" --target-stdout - \
  --launch -- "$BIN" --mode trace multi_tcp_sink 5 4 30
```

After capture, record summary, thread-state, context-switch, syscall, and
virtual-memory evidence.

For System Trace rate math, divide event counts by `data_window_seconds`
from `summary`, not the requested `--time-limit`; the template can record
in Windowed mode and retain only the final event window.

Allocations / Leaks require re-signing raw Cargo-built CLIs with
`get-task-allow` after each build:

```
TRACE_DIR=$(mktemp -d)
ENTITLEMENTS="$TRACE_DIR/task-allow.plist"
cat > "$ENTITLEMENTS" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>com.apple.security.get-task-allow</key><true/>
</dict></plist>
EOF
codesign --force --sign - --entitlements "$ENTITLEMENTS" "$BIN"

TRACE="$TRACE_DIR/smoltcp-alloc.trace"
xcrun xctrace record --template "Allocations" --time-limit 10s \
  --output "$TRACE" --target-stdout - \
  --launch -- "$BIN" --mode trace many_tcp 8 200
```

After capture, record the trace summary and allocation table grouped by
category or call site.

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
cargo flamegraph --example profile_loopback -- --mode bench udp 5
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

`many_*`, `churn`, and `idle_hot` additionally print an RSS/footprint
trajectory sampled every ~250 ms in `--mode bench`. On Linux this reads
`/proc/self/status`; on macOS it uses Mach task VM info. `--mode trace`
disables those periodic samples so Instruments captures are cleaner.

Interpretation rules:

- **`net heap delta` should be a small constant**. Anything else means
  smoltcp itself allocated on the hot path — a regression to investigate.
- **`RSS verdict: bounded`** when the final RSS is within ~1.5× the median.
  `GROWTH` flags a possible leak; drop into massif/heaptrack to confirm.
- **`bytes allocated ≈ bytes freed`** in steady state. A persistent imbalance
  means a buffer that isn't returning to its pool, a held reference, or a
  growing data structure.

### 6.2 massif

```
MASSIF_DIR=$(mktemp -d)
valgrind --tool=massif --pages-as-heap=no \
  --massif-out-file="$MASSIF_DIR/massif.out" \
  target/release/examples/profile_loopback udp 2
ms_print "$MASSIF_DIR/massif.out" | less
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
cargo run --release --example profile_loopback --features dhat-heap -- --mode bench many_tcp 3 100
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
cargo run --release --example profile_loopback -- --mode bench many_tcp 5 1000 2>&1 | \
  grep -E "net heap delta|allocation count"
```

Expect a small constant `net heap delta` and an `allocation count` whose
magnitude tracks the number of `MemTrace::maybe_sample` calls in bench
mode. Materially higher values indicate something is allocating per
packet — usually a `Vec::with_capacity` or `Bytes::from(Vec)` introduced
in the hot path.

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
property tests miss, including parser edge cases such as 6LoWPAN, IPsec AH,
and IPv6 loopback handling.

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

Record current host baselines from 10-second isolated runs instead of carrying
stale headline numbers in this document.

| Shape | Primary use |
|---|---|
| `udp` | software-checksum packet-forwarding throughput and Mpps. |
| `small` | TCP state-machine overhead on small segments. |
| `pingpong` | latency-bound request/response behavior. |
| `firehose` | one-way TCP bulk transfer; useful for ratios because both peers are smoltcp and cwnd dynamics dominate. |

Cubic and Reno raise the `pingpong` RTT/s count on short handshakes
because of the IW10 first-RTT ramp; bulk shapes barely move.

### 13.3 Scaling and fairness (`many_tcp` / `many_tcp_fair` / `many_udp`)

Use separate shapes for separate claims:

| Shape | Evidence role | Gate |
|---|---|---|
| `many_tcp` | High-throughput TCP stress, memory growth, and starvation discovery. | zero-flow count must stay 0; RSS verdict should be `bounded`; Jain is diagnostic because the hot loop intentionally favors throughput. |
| `many_tcp_fair` | Deterministic TCP fairness. One flow gets one bounded send/drain opportunity per round, and the start flow rotates each round. | Jain ≥ 0.95, zero-flow count 0, RSS bounded, lane fallback allocs 0 in trace mode. |
| `many_udp` | UDP control shape without TCP flow-control or cwnd effects. | Jain should be 1.00 or close to it; RSS bounded. |

RSS verdict ≠ `bounded` is a leak or unbounded buffer growth. Nonzero lane
fallback allocations in trace mode mean the harness pool, not smoltcp, polluted
the trace; do not quote that run as performance evidence.

### 13.3.1 Dynamic-buffer / multi-thread shapes

Do not keep fixed Gbps or RSS headline numbers here unless they come from
current isolated runs on the same host and revision. The durable gates are:

| Shape | Durable gate |
|---|---|
| `multi_tcp` | per-thread Jain ≥ 0.95, bounded pool CAS retries, `pool used (end)` returns to 0, trace-mode lane fallback allocs 0. |
| `multi_tcp_sink` | same pool and lane gates as `multi_tcp`; compare Time/CPU Profiler hotspots against `multi_tcp` for lower app-side copy pressure before claiming a copy win. |
| `churn` | achieved close rate tracks target rate, `pool used (end) == 0`, net heap delta does not grow with connection rate. |
| `idle_hot` | `pool used post-create == 0`; steady pool charge is proportional only to active client/server sockets; `n_active=0` is valid and should keep steady pool use at 0. |

Sub-linear scaling at higher thread counts is expected once every worker is
CPU-bound. Jain < 0.95 across threads suggests `MemoryPool` contention or host
scheduling noise; confirm with System Trace before changing pool internals.

### 13.3.2 Context switches

On Linux, every shape prints `voluntary` / `nonvoluntary`
context-switch counts read from `/proc/self/status`. Voluntary means a
thread blocked or yielded; nonvoluntary means the OS scheduler preempted
a running thread. Both should stay tiny in the spin-loop designs.

On macOS, do not emulate `/proc` in the harness. Use System Trace analysis
(§5.1.1) and read `threads`, `context-switches`, `syscalls`, and
`virtual-memory`; use `data_window_seconds` from `summary` for rates.

Nonvoluntary growth at thread counts ≤ core count is a regression signal
to investigate with System Trace: look for syscalls, blocking thread
states, allocator activity, or a synchronization primitive outside the
lock-free `MemoryPool` CAS path.

**Pool CAS retries.** `multi_tcp` also reports `pool CAS retries:` —
the count of `compare_exchange_weak` failures observed across all
threads, surfaced via `MemoryPool::cas_retries()`. Bounded and
sub-linear in thread count is the gate: the pool counter shouldn't
become a serialization point.

| Threads × flows | Aggregate Gbps | Retries (5 s) | Per-thread/s | Jain |
|---:|---:|---:|---:|---:|
| 4 × 30 | 20.5 | 24 | 1.2 | 0.9999 |
| 8 × 30 | 20.5 | 46 | 1.1 | 0.9973 |
| 16 × 30 | 20.2 | 79 | 1.0 | 0.9935 |
| 32 × 30 | 19.7 | 57 | 0.4 | 0.9916 |

Retries stay near 1 per thread per second even with 8× CPU
oversubscription. The pool counter is a successful lock-free
synchronization primitive at this contention level.

### 13.3.2.1 Atomic-operation surface per shape

Hot-path atomics that fire in each shape, listed by emitter. None of
these is per-packet; rates are per-allocation or per-sample event.

| Shape | `CountingAlloc` `fetch_add` (= alloc/free count) | `MemoryPool` charge/refund | `MemoryPool` CAS retries | `MemTrace` RSS/footprint samples |
|---|---:|---:|---:|---:|
| `udp` | ~14 (setup-dominated) | 0 (legacy buffers) | 0 | ~4 / sec |
| `small` | ~7 | 0 | 0 | ~4 / sec |
| `pingpong` | ~7 | 0 | 0 | ~4 / sec |
| `firehose` | ~7 | 0 | 0 | ~4 / sec |
| `many_tcp` N=100 | ~110 (socket + lane setup) | 0 (uses `Socket::new`) | 0 | ~4 / sec |
| `many_udp` N=50 | ~60 | 0 | 0 | ~4 / sec |
| `multi_tcp` N=4×30 | per-thread similar | yes (grow per flow) | ~24 over 5 s | ~4 / sec |
| `churn` 1000/s | ~20K over 5 s | yes (per open/close) | 0–dozens | ~4 / sec |
| `idle_hot` 500+5 | ~85 over 5 s | yes (5 active grow to max) | 0–dozens | ~4 / sec |

All counters are `Relaxed`. The `MemoryPool` and `CountingAlloc`
both use `AtomicUsize::fetch_add` / `compare_exchange_weak` with the
weakest sound ordering. Single-thread shapes pay one uncontended
locked RMW per event (~3 ns on x86); multi-thread shapes pay the
CAS-retry rate from §13.3.2.

### 13.3.3 Per-shape CPU cost map (cachegrind + callgrind)

`perf` isn't always available; in those environments `valgrind`'s
simulated cache/branch model gives a stable cross-shape baseline.
Numbers below are from 1-second runs on x86_64; valgrind slows each
run ~30–50×, but the per-iteration ratios stay representative.

```
valgrind --tool=cachegrind --cache-sim=yes --branch-sim=yes \
  target/release/examples/profile_loopback <shape> 1
```

| Shape | I refs | D refs (R/W) | Branches (cond/ind) | Mispred | LL miss |
|---|---:|---:|---:|---:|---:|
| `udp` | 264 M | 74 M / 41 M | 24.3 M / 1.4 M | 1.9 % | 0.0 % |
| `small` | 97 M | 24 M / 16 M | 12.2 M / 1.3 M | 2.4 % | 0.0 % |
| `pingpong` | 73 M | 18 M / 12 M | 9.1 M / 0.9 M | 2.5 % | 0.0 % |
| `firehose` | 154 M | 44 M / 30 M | 21.4 M / 2.0 M | 1.3 % | 0.0 % |
| `many_tcp N=50` | 243 M | 59 M / 30 M | 40.2 M / 1.9 M | 0.7 % | 0.0 % |
| `many_udp N=50` | 162 M | 40 M / 28 M | 21.3 M / 0.9 M | 0.6 % | 0.0 % |
Run `dynbuf_memcompare legacy <N>` and `dynbuf_memcompare dynamic <N>` as
separate cachegrind processes if you need call-site cost data for the
memory comparison. Do not use `both` mode as numeric evidence.

Interpretation:
- LL miss rate at 0.0 % everywhere → working set fits L3. Regressions
  that push this above 0.5 % indicate a per-flow data-structure bloat.
- Branch mispred at ~1–3 %, lower on bulky / many-flow shapes (more
  predictable instruction streams). Regressions above 5 % point at a
  hot-path conditional whose direction is unpredictable.
- Per-shape ratio of D refs to I refs: ~0.35–0.45 (typical for byte-
  stream processing). Pingpong runs cooler because handshake control
  flow dominates and payload copies are small.

**IPC, per shape.** Every shape's `Report` now prints `wire packets:`
(the count for that run). Divide cachegrind's `I refs` by the
cachegrind run's `wire packets` to get I/pkt; the native run reports
`cycles/pkt` directly; IPC = I/pkt ÷ cycles/pkt.

| Shape | I/pkt (cg) | cycles/pkt (native) | IPC |
|---|---:|---:|---:|
| `udp` | 4027 | 887 | **4.54** |
| `pingpong` | 4310 | 758 | **5.69** |
| `many_tcp N=50` | 4730 | 859 | **5.51** |
| `many_udp N=50` | 4189 | 824 | **5.09** |
| `small` | 9869 | 689 | 14.3 ⚠️ |
| `firehose` | 157535 | 3.17 M | 0.05 ⚠️ |

The four well-behaved shapes (udp, pingpong, many_tcp, many_udp) land
between **4.5 and 5.7 IPC**, the "well-pipelined, well-vectorized"
zone on a modern x86 core (peak ~6 on Zen3 / Golden Cove). Most of
the remaining cycles are memory-bandwidth-bound on the `__memcpy` +
`checksum::data` hot paths — confirmed by the callgrind tops above
and by cachegrind's 0.0 % LL miss rate (working set fits in L3; the
wait isn't main-memory, it's L1/L2 fill cycles).

⚠️ Two shapes don't yield clean IPC numbers, for understood reasons:

- **`small`** — TCP state machine is sensitive to the ~30× slowdown
  valgrind introduces. Under cachegrind the ACK / retransmit cadence
  shifts, so the per-packet instruction count and the native
  per-packet cycle count aren't measuring the same workload. The
  number above (14.3) is artifact, not real IPC.
- **`firehose`** — cwnd-bounded; most native cycles are spent in
  idle-spin waiting for ACKs, not in productive packet work. The TSC
  delta divides over a small wire-packet count, inflating cycles/pkt
  to the millions. Real-IPC-of-productive-work is closer to the
  others; we just can't isolate it without sampling-profiler PMU
  data (`perf stat`), which this container doesn't expose.

Callgrind top symbols per shape (collected via `callgrind_annotate`,
filtered to `*.rs:smoltcp::*` + `libc memcpy`). Each shape lists the
top 4–6 cost centers; the pattern is consistent — payload moves
(`__memcpy`), the vectorized checksum, and the per-socket state
machine dominate every shape:

**`udp`** — 528 M I total
- 27 % `smoltcp::wire::ip::checksum::data` (split across multiple inlining sites)
- 17 % `__memcpy_avx_unaligned_erms`
- 3 % `RingBuffer::dequeue_one_with`
- 3 % `PacketBuffer::enqueue`
- 2.5 % `process_ipv4`
- 2 % `Interface::poll`

**`small`** — 186 M I total
- 4.5 % `__memcpy_avx_unaligned_erms`
- 3.9 % `Socket::seq_to_transmit`
- 3.6 % `Interface::poll`
- 3.6 % `RingBuffer::enqueue_slice`
- 2.5 % `Socket::window_to_update`
- 2.3 % `checksum::data`

**`pingpong`** — 195 M I total
- 4.2 % `__memcpy_avx_unaligned_erms`
- 3.8 % `checksum::data` (wire/ip.rs)
- 2.4 % `wire::tcp::Repr::parse`
- 2.3 % `process_ipv4`
- 2.2 % `Socket::seq_to_transmit`
- 2.0 % `Socket::dispatch`

**`firehose`** — 169 M I total
- 5.8 % `Interface::poll` (state-machine dominated; cwnd dynamics mean little payload moves)
- 4.0 % `Socket::window_to_update`
- 3.1 % `Interface::poll_maintenance`
- 2.7 % `egress_permitted`
- 2.3 % `__memcpy_avx_unaligned_erms`

**`many_tcp N=50`** — 243 M I total
- 3.9 % `Socket::accepts` (per-socket dispatch dominates at 50-flow scale)
- 3.4 % `checksum::data`
- 3.0 % `__memcpy_avx_unaligned_erms`
- 2.2 % `Interface::poll`
- 2.1 % `process_tcp`
- 2.1 % `wire::tcp::Repr::parse`

**`many_udp N=50`** — 215 M I total
- 4.3 % `RingBuffer::dequeue_one_with`
- 4.2 % `process_udp`
- 4.1 % `__memcpy_avx_unaligned_erms`
- 3.9 % `checksum::data`
- 3.2 % `Interface::poll`
- 3.0 % `udp::Socket::accepts`

The top-of-profile shape (`checksum::data` + `__memcpy` + per-socket
state machine) is uniform across every shape — exactly what FORK.md
§5.1 predicts. The dynamic-buffer feature's symbols (`try_grow_*`,
`release_dyn_buffers`, `MemoryPool::*`) **do not appear in the top 20
of any shape's profile** — `#[cold]` placement working as designed.

### 13.3.4 Per-callsite heap attribution (dhat)

```
cargo build --release --example profile_loopback \
  --features socket-tcp-dynamic-buffer,dhat-heap
target/release/examples/profile_loopback --mode bench many_tcp 3 100
# Reads dhat-heap.json
```

Top callsites by total bytes allocated (lifetime, not steady-state):

| Bytes | Callsite |
|---:|---|
| 4.80 MB | `profile_loopback::Packet::with_capacity` (lane buffer pool — 1500 B × queue depth × 2 lanes) |
| 1.64 MB | `profile_loopback::add_tcp_socket` (100 sockets × 16 KB rx+tx) |
| 258 KB | `smoltcp::iface::socket_set::SocketSet::add` (slab growth) |
| 205 KB | `profile_loopback::Lane::new` (VecDeque + pool Vec) |
| 61 KB | `profile_loopback::rss_bytes` (periodic bench-mode RSS/footprint samples) |
| 8 KB | `profile_loopback::ctxsw_counts` (Linux-only `/proc/self/status` reads) |

Smoltcp internals (`process_ipv4`, `dispatch_ip`, `Interface::poll`,
…) **do not appear** — confirming the "smoltcp itself does not
allocate on the hot path" invariant (FORK.md §6.5). Hot-path
allocations live entirely in the harness's lane pool and in bench-mode
sampling instrumentation.

### 13.4 Struct footprint (`sizecheck`)

`size_of::<tcp::Socket>` on a 64-bit host:

| Feature set | Size |
|---:|---:|
| default | 464 B |
| `socket-tcp-dynamic-buffer` | 472 B |
| `socket-tcp-reno` | 488 B |
| `socket-tcp-cubic` | 512 B |

The 5 scattered `bool` fields (`rx_fin_received`, `remote_last_win_unscaled`,
`remote_has_sack`, `nagle`, `synack_paused`) are packed into a single
`flags: u8`. This recovers ~8 B of inter-field padding the compiler would
otherwise emit between bools, so enabling `socket-tcp-dynamic-buffer`
(which adds an `Option<Box<DynBufState>>` = 8 B field) lands at the same
size as default *was* before the pack. Read/write goes through small
`#[inline(always)]` accessors that compile to a single AND-with-mask
plus a branch (read) or AND/OR (write) — same hot-path cost as the
original bool field access.

These move on any field-type or congestion-controller field change.
Record the new values in the commit that moves them.

### 13.5 Allocator state in steady state

Across every shape:

- `net heap delta`: small constant, dominated by harness sampling in
  bench mode. Materially larger → smoltcp itself is allocating on the
  hot path.
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
  XNU `tcp_sbrcv_grow`), and release only after pending ACK/dequeue/RST
  work no longer needs the queues, plus `reset`/`Drop`.

### 14.2 Canonical patterns mirrored

| Pattern | Kernel | Here |
|---|---|---|
| Limit, not reservation | XNU `sbreserve` (no alloc); Linux `sk_rcvbuf` cap | `rx_max`/`tx_max` |
| Global accounting | Linux `tcp_mem` low/pressure/high | `MemoryPool.budget` |
| Lazy alloc on pressure | Linux `tcp_data_queue` charges on arrival; XNU mbuf chain | `try_grow_rx` at dispatch |
| Pressure → window collapse | Linux `__tcp_select_window` zero | grow refuses → backpressure |
| Pressure-tier autotune throttle | Linux `tcp_under_memory_pressure(sk)` gates `tcp_rcv_space_adjust` | `MemoryPool::under_pressure` (75% threshold) forces linear growth |
| Geometric grow | Linux `copied << 1`; XNU ×2/×4 | `max(cur+chunk, cur×2)` |
| Release on safe close/reset points | Linux `tcp_done` returns `sk_forward_alloc` | release after ACK/dequeue/RST work has completed and queues are empty |
| Fallible alloc | n/a (kernel context) | `Vec::try_reserve_exact` |

### 14.3 Cost when feature is **off**

Zero. The `dyn_state` field, the new module, all hooks — all
`#[cfg(feature = "socket-tcp-dynamic-buffer")]`-gated.

### 14.4 Cost when feature is **on** but not used (legacy API)

- Legacy sockets still use `Socket::new(rx_buf, tx_buf)`; dynamic
  buffers are opt-in through `Socket::new_dynamic`.
- The legacy hot path should show no dynamic-buffer growth or release
  frames in Time Profiler. Verify with the recipe in §5.1.1 and quote
  only numbers from isolated `--mode bench` runs.
- If measuring feature-on/off overhead, build two release binaries
  from the same revision and run the same shape in separate processes:

```
cargo build --release --example profile_loopback
target/release/examples/profile_loopback --mode bench udp 10

cargo build --release --example profile_loopback \
  --features socket-tcp-dynamic-buffer
target/release/examples/profile_loopback --mode bench udp 10
```

Do not keep old feature-overhead percentages in this document unless
they are backed by current isolated runs and a matching trace showing
where the cost comes from.

### 14.5 Cost when feature is **on** and used (`new_dynamic`)

- Per-flow steady state: `Vec<u8>` per buffer sized to current
  capacity (between `initial` and `max`).
- Public `listen()` / `connect()` preserve nonzero `rx_initial` and
  `tx_initial` after the internal reset that opens a new connection.
- `can_send()` reports true for zero-initial dynamic TX buffers when
  the buffer can grow under the socket and pool limits.
- Growth path: amortized O(rx_max) total memcpy across O(log(rx_max))
  steps. Geometric.
- Atomic CAS on each grow attempt (pool charge) and on each refund.
  Single-thread per Interface; multi-Interface contention rare.

### 14.6 Memory savings (idle flows)

Measure legacy and dynamic idle sockets as separate processes. The
convenience `both` mode is useful for smoke checks, but allocator state
from the first phase can affect the second phase's RSS and must not be
used as evidence.

```
cargo run --release --example dynbuf_memcompare \
  --features socket-tcp-dynamic-buffer -- legacy 1000
cargo run --release --example dynbuf_memcompare \
  --features socket-tcp-dynamic-buffer -- dynamic 1000
cargo run --release --example dynbuf_memcompare \
  --features socket-tcp-dynamic-buffer -- both 1000   # smoke only
```

Expected invariant: dynamic idle sockets with `rx_initial = tx_initial = 0`
charge 0 bytes to the `MemoryPool`; fixed legacy sockets pay their full
rx+tx buffer allocation at construction.

### 14.7 Test matrix additions

Run alongside §3:

```
cargo test --release --lib --features socket-tcp-dynamic-buffer
cargo test --release --lib --no-default-features \
  --features "alloc,medium-ethernet,proto-ipv4,proto-ipv6,socket-raw,socket-udp,socket-tcp,socket-icmp,proto-ipv6-slaac,socket-tcp-dynamic-buffer"
cargo +nightly miri test --lib --features socket-tcp-dynamic-buffer socket::tcp::test::dyn_buf
cargo run --release --example dynbuf_memcompare --features socket-tcp-dynamic-buffer -- legacy 1000
cargo run --release --example dynbuf_memcompare --features socket-tcp-dynamic-buffer -- dynamic 1000
```

### 14.8 Upstream-sync surface

Touched files:

- `Cargo.toml` — feature decl, example registration.
- `src/storage/ring_buffer.rs` — crate-internal `try_grow` + `release_owned`
  (alloc-gated, appended).
- `src/socket/tcp.rs` — module decl, struct field, `new_dynamic`,
  grow/release helpers, hooks in `listen`/`connect`/`dispatch`/`process`/
  `send_impl`/`recv_slice`/`reset`. All
  `#[cfg(feature = "socket-tcp-dynamic-buffer")]`-gated.
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
