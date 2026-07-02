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

This fork branches from upstream at the **`v0.13.1`** tag — that commit is the
merge-base. The full ancestry is shared with `smoltcp-rs/smoltcp` all the way
back to its first commit; the fork's own work is the commits in
`v0.13.1..HEAD`.

**Shallow-clone gotcha.** CI / cloud / agent checkouts of this repo are often
*shallow*, which truncates local history above `v0.13.1`. In that state
`git merge-base HEAD upstream/main` returns nothing and `git merge` / `git
rebase` onto upstream misbehave — not because the histories are unrelated, but
because the common ancestor is below the fetch horizon. Restore it first:

```
git fetch --unshallow origin     # no-op if already a full clone
git remote add upstream https://github.com/smoltcp-rs/smoltcp.git  # if missing
git fetch upstream
git merge-base HEAD upstream/main # should print the v0.13.1 commit
```

Recommended sync is **cherry-pick**, not a wholesale merge: the local edits to
`src/socket/tcp.rs` and `src/socket/tcp/congestion*.rs` are heavy enough that a
full `git merge upstream/main` conflicts across most of those files. Pull only
the PRs you want:

```
# 1. See what is new upstream and not yet triaged (adds + fetches the
#    `upstream` remote automatically; cross-references §16). Works even in a
#    shallow clone — it only needs the upstream side, which is fetched fully.
tools/upstream-delta.sh

# 2. For each PR marked NEW, on the dev branch, cherry-pick its commits:
git log --oneline v0.13.1..upstream/main      # find the commit SHAs for the PR
git cherry-pick <sha>...                        # or hand-port if it conflicts

# 3. Conflicts cluster in src/socket/tcp.rs and src/socket/tcp/congestion*.rs
#    (the most locally-edited files). Re-apply the local hooks; see §16 for
#    which fork adaptations sit on top of upstream there.

# 4. Run the full test matrix in §3 (+ the harness regression sweep in §4 for
#    anything touching the data path), then record the outcome in §16.
```

Never rebase published commits. Never force-push the published branch. §16
records which post-0.13.1 upstream PRs are already backported or deliberately
skipped — `tools/upstream-delta.sh` flags anything not in that list as `NEW`.

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

cargo test --release --lib --no-default-features \
  --features "alloc,medium-ethernet,proto-ipv4,proto-ipv6,socket-raw,socket-udp,socket-tcp,socket-icmp,proto-ipv6-slaac,socket-tcp-cubic,socket-tcp-reno"

cargo clippy --release --lib --tests
cargo +nightly bench --bench bench
```

The feature-gated rows guard against build-failure regressions in code paths
that only some consumers enable. If clippy newly fires on a path you didn't
touch, fix it rather than allowlisting.

### 3.1 Tooling bootstrap

Linux profiling and fuzz/coverage work assumes the following tools are present
on the profiling host:

```
apt-get update
apt-get install -y valgrind heaptrack heaptrack-gui kcachegrind linux-tools-generic

