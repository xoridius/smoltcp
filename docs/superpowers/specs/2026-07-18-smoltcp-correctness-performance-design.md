# smoltcp Correctness and Constrained-Memory Performance Design

## Purpose

Land the important correctness, compatibility, and evidence fixes identified by
the 2026-07-18 fork audit while keeping the Apple packet-tunnel path at least as
fast and memory-efficient as the pinned baseline. The work covers the smoltcp
fork and the tunnel consumer that pins it.

## Scope

The implementation is split into three independently verifiable workstreams.

1. **Shipping core:** TCP transmitted-high-water tracking, monotonic hosted
   time, per-connection congestion reset and exact IW10, dynamic receive-buffer
   reclamation, SACK recovery completion, and PAWS expiry.
2. **Protocol and platform correctness:** 6LoWPAN UDP checksums, SLAAC prefix
   validation, checked IPv4 reassembly arithmetic, BPF record batching,
   TUN/TAP descriptor compatibility, Neighbor Advertisement cache keys, IPv6
   DAD, IEEE 802.15.4 MIC length validation, and MLDv2 response-code decoding.
3. **Performance evidence and landing:** deterministic correctness and netsim
   gates, constrained-memory traffic matrices, raw RSS and allocation reporting,
   Apple target checks, fuzz coverage, downstream consumer integration, and
   documentation/upstream ledger updates.

No unrelated refactoring, new dependencies, or speculative public interfaces
are allowed. Existing no-alloc/no-std configurations remain supported.

## Canonical protocol model

TCP sequence state follows RFC vocabulary. SND.UNA remains the oldest
unacknowledged sequence. A monotonic SND.NXT-equivalent records the first
sequence after data/control actually transmitted. The existing rewindable send
cursor remains recovery-local and must never be used as proof that a byte was
transmitted. Cumulative ACK and SACK validation are bounded by the monotonic
transmitted high-water value.

Each Reno or CUBIC controller belongs to exactly one TCP control block. Reset, reconnect,
re-listen, and failed passive handshakes reconstruct the selected controller
without changing the selected algorithm. IW10 is assigned exactly according to
RFC 6928 for every accepted MSS, rather than used as a lower bound on old state.

Recovery remains active until its RecoveryPoint is cumulatively acknowledged;
an empty SACK scoreboard does not end recovery. PAWS stores explicit TS.Recent
validity and observation time, accepts TSval zero, and expires stale state after
24 days as required by RFC 7323.

## Hosted time

Backport upstream PR #1177's stable conversion from std::time::Instant. The
shipping tunnel driver stops using wall-clock-backed smoltcp::time::Instant::now
and supplies converted std::time::Instant samples for interface construction,
poll_delay, and poll. Wall-clock conversion remains available only where its
absolute-time semantics are explicitly requested.

## Dynamic-buffer lifetime and memory model

The borrowing Socket::recv interface remains source-compatible. Immediate
reclamation is added through the smallest interface that can prove its return
value does not borrow the socket buffer, or through a safe subsequent mutable
boundary; the tunnel consumer uses that non-borrowing path without adding a
copy. Terminal sockets must refund pool charge promptly after drained data is no
longer borrowed.

Pool accounting continues to bound steady-state logical socket capacity. The
harness separately reports raw RSS, allocator deltas, and pool charge; it does
not subtract virtual capacity from resident memory. Documentation distinguishes
steady-state budget from transient allocator peak during grow. If transient
peak can be reduced without throughput loss, growth is changed accordingly;
otherwise the accurate bound is documented and gated.

## Wire and platform behavior

- 6LoWPAN NHC decompression preserves an inline UDP checksum and computes an
  elided checksum once the complete IPv6/UDP payload is available.
- SLAAC rejects Prefix Information lengths above 128 before CIDR construction.
- IPv4 fragment offsets and lengths use checked arithmetic and reject totals
  beyond 65,535 bytes, including on 16-bit targets.
