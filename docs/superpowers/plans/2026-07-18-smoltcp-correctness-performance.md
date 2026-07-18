# smoltcp Correctness and Constrained-Memory Performance Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the audited smoltcp correctness and compatibility defects, improve the constrained-memory evidence harness, update the Apple tunnel consumer, and land both repositories without a meaningful throughput or RSS regression.

**Architecture:** Keep protocol state inside the existing deep TCP, interface, wire, and PHY modules. Add only the canonical state required by the RFCs, preserve public compatibility, and reuse the existing profiling/RSS harnesses. Capture a matched baseline before production changes, implement every fix test-first, and compare repeated final measurements against that baseline.

**Tech Stack:** Rust 1.91 MSRV/stable/nightly, no_std/alloc feature matrices, shell CI helpers, libFuzzer/ASan, insta netsim snapshots, Linux RSS plus macOS physical-footprint reporting, and the downstream `tunnel-lib-rust` user netstack.

## Global Constraints

- Follow `/root/smoltcp/docs/superpowers/specs/2026-07-18-smoltcp-correctness-performance-design.md` exactly.
- Preserve all existing no_std, no-alloc, alloc, MSRV, 16-bit, Apple, and public source-compatibility contracts.
- Use a failing regression before every production behavior change.
- Do not add dependencies or unrelated refactors.
- Default and iOS-shaped TCP `Socket` sizes must not increase.
- Equivalent-workload pool charge, net heap delta, and allocation count must not increase.
- A raw-RSS increase blocks landing when its paired median exceeds both one host page and 1% in at least three of five matched samples.
- Median throughput may not regress by more than 2%; fairness remains Jain >= 0.95 and no flow may starve.
- Do not refresh a snapshot until the deterministic behavior change is understood and documented.
- Do not run live network devices; BPF verification uses pure synthetic record buffers.
- Preserve unrelated user changes in both repositories.

---

### Task 1: Capture the immutable pre-change baseline

**Files:**
- Create: `docs/perf/2026-07-18-before.md`
- Modify: `examples/profile_loopback.rs` only if an existing shape cannot emit a required baseline metric
- Read: `ci.sh`, `examples/dynbuf_memcompare.rs`
- Read: `/root/tunnel-lib-rust/crates/netstack/tests/rss_budget_*.rs`

**Interfaces:**
- Consumes: fork commit `fd94836`, consumer commit `fc8193b7`.
- Produces: exact baseline commands, five matched samples for performance/RSS shapes, socket sizes, raw logs under `/tmp/smoltcp-perf-before`, and a summarized baseline document used by Task 10.

- [ ] **Step 1: Record environment and pinned revisions**

Run `df -h /root && free -h && nproc && rustc -Vv && git rev-parse HEAD` in `/root/smoltcp`, and the equivalent `git rev-parse HEAD` in `/root/tunnel-lib-rust`. Record output in the baseline document.

- [ ] **Step 2: Capture exact static and correctness baselines**

Run:

```bash
TRACE=0 ./ci.sh quick
cargo test --release --lib
cargo test --release --lib --features socket-tcp-dynamic-buffer
cargo test --release --test sizecheck -- --nocapture
cargo test --release --test sizecheck --no-default-features --features "alloc,medium-ip,proto-ipv4,proto-ipv6,socket-udp,socket-tcp,socket-tcp-dynamic-buffer,socket-tcp-cubic" -- --nocapture
TRACE=0 ./ci.sh netsim
cargo test --release --features "_netsim socket-tcp-cubic socket-tcp-reno" --test netsim netsim_cubic -- --test-threads=1
cargo test --release --features "_netsim socket-tcp-cubic socket-tcp-reno" --test netsim netsim_reno -- --test-threads=1
```

Expected: ordinary suites and NoControl pass; record the known pre-change Reno/CUBIC snapshot mismatches without accepting generated snapshots.

- [ ] **Step 3: Capture five matched performance samples**

