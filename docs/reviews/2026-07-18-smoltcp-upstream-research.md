# smoltcp fork full audit and upstream comparison (2026-07-18)

## Scope and source discipline

This review uses only first-party repository evidence: the pinned Git history,
`FORK.md`, `README.md`, `Cargo.toml`, the fork source, and the fetched canonical
upstream source/history.

- Fork/base comparison: `e347a1e2d3ac33c5ce2c0c114e24b85ae23c4897...300c709a9717cb5b932458891540c5b1ad9b4c5a`.
- Fork HEAD and `origin/main`: `300c709a9717cb5b932458891540c5b1ad9b4c5a`.
- Base/tag/merge-base `v0.13.1`: `e347a1e2d3ac33c5ce2c0c114e24b85ae23c4897`.
- Latest fetched canonical `upstream/main`: `98fd77c723b945d5dcdec6ff438569a7c9c8e625`.
- Remotes: `origin = https://github.com/xoridius/smoltcp.git`, `upstream = https://github.com/smoltcp-rs/smoltcp.git`.
- Worktree was clean before research. No product source was changed.

The fork has 66 commits after `v0.13.1`; canonical upstream has 71 commits not
reachable from the fork (19 merge commits plus 52 content commits). The fork
diff touches 58 files and is `+12,372/-900`, which is useful context for the
guide's “small set of additive changes” description (`FORK.md:7-10`). Line
references below are against fork HEAD unless explicitly marked upstream.

## Executive conclusions

