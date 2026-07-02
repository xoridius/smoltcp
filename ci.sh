#!/usr/bin/env bash

set -euo pipefail

if [[ "${TRACE:-1}" != "0" ]]; then
    set -x
fi

export DEFMT_LOG=trace

MSRV="1.91.0"
IOS_FEATURES="alloc,medium-ip,proto-ipv4,proto-ipv6,socket-udp,socket-tcp,socket-tcp-dynamic-buffer,socket-tcp-cubic"
HOST_PHY_FEATURES="phy-raw_socket,phy-tuntap_interface,medium-ip,medium-ethernet,proto-ipv4,socket-raw"

RUSTC_VERSIONS=(
    $MSRV
    "stable"
    "nightly"
)

FEATURES_TEST=(
    "default"
    "std,proto-ipv4"
    "std,medium-ethernet,phy-raw_socket,proto-ipv6,socket-udp,socket-dns"
    "std,medium-ethernet,phy-tuntap_interface,proto-ipv6,socket-udp"
    "std,medium-ethernet,proto-ipv4,proto-ipv4-fragmentation,socket-raw,socket-dns"
    "std,medium-ethernet,proto-ipv4,multicast,socket-raw,socket-dns"
    "std,medium-ethernet,proto-ipv4,socket-udp,socket-tcp,socket-dns"
    "std,medium-ethernet,proto-ipv4,proto-dhcpv4,socket-udp"
    "std,medium-ethernet,medium-ip,medium-ieee802154,proto-ipv6,multicast,proto-rpl,socket-udp,socket-dns,auto-icmp-echo-reply"
    "std,medium-ethernet,proto-ipv6,socket-tcp"
    "std,medium-ethernet,proto-ipv6,socket-tcp,proto-ipv6-slaac"
    "std,medium-ethernet,medium-ip,proto-ipv4,socket-icmp,socket-tcp"
    "std,medium-ip,proto-ipv6,socket-icmp,socket-tcp"
    "std,medium-ieee802154,proto-sixlowpan,socket-udp,auto-icmp-echo-reply"
    "std,medium-ieee802154,proto-sixlowpan,proto-sixlowpan-fragmentation,socket-udp,auto-icmp-echo-reply"
    "std,medium-ieee802154,proto-rpl,proto-sixlowpan,proto-sixlowpan-fragmentation,socket-udp,auto-icmp-echo-reply"
    "std,medium-ip,proto-ipv4,proto-ipv6,socket-tcp,socket-udp"
    "std,medium-ethernet,medium-ip,medium-ieee802154,proto-ipv4,proto-ipv6,multicast,proto-rpl,socket-raw,socket-udp,socket-tcp,socket-icmp,socket-dns,async,auto-icmp-echo-reply,proto-ipv6-slaac"
    "std,medium-ip,proto-ipv4,proto-ipv6,multicast,socket-raw,socket-udp,socket-tcp,socket-icmp,socket-dns,async"
    "std,medium-ieee802154,medium-ip,proto-ipv4,socket-raw,auto-icmp-echo-reply"
    "std,medium-ethernet,proto-ipv4,proto-ipsec,socket-raw"
    "alloc,medium-ethernet,proto-ipv4,proto-ipv6,socket-raw,socket-udp,socket-tcp,socket-icmp,proto-ipv6-slaac"
    "std,medium-ip,proto-ipv4,proto-ipv6,socket-tcp,socket-tcp-dynamic-buffer"
    "alloc,medium-ethernet,proto-ipv4,proto-ipv6,socket-raw,socket-udp,socket-tcp,socket-icmp,proto-ipv6-slaac,socket-tcp-dynamic-buffer"
    "alloc,medium-ethernet,proto-ipv4,proto-ipv6,socket-raw,socket-udp,socket-tcp,socket-icmp,proto-ipv6-slaac,socket-tcp-cubic,socket-tcp-reno"
)

FEATURES_CHECK=(
    "medium-ip,medium-ethernet,medium-ieee802154,proto-ipv6,proto-ipv6-slaac,multicast,proto-dhcpv4,proto-ipsec,socket-raw,socket-udp,socket-tcp,socket-icmp,socket-dns,async"
    "defmt,medium-ip,medium-ethernet,proto-ipv6,proto-ipv6-slaac,multicast,proto-dhcpv4,socket-raw,socket-udp,socket-tcp,socket-icmp,socket-dns,async"
    "defmt,alloc,medium-ip,medium-ethernet,proto-ipv6,proto-ipv6-slaac,multicast,proto-dhcpv4,socket-raw,socket-udp,socket-tcp,socket-icmp,socket-dns,async"
    "medium-ieee802154,proto-sixlowpan,socket-dns,auto-icmp-echo-reply"
    "alloc,medium-ethernet,proto-ipv4,proto-ipv6,socket-raw,socket-udp,socket-tcp,socket-icmp,proto-ipv6-slaac,socket-tcp-cubic,socket-tcp-reno"
)