For each software-checksum and `offload` variant, run one unrecorded warm-up followed by five release samples of `udp`, `firehose`, `pingpong`, `small`, `many_tcp 3 8`, `many_tcp_fair 3 8`, `many_udp 3 8`, `many_tcp 3 50`, `many_tcp_fair 3 50`, `many_udp 3 50`, `many_tcp 3 100`, `many_tcp_fair 3 100`, and `many_udp 3 100`. Run dynamic-buffer shapes `multi_tcp 3 2 50`, `multi_tcp_sink 3 2 50`, `churn 3 500`, `idle_hot 3 1000 0`, and `idle_hot 3 1000 10` in both software-checksum and `offload` variants with the same warm-up plus five-sample protocol. Save every raw output under `/tmp/smoltcp-perf-before/<shape>-<sample>.log` and record medians for throughput, raw RSS start/end, heap delta, allocations, fairness, active pool charge, pool charge after teardown, and fallback allocations. If a current shape omits a required metric, first add reporting-only instrumentation with a failing pure unit test and synchronization outside its timed steady-state loop; do not change its traffic or library behavior.

- [ ] **Step 4: Capture idle socket and downstream constrained-memory baselines**

Run one warm-up plus five separate-process `dynbuf_memcompare` legacy/dynamic samples for 300 and 1,000 flows, recording raw RSS start/end as well as deltas. In `/root/tunnel-lib-rust`, run the focused release RSS suites registered as `rss_budget_tcp`, `rss_budget_combined`, `rss_budget_tcp_slow`, `rss_budget_tcp_rst`, `rss_budget_tcp_pressure`, `rss_budget_udp_pressure`, and `rss_budget_tcp_pool` using the repository's documented feature set. For each of the three absolute-RSS bodies, run one unrecorded warm-up followed by five fresh test-process samples and record the exact command plus raw before/after/delta medians; label Linux results supplemental rather than Apple jetsam truth. Record all platform-neutral passes and diagnostics.

- [ ] **Step 5: Verify and commit the baseline document**

Run `git diff --check`, verify the document names every command and sample count, then commit:

```bash
git add examples/profile_loopback.rs docs/perf/2026-07-18-before.md
git commit -m "docs: capture pre-fix smoltcp performance baseline"
```

### Task 2: Enforce transmitted TCP high-water and correct SACK recovery

**Files:**
- Modify: `src/socket/tcp.rs`
- Test: `src/socket/tcp.rs` TCP unit-test module
- Test: `tests/sizecheck.rs`

**Interfaces:**
- Produces: private `local_seq_next: TcpSeqNumber` (RFC `SND.NXT`) distinct from the rewindable recovery cursor; cumulative ACK and sender SACK validation bounded by actually transmitted sequence space.
- Preserves: public `Socket` interface and default/iOS socket sizes.

- [ ] **Step 1: Add failing cumulative-ACK and SACK regressions**

Create one fixture with a transmit queue larger than the allowed first dispatch. Assert that an ACK through the queued-unsent suffix is rejected and leaves unsent bytes queued. Create a second fixture that ACKs the sent prefix while SACKing the queued-unsent suffix; assert the SACK range is ignored, the unsent bytes remain dispatchable, and the retransmission timer remains armed.

- [ ] **Step 2: Verify RED**

Run `cargo test --lib reject_ack_for_queued_unsent_data -- --nocapture` and `cargo test --lib ignore_sack_for_queued_unsent_data -- --nocapture`. Expected: the cumulative case dequeues unsent bytes and the SACK case advances/strands the cursor.

- [ ] **Step 3: Add canonical transmitted high-water state**

Add one private sequence value representing the first sequence after everything actually transmitted. Initialize it with the local initial sequence, advance it only after successful segment emission using `max(old, emitted_end)`, reset it on every TCB reset, and include SYN/FIN sequence space. Keep `remote_last_seq` as the rewindable send/recovery cursor. Bound acceptable cumulative ACK and `ingest_sack_ranges` by the transmitted high-water rather than `tx_buffer.len()`.

- [ ] **Step 4: Correct partial recovery classification**

Add a failing regression where a partial cumulative ACK consumes the last SACK range below `RecoveryPoint`. Make recovery membership depend only on `RecoveryPoint`; keep Reno/CUBIC in recovery and rewind to the next unacknowledged hole even when the scoreboard becomes empty.

- [ ] **Step 5: Verify GREEN and footprint**

Run the three focused tests, all TCP library tests with CUBIC/Reno, release sizecheck for default and iOS features, and `TRACE=0 ./ci.sh netsim`. If `Socket` grows, recover equivalent padding/representation space without changing semantics before continuing.

- [ ] **Step 6: Commit**