- BPF reads retain their buffer and yield every validated aligned record in
  order on Apple and BSD layouts; malformed/truncated records are dropped
  without combining or losing adjacent valid records.
- TunTapInterface keeps a source-compatible RawFd constructor and adds an
  ownership-explicit constructor; ownership transfer/duplication is tested.
- Neighbor Advertisements update the advertised target, valid DAD solicitations
  receive the RFC-required response, IEEE 802.15.4 checked frames include MIC
  length, and MLDv2 response codes use RFC 3810 floating decoding.

## Test-first implementation

Every behavior change begins with a focused regression that fails for the
audited reason. Production code is added only after the failure is observed.
Focused tests run after each change, followed by the relevant feature matrix.
Each independent task is implemented by a fresh subagent and receives a separate
spec/code-quality review before it is accepted.

Required correctness gates include:

- transmitted-prefix plus queued-unsent ACK and SACK cases;
- controller reuse, absent MSS, MSS 48, failed passive handshake, Reno, and
  CUBIC cases;
- last-SACK-range partial ACK and PAWS zero/24-day-wrap cases;
- borrowing and non-borrowing terminal dynamic receive cases with exact pool
  accounting;
- fragmented/nonfragmented 6LoWPAN UDP delivery with default checksums;
- malformed SLAAC RA, 16-bit-independent fragment arithmetic, multi-record BPF,
  NA target, DAD, truncated MIC, and MLDv2 boundary cases;
- no-default-features, no-alloc/no-std, MSRV, stable, nightly, 16-bit compile,
  Apple cross-target, fuzz-build, and parser-fuzz lanes.

## Performance and RSS acceptance

A baseline is captured before production edits using the same build, host,
duration, flow counts, and feature sets used for the final comparison. Each
performance result uses warm-up plus repeated samples and compares medians.

Hard gates:

- TCP Socket size does not increase in default or iOS feature shapes.
- Dynamic idle pool charge remains zero.
- Steady-state pool charge, net heap delta, and allocation count do not increase
  for an equivalent workload. A raw-RSS increase blocks landing when its paired
  median exceeds both one host page and 1% and repeats in at least three of five
  matched samples; smaller movement is recorded as measurement noise.
- No median throughput regression greater than 2%. Any larger decrease blocks
  landing unless measurement variance is demonstrated with additional repeats
  and the user-approved requirement is still met.
- Fairness remains Jain >= 0.95 for many_tcp_fair, no flow starves, no packet
  lane fallback allocation appears, churn refunds the pool to zero, and memory
  traces remain bounded.

Traffic matrix:

- UDP firehose and TCP echo/sink, with checksum offload variants where supported;
- many_tcp, many_tcp_fair, and many_udp at small, medium, and high flow counts;
- multi_tcp and multi_tcp_sink;
- idle-only, idle_hot, churn, RST/unread-RX drain, pool-pressure saturation, and
  a mixed TCP/UDP constrained-memory shape;
- legacy versus dynamic buffers at 300 and 1,000 idle flows;
- NoControl, Reno, and CUBIC loss/buffer netsim sweeps;
- downstream rss_budget_tcp, combined, slow-reader, RST, pool, TCP pressure, and
  UDP pressure suites under the constrained Apple profile.

Missing shapes are added to the existing profile_loopback or downstream RSS
harness, not to a parallel benchmark framework. Machine-readable summaries may
be added only where needed for reproducible comparison; human-readable output
remains.

## Landing sequence

Work lands as small reviewed commits in dependency order: baseline/gates, TCP
core, hosted time and consumer migration, dynamic lifetime, protocol/platform
fixes, harness/CI improvements, then docs/upstream ledger. The smoltcp fork is
pushed first; the tunnel consumer then updates its exact git pin and lockfile,
runs its full constrained-memory matrix, and is pushed only after all gates pass.

If a correctness fix cannot meet the performance/RSS gates, landing stops for a
redesign; the defect is not hidden by weakening the gate or refreshing a
snapshot without explanation.