usage() {
    cat <<'USAGE'
usage: ./ci.sh <command> [args]

Core commands:
  check [msrv|stable|nightly]   Cargo check matrix.
  test [msrv|stable|nightly]    Cargo test matrix.
  clippy                        Clippy on tests/examples.
  build_16bit                   Nightly build-std check for 16-bit pointers.
  coverage                      cargo-llvm-cov matrix.
  netsim                        Stable NoControl loss-recovery netsim sweep.
  all                           Core matrix above.

Local fork evidence:
  quick                         Fast local smoke: fmt, iOS check, host phy check, iOS sizecheck.
  sizecheck                     Print default and iOS-shaped footprint numbers (diagnostic, never fails).
  ios-gate                      iOS Network Extension memory-shape proofs. Asserts: idle
                                dynamic sockets charge 0 pool bytes (create + Drop), churn
                                refunds every pool byte after teardown, RSS bounded.
  profile-smoke [seconds]       Short throughput/fairness/RSS harness smoke. Asserts:
                                many_tcp_fair is FAIR with no starved flows, idle_hot
                                charges 0 pool bytes for idle sockets, RSS bounded.
  fuzz-build                    Build all fuzz targets on nightly.
  fuzz-smoke [seconds] [target] Short ASan fuzz smoke, default wire_parsers.

Set TRACE=0 for quieter local output.
USAGE
}

test() {
    local version=$1
    rustup toolchain install $version

    for features in "${FEATURES_TEST[@]}"; do
        cargo +$version test --no-default-features --features "$features"
    done
}

netsim() {
    # Serialized: the test uses a global CLOCK and the process-wide logger.
    cargo test --release --features _netsim netsim -- --test-threads=1
}

check() {
    local version=$1
    rustup toolchain install $version

    export DEFMT_LOG="trace"

    for features in "${FEATURES_CHECK[@]}"; do
        cargo +$version check --no-default-features --features "$features"
    done

    cargo +$version check --examples

    if [[ $version == "nightly" ]]; then
        cargo +$version check --benches
    fi
}

clippy() {
    rustup toolchain install $MSRV
    rustup component add clippy --toolchain=$MSRV
    cargo +$MSRV clippy --tests --examples -- -D warnings
    cargo +$MSRV clippy --tests --examples --features socket-tcp-dynamic-buffer -- -D warnings
}

build_16bit() {
    rustup toolchain install nightly
    rustup +nightly component add rust-src

    TARGET_WITH_16BIT_POINTER=msp430-none-elf
    for features in "${FEATURES_CHECK[@]}"; do
        cargo +nightly build -Z build-std=core,alloc --target "$TARGET_WITH_16BIT_POINTER" --no-default-features --features="$features"
    done
}

coverage() {
    for features in "${FEATURES_TEST[@]}"; do
        cargo llvm-cov --no-report --no-default-features --features "$features"
    done
    cargo llvm-cov report --lcov --output-path lcov.info
}

version_arg() {
    case "${1:-}" in
        "") return 1 ;;
        msrv) printf '%s\n' "$MSRV" ;;
        *) printf '%s\n' "$1" ;;
    esac
}

run_test_matrix() {
    if version="$(version_arg "${1:-}")"; then
        test "$version"
    else
        for version in "${RUSTC_VERSIONS[@]}"; do
            test "$version"
        done
    fi
}

run_check_matrix() {
    if version="$(version_arg "${1:-}")"; then
        check "$version"
    else
        for version in "${RUSTC_VERSIONS[@]}"; do
            check "$version"
        done
    fi
}

sizecheck() {
    cargo test --release --test sizecheck -- --nocapture
    cargo test --release --test sizecheck --no-default-features --features "$IOS_FEATURES" -- --nocapture
}

quick() {
    cargo fmt --check
    cargo check --no-default-features --features "$IOS_FEATURES"
    cargo check --features "$HOST_PHY_FEATURES"
    cargo test --release --test sizecheck --no-default-features --features "$IOS_FEATURES" -- --nocapture
}

# --- Gate plumbing -----------------------------------------------------------
#
# The harness shapes are measurement tools and never assert; the gates live
# here. `run_gated` captures a command's output, `require` then fails the run
# unless the given pattern is present. A missing line is a failure too, so a
# harness output-format change breaks the gate loudly instead of letting it
# pass vacuously. The gated values are the durable invariants from FORK.md
# §13.4 (pool refunds, lazy-alloc, fairness, RSS boundedness) — not
# host-dependent throughput numbers, which per §13 policy are never pinned.

GATE_LOG="$(mktemp)"
trap 'rm -f "$GATE_LOG"' EXIT

run_gated() {
    : >"$GATE_LOG"
    "$@" 2>&1 | tee "$GATE_LOG"
}