```bash
git add src/socket/tcp.rs tests/sizecheck.rs
git commit -m "tcp: bound ACK and SACK by transmitted sequence space"
```

### Task 3: Reset congestion controllers and implement exact IW10

**Files:**
- Modify: `src/socket/tcp/congestion.rs`
- Modify: `src/socket/tcp/congestion/reno.rs`
- Modify: `src/socket/tcp/congestion/cubic.rs`
- Modify: `src/socket/tcp.rs`

**Interfaces:**
- Produces: `AnyController::reset(&mut self)`, which reconstructs its current variant, and `initial_window(mss: u32) -> u32`, the shared RFC 6928 helper in `congestion.rs`.
- Preserves: selected `CongestionControl` variant and public configuration.

- [ ] **Step 1: Add failing controller and socket-reuse tests**

Test exact IW10 for MSS 48, 536, and 1460 in Reno and CUBIC. Grow a controller, enter recovery, reset/reconnect or reset/re-listen the socket, and assert clean cwnd, rwnd, ssthresh, MSS, recovery flags, and CUBIC epoch. Add a failed `SynReceived -> Listen` handshake case and a peer-without-MSS case.

- [ ] **Step 2: Verify RED**

Run the new controller and TCP tests individually. Expected: MSS 48 reports 2,048 instead of 480 and reused controllers retain old state.

- [ ] **Step 3: Implement one canonical IW10 helper and variant reset**

Use the RFC 6928 formula `min(10*MSS, max(2*MSS, 14600))` with saturating/window-width conversion. Reconstruct the selected controller variant from its enum on every new TCB boundary, including failed passive handshakes. Apply the negotiated or default MSS after reset and assign, rather than floor, the initial cwnd.

- [ ] **Step 4: Verify GREEN and controllers under loss**

Run focused tests, all TCP+CUBIC+Reno tests, and serial NoControl/Reno/CUBIC netsims. Explain deterministic snapshot changes before updating only the controller snapshots.

- [ ] **Step 5: Commit**

```bash
git add src/socket/tcp.rs src/socket/tcp/congestion
git commit -m "tcp: reset congestion state and apply exact IW10"
```

### Task 4: Make PAWS state explicit and monotonic

**Files:**
- Modify: `src/socket/tcp.rs`

**Interfaces:**
- Produces: explicit `REMOTE_TSVAL_VALID` socket flag alongside the existing TS.Recent value, plus `last_remote_tsval_at: Instant`; 24-day expiry.
- Preserves: timestamp negotiation and public timestamp-generator interface.

- [ ] **Step 1: Add failing timestamp tests**

Add cases for TSval zero being valid, immediate older TSval rejection, newer TSval acceptance, and an older/wrapped value accepted after more than 24 days with TS.Recent refreshed.

- [ ] **Step 2: Verify RED**

Run each exact PAWS test. Expected: zero bypasses PAWS and long-idle wrapped traffic remains rejected.

- [ ] **Step 3: Implement explicit TS.Recent validity/age**

Replace the zero sentinel with optional timestamp state and record the accepted observation time. Skip the old-value rejection when the observation is older than 24 days, then update TS.Recent using the existing in-sequence acceptance rules. Fit the state into existing socket padding or recover space elsewhere so both size gates remain flat.

- [ ] **Step 4: Verify and commit**

Run all timestamp/TCP tests and both sizechecks, then commit `tcp: expire stale PAWS timestamp state`.

### Task 5: Add non-borrowing receive reclamation and migrate the consumer

**Files:**
- Modify: `src/socket/tcp.rs`
- Test: `src/socket/tcp/test/dyn_buf.rs`
- Modify: `/root/tunnel-lib-rust/crates/netstack/src/user/tcp_table.rs`

**Interfaces:**
- Produces: `Socket::recv_with<F, R>(&mut self, f: F) -> Result<R, RecvError>` where `F: for<'b> FnOnce(&'b mut [u8]) -> (usize, R)`; `R` cannot borrow from the RX slice and the method runs terminal dynamic release hooks before returning.
- Preserves: existing borrowing `Socket::recv` and zero-copy consumer ring writes.

- [ ] **Step 1: Add a failing pool-reclamation regression**

Drain unread terminal RX through the new non-borrowing closure path and assert `MemoryPool::used() == 0` immediately after return while the socket object remains alive. Keep a separate test proving the existing borrowing API remains usable.