1. **Backport upstream PR #1177, then migrate the shipping consumer to it.**
   Fork src/time.rs:105-110 converts std::time::Instant with other.elapsed(),
   reversing ordering and keeping repeated current samples near zero. Upstream
   commits 11fb2c2... and c541dce... add the regression and stable process
   referential. The Apple consumer currently bypasses that conversion and calls
   smoltcp::time::Instant::now(), which is backed by SystemTime and separately
   violates the type's monotonic contract. Backport
   [#1177](https://github.com/smoltcp-rs/smoltcp/pull/1177), then change interface
   construction, poll_delay, and poll to use converted std::time::Instant values.
2. **Resolve the congestion-controller state before the next upstream sync.**
   `FORK.md:1263` says `reno.rs` and `cubic.rs` are verbatim upstream except IW10
   and explicitly says the earlier `u32` field shrink was dropped. HEAD still
   stores Reno's window fields as `u32` (`src/socket/tcp/congestion/reno.rs:19-23`)
   and CUBIC's as `u32` (`src/socket/tcp/congestion/cubic.rs:43-48`), with
   conversion helpers and many corresponding implementation/test differences.
   `git diff 98fd77c... -- src/socket/tcp/congestion/{reno,cubic}.rs` is extensive,
   not a one-hunk IW10 delta. Blame traces both the contrary documentation and
   much of the live `u32` implementation to squash commit
   `95c38ba4261742143643a8e8b450d2932b79eb48`. Treating the files as verbatim
   during a sync would overwrite real fork behavior or cause an incorrect
   conflict resolution.

3. **Fix and extend the IW10 implementation.** Both controllers claim the exact
   RFC 6928 formula but assign `cwnd = max(old_cwnd, IW10)`
   (`reno.rs:125-135`, `cubic.rs:283-294`). A newly created controller starts at
   2,048 bytes (`reno.rs:32-41`, `cubic.rs:57-76`), so a SYN advertising the
   accepted minimum MSS 48 (`src/socket/tcp.rs:584-587,2438-2445`) leaves an
   initial window of 2,048 bytes instead of `min(10*48,max(2*48,14600)) = 480`.
   The executable send path then admits roughly 42 MSS before an ACK rather than
   10. Reuse is worse: `Socket::reset` resets RTT/TCP state but not
   `congestion_controller` (`src/socket/tcp.rs:1337-1393`), and `listen`/`connect`
   call that reset (`src/socket/tcp.rs:1426-1431,1517-1525`), so a large old cwnd
   can carry into a new connection. Existing IW10 tests cover MSS 536/1460 only
   (`reno.rs:502-512`, `cubic.rs:738-744`) and do not cover reuse.

4. **Qualify or enforce the dynamic pool's memory guarantee.** The iOS guidance
   says the 24 MiB pool “bounds the worst case” and that exhaustion becomes
   throughput rather than jetsam (`FORK.md:1157-1173`). Pool accounting charges
   logical buffer-capacity increments (`src/socket/tcp.rs:906-925`), but growth
   allocates and fills a new `Vec` while the old `Vec` is still live, then swaps
   it in (`src/storage/ring_buffer.rs:445-465`). Peak allocation is consequently
   old capacity plus new capacity for that grow, and `try_reserve_exact` may also
   return allocator capacity above logical length. The pool bounds steady-state
   logical lengths, not transient or allocator-resident bytes. For the recommended
   64 KiB per-direction cap this is bounded but nonzero; for the public 1 GiB cap
   it can be material. The document and iOS gate should state/test the peak model.

5. **Refresh upstream triage now.** Section 16 handles upstream through #1164
   and deliberately skips three overlapping areas, but seven later PRs are still
   NEW: #1169, #1170, #1172, #1173, #1175, #1176, and #1177. The shipped
   `tools/upstream-delta.sh:38-78` will correctly flag them because none is named
   in §16.

## Validated full-audit findings

No P0 was found. “P1” means fix before relying on the affected feature or before
the next fork sync; not every P1 is exposed by the current Apple data plane.

### P1: high-priority correctness and compatibility

| Finding | Evidence and fix | Relationship and exposure |
|---|---|---|
| TCP ACK/SACK accepts queued but never transmitted bytes | ACK upper bound is SND.UNA + tx_buffer.len() at src/socket/tcp.rs:2182-2204; sender SACK also bounds against the whole queue at 2919-2940. A peer can dequeue unsent bytes or move the recovery cursor past them and idle the RTO. Add a monotonic per-connection SND.NXT/RFC 6675 HighData, distinct from the rewindable recovery cursor, and validate both ACK and SACK against it. | Cumulative-ACK defect is inherited and remains upstream; the SACK consequence is fork-only. The shipping consumer queues up to 16 KiB between polls and uses CUBIC/SACK, although crafted local L3 ingress is required to trigger silent loss. |
| Hosted clocks are not monotonic | Instant::now() uses SystemTime at src/time.rs:64-73; From<std::time::Instant> is broken at 105-110. Backport #1177 and move the Apple driver to converted monotonic timestamps. | #1177 fixes the conversion only; latest upstream still has wall-clock now(). This is directly on the shipping driver timer path. |
| Congestion state crosses connections; IW10 is wrong for small MSS | Socket::reset omits the controller (src/socket/tcp.rs:1337-1394); Reno/CUBIC use max(old, IW10) (reno.rs:125-135, cubic.rs:283-294). Fresh MSS 48 therefore opens 2,048 bytes/~42 segments instead of 480 bytes/10. Reset the selected controller at every new TCB boundary and assign exact IW10. | Reset defect remains upstream; IW10 behavior is fork-only. The current consumer uses fresh sockets, but crafted tiny-MSS SYNs remain exposed. |
| Default-checksum 6LoWPAN UDP is reconstructed with zero checksum and then rejected | src/iface/interface/sixlowpan.rs:788-810 ignores the NHC checksum and src/wire/udp.rs:280-287 writes zero; IPv6 UDP rejects zero at 249-260. Preserve inline checksums and compute elided checksums after complete reassembly; test fragmented/nonfragmented socket delivery with default capabilities. | Fork-specific interaction caused by the fork's correct IPv6 zero-checksum rejection. Not enabled by the medium-IP consumer. |
| SLAAC Prefix Information can panic interface polling | Validation omits prefix_len <= 128 (src/wire/ndiscoption.rs:408-412) before Ipv6Cidr::new asserts (src/iface/slaac.rs:169-195, src/wire/ipv6.rs:257-266). Reject 129-255 and add full ingress RA tests. | Inherited and still upstream; not enabled by the current Apple feature set. |
| IPv4 reassembly arithmetic can overflow on 16-bit targets | Last-fragment size and offset + data.len() are unchecked at src/iface/interface/ipv4.rs:123-128 and src/iface/fragmentation.rs:141-153. Use checked_add, enforce the 65,535-byte IPv4 limit, and unit-test the checked helper independent of host pointer width. | Inherited and still upstream. Compile-only 16-bit CI cannot exercise it. |
| BPF batched reads discard or combine records | Apple drops every record after the first; BSD includes padding/later headers in one frame (src/phy/sys/bpf.rs:182-188,274-292). Retain the read buffer and iterate validated aligned records across receives; add pure two-record/padding/truncation tests for both layouts. | Inherited batching defect; not used by the current medium-IP tunnel, but P1 for promised BSD/macOS raw-socket support. |
| TunTapInterface::from_fd is a silent API break | Fork changed RawFd to OwnedFd at src/phy/tuntap_interface.rs:40 while still reporting 0.13.1. Preserve a duplicating from_fd(RawFd, ...), add from_owned_fd, and compile-test both—or version the break. | Fork-only; base/latest retain RawFd. Not used by the current consumer. |

### P2: next correctness and evidence tranche

- Partial ACK recovery is tied to a nonempty SACK scoreboard and can exit
  Reno/CUBIC below RecoveryPoint (src/socket/tcp.rs:2693-2765). Determine
  recovery solely from RecoveryPoint and add a last-SACK-range regression.
- PAWS never expires TS.Recent and treats TSval zero as invalid
  (src/socket/tcp.rs:2216-2232,2769-2777). Store explicit validity and
  observation time; expire after 24 days per RFC 7323 section 5.5.
- Closure-based Socket::recv can drain terminal RX without the dynamic release
  hooks used by recv_slice (src/socket/tcp.rs:1813-1869). Reclaim at the next
  safe non-borrowing mutable boundary or add a non-borrowing drain API.
- Neighbor Advertisements cache the IPv6 source rather than the advertised
  target (src/iface/interface/ipv6.rs:459-474); valid IPv6 DAD solicitations
  are also discarded at 189-202.
- IEEE 802.15.4 new_checked does not include the security-level MIC length
  before the public MIC accessor subtracts it (src/wire/ieee802154.rs:305-347,
  677-690).
- MLDv2 floating Maximum Response Code is treated as literal milliseconds
  (src/iface/interface/multicast.rs:520-531), not RFC 3810 exponent/mantissa.
- The dynamic pool bounds charged steady-state logical capacity, not transient
  old-plus-new vector capacity during growth. Narrow the jetsam claim or
  measure and reserve the real allocator peak.
- Controller netsims are advisory and omitted from the aggregate gate; their
  committed Reno and CUBIC snapshots are stale. Both failed deterministically
  and serially in this audit at 4-32 KiB buffers.
- “iOS” gates run only on Ubuntu host targets; fuzz CI runs one target; the
  round-trip and TCP-header fuzz oracles/corpora are incomplete.
- README/FORK claims about receiver SACK, timestamps, controller parity, hard
  pool bounds, and cherry-pick history do not match the source.

## Deliberate component coverage

| Component | Audit result |
|---|---|
| TCP state/ACK/SACK/timers/retransmission/PAWS | P1 sent-high-water; P2 recovery and PAWS. Principal process, dispatch, and timer paths reviewed. |
| Reno/CUBIC/NoControl | P1 reset/IW10; P2 recovery exit. No additional P0-P2 in NoControl. |
| Dynamic buffers/pool/storage | P2 closure drain and peak-memory evidence. Accounting, allocation, growth preservation, Drop/refund, Assembler, RingBuffer, and PacketBuffer otherwise clean. |
| Other sockets | DHCPv4, DNS/mDNS, ICMPv4/v6, raw, UDP, socket module, and async wakers had no additional P0-P2. |
| IPv4/IPv6 interface, NDISC/SLAAC, multicast | P1 SLAAC/16-bit fragmentation; P2 NA/DAD/MLDv2. Routes, address selection, socket set/meta, and ordinary poll/dispatch had no additional finding. |
| Wire protocols | P1 6LoWPAN/UDP; P2 IEEE 802.15.4 MIC. Ethernet, ARP, base IPv4/IPv6, ICMP, DHCP/DNS, IPsec AH, IGMP, and RPL had no additional finding. |
| PHY/platform/time | P1 BPF, clocks, and FD API. Other PHY middleware and Linux raw/TUN/TAP cleanup/ownership paths had no additional finding. |
| Build/config/macros/examples/benches/fuzz/CI | No additional production issue; evidence gaps above. MSRV examples, generation review, shell syntax, and all fuzz-target builds passed. |

## Production consumer impact

The fork is shipping infrastructure: /root/tunnel-lib-rust/Cargo.lock:6498-6500
pins 300c709...; Apple defaults select the user netstack and its
constrained-memory profile, which creates 32 KiB dynamic socket maxima backed by
a shared 24 MiB pool. Each admitted SYN creates a fresh CUBIC socket and the
driver supplies a custom Medium::Ip device.

The immediate shipping order is therefore:

1. add a transmitted high-water bound for ACK and sender SACK;
2. backport #1177 and migrate all three interface timer sites to monotonic
   std::time::Instant conversion;
3. centralize or safely defer terminal dynamic-RX reclamation;
4. correct exact IW10/controller reset;
5. fix BPF/6LoWPAN/SLAAC/16-bit paths according to retained feature support,
   even though they are outside the current tunnel plane.

## Verification performed

- Release library: 716 passed; dynamic-buffer release library: 761 passed.
- Runtime core with dynamic buffers + CUBIC + Reno: 787 passed.
- TRACE=0 ./ci.sh quick and MSRV clippy: passed.
- TRACE=0 ./ci.sh test msrv: all 25 feature combinations passed.
- NoControl netsim: passed.
- CUBIC and Reno netsims: failed deterministically against stale snapshots;
  generated .snap.new files were removed without altering committed files.
- iOS gate: 42 dynamic-buffer tests and size check passed; 300 idle sockets
  charged 0 KiB and measured about 0.57 KiB/flow RSS.
- One-second profile smoke: UDP, many-flow fairness, and idle/hot shapes passed.
- All fuzz targets and logging variant built; 10-second ASan wire_parsers smoke
  completed 1,264,707 executions without a crash.
- cargo fmt --check and git diff --check passed before consolidation.
- Miri was unavailable on the installed toolchain. Live BPF/device tests were
  intentionally not run under FORK.md section 3.4.

## Why the fork exists and who it serves

The general relationship statement names four categories: RFC fixes, hosted
wire-path performance, Darwin/BSD phy hardening, and an in-process profiling
harness (`FORK.md:5-10`). The concrete consumer is a hosted Apple packet-tunnel
client, not a new bare-metal stack architecture:

- Dynamic buffers are explicitly for “memory-constrained hosts that admit many
  concurrent flows,” especially packet-tunnel clients (`FORK.md:998-1003`).
- The recommended deployment is an `NEPacketTunnelProvider`-style consumer under
  a claimed 50 MB extension memory limit, with a shared 24 MiB pool and 64 KiB
  per-direction maxima (`FORK.md:1137-1153`).
- Zero-initial buffers admit many idle DNS/keep-alive flows cheaply
  (`FORK.md:1168-1173`), while CUBIC plus SACK is recommended for lossy cellular
  paths (`FORK.md:1175-1178`).
- `Cargo.toml:81-83` keeps this behavior opt-in behind
  `socket-tcp-dynamic-buffer = ["socket-tcp", "alloc"]`; the documented default
  remains the fixed-buffer, allocation-free model (`README.md:8-17`).
- Darwin/BSD BPF work supports hosted Apple validation and raw-device use, but
  the iOS-shaped CI feature set is medium-IP and does not enable host BPF
  (`ci.sh:11-13`). It is fork-scope production hardening, not the core memory
  mechanism for the packet-tunnel consumer.

## Essential production work versus evidence/performance harness

### Consumer/fork production behavior

| Area | Evidence and role |
|---|---|
| Dynamic TCP buffers | `3711cfc31943312874b3f852ff46ecf5d988058f` introduced `MemoryPool`, `DynamicBufferConfig`, and `Socket::new_dynamic`; the live API/config is at `Cargo.toml:79-84`, `src/socket/tcp.rs:721-801`, and `src/socket/tcp/dynbuf.rs:54-341`. This is the defining iOS-concurrency change. |
| Sender SACK recovery | `6bec4a59e7b42ab63a60a07230a08883aa855db2` adds the scoreboard/recovery point. Live state and dispatch are at `src/socket/tcp.rs:533-546,2640-2765,2907-3016,3312-3385,3541-3555`. This targets lossy cellular throughput. |
| Congestion control / RFC behavior | IW10 (`46b169ded3127d71131b5219e5732285a9873f55`), PAWS (`61cd55019643b43c62c8eda3b0de8b4822cda644`), SYN window scaling, effective-MSS/OOW backports, and upstream's Reno/CUBIC redesign materially change TCP behavior. |
| Wire hot path | `dc75b44e89f66e8f6918f3b90beca6bb66924a82` and `b147f88a6ff5208149d71868a0259c7ee686f33d` replace checksum/pseudo-header work (`src/wire/ip.rs:762-925`); `7a68a60ed1fe8ce31d3ec23eec539f0ae73bf61b` fixes big-endian correctness. This is production tunnel throughput. |
| Host phy and input hardening | Panic-to-drop/log changes and Apple BPF ownership/sizing culminate in `21112487881c485a57222a8ca02ddd4185303e5d` and `300c709a9717cb5b932458891540c5b1ad9b4c5a`. 6LoWPAN/RPL/IPsec/IPv6/UDP validation commits are also production correctness, though not central to the iOS medium-IP use case. |
| Footprint/static dispatch | `36e15e0534abca3d158955bd194161e1c6dcc43c`, bool packing, and the still-live `u32` controller fields reduce layout/dispatch cost. These are production performance changes, but §16 incorrectly says part of this surface was dropped. |

### Diagnostic/evidence-only work

| Area | Evidence and role |
|---|---|
| End-to-end profiler | `cf07828549b4911107ec66b51de9337088d76b8c` and follow-ups created `examples/profile_loopback.rs` (3,285 added lines in the base-to-fork diff). It measures shapes; it is not library behavior. |
| Dynamic-memory comparison | `f210db2e969435f0d04fbf6b7db9c53503f03352` adds `examples/dynbuf_memcompare.rs`; §14.6 calls it evidence/smoke. |
| Microbench/size diagnostics | `benches/bench.rs` and `tests/sizecheck.rs`; §12 explicitly calls sizecheck diagnostic and says not to upstream it (`FORK.md:865-875`). |
| Fuzz/property/Miri lanes | `be3f6e25dacc363c3e49c3d54064cfaf0f33a641` plus later fuzz changes. These protect production parsers but do not ship runtime behavior. |
| CI, snapshots, docs, delta tooling | `ci.sh`, `tests/snapshots/*`, the 1,277-line `FORK.md`, and `tools/upstream-delta.sh` are maintenance/evidence infrastructure. LTO profile `2c7a881ba4846ded67a334bdd080ed519560a2a0` is build/performance policy, not a protocol feature. |

## Documentation and history claims versus code

| Claim | Finding |
|---|---|
| “Small set of additive changes” and “no architectural divergence” (`FORK.md:7-10`) | Overstated. The exact fork diff is +12,372/-900 across 58 files; it adds an alternate buffer-ownership/accounting model and a new sender recovery scoreboard inside the central TCP state machine. Public APIs are additive, but the implementation is materially divergent. |
| Reno/CUBIC verbatim except IW10; `u32` shrink dropped (`FORK.md:1263`) | False at HEAD. See `reno.rs:5-23` and `cubic.rs:9-48`; the pinned upstream-vs-fork diff is extensive. This is the most consequential maintenance-document error. |
| Changes in §16 “were cherry-picked” and each fork commit names its source (`FORK.md:1250-1254`) | They are semantic/squashed backports in `95c38ba4261742143643a8e8b450d2932b79eb48`, not one auditable fork commit per upstream PR. `git cherry -v 300c709... 98fd77c...` reports every upstream content commit as patch-ID `+`, including the entries called verbatim. Several final files (`build.rs`, `src/macros.rs`, `src/storage/packet_buffer.rs`) do match upstream, but the history wording is inaccurate. |
| Receiver-side SACK generation is not implemented (`README.md:132-133`) | False. `Socket::ack_reply` emits SACK ranges from the receive `Assembler` whenever the peer negotiated SACK (`src/socket/tcp.rs:2005-2045`). This receiver behavior predates the fork; commit `21112487881c485a57222a8ca02ddd4185303e5d` introduced the incorrect README wording while describing the new sender consumption. |
| Timestamping is not supported (`README.md:136`) | False/obsolete. The socket exposes `set_tsval_generator`/`timestamp_enabled` (`src/socket/tcp.rs:1039-1047`), generates timestamp replies (`1983-1987`), negotiates/disables them during handshakes (`2462-2466,2511-2515`), and the fork adds PAWS processing/tests. |
| Dynamic sockets with nonzero initial sizes are preserved through `listen`/`connect` (`FORK.md:1062-1065`) | Only conditional. `restore_dyn_initial_buffers` returns without restoring if the pool refuses the charge (`src/socket/tcp.rs:958-979`), and constructor allocation can similarly fall back to zero (`769-789`). The claim needs “when pool and allocation permit.” |
| SACK is “zero-cost on lossless connections” (`FORK.md:1189-1191`) | Only true in the narrow data-path sense. Empty-scoreboard branches avoid the walk, but every socket unconditionally carries `sack_scoreboard` and `recovery_point` (`src/socket/tcp.rs:533-546`), and ACK/dispatch sites still test the state. §15 itself later acknowledges +64 B scoreboard storage (`1193-1200`). |
| 24 MiB pool bounds the worst case (`FORK.md:1157-1173`) | False for peak/allocator-resident memory; true only for charged logical steady-state lengths. See the double-live-`Vec` growth path discussed above. |

## Spec requirements missing, partial, extra, or apparently wrong

### Missing/partial

- **No exact IW10 invariant across all accepted MSS values or socket reuse.** The
  tests named by §16 cover only ordinary MSS values; the accepted 48-byte minimum
  and a reused socket are not covered. The implementation fails both cases as
  described in Executive conclusion 3.
- **The dynamic-memory proof does not measure the stated worst-case peak.**
  `ios_gate` runs dynamic tests, sizecheck, and a 300-socket steady-state example
  (`ci.sh:177-181`), but no lane asserts peak allocation during geometric growth,
  the exact place where old/new vectors coexist.
- **The documented landing gate is not embodied by one command.** §3 says every
  listed command, including the nightly benchmark, must pass before main
  (`FORK.md:69-97`), and §14.7 adds Miri plus separate legacy/dynamic examples
  (`1091-1102`). `ci.sh all` runs test/check/clippy/16-bit/coverage/NoControl
  netsim only (`ci.sh:201-208`); it omits the benchmark, `ios-gate`, Miri,
  fuzz, and the separate legacy comparison. Separate commands exist for some of
  them, so this is an enforcement/evidence gap rather than proof the tests fail.
- **Upstream triage is stale by seven PRs.** §2 promises §16 as the handled/skip
  ledger (`FORK.md:39-64`), but the ledger stops before #1169 while the pinned
  upstream is through #1177.

### Fork behavior not requested by the current guide

- The live `u32` Reno/CUBIC representation and its conversion/saturation helpers
  are the clearest extra behavior: §16 expressly says this optimization was
  dropped. It should either be removed to match the guide or re-documented and
  assessed as an intentional divergence.
- The fork makes sender SACK always-on rather than feature-gated, increasing the
  socket/state-machine surface for all TCP users. §15 documents that choice, so
  it is not hidden, but it sits uneasily with §1's “small/additive/no divergence”
  constraint and should be treated as an explicit architectural exception.
- The extensive profiler, dhat dependency/feature, LTO profile, and size/fuzz
  infrastructure exceed the packet-tunnel runtime requirement. They are justified
  as evidence tooling, but should not be confused with consumer-essential code.

### Implementation that appears wrong

- IW10 uses a floor (`max(old_cwnd, IW10)`) rather than establishing the initial
  window, fails at small MSS, and preserves old per-connection congestion state
  across socket reuse. This has a direct send-volume path through
  `cwnd_remaining` (`src/socket/tcp.rs:3011-3016`) and dispatch's send-size clamp
  (`3522-3537`).
- The pool's public/documented safety property is stronger than its allocator
  implementation. Either grow in place with accounting based on actual capacity,
  reserve transient headroom, or narrow the guarantee and test the real peak.
- README's SACK and timestamp capability statements are factually opposite to
  the source and can cause a consumer to make the wrong feature/interoperability
  decision.

Known-but-documented exclusions are not findings here: receiver SWS avoidance is
explicitly acknowledged as upstream behavior (`FORK.md:821-828`), and the
dispatch half of upstream #1155 is deliberately displaced by the fork's SACK
selector (`FORK.md:1265-1274`).

## Complete post-v0.13.1 upstream classification

The table accounts for all 71 canonical commits after `v0.13.1`: each row names
the merge commit and every content commit belonging to that PR. “Handled” means
the behavior is represented in the fork; it does **not** endorse §16's verbatim
wording.

| PR | Exact merge/content commits | Status at fork HEAD | Evidence/action |
|---|---|---|---|
| [#1162](https://github.com/smoltcp-rs/smoltcp/pull/1162) | merge `0c55b2dd361d67a4afc9ac9a8a3adf34dd8485f9`; content `9d532a7b83758a63750daaccd0f42037af4babaa` | Handled/backported | OOW data ACKs not rate-limited; §16 row `FORK.md:1261`, fork squash `95c38ba4261742143643a8e8b450d2932b79eb48`. |
| [#1164](https://github.com/smoltcp-rs/smoltcp/pull/1164) | merge `ee997804ae0918fe07b4c95ce8bac4004047f9ce`; content `566128f807ae559eaa9c8f86807e0b972258cd77` | Handled/backported | More compliant OOW RST/data handling; grouped with #1162 in §16. |
| [#1159](https://github.com/smoltcp-rs/smoltcp/pull/1159) | merge `45c02fc7941bb1857cd61b0bfe99b2d079eec033`; content `e4b5b68179c9195882d58f7449cf4bae268767e7` | Handled/backported | Deterministic `BTreeMap` config generation; current `build.rs` matches pinned upstream for this file. |
| [#1143](https://github.com/smoltcp-rs/smoltcp/pull/1143) | merge `10caabc9ad41271018644bef0cd148bb8c2c6c3b`; content `a047297e3da70835dde4088ca8b9020f29c730ee`, `a35543e668bb226cb3ab7ac5a7d03017e9efb6b8`, `a73d91c7bd569408157a97689d3e7a98ca7b4c17` | Deliberately skipped | §16 says the fork carries its own fuzz suite (`FORK.md:1272`). Keep the skip, but compare API/build coverage when fuzz dependencies change. |
| [#1161](https://github.com/smoltcp-rs/smoltcp/pull/1161) | merge `a3c788e2a34973067be909d0812267095d5b9105`; content `1c1e41881937884ab9cf8e717545f429907ad8d2`, `16100ffb9b3683f1fa3bbf292badb60cd16fac43`, `b0b7bcfd266822c4b854e6add3be26a55b01307d` | Handled/adapted | Option-aware effective MSS plus degenerate MSS clamp; adapted for fork `u32` and SACK dispatch (`FORK.md:1262`). |
| [#1153](https://github.com/smoltcp-rs/smoltcp/pull/1153) | merge `6773432e19fccb9d0cae3c7149605e2f27118fb4`; content `af61932ef57acbda60bbd6421d7e11911c81548d`, `f3f77074bd39ae992ad02b5a9dc3106318f68dcb` | Deliberately skipped | Upstream multiflow netsim/snapshots do not fingerprint this fork (`FORK.md:1273-1274`). |
| [#1154](https://github.com/smoltcp-rs/smoltcp/pull/1154) | merge `ef81da35a4856e2ec3f4b544f888c6df31ed9db7`; content `260b2e3e1722aec4e49ca3f265fea90c542b5639`, `216a103d4b3bd04f6047eb4af0514cd3042ae69b`, `dbb897f3a79b5f0ac6753e29d330025289c7e5ba`, `2fdbc1175db82c39c6890e8eac41224c6e1b06f8`, `ba2ab35a583f3786b20cc31343322f3a750174eb`, `004ec9546139929f0258a8b971e546cba94143e4`, `058524be3b586772d80cbad9b0fe296e8a563224`, `521e722b0c58a401e176d82bdecb07e67d4d39f7`, `b7b09e72a7cc6a3b180deabb7d06157d181375b9` | Handled/adapted | New controller API, cwnd application, zero-window-probe exemption, tests/CI. Integrated with static dispatch and SACK; not patch-identical. |
| [#1156](https://github.com/smoltcp-rs/smoltcp/pull/1156) | merge `85c5bcc76c42464c28931b5537d9e89252bce4cb`; content `0f4f7ec1b77f0415655e2dfbd554f15f13bd7d9f`, `3cccd4a9d1637d5899eb0b3e339fb58111d2b2b6`, `252399726a2bfaa7898ea600f130d0b3e9f44223`, `8a616d2fabd73f45828203710de581ec1d1781b1`, `050f026307451e15b86b61a5e8872b48d5fef036` | Handled/adapted, documentation inaccurate | Reno fast recovery/RTO fixes exist, but the file is not verbatim because `u32` and IW10 remain. |
| [#1152](https://github.com/smoltcp-rs/smoltcp/pull/1152) | merge `94ce96c1a13bba592c81433a115e95206f90ad33`; content `f5a98f8c15846c9a8b6fbac435e22a98d054274a` | Handled/backported | `collapse_debuginfo` logging wrappers; current `src/macros.rs` matches pinned upstream for this file. |
| [#1150](https://github.com/smoltcp-rs/smoltcp/pull/1150) | merge `c9bc3c3fbcf1916a7049e61042de7766e5d05c47`; content `eb6893691c50fba1129fcc20ddfa69de28cfe344`, `1d7cdb84a98d2d3186e4def2c9e6b73bd4c17f51` | Handled/backported | Metadata is reserved before payload closure; current `src/storage/packet_buffer.rs` matches pinned upstream for this file. |
| [#1157](https://github.com/smoltcp-rs/smoltcp/pull/1157) | merge `8ef276709cd4a0721cd6d98c2ed6d4fede5dd394`; content `d7ecb7494680ada0d9896203848f76cd429af0f3`, `c414f07ebddc00e54c16ea4cffb2e89fe2839012`, `7fdcabe9890664c60bac02d6452c57e89a46a1dc`, `5a5676340376f0e6fe27a71e042ed7fcc1b5592b`, `f7aa7231a77a33d94b877900f553a7c0e68153d2`, `3301f78fa21ca9978623478805308d7d8519d479`, `c7595bc5aa0439d9852768edba9cc1fa6e1d7bcf`, `68bad6c051e8b107a0164477627fe84997956928`, `136b813badf8645505b26839b202451f5ce760a5`, `53e6cdfaecc41ecfecef24fdad27e9faca2e8c12` | Handled/adapted, documentation inaccurate | CUBIC/RRT/cube-root fixes exist, but the file is not verbatim because `u32`, overflow helpers, and IW10 remain. |
| [#1155](https://github.com/smoltcp-rs/smoltcp/pull/1155) | merge `d9582fc3cbc66d5b09b9d29649d4cb0d50a75358`; content `d84a898051a5c2a9067eeba9617ec9631f31f3b2`, `f3a64e081c4abf19435cf55ff74ffc6443cef2cd`, `29340b71ffca86f130746c7dd3c7a9a09495dda9`, `9fe54033a05c44a1321194d9cfb4d2a4a1df2e8c`, `24f83e979512e8222fc1b0f3754f0b2100a7cbab` | Partly handled, partly deliberately skipped | RTT estimator split (`29340b7...`) was adapted. Dispatch single-segment fast retransmit and associated tests/snapshot were skipped because the SACK hole walker owns that region (`FORK.md:1267-1271`). |
| [#1169](https://github.com/smoltcp-rs/smoltcp/pull/1169) | merge `898386e10165e0e5b9e17b9bfb8179c83980f775`; content `ec3dff448d4a035e1b77efbdbf4f6bacc389a907` | **NEW / untriaged** | Adds upstream contribution/LLM policy only. Decide explicitly whether fork governance adopts it; no runtime code. |
| [#1170](https://github.com/smoltcp-rs/smoltcp/pull/1170) | merge `217670cdc5ccf09c7dc308af6f1256e0d6c73eab`; content `340b0f06dde940551083e00e9c1017c2d747a7d7` | **NEW / untriaged** | Removes an accidental Amaranth reference from #1169. Only relevant if #1169 is adopted. |
| [#1172](https://github.com/smoltcp-rs/smoltcp/pull/1172) | merge `6774bcbd32cb4b591165b7f51fea8060f2849953`; content `6ce5cf4036a63b2f2d29de89d02c687007d216d2`, `b31b917a98f21e9254b5c03e177cfa1087b364ce` | **NEW / untriaged; adopt when API useful** | Re-exports `wire::checksum` and updates changelog. One-line code change in `src/wire/mod.rs`; no conflict with the fork algorithm. Useful to consumers that need discontiguous RFC 1071 sums. |
| [#1173](https://github.com/smoltcp-rs/smoltcp/pull/1173) | merge `764ef3a8cc38d543f407555772f495d4810c8895`; content `865acbcc5c8046689d0fb097306d71f6a9690c30` | **NEW / untriaged; low-risk adopt** | Fixes four broken rustdoc links in `interface/mod.rs`, `socket/dns.rs`, `socket/mod.rs`, and `wire/ip.rs`. Documentation only; minor textual overlaps. |
| [#1175](https://github.com/smoltcp-rs/smoltcp/pull/1175) | merge `d2d8abc048e5b298d1b14c65d44cb59c9e04aaa2`; content `9bca980e6312613d72c0e00caa5040877c4371c3` | **NEW / untriaged; recommended** | Replaces std-gated `std::error::Error` impls with unconditional `core::error::Error` across ten files. Valuable to no_std/alloc users; mechanical conflicts in heavily edited `tcp.rs`/`assembler.rs`, but no behavior conflict. MSRV 1.91 is sufficient. |
| [#1176](https://github.com/smoltcp-rs/smoltcp/pull/1176) | merge `4d32d47d6905dc41e8baa2f9c29ab7f0bc81639d`; content `ed28a5d8a19bd58a26da0354608e4a77ba14ba63` | **NEW / untriaged; benchmark, do not cherry-pick blindly** | Direct overlap with fork `checksum::data`. Upstream uses native-endian 4-byte manual unrolling and `u32`; fork uses 64-byte/two-`u64` chains plus big-endian handling (`src/wire/ip.rs:772-863`). Compare with the fork's existing cross-target/property/bench gates before choosing or combining implementations. |
| [#1177](https://github.com/smoltcp-rs/smoltcp/pull/1177) | merge `98fd77c723b945d5dcdec6ff438569a7c9c8e625`; content `11fb2c2cb2e8c7a52dc7024a27cb21884ca81b40`, `c541dce9917760adaec8a1043924b3b58801a4c3` | **NEW / urgent adopt** | Proven `std::time::Instant` conversion bug and regression test. No overlap; hand-port or cherry-pick both content commits, then run std/time and full §3 gates. |

No direct-to-main first-parent commits exist in the pinned range; all 71 commits
are accounted for by these 19 merge PRs.

## Upstream adoption order

1. Hand-port/cherry-pick #1177's test and fix first; it is isolated and has a
   first-party executable regression.
2. Before integrating more upstream TCP work, decide whether controllers should
   actually be upstream-verbatim or keep the `u32` fork. In the same change,
   reset per-connection controller state and make IW10 exact for all accepted MSS
   values; add minimum-MSS and socket-reuse tests.
3. Correct README SACK/timestamp claims and §16's backport/controller wording;
   add #1169-#1177 outcomes so `upstream-delta.sh` returns no NEW entries.
4. Adopt #1173 and #1175 as low-risk maintenance/API improvements; adopt #1172
   if external checksum access is useful to the consumer.
5. Evaluate #1176 as an algorithm alternative, not a normal cherry-pick. Run the
   fork checksum reference/property tests, big-endian build/codegen check, and
   the existing packet-size microbench on the same targets.
6. Make the iOS memory claim precise and add peak-allocation evidence before
   treating the pool budget as a hard jetsam bound.