require() {
    local desc="$1" pattern="$2"
    if ! grep -Eq "$pattern" "$GATE_LOG"; then
        echo "GATE FAIL: ${desc} — pattern not found in output: ${pattern}" >&2
        exit 1
    fi
    echo "GATE OK:   ${desc}"
}

ios_gate() {
    cargo test --release --lib --no-default-features --features "std,$IOS_FEATURES" dyn_buf -- --test-threads=1
    cargo test --release --test sizecheck --no-default-features --features "$IOS_FEATURES" -- --nocapture

    # Idle-footprint invariant (FORK.md §14.6): dynamic sockets with zero
    # initial buffers must charge nothing to the pool, at creation and
    # after Drop.
    run_gated cargo run --release --example dynbuf_memcompare \
        --features socket-tcp-dynamic-buffer -- dynamic 300
    require "idle dynamic sockets charge 0 pool bytes" \
        'pool charged after N idle sockets: +0 KiB'
    require "dropped dynamic sockets refund the pool to 0" \
        'pool charged after Drop: +0 KiB'

    # Lifecycle-refund invariant (FORK.md §13.4): after connection churn and
    # teardown, every pool byte must be refunded. The at-deadline reading is
    # a bounded diagnostic and is deliberately not gated.
    run_gated cargo run --release --example profile_loopback \
        --features socket-tcp-dynamic-buffer -- --mode bench churn 5 200
    require "churn refunds every pool byte after teardown" \
        'pool used \(end\): +0 KiB'
    require "churn RSS bounded" 'RSS verdict: bounded'
}

profile_smoke() {
    local seconds="${1:-2}"
    cargo run --release --example profile_loopback -- --mode bench udp "$seconds"

    # Deterministic TCP fairness signal (FORK.md §13.3): Jain >= 0.95 and no
    # zero-byte flows is printed as "FAIR (no starvation)".
    run_gated cargo run --release --example profile_loopback -- \
        --mode bench many_tcp_fair "$seconds" 8
    require "many_tcp_fair verdict is FAIR with no starvation" \
        'verdict: FAIR \(no starvation\)'
    require "many_tcp_fair RSS bounded" 'RSS verdict: bounded'

    # Lazy-alloc invariant (FORK.md §13.4): idle dynamic sockets charge
    # nothing at creation; steady-state charge must not exceed the printed
    # active-socket upper bound.
    run_gated cargo run --release --example profile_loopback \
        --features socket-tcp-dynamic-buffer -- --mode bench idle_hot "$seconds" 50 2
    require "idle sockets are charge-free at creation" \
        'pool used post-create: +0 KiB'
    require "idle_hot RSS bounded" 'RSS verdict: bounded'
    local steady bound
    steady="$(grep -Eo 'pool used steady: +[0-9]+' "$GATE_LOG" | grep -Eo '[0-9]+$' || true)"
    bound="$(grep -Eo 'steady upper bound is [0-9]+' "$GATE_LOG" | grep -Eo '[0-9]+$' || true)"
    if [[ -z "$steady" || -z "$bound" ]]; then
        echo "GATE FAIL: idle_hot steady-pool lines missing from output" >&2
        exit 1
    fi
    if (( steady > bound )); then
        echo "GATE FAIL: idle_hot steady pool ${steady} KiB exceeds active-socket bound ${bound} KiB" >&2
        exit 1
    fi
    echo "GATE OK:   idle_hot steady pool ${steady} KiB within bound ${bound} KiB"
}

fuzz_build() {
    cargo +nightly fuzz build
    cargo +nightly fuzz build --features log
}

fuzz_smoke() {
    local seconds="${1:-30}"
    local target="${2:-wire_parsers}"
    cargo +nightly fuzz run -s address "$target" -- -max_len=1536 -max_total_time="$seconds"
}

run_all() {
    run_test_matrix
    run_check_matrix
    clippy
    build_16bit
    coverage
    netsim
}

cmd="${1:-help}"
shift || true

case "$cmd" in
    help|-h|--help)
        set +x
        usage
        ;;
    test)
        run_test_matrix "$@"
        ;;
    check)
        run_check_matrix "$@"
        ;;
    clippy)
        clippy
        ;;
    build_16bit)
        build_16bit
        ;;
    coverage)
        coverage
        ;;
    netsim)
        netsim
        ;;
    all)
        run_all
        ;;
    quick)
        quick
        ;;
    sizecheck)
        sizecheck
        ;;
    ios-gate)
        ios_gate
        ;;
    profile-smoke)
        profile_smoke "$@"
        ;;
    fuzz-build)
        fuzz_build
        ;;
    fuzz-smoke)
        fuzz_smoke "$@"
        ;;
    *)
        set +x
        usage >&2
        echo "error: unknown command '$cmd'" >&2
        exit 2
        ;;
esac