- [ ] **Step 2: Verify RED**

Compile/run the focused dynamic-buffer test before the method exists; expected compile failure identifies the missing interface.

- [ ] **Step 3: Implement the minimal deep interface**

Add `recv_with` with the higher-ranked signature above. Delegate dequeue behavior to `recv_impl`, then call both terminal release hooks. Do not duplicate receive error checks or add a new buffer type.

- [ ] **Step 4: Migrate the tunnel consumer without a copy**

Change the generic closure receive call in `tcp_table.rs` to `recv_with`. Run the netstack focused tests and verify its ring-buffer write path and return value are unchanged.

- [ ] **Step 5: Verify memory/performance and commit in each repository**

Run dynamic-buffer tests, `ios-gate`, `idle_hot`, `churn`, and downstream RST/slow-reader/pool RSS suites. Commit smoltcp first, then the consumer call-site change as a separate commit without updating the git pin yet.

### Task 6: Backport monotonic time conversion and migrate hosted polling

**Files:**
- Modify: `src/time.rs`
- Modify: `/root/tunnel-lib-rust/crates/netstack/src/user/driver.rs`
- Modify tests that construct/poll the user interface with `SmolInstant::now()` where they exercise hosted time.

**Interfaces:**
- Produces: stable `From<std::time::Instant> for smoltcp::time::Instant` using one process referential.
- Preserves: explicit `SystemTime` conversions for absolute-time callers.

- [ ] **Step 1: Backport #1177's failing tests first**

Add tests that converting the same std instant twice is stable and that two ordered std instants remain ordered after conversion. Run them and observe failure.

- [ ] **Step 2: Implement upstream's stable referential**

Use `std::sync::LazyLock<std::time::Instant>` and `saturating_duration_since`, matching upstream commit `c541dce...`. Run time and full library tests.

- [ ] **Step 3: Migrate the shipping driver**

At interface construction, `poll_delay`, and `poll`, replace wall-clock `SmolInstant::now()` with `std::time::Instant::now().into()`. Keep one sampled instant per logical poll cycle where possible so timer decisions share a timestamp.

- [ ] **Step 4: Verify and commit**

Run smoltcp time/full tests and downstream user-netstack integration tests. Commit smoltcp as `time: make std instant conversion monotonic` and consumer as `netstack: drive smoltcp with a monotonic clock`.

### Task 7: Fix 6LoWPAN, SLAAC, and checked IPv4 reassembly

**Files:**
- Modify: `src/iface/interface/sixlowpan.rs`
- Modify: `src/wire/ndiscoption.rs`
- Modify: `src/iface/slaac.rs`
- Modify: `src/iface/interface/ipv4.rs`
- Modify: `src/iface/fragmentation.rs`
- Test: `src/iface/interface/tests/sixlowpan.rs`, IPv6/SLAAC tests, fragmentation tests

**Interfaces:**
- Produces: default-capability 6LoWPAN UDP delivery, panic-free SLAAC validation, and checked fragmentation length helpers.

- [ ] **Step 1: Add failing end-to-end/proof tests**

Add fragmented and nonfragmented compressed UDP delivery tests without disabling checksum verification; full ingress RAs with prefix lengths 129 and 255; and a host-width-independent checked helper test for maximum IPv4 fragment end.

- [ ] **Step 2: Verify RED**

Run each focused feature test. Expected: UDP delivery drops, RA panics, and checked fragment helper is missing.

- [ ] **Step 3: Implement minimal wire/interface fixes**

Carry inline NHC checksums into the reconstructed UDP header and compute elided checksums only once the full payload exists. Add `prefix_len <= 128` to Prefix Information validity. Centralize fragment-end checked arithmetic, reject overflow/over-65,535 totals before allocation/copy, and reuse it in total-size and add paths.

- [ ] **Step 4: Verify feature matrices and commit**

Run focused tests, the IEEE802154/6LoWPAN feature combinations, SLAAC combinations, fragmentation suites, MSRV check, and `build_16bit`. Commit `wire: validate compressed UDP and fragment bounds` and `iface: reject invalid SLAAC prefixes` if the changes review better separately.

### Task 8: Fix NDISC, IEEE 802.15.4 MIC, and MLDv2 timing