rustup component add llvm-tools-preview
rustup +nightly component add miri rust-src
cargo install --locked cargo-fuzz cargo-llvm-cov flamegraph samply
```

`linux-tools-generic` provides the packaged `perf`; `valgrind` provides
`ms_print`; `heaptrack-gui` and `kcachegrind` are optional GUI viewers but worth
having on any workstation used for deep profiling. On macOS, use the Instruments
recipes in §5.1.1 instead of the Linux-only tools.

### 3.2 Evidence map

Use this table to choose the first command to run. Detailed interpretation lives
in the referenced sections; keep current numeric results in PRs or release
notes, not as standing values in this file.

| Question | First evidence to collect |
|---|---|
| Did the ordinary library matrix regress? | §3 test matrix. |
| Did loss recovery or congestion control regress? | `./ci.sh netsim` for the stable NoControl sweep (§15), plus targeted TCP tests. Run `netsim_cubic`/`netsim_reno` manually when touching congestion controllers. |
| What is tunnel-like throughput? | `./ci.sh profile-smoke` first, then `profile_loopback --mode bench udp <seconds>` and the same run with `offload` (§4.2, §4.3). |
| Are many flows fair and bounded? | `./ci.sh profile-smoke` first, then `many_tcp_fair`, `many_tcp`, and `many_udp` (§4.2). |
| Are dynamic TCP buffers safe for packet-tunnel memory budgets? | `./ci.sh ios-gate`, then `dynbuf_memcompare` plus `idle_hot` and `churn` (§4.2.1, §14.6). Subtract lane `reserved total`; it is harness memory. |
| Where is CPU time going? | `perf record` / `perf report`, `cargo flamegraph`, or `samply` (§5). |
| Where is heap growth coming from? | Built-in RSS/allocator fields first, then `heaptrack`, Massif, or `dhat-heap` (§6). |
| Is parser hardening still covered? | `./ci.sh fuzz-build`, `./ci.sh fuzz-smoke`, and Miri proof lanes (§7). `fuzz-smoke` defaults to the broad `wire_parsers` target. |
| Did socket footprints change? | `./ci.sh sizecheck` (§4.4). |
| Did codegen for checksum paths change? | Cross-target assembly checks (§5.3). |

### 3.3 Apple behavior reference

For behavior that matters on macOS or iOS, check Apple's XNU source alongside
Linux and upstream smoltcp. Keep the clone out of this repository:

```
XNU_DIR="${TMPDIR:-/tmp}/xnu"
git clone --depth=1 https://github.com/apple-oss-distributions/xnu "$XNU_DIR"
```

Useful starting points:

- TCP receive/send behavior: `bsd/netinet/tcp_input.c`,
  `bsd/netinet/tcp_output.c`, `bsd/netinet/tcp_subr.c`,
  `bsd/netinet/tcp_timer.c`, `bsd/netinet/tcp_var.h`.
- Congestion control: `bsd/netinet/tcp_cc.c`, `tcp_cubic.c`,
  `tcp_newreno.c`, `tcp_rack.c`.
- Socket-buffer and mbuf pressure: `bsd/kern/uipc_socket2.c`,
  `bsd/kern/uipc_mbuf*.c`, `bsd/sys/socketvar.h`.
- BPF/raw-device behavior: `bsd/net/bpf.c`, `bsd/net/bpf*.h`.

When a change is Apple-facing and behavioral, quote the XNU file/function in
the PR or commit notes. Do not copy XNU constants blindly when smoltcp's
fixed-buffer model needs a smaller analogue; record the translation.

### 3.4 Agent/device safety

Default validation for this fork is non-device: cargo, netsim,
`profile_loopback`, fuzz, size, and iOS target checks. Agents must not run
`sudo`, open `/dev/*` or `/dev/bpf*`, inspect or bind host interfaces, or
generate live host traffic with tools such as `route`, `ifconfig`,
`networksetup`, `scutil`, `tcpdump`, `ping`, or `curl` unless the user
explicitly reauthorizes device access in the same turn.

If a real macOS BPF proof is requested, stop and state that it requires
explicit host-device permission. Without that permission, report BPF runtime
smoke as intentionally skipped, not proven.

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
  margin. Compare against the same host, target, toolchain, and compiler flags;
  do not compare absolute values across machines.
- `bench_parse_verify_tcp` — closest single number to "RX hot path cost per
  packet."
- `bench_emit_ipv4` minus `bench_emit_ipv6` estimates the IPv4 header
  checksum step (IPv6 has no header checksum).

For comparisons that matter, use a quiet host, keep power/thermal state
stable, and re-run three times. Treat large same-host regressions as a prompt
to inspect codegen (§5.3) and sampled hotspots (§5.1), not as proof by
themselves.

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

Four shapes require `--features socket-tcp-dynamic-buffer`. They
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
| `churn` | open+close rate sustained, post-teardown `pool used (end)` returns to 0, `net heap delta` bounded | post-teardown `pool used (end) > 0` (leaked reservations); `net heap delta` growing with rate (allocator-on-hot-path). The separate `pool used at deadline` line is diagnostic: in-flight connections plus aborted-with-unread-rx sockets (readable until the slot recycles, per the `may_recv`-after-Closed contract) legitimately hold a small bounded charge at cutoff |
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

### 4.4 Capturing host baselines

Run on a quiet host. Pin the CPU governor where the platform supports it:

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

- `__memmove` dominating the profile → allocator-bound or excessive copies;
  check `net heap delta` and confirm steady state.
- `checksum::data` dominating unexpectedly → auto-vectorizer may not have
  engaged; verify codegen (§5.3).
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
trajectory sampled periodically in `--mode bench`. On Linux this reads
`/proc/self/status`; on macOS it uses Mach task VM info. `--mode trace`
disables periodic samples so Instruments captures are cleaner.

Interpretation rules:

- **`net heap delta` should be a small constant**. Anything else means
  smoltcp itself allocated on the hot path — a regression to investigate.
- **`RSS verdict: bounded`** when final RSS stays within the harness threshold
  relative to the median. `GROWTH` flags a possible leak; drop into
  massif/heaptrack to confirm.
- **`reserved total` in lane stats is profiling-harness memory**, not smoltcp
  socket memory. The paired in-memory link preallocates packet buffers so
  trace-mode runs avoid allocator noise. For iOS packet-tunnel budget claims,
  use the TCP pool readings and `dynbuf_memcompare`; the lane reservation is
  a local transport artifact that `NEPacketTunnelFlow` does not allocate.
- **`bytes allocated` should closely track `bytes freed`** in steady state. A persistent imbalance
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
know *which* callsite, not just how much. Stricter than CountingAlloc and
slower than baseline. Don't ship CI on it — run on demand.

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
cargo test --release --lib test_win_shift_capped_at_rfc7323_max
cargo test --release --lib --features socket-tcp-dynamic-buffer large_rx_max_grows_until_window_advertisable
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
- `test_win_shift_capped_at_rfc7323_max` — the window-scale shift must never
  exceed 14 (RFC 7323); the naive log2 formula yields 15 at exactly 1 GiB.
- `test_sack_*` — sender-side SACK recovery (§15): hole-walk ordering,
  selective-only under Cubic, re-lost-hole repair on partial ACK, RTO
  scoreboard discard, hostile-block ingest, recovery across the 2^32 wrap.
- `large_rx_max_grows_until_window_advertisable` — dynamic-buffer rx growth
  must continue until at least one scale granule (`1 << shift`) of window is
  free; stopping earlier truncates the advertised window to a permanent zero
  and deadlocks the connection behind zero-window probes.

### 7.1 Coverage-guided fuzzing (`fuzz/`)

The `fuzz/` directory carries libFuzzer harnesses for the wire parsers.
Coverage-guided fuzzing finds the kinds of input-validation bugs that
property tests miss, including parser edge cases such as 6LoWPAN, IPsec AH,
and IPv6 loopback handling.

```
cargo install cargo-fuzz   # one-time
cargo +nightly fuzz build
cargo +nightly fuzz run -s address wire_parsers      -- -max_total_time=120 -max_len=2000
cargo +nightly fuzz run -s address wire_roundtrip    -- -max_total_time=60  -max_len=2000
cargo +nightly fuzz run -s address tcp_headers       -- -max_total_time=60  -max_len=56
cargo +nightly fuzz run -s address sixlowpan_packet  -- -max_total_time=60  -max_len=2000
cargo +nightly fuzz run -s address dhcp_header       -- -max_total_time=60  -max_len=2000
cargo +nightly fuzz run -s address ieee802154_header -- -max_total_time=60  -max_len=2000
cargo +nightly fuzz run -s address packet_parser     -- -max_total_time=60  -max_len=2000
```

Target map:

| Target | What it fuzzes |
|---|---|
| `wire_parsers` | IPv4/IPv6, TCP, UDP, IPsec AH, 6LoWPAN ExtHeader, ICMPv4/v6. Parse-only. Discriminates by `data[0] & 0x07`. |
| `wire_roundtrip` | Differential round-trip: parse → emit → re-parse → assert equal. Catches "accepts but emits malformed" drift. |
| `tcp_headers` | Established TCP socket pair plus a mutated TCP header. State-machine smoke target for TCP option parsing and ACK/recovery edges. |
| `sixlowpan_packet` | 6LoWPAN dispatch plus fragmentation/IPHC parsing. |
| `packet_parser` | `PrettyPrinter<EthernetFrame>` end-to-end. Survival check for the pretty-printer. |
| `dhcp_header` | `DhcpPacket::new_checked` + `DhcpRepr::parse` + emit. |
| `ieee802154_header` | `Ieee802154Frame` parse/emit. |

Operational notes:

- Corpus is committed under `fuzz/corpus/<target>/`. Don't let it grow
  unbounded; trim with `cargo fuzz cmin <target>` if it gets large.
- A crash drops a reproducer at `fuzz/artifacts/<target>/crash-<sha>`.
  Convert to a unit test: `cat fuzz/artifacts/.../crash-... | xxd` and
  pin the bytes in the corresponding `wire::*::test` module.
- Treat audit findings the same way: no vulnerability report and no fix without
  an executable proof. Prefer a Miri-failing unit test for unsafe or aliasing
  claims; otherwise use an ASan fuzz reproducer or a focused unit test. Keep
  the proof as the first regression, then land the fix.
- Run for tens of minutes per parser before treating the result as
  meaningful. New-units-added stalling near zero is the signal that
  coverage has plateaued; bumping `-max_len` or expanding the
  discriminator usually re-opens it.
- Keep a separate large-input run for size/overflow claims. The short smoke
  commands above deliberately cap input size; deep runs should also exercise
  large packets:

```
cargo +nightly fuzz run -s address wire_parsers   -- -max_total_time=600 -max_len=65535
cargo +nightly fuzz run -s address wire_roundtrip -- -max_total_time=300 -max_len=65535
```

Coverage gap (tracked): `tcp_headers` mutates TCP headers against an
established socket pair, but it is still narrow compared with an arbitrary
`Interface` state-machine target. Congestion controllers, dynamic-buffer
growth/refusal, and SACK recovery (§15) are covered primarily by unit tests
and per-controller netsim sweeps. A richer target based on upstream's #1143
`iface` fuzzer, adapted to this fork's retained `tcp_headers`/`FuzzInjector`
setup, would close it. Worth a dedicated run.

### 7.2 Miri / proof lanes

```
rustup +nightly component add miri rust-src
MIRIFLAGS="-Zmiri-tree-borrows" cargo +nightly miri test --lib socket::tcp
MIRIFLAGS="-Zmiri-tree-borrows" cargo +nightly miri test --lib test_deconstruct
MIRIFLAGS="-Zmiri-tree-borrows" cargo +nightly miri test --lib \
  --features socket-tcp-dynamic-buffer socket::tcp::test::dyn_buf::next_capacity_math
```

Detects UB, aliasing violations, out-of-bounds reads, and uninitialised
memory access — the things release builds compile away silently. Runs much
slower than a normal test; restrict to the `wire` and `socket::tcp` modules in
regular use. Add to a "deep" CI lane, not the default one.

Use Tree Borrows (`-Zmiri-tree-borrows`) for audit proofs; it is the less
false-positive-prone aliasing model for the sort of safe wrapper and buffer
borrowing code smoltcp uses. Miri cannot model most host syscalls or C FFI, so
it is not the right verifier for `phy::sys`, raw sockets, BPF, or the Mach RSS
helpers in the profiling examples. For those, use ASan/LSan/TSan-capable fuzz
or integration repros where practical.

Smoltcp uses very little `unsafe`, so MIRI's value here is mostly
catching slice-arithmetic mistakes in the wire parsers and TCP option
walking — same surface as the property tests, with a different shaped
detector.

Deep lanes are useful before releases, but too slow for routine local gating:

```
MIRIFLAGS="-Zmiri-tree-borrows" cargo +nightly miri test --lib wire \
  -- --skip wire::ip::checksum::tests::checksum_matches_reference
MIRIFLAGS="-Zmiri-tree-borrows" cargo +nightly miri test --lib \
  --features socket-tcp-dynamic-buffer socket::tcp::test::dyn_buf \
  -- --skip socket::tcp::test::dyn_buf::pool_capacity_floor_300_sockets_24mib_budget
```

Do not delete these without an explicit justification in the same commit.

### 7.3 32-bit and pointer-width audit

The ordinary matrix includes a 16-bit pointer build, which is useful for
compile-time truncation pressure, but it does not run the code. For any bug
hypothesis involving `usize`, large lengths, or buffer caps, add a runnable
32-bit Linux lane on a host/toolchain that supports it:

```
rustup target add i686-unknown-linux-gnu
cargo test --target i686-unknown-linux-gnu --lib \
  --features socket-tcp-dynamic-buffer socket::tcp::test::dyn_buf
cargo test --target i686-unknown-linux-gnu --lib test_win_shift_capped_at_rfc7323_max
```

On non-x86 hosts, use the local equivalent 32-bit target plus its linker/runner
(for example an ARMv7 target under QEMU). Keep this as an on-demand deep audit
lane unless Linux 32-bit consumers become product-critical.

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

When the reproducer is a Miri/sanitizer/unit proof, use it as the bisect
driver instead of manual judgement:

```
git bisect start HEAD v0.13.1
git bisect run bash -lc 'MIRIFLAGS="-Zmiri-tree-borrows" cargo +nightly miri test --lib <proof_test>'
```

### 10.1 Receive-window update policy

`Socket::window_to_update` is still the upstream Linux-style "significant
update" heuristic, not full RFC 1122 receiver-side SWS avoidance. Fixed-time-step
unidirectional harness shapes such as `firehose` can expose receive-window
reopen stalls when delayed ACKs are disabled. Route changes through the evidence
map above: prove the predicate with focused TCP tests, then run `./ci.sh netsim`
and the §4.2 throughput/fairness harnesses before changing snapshots or docs.

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
  `Config::random_seed`. Rejecting a zero seed at `Interface::new` would panic
  for every consumer using the `Config::new()` default; instead
  `Interface::new` logs a debug warning when the seed is zero. Production
  deployments must set per-boot entropy.
- **Bounding `poll()` ingress work.** `poll()` intentionally drains the
  device queue and documents the DoS trade-off; consumers that need bounded
  per-call latency use the upstream `poll_ingress_single()` /
  `poll_egress()` / `poll_maintenance()` primitives. Changing `poll()` to
  one-packet-per-call silently breaks the semantics every existing consumer
  was written against; rejected.
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
| RFC compliance (PAWS, IW10) | Yes; file with RFC citation. |
| SACK-based selective retransmission (§15) | Yes; RFC 2018/6675/6582 citations plus the netsim multi-seed evidence. The NoControl redundant pass needs design discussion. |
| Phy hardening (panic → log+drop) | Likely; brief design discussion. |
| Field-type shrink (`usize → u32`) | Low-controversy; easy PR. |
| Static-dispatch wrappers on `AnyController` | Touches trait surface; needs maintainer buy-in. |
| Profiling harness | Maintainer-discretion; scope discussion. |
| Sizecheck diagnostic | Skip — diagnostic, not a behavioral test. |

When a commit is accepted upstream, drop it from this fork on the next sync.

## 13. Reference baselines

This section defines what to measure and what a clean result means. It does
not carry fixed throughput, trace, cachegrind, dhat, or struct-size numbers.
Those move with host, kernel, toolchain, compiler flags, feature set, signing
state, and workload length. Put current numeric evidence in PRs or release
notes with the commit, machine, toolchain, feature set, command, and run mode.

### 13.1 Wire microbench (`cargo +nightly bench --bench bench`)

Record all checksum, emit, and parse rows from §4.1. The durable regression
signals are:

- a same-host `checksum_*` row jumping materially while compiler flags stayed
  the same: inspect the emitted checksum code (§5.3);
- `bench_emit_tcp` rising while raw checksum rows hold steady: inspect the
  pseudo-header path;
- `bench_parse_verify_tcp` rising while emit/checksum rows hold steady:
  inspect the RX parse/verify path.

### 13.2 End-to-end shapes (`profile_loopback`)

Record current host baselines from isolated runs. Keep the shape roles stable:

| Shape | Primary use |
|---|---|
| `udp` | software-checksum packet-forwarding throughput and Mpps. |
| `small` | TCP state-machine overhead on small segments. |
| `pingpong` | latency-bound request/response behavior. |
| `firehose` | one-way TCP bulk transfer; useful for ratios because both peers are smoltcp and cwnd dynamics dominate. |

Cubic and Reno should improve short `pingpong` handshakes because of the IW10
first-RTT ramp. Bulk shapes are mostly useful as same-host ratios.

### 13.3 Scaling and fairness (`many_tcp` / `many_tcp_fair` / `many_udp`)

Use separate shapes for separate claims:

| Shape | Evidence role | Gate |
|---|---|---|
| `many_tcp` | High-throughput TCP stress, memory growth, and starvation discovery. | zero-flow count must stay 0; RSS verdict should be `bounded`; Jain is diagnostic because the hot loop intentionally favors throughput. |
| `many_tcp_fair` | Deterministic TCP fairness. One flow gets one bounded send/drain opportunity per round, and the start flow rotates each round. | Jain >= 0.95, zero-flow count 0, RSS bounded, lane fallback allocs 0 in trace mode. |
| `many_udp` | UDP control shape without TCP flow-control or cwnd effects. | Jain should be 1.00 or close to it; RSS bounded. |

RSS verdict other than `bounded` is a leak or unbounded buffer-growth signal.
Nonzero lane fallback allocations in trace mode mean the harness pool, not
smoltcp, polluted the trace; do not quote that run as performance evidence.

### 13.4 Dynamic-buffer / multi-thread shapes

Do not keep fixed Gbps or RSS headline numbers here unless they come from
current isolated runs on the same host and revision. The durable gates are:

| Shape | Durable gate |
|---|---|
| `multi_tcp` | per-thread Jain >= 0.95, bounded pool CAS retries, `pool used (end)` returns to 0, trace-mode lane fallback allocs 0. |
| `multi_tcp_sink` | same pool and lane gates as `multi_tcp`; compare Time/CPU Profiler hotspots against `multi_tcp` for lower app-side copy pressure before claiming a copy win. |
| `churn` | achieved close rate tracks target rate, post-teardown `pool used (end) == 0` (the at-deadline reading may be small and bounded), net heap delta does not grow with connection rate. |
| `idle_hot` | `pool used post-create == 0`; steady pool charge is proportional only to active client/server sockets; lane `reserved total` is harness-only and must be kept separate from iOS socket-budget claims; `n_active=0` is valid and should keep steady pool use at 0. |

Sub-linear scaling at higher thread counts is expected once every worker is
CPU-bound. Jain below the gate across threads suggests `MemoryPool` contention
or host scheduling noise; confirm with System Trace before changing pool
internals.

The pool-refund, lazy-alloc, fairness, and RSS-boundedness rows above are
machine-enforced: `./ci.sh ios-gate` and `./ci.sh profile-smoke` grep the
harness output for the passing form and exit non-zero when an invariant — or
the output line carrying it — is missing. Host-dependent throughput values
stay ungated per the §13 policy.

### 13.5 Context switches and pool contention

On Linux, every shape prints `voluntary` / `nonvoluntary` context-switch counts
read from `/proc/self/status`. Voluntary means a thread blocked or yielded;
nonvoluntary means the OS scheduler preempted a running thread. Both should
stay small in the spin-loop designs.

On macOS, do not emulate `/proc` in the harness. Use System Trace analysis
(§5.1.1) and read `threads`, `context-switches`, `syscalls`, and
`virtual-memory`; use `data_window_seconds` from `summary` for rates.

`multi_tcp` and `multi_tcp_sink` report `pool CAS retries:` from
`MemoryPool::cas_retries()`. Bounded and sub-linear retry growth is the gate;
if retries become a throughput limiter, confirm that the target threads are
CPU-bound and not blocked on syscalls, allocator activity, or another
synchronization primitive.

### 13.6 CPU cost maps and heap attribution

Use cachegrind, callgrind, dhat, or Instruments as diagnostics, not as fixed
documentation tables. Preserve raw profiler output outside the repo and quote
numbers only from current isolated runs.

Expected hot categories:

- packet checksum work in `wire::ip::checksum::data`;
- payload moves in libc copy/move routines and socket-buffer enqueue/dequeue;
- TCP/UDP state-machine work in `Socket::process`, `Socket::dispatch`, and
  `Interface::poll`.

Dynamic-buffer growth/refund functions should not be sampled hot in steady
state. If they are, inspect grow thresholds, pool pressure, and whether the
workload is repeatedly opening/closing sockets rather than measuring steady
packet forwarding.

For heap attribution, the invariant is that smoltcp does not allocate per
packet in steady state. Harness lane pools and benchmark sampling can allocate
during setup or periodic measurement; smoltcp hot-path callsites should not
appear as growing allocation sources.

### 13.7 Struct footprint and allocator state

Run `sizecheck` after field-layout, congestion-controller, or dynamic-buffer
changes. Record the new values in the commit or PR that moves them, not as
standing numbers in this guide.

Across every steady-state shape:

- `net heap delta`: should stay a small constant, dominated by harness setup or
  sampling in bench mode;
- `allocation count`: should track setup/sampling events, not packet count;
- packet-count-correlated allocations usually mean a `Vec::with_capacity`,
  boxing, or owned buffer conversion entered the hot path.

## 14. Dynamic-buffer TCP sockets (`socket-tcp-dynamic-buffer`)

Pool-backed, lazy, resizable rx/tx buffers for TCP. Opt-in. Designed
for memory-constrained hosts that admit many concurrent flows, such as
packet-tunnel clients under tight resident-memory budgets. Disabled by default;
the legacy `Socket::new(rx_buf, tx_buf)` API is bit-for-bit unchanged.

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
- `src/socket/tcp/test/dyn_buf.rs` — dynamic-buffer TCP regression tests.
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

### 14.10 Recommended configuration for iOS packet tunnels

Starting point for a `NEPacketTunnelProvider`-style consumer under the
50 MB extension memory limit:

```rust
let pool = tcp::MemoryPool::new(24 * 1024 * 1024); // one per process

let cfg = tcp::DynamicBufferConfig {
    rx_initial: 0,
    rx_max: 64 * 1024,
    tx_initial: 0,
    tx_max: 64 * 1024,
    grow_chunk: 4 * 1024,
};
let socket = tcp::Socket::new_dynamic(cfg, Some(pool.clone()));
```

Rationale:

- **Pool = 24 MiB**, roughly half the 50 MB jetsam budget. The rest is
  headroom for wire/device buffers, the `Interface`, non-TCP sockets, the
  consumer's own state, and the Network-framework overhead the extension
  pays regardless. The pool bounds the worst case; idle flows cost ~0
  (`dynbuf_memcompare`: ~0.5 KiB/flow RSS vs ~55 KiB/flow for legacy
  64 KiB fixed buffers).
- **`rx_max = tx_max = 64 KiB`** keeps the window-scale shift at 1 and is
  enough to fill typical mobile BDPs (e.g. 100 Mbit/s × 5 ms ≈ 62 KB).
  Don't raise `rx_max` past what the flow's bandwidth-delay product needs:
  large maxima raise the negotiated shift, and with it the growth floor
  (one scale granule, `1 << shift`) and worst-case per-flow cost under load.
- **`initial = 0`** so admission is free: hundreds of idle flows (DNS,
  keep-alive idle HTTP) charge nothing until traffic arrives.
- **Worst case** = `budget` bytes of socket buffers across all flows, e.g.
  192 concurrent flows all grown to both 64 KiB maxima. Past that, growth
  refusal converts into advertised-window backpressure per flow instead of
  memory growth — the failure mode is throughput, not jetsam.

Build with `socket-tcp-cubic` for cellular deployments: a real congestion
controller keeps loss recovery (§15) strictly selective — the cwnd budget
goes to reassembly holes — where `NoControl` deliberately falls back to a
redundant in-order pass after the holes.

Validate a configuration change with `./ci.sh ios-gate`, then run longer
`dynbuf_memcompare`, `churn`, and `idle_hot` shapes (§4.2.1) when changing pool
or buffer sizing. When using `profile_loopback` on macOS, keep the printed lane
`reserved total` out of the iOS memory budget: it is the harness's preallocated
paired-link packet pool, not memory that dynamic TCP sockets or
`NEPacketTunnelFlow` consume in a packet tunnel extension.

## 15. SACK-based selective retransmission

The sender consumes incoming SACK blocks (RFC 2018) instead of ignoring
them. Always on; zero-cost on lossless connections (the scoreboard is
empty and every consumer short-circuits on `is_empty`).

Design, in `src/socket/tcp.rs`:

- **Scoreboard**: peer-SACKed ranges in a second fixed-size `Assembler`
  (+64 B per socket, no allocation), stored as offsets relative to
  SND.UNA; `Assembler::shift_front` slides it with the cumulative ACK.
  Ingest validates hostile blocks with wrapping-ordered comparisons
  before any subtraction, trims to SND.UNA, clamps to buffered data, and
  drops on overflow — worst case equals pre-SACK behavior.
- **Invariant I1**: `remote_last_seq` never rests inside a SACKed range
  (`normalize_tx_cursor`, enforced at every cursor/scoreboard mutation).
  The pure observers (`seq_to_transmit`, `poll_at`, `egress_interest`)
  therefore needed no changes. The segment selector clamps a hole
  retransmission at the next SACKed block.
- **Recovery point** (RFC 6582 §3): armed at dupack #3 and on RTO. For
  managed controllers both sites arm at HighData — the highest sequence
  transmitted (`rtte.max_seq_sent`; RFC 6582 "recover", Linux
  `high_seq = snd_nxt`) — never at the send cursor, which legally rewinds
  during recovery and would understate the point (early exit with holes
  outstanding, then a double window cut from the next dupack burst).
  Each episode records how it started: partial ACKs below the point are
  withheld from the controller's ordinary new-data `on_ack` only during
  FAST recovery (RFC 6582 §3.2 — Reno/Cubic would otherwise exit and
  deflate to ssthresh mid-episode), while an RTO-armed episode keeps
  delivering them so the post-RTO drain grows in slow start on every ACK
  (RFC 5681 §3.1). Under a managed controller a new episode — and its
  `on_loss` window cut — is armed only once the previous recovery point
  is cumulatively ACKed (RFC 6582 §3.2 heuristic). Partial ACKs below
  the point rewind-and-walk the next hole immediately; reaching it ends
  recovery. RTO discards the scoreboard and resends conservatively
  (RFC 2018 §8). `NoControl` deliberately keeps the legacy behavior —
  cursor-armed points, re-armed at every trigger — because it has no
  window to protect and the redundant-pass machinery below keys off the
  latest trigger.
- **Redundant pass, `NoControl` only**: when the selective walk exhausts
  while recovery is open, one bounded in-order resend of the window —
  holes always first, then redundancy fills the unmanaged pipe and
  solicits a fresh ACK (lost-cumACK repair in one RTT). Under Reno/Cubic
  (`AnyController::manages_window`) the pass is skipped.

The `in_flight` signal handed to the congestion controller is still cursor
based, but it subtracts SACKed ranges below `remote_last_seq` during the
selective walk. That keeps peer-confirmed bytes from consuming cwnd when the
cursor jumps over them, while deliberately counting the full window during
the `NoControl` redundant pass. It is not a full RFC 6675 pipe estimator: bytes
outstanding above the cursor are still outside this accounting, which remains
slightly conservative for cwnd growth.

Evidence gates (re-measure per host; see §13 policy): deterministic
netsim across 16 seeds at 32 KiB buffers — mean ±0% at 2% loss, +6% at
5%, +36% at 10%; real-kernel TUN interop at 5% loss — multi-fold faster
with the RTO tail eliminated, byte-exact. Clean-path throughput
unchanged. RACK-TLP and pacing remain out of scope (§11) until profile
evidence demands them.

In-tree regression coverage: `tests/netsim.rs` runs the buffer×loss sweep.
`./ci.sh netsim` runs the stable NoControl snapshot, which exercises the SACK
repair path without controller back-off. When changing congestion controllers,
also run the feature-gated controller snapshots directly:

```
cargo test --release --features "_netsim socket-tcp-cubic socket-tcp-reno" \
    --test netsim netsim_cubic -- --test-threads=1
cargo test --release --features "_netsim socket-tcp-cubic socket-tcp-reno" \
    --test netsim netsim_reno -- --test-threads=1
```

The controller snapshots should show Cubic/Reno throttling on loss as real
controllers must; refresh them only after reviewing the throughput table.

## 16. Backported post-0.13.1 upstream changes

The fork branches from `v0.13.1` (see §2). The changes below were
cherry-picked from upstream commits that landed on `upstream/main` after that
tag. A future maintainer reconciling against upstream should treat these as
already present and avoid re-applying them. Each fork commit names the
upstream PR/commit it came from.

| Upstream PR | What | Fork adaptation |
|---|---|---|
| #1150 | `PacketBuffer`: reserve metadata slot before payload closure | verbatim |
| #1152 | `#[collapse_debuginfo]` on logging macros | verbatim |
| #1159 | deterministic `config.rs` via `BTreeMap` | verbatim |
| #1162 + #1164 | out-of-window RX: drop OOW RST, exempt OOW data ACKs from rate limiting | verbatim production change; netsim snapshot rebaselined |
| #1161 | effective MSS subtracts options length; `MIN_REMOTE_MSS` clamp | adapted for `remote_mss: u32`; preserves the adjacent SACK clamp |
| #1154 / #1156 / #1157 + RTT parts of #1155 | RFC-compliant congestion-control redesign (new Controller API, Reno/CUBIC fast recovery, RTT estimator `on_rto`/`on_retransmit` split, `smoothed_rtt`) | `reno.rs`/`cubic.rs` are taken **verbatim from upstream** save for one fork delta: `set_mss` opens the window at RFC 6928 IW10 instead of upstream's 2*MSS (faster first-RTT ramp; guarded by `*_iw10_on_set_mss`/`*_rwnd_is_grow_only` tests). The fork's static-dispatch `AnyController` wrappers live in `congestion.rs` and are unaffected. The pre-redesign fork shrank these window fields to `u32` and tracked rwnd shrinks in the controller; both were **dropped** — the `u32` shrink saved ~24 B/socket (negligible vs the buffer pool) at the cost of pervasive casts and heavy divergence, and the controller rwnd-shrink collapsed cwnd on transient receive-window dips once the cwnd-vs-in-flight accounting was corrected. The live receive window is still enforced at the socket layer. |

Deliberately **not** taken:

- The dispatch-side fast-retransmit rework in #1155
  (`pending_fast_retransmit` + single-segment resend from `flight_size()`).
  The fork's SACK selective retransmission (§15) is a strictly more
  capable loss-recovery path occupying the same dispatch region; the new
  congestion controllers supply the cwnd dynamics that recovery defers to.
- The fuzz-suite revamp (#1143) — the fork carries its own suite (§7.1).
- The netsim harness rewrite / multiflow test (#1153) — its snapshots are
  upstream-stack throughput fingerprints that do not match this fork.

Still ahead of upstream (candidate to upstream per §12): RFC 6928 IW10 in
`set_mss` (upstream's redesigned Reno/CUBIC open at 2*MSS).

## 17. Candidate future work

Ideas that passed review triage but are not yet scoped. Each would be a
deliberate exception to §1's "no architectural divergence" policy, so it
needs an explicit scope decision (and an upstream-discussion attempt per
§12) before any code lands.

### 17.1 O(1) ingress demux + egress dirty-list

The one remaining ≥2x-class CPU lever at realistic tunnel flow counts.
Three inherited-from-upstream O(n_sockets) walks dominate once an
`Interface` carries hundreds of (mostly idle) flows — the browser-driven
packet-tunnel shape:

- **Ingress demux**: every inbound TCP segment walks all sockets calling
  `accepts()` until one matches (`src/iface/interface/tcp.rs`, the
  `for tcp_socket in sockets.items_mut()` loop). Per-packet cost grows
  linearly with flow count.
- **Egress scan**: every `poll()` visits every socket — including idle
  ones — in `socket_egress` (`src/iface/interface/mod.rs`, the
  `for item in sockets.items_mut()` loop).
- **`poll_at` scan**: computing the next wake time iterates all sockets
  again (`src/iface/interface/mod.rs`, `Interface::poll_at`).

The `idle_hot` shape measures exactly this: with 1000 idle + 4 active
sockets, every poll still pays a 1004-socket walk twice.

Design sketch:

- A 4-tuple → `SocketHandle` hash index maintained at the points where a
  socket's `tuple` is set or cleared (connect, passive-open promotion,
  reset, abort, close, `SocketSet::remove`). Established-connection
  lookups become O(1); SYNs to listeners fall back to a (short) scan of
  listen sockets only.
- An egress-interest dirty list: sockets enroll when a state change makes
  egress work possible (the conditions `egress_interest` / `poll_at`
  already express) and drop out when drained; `poll()` visits only
  enrolled sockets plus timer-due ones. A timer min-heap (or wheel) over
  per-socket `poll_at` values replaces the full scan.
- Keep it additive and feature-gated; the index lives beside `SocketSet`
  without changing socket or interface public API.

Evidence plan before/after: the `many_tcp` N-sweep (50→2000, §4.2) and
`idle_hot 1000 4` (§4.2.1). Acceptance: per-packet cost flat (not linear)
in N at high flow counts, no regression at N ≤ 8, `poll_at` cost
proportional to active flows only. Risks to watch: index/tuple desync on
abort/reuse paths (needs a churn-shape invariant check), and hash-flood
resistance of the tuple hash (seed it from `Config::random_seed`).
