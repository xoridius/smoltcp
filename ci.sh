#!/usr/bin/env bash

set -euo pipefail

if [[ "${TRACE:-1}" != "0" ]]; then
    set -x
fi

export DEFMT_LOG=trace

MSRV="1.91.0"
IOS_FEATURES="alloc,medium-ip,proto-ipv4,proto-ipv6,socket-udp,socket-tcp,socket-tcp-dynamic-buffer,socket-tcp-cubic"
TUNNEL_STATIC_FEATURES="std,libc,log,medium-ip,proto-ipv4,proto-ipv4-fragmentation,proto-ipv6,proto-ipv6-fragmentation,socket-tcp,socket-udp,socket-tcp-cubic,auto-icmp-echo-reply"
TUNNEL_DYNAMIC_FEATURES="$TUNNEL_STATIC_FEATURES,socket-tcp-dynamic-buffer"
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
  netsim                        Stable NoControl, CUBIC, and Reno netsim sweeps.
  apple-check                   Cross-check macOS and iOS targets on stable.
  docs                          Rustdoc with warnings denied for default and iOS shapes.
  all                           Portable core matrix; excludes Apple, docs, perf, fuzz, and Miri.

Local fork evidence:
  quick                         Fast local smoke: fmt, iOS check, host phy check, iOS sizecheck.
  sizecheck                     Print default and iOS-shaped footprint numbers.
  ios-gate                      iOS Network Extension memory-shape proofs.
  ios-full-gate                 smoltcp-side constrained-memory traffic matrix.
  profile-smoke [seconds]       Short throughput/fairness/RSS harness smoke.
  fuzz-build                    Build all fuzz targets on nightly.
  fuzz-smoke [seconds] [target] Short reproducibly seeded ASan smoke (LSan where supported); defaults to all targets.

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
    cargo test --release --features "_netsim socket-tcp-cubic socket-tcp-reno" --test netsim -- --test-threads=1
}

apple_check() {
    rustup target add --toolchain stable \
        x86_64-apple-darwin \
        aarch64-apple-ios \
        aarch64-apple-ios-sim

    cargo +stable check --target x86_64-apple-darwin \
        --lib --tests --examples \
        --features "socket-tcp-dynamic-buffer,socket-tcp-cubic,socket-tcp-reno"
    cargo +stable check --target x86_64-apple-darwin \
        --lib --examples --no-default-features \
        --features "$TUNNEL_DYNAMIC_FEATURES"

    for target in aarch64-apple-ios aarch64-apple-ios-sim; do
        cargo +stable check --target "$target" --lib \
            --no-default-features --features "$TUNNEL_DYNAMIC_FEATURES"
    done
}

docs() {
    RUSTDOCFLAGS="-Dwarnings" cargo +stable doc --no-deps
    RUSTDOCFLAGS="-Dwarnings" cargo +stable doc --no-deps --lib \
        --no-default-features --features "$IOS_FEATURES"
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

ios_gate() {
    cargo test --release --lib --no-default-features --features "$TUNNEL_DYNAMIC_FEATURES" dyn_buf -- --test-threads=1
    cargo test --release --test sizecheck --no-default-features --features "$IOS_FEATURES" -- --nocapture
    cargo run --release --example dynbuf_memcompare --no-default-features \
        --features "$TUNNEL_DYNAMIC_FEATURES" -- dynamic 300
}

run_profile_commands() {
    local binary=$1
    local manifest=$2
    local command
    local -a argv

    while IFS= read -r command || [[ -n "$command" ]]; do
        read -r -a argv <<< "$command" || return
        "$binary" "${argv[@]}" || return
    done < "$manifest"
}

ios_full_gate() {
    local target_dir="${CARGO_TARGET_DIR:-target}"

    cargo test --release --lib --no-default-features --features "$TUNNEL_DYNAMIC_FEATURES" dyn_buf -- --test-threads=1
    sizecheck
    apple_check

    cargo build --release --target-dir "$target_dir" --example dynbuf_memcompare \
        --no-default-features --features "$TUNNEL_DYNAMIC_FEATURES"
    for flows in 300 1000; do
        "$target_dir/release/examples/dynbuf_memcompare" legacy "$flows"
        "$target_dir/release/examples/dynbuf_memcompare" dynamic "$flows"
    done

    cargo build --release --target-dir "$target_dir" --example profile_loopback \
        --no-default-features --features "$TUNNEL_STATIC_FEATURES"
    run_profile_commands "$target_dir/release/examples/profile_loopback" \
        ci/ios-full-gate-static.txt

    cargo build --release --target-dir "$target_dir" --example profile_loopback \
        --no-default-features --features "$TUNNEL_DYNAMIC_FEATURES"
    run_profile_commands "$target_dir/release/examples/profile_loopback" \
        ci/ios-full-gate-dynamic.txt

    netsim
}

profile_smoke() {
    local seconds="${1:-1}"
    cargo run --release --example profile_loopback --no-default-features \
        --features "$TUNNEL_STATIC_FEATURES" -- --mode bench udp "$seconds"
    cargo run --release --example profile_loopback --no-default-features \
        --features "$TUNNEL_STATIC_FEATURES" -- --mode bench many_tcp_fair "$seconds" 8
    cargo run --release --example profile_loopback --no-default-features \
        --features "$TUNNEL_DYNAMIC_FEATURES" -- --mode bench idle_hot "$seconds" 50 2
}

fuzz_build() {
    cargo +nightly fuzz build
    cargo +nightly fuzz build --features log
}

fuzz_smoke() {
    local seconds="${1:-30}"
    local selected="${2:-all}"
    local targets
    local target

    if [[ "$selected" == "all" ]]; then
        targets="$(cargo +nightly fuzz list)"
    else
        targets="$selected"
    fi

    while IFS= read -r target; do
        [[ -z "$target" ]] && continue
        cargo +nightly fuzz run --sanitizer address "$target" -- \
            -seed=1 -max_len=1536 -timeout=5 -max_total_time="$seconds"
    done <<< "$targets"
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
    apple-check)
        apple_check
        ;;
    docs)
        docs
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
    ios-full-gate)
        ios_full_gate
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