**Files:**
- Modify: `src/iface/interface/ipv6.rs`
- Modify: `src/iface/interface/tests/ipv6.rs`
- Modify: `src/wire/ieee802154.rs`
- Modify: `src/wire/mld.rs`
- Modify: `src/iface/interface/multicast.rs`

**Interfaces:**
- Produces: target-keyed Neighbor Advertisement cache updates, valid DAD handling, checked MIC slices, and RFC 3810 MRC decoding.

- [ ] **Step 1: Add failing regressions**

Reverse the source-keyed NA test to require the advertised target. Add a valid unspecified-source DAD NS and assert an unsolicited all-nodes NA with Solicited clear. Add truncated 4/8/16-byte MIC cases to `new_checked`. Add MRC decode boundaries `0x7fff`, `0x8000`, and `0xffff` plus scheduled-delay bounds.

- [ ] **Step 2: Verify RED**

Run each exact test and confirm the audited failure rather than a fixture error.

- [ ] **Step 3: Implement RFC behavior**

Use `target_addr` as the neighbor-cache key. Admit only valid DAD NS packets: unspecified source, no SLLAO, correct solicited-node destination; reply to `ff02::1` with Solicited clear. Include required MIC length in `check_len`. Decode MLDv2 floating MRC through one wire-layer helper used by interface scheduling.

- [ ] **Step 4: Verify and commit**

Run IPv6/NDISC/SLAAC/multicast and IEEE802154 feature tests plus full library tests. Commit by coherent protocol area.

### Task 9: Preserve every BPF record and restore TUN/TAP compatibility

**Files:**
- Modify: `src/phy/sys/bpf.rs`
- Modify: `src/phy/raw_socket.rs` if pending-record storage belongs in the adapter
- Modify: `src/phy/tuntap_interface.rs`
- Modify: `src/phy/sys/tuntap_interface.rs`
- Test: pure unit tests in the same PHY modules

**Interfaces:**
- Produces: internal BPF record iterator/pending buffer; `from_fd(RawFd, ...)` compatibility plus `from_owned_fd(OwnedFd, ...)`.

- [ ] **Step 1: Add failing synthetic BPF tests**

Construct two aligned Apple records and two BSD-layout records with padding. Assert sequential `recv` calls return exactly both payloads. Add truncated-header/payload and invalid-alignment cases that return an error/drop without merging records.

- [ ] **Step 2: Verify RED**

Run host-compilable pure decoder tests. Expected: Apple loses record two and BSD returns combined bytes.

- [ ] **Step 3: Implement pending-record iteration**

Keep the read buffer and validated cursor inside the BPF implementation. Parse one record per call, advance with the platform word-alignment rule, and issue a new read only after pending records are exhausted. Avoid per-packet allocation after initialization.

- [ ] **Step 4: Add failing descriptor ownership compile/runtime tests**

Test both constructor names with pipe/socket descriptors. Verify `from_owned_fd` consumes ownership and the compatibility constructor duplicates the descriptor so caller ownership remains explicit and double-close is impossible.

- [ ] **Step 5: Implement, cross-check, and commit**

Add the compatibility adapter and ownership-explicit path. Run host PHY tests and `cargo check` for `x86_64-apple-darwin` and `aarch64-apple-ios`; do not open a live BPF device. Commit `phy: preserve batched BPF records` and `phy: restore raw-fd TUN/TAP compatibility`.

### Task 10: Strengthen constrained-memory harnesses and compare performance

**Files:**
- Modify: `examples/profile_loopback.rs`
- Modify: `examples/dynbuf_memcompare.rs`
- Modify: `ci.sh`
- Modify: `.github/workflows/test.yml`
- Modify: `.github/workflows/fuzz.yml`
- Create: `docs/perf/2026-07-18-after.md`
- Modify: `FORK.md`, `README.md`

**Interfaces:**
- Produces: truthful raw-RSS/pool/allocator reporting, complete local `ios-full-gate`, required controller netsims, all-target fuzz build/smoke, and matched before/after result report.

- [ ] **Step 1: Add harness-output tests before reporting changes**

Extract/test pure memory-report calculations so no code subtracts allocated capacity from RSS. Add argument/shape tests for every supported traffic shape and a command-list test for the full iOS matrix.

- [ ] **Step 2: Verify RED and correct reporting**

