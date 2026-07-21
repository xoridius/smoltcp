# Apple validation and profiling

Apple work has three evidence levels. Keep them separate:

1. Cross-builds prove that supported macOS and iOS targets compile.
2. Native macOS runs exercise smoltcp with real Apple process-footprint
   accounting.
3. A signed Network Extension on the target device is the only useful proof of
   complete extension footprint, lifecycle behavior, and jetsam headroom.

None of the repository checks requires `sudo`.

## Repository gates

Cross-check Intel and arm64 macOS plus arm64 iOS device and simulator targets:

```sh
TRACE=0 ./ci.sh apple-check
```

On macOS, run the complete constrained-memory gate before release:

```sh
CARGO_TARGET_DIR=target/apple-native TRACE=0 ./ci.sh ios-full-gate
```

For shorter iterations, use:

```sh
TRACE=0 ./ci.sh ios-gate
TRACE=0 ./ci.sh profile-smoke 3
```

These commands use paired in-process devices. They do not open TUN/TAP or BPF
devices, alter routes, or require elevated privileges. The full gate runs TCP
lifecycle and pool tests, size checks, Apple cross-builds, idle-memory
comparisons, every traffic shape in `ci/ios-full-gate-static.txt` and
`ci/ios-full-gate-dynamic.txt`, and the serialized TCP network simulation.

On Apple hosts, harness output named `apple_phys_footprint` comes from
`proc_pid_rusage`. It is native macOS process evidence, not an iOS jetsam
result. CI runs the same full gate on an Apple Silicon macOS runner.

## Network Extension boundary

A packet-tunnel integration should expose Network Extension packets through a
consumer-owned `phy::Device` with `Medium::Ip`. The consumer owns packet-flow
I/O, scheduling, cancellation, and the extension lifecycle; smoltcp owns the
IP/TCP/UDP state machine behind the `Device` boundary.

Do not use `TunTapInterface` for this integration; it is a Linux/Android hosted
adapter. The macOS `RawSocket` backend uses BPF and is also outside the Network
Extension data path. Its presence does not make privileged live-interface
testing part of the production gate.

The host app and extension still need the appropriate signing, entitlements,
and Network Extension capability. Apple documents that setup in
[Configuring network extensions](https://developer.apple.com/documentation/xcode/configuring-network-extensions).

## Xcode and Instruments

Profile an optimized, symbolized build on the intended device or Mac:

1. Build the host app and extension with the Profile action and retain the
   matching dSYMs.
2. Start the tunnel normally, without `sudo`.
3. Choose **Product > Profile** in Xcode, or open Instruments and attach to the
   extension process rather than the host app.
4. Record **Time Profiler** for CPU attribution, **Allocations** for heap and
   anonymous VM activity, and **System Trace** for scheduling and blocking.
5. Use Xcode's memory report for current and peak process memory. Treat
   Allocations and physical footprint as different metrics.

Template names can vary by installed Xcode. Discover the local names and CLI
syntax instead of copying an old command:

```sh
xcrun xctrace list templates
xcrun xctrace help record
```

Apple's current overviews are [Improving your app's performance](https://developer.apple.com/documentation/xcode/improving-your-app-s-performance)
and [Gathering information about memory use](https://developer.apple.com/documentation/xcode/gathering-information-about-memory-use).

Use the repository traffic manifests as the workload checklist. For device
validation, reproduce idle and hot-idle populations, bulk and small-packet
TCP, UDP, bidirectional traffic, flow fairness, connection churn, unread-RST
delivery, pool pressure, and mixed TCP/UDP traffic. Capture at least five
matched runs after warm-up when comparing revisions.

Record the smoltcp revision, consumer revision, device model, OS and Xcode
versions, build configuration, features, pool and socket limits, workload,
warm-up, sample duration, and thermal state. Keep `.trace` files and packet
captures out of this repository; publish compact, hash-backed measurements in
`docs/perf/` only after a controlled matched comparison.