Observe the existing idle-hot test validates capacity subtraction. Replace it with raw RSS plus separately labeled lane-reserved virtual capacity, pool charge, and allocator counters. Keep macOS physical footprint and Linux RSS semantics explicit.

- [ ] **Step 3: Extend existing gates, not frameworks**

Add an `ios-full-gate` command that runs dynamic tests, both sizechecks, 300/1,000 idle comparisons, idle-only, idle/hot, churn, multi TCP echo/sink, many TCP fair/stress, many UDP, software/offload variants, and all controller netsims serially. Reuse the downstream `rss_budget_combined`, `rss_budget_tcp_pressure`, and `rss_budget_udp_pressure` tests for mixed TCP/UDP and pressure shapes instead of duplicating them in smoltcp.

- [ ] **Step 4: Make CI evidence mandatory**

Remove `continue-on-error` from netsim, include Reno/CUBIC snapshots after their explained update, add `cargo check` for `x86_64-apple-darwin` and `aarch64-apple-ios`, build all fuzz targets, and smoke the broad parser/roundtrip targets with seeded corpora. Keep noisy performance comparison local rather than a shared-runner hard threshold.

- [ ] **Step 5: Run the complete matched after-matrix**

Repeat every Task 1 command with the same host, build, duration, flow counts, and five samples. Compute paired medians. Block and redesign any socket-size increase, exact accounting/allocation increase, repeatable RSS regression, throughput loss >2%, fairness <0.95, starvation, fallback allocation, unbounded trace, or nonzero post-churn pool.

- [ ] **Step 6: Document results and commit**

Write exact before/after tables including noise and known limitations. Correct README/FORK capability, upstream, pool-bound, controller, and harness claims. Commit `harness: gate constrained-memory traffic shapes` and `docs: record smoltcp fix performance results`.

### Task 11: Update the consumer pin, run full verification, review, and land

**Files:**
- Modify: `/root/tunnel-lib-rust/Cargo.toml`
- Modify: `/root/tunnel-lib-rust/Cargo.lock`
- Modify downstream files from Tasks 5-6 already committed locally

**Interfaces:**
- Consumes: final pushed smoltcp commit.
- Produces: exact consumer dependency pin and fully verified/pushed main branches.

- [ ] **Step 1: Run smoltcp's complete fresh verification**

Run format, diff check, MSRV/stable/nightly test/check matrices, clippy, 16-bit build, coverage, NoControl/Reno/CUBIC netsim, `ios-full-gate`, profile matrix, fuzz build/smoke, Apple target checks, and release sizechecks. Confirm only intended files changed.

- [ ] **Step 2: Dispatch broad final code review and fix every Critical/Important finding**

Create the full review package from `fd94836` to final HEAD. The reviewer checks the approved spec, every task report, tests, public compatibility, RFC behavior, unsafe/FFI bounds, and performance evidence. One fix subagent handles the complete actionable list and reruns affected tests; re-review until clean.

- [ ] **Step 3: Push smoltcp main and read back the remote SHA**

Push only after the final review and gates. Verify `origin/main` equals local HEAD.

- [ ] **Step 4: Update the downstream exact git pin and lockfile**

Point the consumer dependency at the landed smoltcp main revision, update `Cargo.lock`, and verify the resolved source contains that exact SHA. Do not use a path override as final evidence.

- [ ] **Step 5: Run downstream correctness and constrained-memory gates**

Run `cargo test -p netstack --release --test rss_budget_tcp --test rss_budget_combined --test rss_budget_tcp_slow --test rss_budget_tcp_rst --test rss_budget_tcp_pressure --test rss_budget_udp_pressure --test rss_budget_tcp_pool`, the netstack unit/integration suites, Apple-facing feature checks, formatting, clippy, and the repository's required CI command. Repeat focused throughput/RSS samples affected by the clock/receive changes.

- [ ] **Step 6: Commit, final-review, and push the consumer**

Commit the call-site/clock changes and dependency pin intentionally, dispatch a final downstream diff review, fix actionable findings, push main, and verify remote readback.

- [ ] **Step 7: Final result report**

Report landed SHAs, all test counts/gates, before/after medians for every shape, socket sizes, RSS/allocation/pool outcomes, controller snapshot explanation, fuzz executions, Apple/16-bit coverage, and any explicitly unavailable live-device/Miri evidence.
