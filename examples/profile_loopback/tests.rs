use super::*;

fn args(values: &[&str]) -> Result<Config, String> {
    parse_args(values.iter().map(|value| (*value).to_owned()))
}

fn nz64(value: u64) -> NonZeroU64 {
    std::num::NonZeroU64::new(value).unwrap()
}

fn nz(value: usize) -> NonZeroUsize {
    std::num::NonZeroUsize::new(value).unwrap()
}

fn config(mode: RunMode, seconds: u64, shape: TrafficShape, offload: bool) -> Config {
    Config {
        mode,
        seconds: nz64(seconds),
        shape,
        offload_checksums: offload,
    }
}

fn manifest_configurations(manifest: &str) -> Vec<Config> {
    manifest
        .lines()
        .enumerate()
        .map(|(line, command)| {
            assert!(!command.trim().is_empty(), "blank line {}", line + 1);
            parse_args(command.split_ascii_whitespace().map(str::to_owned))
                .unwrap_or_else(|error| panic!("command {command:?}: {error}"))
        })
        .collect()
}

fn gate_configs(shapes: impl IntoIterator<Item = TrafficShape>) -> Vec<Config> {
    shapes
        .into_iter()
        .flat_map(|shape| {
            [
                config(RunMode::Bench, 3, shape, false),
                config(RunMode::Bench, 3, shape, true),
            ]
        })
        .collect()
}

fn static_gate_configs() -> Vec<Config> {
    let single = [
        TrafficShape::Udp,
        TrafficShape::Firehose,
        TrafficShape::PingPong,
        TrafficShape::Small,
    ];
    let many = [8, 50, 100].into_iter().flat_map(|flows| {
        [
            TrafficShape::ManyTcp { flows: nz(flows) },
            TrafficShape::ManyTcpFair { flows: nz(flows) },
            TrafficShape::ManyUdp { flows: nz(flows) },
        ]
    });
    gate_configs(single.into_iter().chain(many))
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn dynamic_gate_configs() -> Vec<Config> {
    gate_configs([
        TrafficShape::MultiTcp {
            threads: nz(2),
            flows_per_thread: nz(50),
        },
        TrafficShape::MultiTcpSink {
            threads: nz(2),
            flows_per_thread: nz(50),
        },
        TrafficShape::Churn { rate: nz(500) },
        TrafficShape::RstUnreadRx { flows: nz(100) },
        TrafficShape::PoolPressure { flows: nz(50) },
        TrafficShape::MixedTcpUdp {
            tcp_flows: nz(50),
            udp_flows: nz(50),
        },
        TrafficShape::IdleHot {
            idle: 1000,
            active: 0,
        },
        TrafficShape::IdleHot {
            idle: 1000,
            active: 10,
        },
    ])
}

fn assert_errors(cases: Vec<(Vec<&str>, &str)>) {
    for (input, expected) in cases {
        let error = args(&input).unwrap_err();
        assert!(
            error.contains(expected),
            "input {input:?}: expected {expected:?}, got {error:?}"
        );
    }
}

#[test]
fn parse_args_returns_complete_config_for_every_static_shape() {
    let cases: &[(&[&str], Config)] = &[
        (
            &["udp", "1"],
            config(RunMode::Bench, 1, TrafficShape::Udp, false),
        ),
        (
            &["--mode", "trace", "firehose", "2", "offload"],
            config(RunMode::Trace, 2, TrafficShape::Firehose, true),
        ),
        (
            &["--mode=bench", "pingpong", "3", "offload"],
            config(RunMode::Bench, 3, TrafficShape::PingPong, true),
        ),
        (
            &["small", "4", "offload"],
            config(RunMode::Bench, 4, TrafficShape::Small, true),
        ),
        (
            &["all", "5"],
            config(RunMode::Bench, 5, TrafficShape::All, false),
        ),
        (
            &["many_tcp", "6", "7"],
            config(
                RunMode::Bench,
                6,
                TrafficShape::ManyTcp { flows: nz(7) },
                false,
            ),
        ),
        (
            &["many_tcp_fair", "8", "9", "offload"],
            config(
                RunMode::Bench,
                8,
                TrafficShape::ManyTcpFair { flows: nz(9) },
                true,
            ),
        ),
        (
            &["many_udp", "10", "11", "offload"],
            config(
                RunMode::Bench,
                10,
                TrafficShape::ManyUdp { flows: nz(11) },
                true,
            ),
        ),
    ];

    for (input, expected) in cases {
        assert_eq!(args(input), Ok(*expected), "input: {input:?}");
    }
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
#[test]
fn parse_args_returns_complete_config_for_every_dynamic_shape() {
    let cases: &[(&[&str], Config)] = &[
        (
            &["multi_tcp", "1", "2", "3"],
            config(
                RunMode::Bench,
                1,
                TrafficShape::MultiTcp {
                    threads: nz(2),
                    flows_per_thread: nz(3),
                },
                false,
            ),
        ),
        (
            &["multi_tcp_sink", "4", "5", "6", "offload"],
            config(
                RunMode::Bench,
                4,
                TrafficShape::MultiTcpSink {
                    threads: nz(5),
                    flows_per_thread: nz(6),
                },
                true,
            ),
        ),
        (
            &["churn", "7", "8", "offload"],
            config(RunMode::Bench, 7, TrafficShape::Churn { rate: nz(8) }, true),
        ),
        (
            &["rst_unread_rx", "8", "9"],
            config(
                RunMode::Bench,
                8,
                TrafficShape::RstUnreadRx { flows: nz(9) },
                false,
            ),
        ),
        (
            &["pool_pressure", "9", "10", "offload"],
            config(
                RunMode::Bench,
                9,
                TrafficShape::PoolPressure { flows: nz(10) },
                true,
            ),
        ),
        (
            &["mixed_tcp_udp", "10", "11", "12"],
            config(
                RunMode::Bench,
                10,
                TrafficShape::MixedTcpUdp {
                    tcp_flows: nz(11),
                    udp_flows: nz(12),
                },
                false,
            ),
        ),
        (
            &["idle_hot", "9", "10", "0", "offload"],
            config(
                RunMode::Bench,
                9,
                TrafficShape::IdleHot {
                    idle: 10,
                    active: 0,
                },
                true,
            ),
        ),
        (
            &["idle_hot", "9", "0", "10"],
            config(
                RunMode::Bench,
                9,
                TrafficShape::IdleHot {
                    idle: 0,
                    active: 10,
                },
                false,
            ),
        ),
    ];

    for (input, expected) in cases {
        assert_eq!(args(input), Ok(*expected), "input: {input:?}");
    }
}

#[test]
fn parse_args_accepts_each_mode_and_literal_offload() {
    let cases: &[(&[&str], RunMode, bool)] = &[
        (&["udp", "1"], RunMode::Bench, false),
        (&["--mode", "bench", "udp", "1"], RunMode::Bench, false),
        (&["--mode=bench", "udp", "1"], RunMode::Bench, false),
        (&["--mode", "trace", "udp", "1"], RunMode::Trace, false),
        (&["--mode=trace", "udp", "1"], RunMode::Trace, false),
        (&["udp", "1", "offload"], RunMode::Bench, true),
    ];

    for (input, expected_mode, expected_offload) in cases {
        let config = args(input).unwrap();
        assert_eq!(config.mode, *expected_mode, "input: {input:?}");
        assert_eq!(
            config.offload_checksums, *expected_offload,
            "input: {input:?}"
        );
    }
}

#[test]
fn parse_args_accepts_maximum_representable_numbers() {
    assert_eq!(
        parse_args(vec![
            "many_tcp".to_owned(),
            u64::MAX.to_string(),
            usize::MAX.to_string(),
        ]),
        Ok(config(
            RunMode::Bench,
            u64::MAX,
            TrafficShape::ManyTcp {
                flows: nz(usize::MAX),
            },
            false,
        ))
    );
}

#[test]
fn full_gate_static_command_list_matches_26_cell_matrix() {
    let expected = static_gate_configs();
    assert_eq!(expected.len(), 26);
    assert_eq!(
        manifest_configurations(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/ci/ios-full-gate-static.txt"
        ))),
        expected
    );
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
#[test]
fn full_gate_dynamic_command_list_matches_16_cell_matrix() {
    let expected = dynamic_gate_configs();
    assert_eq!(expected.len(), 16);
    assert_eq!(
        manifest_configurations(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/ci/ios-full-gate-dynamic.txt"
        ))),
        expected
    );
}

#[test]
fn parse_args_rejects_invalid_static_commands() {
    let usize_overflow = (usize::MAX as u128 + 1).to_string();
    assert_errors(vec![
        (vec![], "missing traffic shape"),
        (vec!["udp"], "missing seconds"),
        (vec!["udp", "0"], "seconds must be non-zero"),
        (vec!["udp", ""], "invalid seconds ''"),
        (vec!["udp", "nope"], "invalid seconds 'nope'"),
        (
            vec!["udp", "18446744073709551616"],
            "invalid seconds '18446744073709551616'",
        ),
        (vec!["unknown", "1"], "unknown traffic shape 'unknown'"),
        (vec!["", "1"], "unknown traffic shape ''"),
        (vec!["--wat", "udp", "1"], "unknown option '--wat'"),
        (vec!["--mode"], "missing value for --mode"),
        (vec!["--mode=", "udp", "1"], "mode cannot be empty"),
        (vec!["--mode", "fast", "udp", "1"], "invalid mode 'fast'"),
        (
            vec!["--mode", "bench", "--mode", "trace", "udp", "1"],
            "--mode must appear before the traffic shape",
        ),
        (
            vec!["udp", "1", "--mode", "trace"],
            "--mode must appear before the traffic shape",
        ),
        (vec!["udp", "1", "1"], "invalid offload value '1'"),
        (vec!["udp", "1", "true"], "invalid offload value 'true'"),
        (vec!["udp", "1", "false"], "invalid offload value 'false'"),
        (
            vec!["udp", "1", "offload", "extra"],
            "unexpected trailing argument 'extra'",
        ),
        (
            vec!["udp", "1", "offload", "--wat"],
            "unknown option '--wat'",
        ),
        (vec!["udp", "offload", "1"], "invalid seconds 'offload'"),
        (vec!["many_tcp", "1"], "missing flows"),
        (vec!["many_tcp", "1", "0"], "flows must be non-zero"),
        (vec!["many_tcp", "1", "nope"], "invalid flows 'nope'"),
        (
            vec!["many_tcp", "1", usize_overflow.as_str()],
            "invalid flows",
        ),
        (
            vec!["many_udp", "1", "2", "TRUE"],
            "invalid offload value 'TRUE'",
        ),
    ]);
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
#[test]
fn parse_args_rejects_invalid_dynamic_commands() {
    let usize_overflow = (usize::MAX as u128 + 1).to_string();
    assert_errors(vec![
        (vec!["multi_tcp", "1"], "missing threads"),
        (vec!["multi_tcp", "1", "0", "2"], "threads must be non-zero"),
        (
            vec!["multi_tcp", "1", "nope", "2"],
            "invalid threads 'nope'",
        ),
        (
            vec!["multi_tcp", "1", usize_overflow.as_str(), "2"],
            "invalid threads",
        ),
        (vec!["multi_tcp_sink", "1", "2"], "missing flows per thread"),
        (
            vec!["multi_tcp_sink", "1", "2", "0"],
            "flows per thread must be non-zero",
        ),
        (
            vec!["multi_tcp_sink", "1", "2", "nope"],
            "invalid flows per thread 'nope'",
        ),
        (vec!["churn", "1"], "missing rate"),
        (vec!["churn", "1", "0"], "rate must be non-zero"),
        (vec!["churn", "1", "nope"], "invalid rate 'nope'"),
        (vec!["rst_unread_rx", "1"], "missing flows"),
        (vec!["rst_unread_rx", "1", "0"], "flows must be non-zero"),
        (vec!["pool_pressure", "1"], "missing flows"),
        (vec!["pool_pressure", "1", "0"], "flows must be non-zero"),
        (vec!["mixed_tcp_udp", "1"], "missing TCP flows"),
        (
            vec!["mixed_tcp_udp", "1", "0", "2"],
            "TCP flows must be non-zero",
        ),
        (vec!["mixed_tcp_udp", "1", "2"], "missing UDP flows"),
        (
            vec!["mixed_tcp_udp", "1", "2", "0"],
            "UDP flows must be non-zero",
        ),
        (
            vec!["mixed_tcp_udp", "1", "2", "nope"],
            "invalid UDP flows 'nope'",
        ),
        (vec!["idle_hot", "1"], "missing idle flows"),
        (vec!["idle_hot", "1", "2"], "missing active flows"),
        (
            vec!["idle_hot", "1", "nope", "2"],
            "invalid idle flows 'nope'",
        ),
        (
            vec!["idle_hot", "1", "2", "nope"],
            "invalid active flows 'nope'",
        ),
        (
            vec!["idle_hot", "1", "0", "0"],
            "idle_hot requires at least one idle or active flow",
        ),
        (
            vec!["idle_hot", "1", "2", "3", "offload", "extra"],
            "unexpected trailing argument 'extra'",
        ),
    ]);
}

#[cfg(not(feature = "socket-tcp-dynamic-buffer"))]
#[test]
fn parse_args_reports_the_required_feature_for_dynamic_shapes() {
    for shape in [
        "multi_tcp",
        "multi_tcp_sink",
        "churn",
        "rst_unread_rx",
        "pool_pressure",
        "mixed_tcp_udp",
        "idle_hot",
    ] {
        let input = [shape, "1", "1", "1"];
        assert_eq!(
            args(&input),
            Err(format!(
                "traffic shape '{shape}' requires feature 'socket-tcp-dynamic-buffer'"
            ))
        );
    }
}

#[test]
fn tcp_workload_validation_requires_establishment_and_work() {
    assert!(validate_tcp_transfer("firehose", true, true, 1, 1).is_ok());
    for result in [
        validate_tcp_transfer("firehose", false, true, 1, 1),
        validate_tcp_transfer("firehose", true, false, 1, 1),
        validate_tcp_transfer("firehose", true, true, 0, 1),
        validate_tcp_transfer("firehose", true, true, 1, 0),
    ] {
        assert!(result.is_err());
    }

    assert!(validate_pingpong(true, true, 1).is_ok());
    assert!(validate_pingpong(false, true, 1).is_err());
    assert!(validate_pingpong(true, false, 1).is_err());
    assert!(validate_pingpong(true, true, 0).is_err());
}

#[test]
fn small_and_pingpong_finish_with_both_tcp_peers_established() {
    assert_eq!(shape_small(1, false), Ok(()));
    assert_eq!(shape_pingpong(1, false), Ok(()));
}

#[test]
fn all_cli_configuration_runs_every_real_shape_successfully() {
    let config = args(&["all", "1"]).unwrap();
    assert_eq!(run_config(config), Ok(()));
}

#[test]
fn extreme_static_workloads_return_errors_without_panicking() {
    for result in [
        shape_firehose(u64::MAX, false),
        shape_small(u64::MAX, false),
        shape_pingpong(u64::MAX, false),
        shape_many_tcp_fair(1, usize::MAX, false, RunMode::Bench),
        shape_many_udp(1, usize::MAX, false, RunMode::Bench),
        shape_many_tcp(1, usize::MAX, false, RunMode::Bench),
        shape_udp_firehose(u64::MAX, false),
        run_config(config(RunMode::Bench, u64::MAX, TrafficShape::All, false)),
    ] {
        assert!(result.is_err(), "result: {result:?}");
    }
    assert!(validate_unique_flow_count("many_tcp", MAX_UNIQUE_FLOWS).is_ok());
    assert!(validate_unique_flow_count("many_tcp", MAX_UNIQUE_FLOWS + 1).is_err());
}

#[test]
fn udp_workload_validation_requires_bindings_and_work() {
    assert!(validate_udp_transfer("udp", true, true, 1, 1).is_ok());
    for result in [
        validate_udp_transfer("udp", false, true, 1, 1),
        validate_udp_transfer("udp", true, false, 1, 1),
        validate_udp_transfer("udp", true, true, 0, 1),
        validate_udp_transfer("udp", true, true, 1, 0),
    ] {
        assert!(result.is_err());
    }
}

#[test]
fn memory_trace_keeps_a_bounded_current_tail_sample() {
    let mut trace = MemTrace::start(RunMode::Bench);
    let capacity = trace.samples.capacity();
    trace.samples.resize(capacity, (0, u64::MAX, 0));
    trace.maybe_sample(0);
    assert_eq!(trace.samples.len(), capacity);
    assert_eq!(trace.samples.capacity(), capacity);
    assert_ne!(trace.samples.last().unwrap().1, u64::MAX);
}

#[test]
fn many_flow_validation_gates_setup_work_and_starvation() {
    let fair = Fairness::from(&[100, 100]);
    assert!(validate_established_flows("many_tcp", 2, 2, &fair).is_ok());
    assert!(validate_established_flows("many_tcp", 1, 2, &fair).is_err());
    assert!(validate_established_flows("many_tcp", 2, 2, &Fairness::from(&[0, 0])).is_err());
    assert!(validate_established_flows("many_tcp", 2, 2, &Fairness::from(&[0, 100])).is_err());
    assert!(validate_established_flows("many_tcp", 2, 2, &Fairness::from(&[1, 100])).is_err());

    let unfair_without_starvation = Fairness::from(&[10, 100]);
    assert!(validate_established_flows("many_tcp", 2, 2, &unfair_without_starvation).is_ok());
    assert!(validate_fairness("many_tcp_fair", &unfair_without_starvation).is_err());
    assert!(validate_fairness("many_tcp_fair", &fair).is_ok());

    assert!(validate_udp_bindings("many_udp", true, true).is_ok());
    assert!(validate_udp_bindings("many_udp", false, true).is_err());
    assert!(validate_udp_bindings("many_udp", true, false).is_err());
    assert!(validate_flow_stats("many_udp", &fair).is_ok());
    assert!(validate_fairness("many_udp", &unfair_without_starvation).is_err());
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn worker_stats(received: u64) -> MultiTcpWorkerStats {
    MultiTcpWorkerStats {
        established: 2,
        expected_flows: 2,
        sent: received,
        received,
        elapsed_us: 1_000_000,
        lane_stats: LaneStats::default(),
    }
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
#[test]
fn multi_tcp_validation_gates_workers() {
    let workers = [Ok(worker_stats(100)), Ok(worker_stats(100))];
    assert!(validate_multi_tcp_workers("multi_tcp", &workers).is_ok());

    let mut incomplete = worker_stats(100);
    incomplete.established = 1;
    let invalid = [Err("listen failed".to_owned()), Ok(worker_stats(100))];
    assert!(validate_multi_tcp_workers("multi_tcp", &invalid).is_err());
    let invalid = [Ok(incomplete), Ok(worker_stats(100))];
    assert!(validate_multi_tcp_workers("multi_tcp", &invalid).is_err());
    assert!(
        validate_multi_tcp_workers("multi_tcp", &[Ok(worker_stats(0)), Ok(worker_stats(0))])
            .is_err()
    );
    assert!(
        validate_multi_tcp_workers("multi_tcp", &[Ok(worker_stats(1)), Ok(worker_stats(100))])
            .is_err()
    );
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
#[test]
fn rst_unread_rx_validation_requires_retention_drain_and_refund() {
    assert!(validate_rst_unread_rx(2, 2, 2048, 2048, 16384, 0, 0).is_ok());
    for result in [
        validate_rst_unread_rx(2, 1, 2048, 2048, 16384, 0, 0),
        validate_rst_unread_rx(2, 2, 0, 0, 16384, 0, 0),
        validate_rst_unread_rx(2, 2, 2048, 1024, 16384, 0, 0),
        validate_rst_unread_rx(2, 2, 2048, 2048, 0, 0, 0),
        validate_rst_unread_rx(2, 2, 2048, 2048, 16384, 1, 0),
        validate_rst_unread_rx(2, 2, 2048, 2048, 16384, 0, 1),
    ] {
        assert!(result.is_err());
    }
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
#[test]
fn pool_pressure_validation_requires_saturation_backpressure_and_fair_progress() {
    let received = [2048, 2048];
    let fair = fairness_after_prefill(&received, 1024);
    assert!(validate_pool_pressure(2, 65536, 65536, 8192, &fair, 0).is_ok());
    for result in [
        validate_pool_pressure(1, 65536, 65536, 8192, &fair, 0),
        validate_pool_pressure(2, 61440, 65536, 8192, &fair, 0),
        validate_pool_pressure(2, 65536, 65536, 0, &fair, 0),
        validate_pool_pressure(
            2,
            65536,
            65536,
            8192,
            &fairness_after_prefill(&[1024, 1024], 1024),
            0,
        ),
        validate_pool_pressure(
            2,
            65536,
            65536,
            8192,
            &fairness_after_prefill(&[2048, 1024], 1024),
            0,
        ),
        validate_pool_pressure(2, 65536, 65536, 8192, &fair, 1),
    ] {
        assert!(result.is_err());
    }
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
#[test]
fn churn_validation_requires_95_percent_of_target() {
    assert!(validate_churn_rate(500, 3.0, 1_425, 1_425).is_ok());
    assert!(validate_churn_rate(500, 3.0, 1_424, 1_425).is_err());
    assert!(validate_churn_rate(500, 3.0, 1_500, 1_424).is_err());
    assert!(validate_churn_rate(500, 3.0, 1_500, 1_501).is_err());
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
#[test]
fn idle_hot_link_capacity_depends_only_on_active_flows() {
    assert_eq!(idle_hot_queue_depth(0), Ok(2));
    assert_eq!(idle_hot_queue_depth(1), Ok(64));
    assert_eq!(idle_hot_queue_depth(10), Ok(160));
    assert_eq!(idle_hot_queue_depth(100), Ok(1600));
    assert_eq!(idle_hot_queue_depth(1000), Ok(16000));
    assert!(idle_hot_queue_depth(usize::MAX).is_err());
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
#[test]
fn extreme_dynamic_workloads_return_errors_without_panicking() {
    let results = [
        shape_multi_tcp(1, usize::MAX, 1, false),
        shape_multi_tcp_sink(1, 1, usize::MAX, false),
        shape_rst_unread_rx(1, usize::MAX, false, RunMode::Bench),
        shape_pool_pressure(1, usize::MAX, false, RunMode::Bench),
        shape_mixed_tcp_udp(1, usize::MAX, 1, false, RunMode::Bench),
        shape_mixed_tcp_udp(1, 1, usize::MAX, false, RunMode::Bench),
        shape_idle_hot(1, usize::MAX, 1, false, RunMode::Bench),
        shape_churn(1, usize::MAX, false, RunMode::Bench),
        shape_multi_tcp(u64::MAX, 1, 1, false),
        shape_churn(u64::MAX, 1, false, RunMode::Bench),
        shape_idle_hot(u64::MAX, 1, 0, false, RunMode::Bench),
    ];
    for result in results {
        assert!(result.is_err(), "result: {result:?}");
    }
    assert!(validate_unique_worker_count("multi_tcp", MAX_UNIQUE_WORKERS).is_ok());
    assert!(validate_unique_worker_count("multi_tcp", MAX_UNIQUE_WORKERS + 1).is_err());
    assert_eq!(worker_subnet("multi_tcp", 0), Ok([10, 0, 0]));
    assert_eq!(
        worker_subnet("multi_tcp", MAX_UNIQUE_WORKERS - 1),
        Ok([10, 255, 255])
    );
}

#[test]
fn duplicate_and_misplaced_modes_use_the_same_error() {
    let duplicate = args(&["--mode", "bench", "--mode", "trace", "udp", "1"]).unwrap_err();
    let misplaced = args(&["udp", "1", "--mode", "trace"]).unwrap_err();
    assert_eq!(duplicate, misplaced);
}

fn link(mtu: usize, depth: usize) -> PairedLink {
    PairedLink::new(mtu, depth, false)
}

fn queue_packet(lane: &mut Lane, bytes: &[u8]) {
    let mut packet = lane
        .try_take_packet()
        .expect("packet pool exhausted in test setup");
    packet.buf[..bytes.len()].copy_from_slice(bytes);
    packet.len = bytes.len();
    lane.queue_pkt(packet);
}

#[test]
fn transmit_token_construction_preserves_lane() {
    let mut link = link(64, 2);
    let mut stats = DeviceStats::default();
    let mut device = link.device(LinkEndpoint::A, &mut stats);

    let token = device.transmit(Instant::from_millis(0)).unwrap();

    drop(token);
    assert_eq!(link.a_to_b.pool.len(), 2);
    assert!(link.a_to_b.queue.is_empty());
}

#[test]
fn standalone_transmit_preserves_last_response_credit() {
    let mut link = link(64, 2);
    queue_packet(&mut link.a_to_b, &[1]);
    let mut stats = DeviceStats::default();
    let mut device = link.device(LinkEndpoint::A, &mut stats);

    assert!(device.transmit(Instant::from_millis(0)).is_none());
    assert_eq!(link.a_to_b.pool.len(), 1);
    assert_eq!(link.a_to_b.queue.len(), 1);
    assert_eq!(link.a_to_b.stats.tx_backpressure, 1);
}

#[test]
fn transmit_consume_reuses_preallocated_packet_and_queue_storage() {
    let mut link = link(64, 2);
    let packet_buffer = link.a_to_b.pool.last().unwrap().buf.as_ptr();
    let packet_capacity = link.a_to_b.pool.last().unwrap().buf.capacity();
    let queue_capacity = link.a_to_b.queue.capacity();
    let mut stats = DeviceStats::default();
    let mut device = link.device(LinkEndpoint::A, &mut stats);
    let token = device.transmit(Instant::from_millis(0)).unwrap();

    phy::TxToken::consume(token, 4, |buffer| buffer.copy_from_slice(&[1, 2, 3, 4]));

    let tx = &link.a_to_b;
    assert_eq!(tx.pool.len(), 1);
    assert_eq!(tx.queue.capacity(), queue_capacity);
    assert_eq!(tx.queue[0].buf.as_ptr(), packet_buffer);
    assert_eq!(tx.queue[0].buf.capacity(), packet_capacity);
    assert_eq!(&tx.queue[0].buf[..tx.queue[0].len], &[1, 2, 3, 4]);
}

#[test]
fn paired_receive_backpressure_leaves_rx_queued() {
    let mut link = link(64, 1);
    *link.b_to_a = Lane::new(64, 2);
    queue_packet(&mut link.b_to_a, &[1]);
    queue_packet(&mut link.b_to_a, &[2]);
    let reserved = link.a_to_b.try_take_packet().unwrap();
    let mut stats = DeviceStats::default();

    {
        let mut device = link.device(LinkEndpoint::A, &mut stats);
        assert!(device.receive(Instant::from_millis(0)).is_none());
    }
    assert_eq!(link.b_to_a.queue.len(), 2);
    assert_eq!(link.a_to_b.stats.rx_backpressure, 1);

    link.a_to_b.return_pkt(reserved);
    let mut device = link.device(LinkEndpoint::A, &mut stats);
    let (rx_token, tx_token) = device.receive(Instant::from_millis(0)).unwrap();
    assert_eq!(phy::RxToken::consume(rx_token, |bytes| bytes[0]), 1);
    drop(tx_token);
    let (rx_token, tx_token) = device.receive(Instant::from_millis(0)).unwrap();
    assert_eq!(phy::RxToken::consume(rx_token, |bytes| bytes[0]), 2);
    drop(tx_token);
}

#[test]
fn paired_receive_tx_token_construction_preserves_tx_pool() {
    let mut link = link(64, 1);
    queue_packet(&mut link.b_to_a, &[1, 2, 3]);
    let mut stats = DeviceStats::default();
    let mut device = link.device(LinkEndpoint::A, &mut stats);

    let (rx_token, tx_token) = device.receive(Instant::from_millis(0)).unwrap();
    phy::RxToken::consume(rx_token, |bytes| assert_eq!(bytes, [1, 2, 3]));
    drop(tx_token);

    assert_eq!(link.a_to_b.pool.len(), 1);
    assert_eq!(link.b_to_a.pool.len(), 1);
}

#[test]
fn paired_response_consumes_final_credit() {
    let mut link = link(64, 1);
    queue_packet(&mut link.b_to_a, &[1]);
    let mut stats = DeviceStats::default();
    let mut device = link.device(LinkEndpoint::A, &mut stats);

    let (rx_token, tx_token) = device.receive(Instant::from_millis(0)).unwrap();
    phy::RxToken::consume(rx_token, |_| ());
    phy::TxToken::consume(tx_token, 1, |buffer| buffer[0] = 2);

    assert!(link.a_to_b.pool.is_empty());
    assert_eq!(&link.a_to_b.queue[0].buf[..1], &[2]);
}

#[test]
fn oversized_transmit_panics_and_preserves_credit() {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    let mut link = link(64, 2);
    let mut stats = DeviceStats::default();
    let mut device = link.device(LinkEndpoint::A, &mut stats);
    let token = device.transmit(Instant::from_millis(0)).unwrap();

    let result = catch_unwind(AssertUnwindSafe(|| {
        phy::TxToken::consume(token, 65, |_| ());
    }));

    assert!(result.is_err());
    assert_eq!(link.a_to_b.pool.len(), 2);
    assert!(link.a_to_b.queue.is_empty());
}

#[test]
fn transmit_callback_panic_returns_checked_out_packet() {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    let mut link = link(64, 2);
    let mut stats = DeviceStats::default();
    let mut device = link.device(LinkEndpoint::A, &mut stats);
    let token = device.transmit(Instant::from_millis(0)).unwrap();

    let result = catch_unwind(AssertUnwindSafe(|| {
        phy::TxToken::consume(token, 1, |buffer| {
            buffer[0] = 1;
            panic!("callback panic");
        });
    }));

    assert!(result.is_err());
    assert_eq!(link.a_to_b.pool.len(), 2);
    assert!(link.a_to_b.queue.is_empty());
}

#[test]
fn symmetrically_saturated_lanes_make_response_progress() {
    let mut link = link(64, 2);
    queue_packet(&mut link.a_to_b, &[1]);
    queue_packet(&mut link.b_to_a, &[2]);
    let mut stats_a = DeviceStats::default();
    let mut stats_b = DeviceStats::default();

    {
        let mut device = link.device(LinkEndpoint::A, &mut stats_a);
        assert!(device.transmit(Instant::from_millis(0)).is_none());
    }
    {
        let mut device = link.device(LinkEndpoint::B, &mut stats_b);
        assert!(device.transmit(Instant::from_millis(0)).is_none());
    }
    {
        let mut device = link.device(LinkEndpoint::A, &mut stats_a);
        let (rx, tx) = device.receive(Instant::from_millis(0)).unwrap();
        assert_eq!(phy::RxToken::consume(rx, |bytes| bytes[0]), 2);
        phy::TxToken::consume(tx, 1, |buffer| buffer[0] = 3);
    }
    {
        let mut device = link.device(LinkEndpoint::B, &mut stats_b);
        let (rx, tx) = device.receive(Instant::from_millis(0)).unwrap();
        assert_eq!(phy::RxToken::consume(rx, |bytes| bytes[0]), 1);
        drop(tx);
    }

    assert_eq!(link.a_to_b.pool.len(), 1);
    assert_eq!(link.b_to_a.pool.len(), 2);
}

#[test]
fn lane_stats_reports_reserved_packet_memory() {
    let lane = Lane::new(1500, 3);
    let stats = lane.stats();
    let payload_bytes: usize = lane.pool.iter().map(|packet| packet.buf.capacity()).sum();
    let packet_slot_bytes =
        (lane.queue.capacity() + lane.pool.capacity()) * core::mem::size_of::<Packet>();

    assert_eq!(stats.reserved_payload_bytes, payload_bytes);
    assert_eq!(stats.reserved_packet_slot_bytes, packet_slot_bytes);
    assert_eq!(
        stats.reserved_total_bytes(),
        stats.reserved_payload_bytes + stats.reserved_packet_slot_bytes
    );
    assert_eq!(stats.tx_backpressure, 0);
    assert_eq!(stats.rx_backpressure, 0);
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
#[test]
fn allocator_phase_tracks_and_resets_requested_live_peak() {
    let telemetry = AllocatorTelemetry::new();
    telemetry.record_alloc(8);

    let phase = telemetry.begin().unwrap();
    telemetry.record_alloc(16);
    telemetry.record_dealloc(8);
    assert_eq!(
        phase.finish(),
        AllocatorPeak {
            live_start: 8,
            live_end: 16,
            live_peak: 24,
        }
    );

    let phase = telemetry.begin().unwrap();
    telemetry.record_alloc(4);
    assert_eq!(
        phase.finish(),
        AllocatorPeak {
            live_start: 16,
            live_end: 20,
            live_peak: 20,
        }
    );
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
#[test]
fn allocator_phase_has_single_raii_owner() {
    let telemetry = AllocatorTelemetry::new();
    let phase = telemetry.begin().unwrap();
    assert!(telemetry.begin().is_err());
    drop(phase);
    assert!(telemetry.begin().is_ok());
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
#[test]
fn dynamic_memory_report_keeps_distinct_memory_boundaries() {
    let before = AllocSnap {
        alloc_bytes: 1_000,
        alloc_count: 10,
        free_bytes: 400,
        process_memory: ProcessMemorySample {
            current_bytes: 8_192,
            lifetime_peak_bytes: Some(8_192),
        },
        ctxsw_voluntary: 0,
        ctxsw_nonvoluntary: 0,
        cpu_ns: 0,
    };
    let after = AllocSnap {
        alloc_bytes: 1_600,
        alloc_count: 14,
        free_bytes: 850,
        process_memory: ProcessMemorySample {
            current_bytes: 12_288,
            lifetime_peak_bytes: Some(16_384),
        },
        ctxsw_voluntary: 0,
        ctxsw_nonvoluntary: 0,
        cpu_ns: 0,
    };

    let report = DynamicMemoryReport::from_snapshots(
        before,
        after,
        AllocatorPeak {
            live_start: 2_000,
            live_end: 2_150,
            live_peak: 67_536,
        },
        PoolUsage {
            start: 0,
            end: 65_536,
            budget: 65_536,
            after_teardown: 0,
        },
    )
    .unwrap();

    assert_eq!(report.process_memory_start, 8_192);
    assert_eq!(report.process_memory_end, 12_288);
    assert_eq!(report.process_memory_lifetime_peak, Some(16_384));
    assert_eq!(report.bytes_allocated, 600);
    assert_eq!(report.bytes_freed, 450);
    assert_eq!(report.net_heap_delta, 150);
    assert_eq!(report.allocation_count, 4);
    assert_eq!(report.allocator_live_start, 2_000);
    assert_eq!(report.allocator_live_end, 2_150);
    assert_eq!(report.allocator_peak_live, 67_536);
    assert_eq!(report.allocator_peak_growth, 65_536);
    assert_eq!(report.allocator_peak_bound, 131_072);
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
#[test]
fn dynamic_memory_report_rejects_invalid_peak_and_pool_boundaries() {
    let snapshot = AllocSnap {
        alloc_bytes: 0,
        alloc_count: 0,
        free_bytes: 0,
        process_memory: ProcessMemorySample {
            current_bytes: 1,
            lifetime_peak_bytes: Some(1),
        },
        ctxsw_voluntary: 0,
        ctxsw_nonvoluntary: 0,
        cpu_ns: 0,
    };
    let peak = |live_start, live_end, live_peak| AllocatorPeak {
        live_start,
        live_end,
        live_peak,
    };
    let pool = |start, end, budget, after_teardown| PoolUsage {
        start,
        end,
        budget,
        after_teardown,
    };

    assert!(
        DynamicMemoryReport::from_snapshots(
            snapshot,
            snapshot,
            peak(10, 10, 9),
            pool(0, 0, 100, 0),
        )
        .is_err()
    );
    assert!(
        DynamicMemoryReport::from_snapshots(
            snapshot,
            snapshot,
            peak(0, 201, 201),
            pool(0, 100, 100, 0),
        )
        .is_err()
    );
    assert!(
        DynamicMemoryReport::from_snapshots(
            snapshot,
            snapshot,
            peak(0, 0, 0),
            pool(0, 0, usize::MAX, 0),
        )
        .is_err()
    );
    assert!(
        DynamicMemoryReport::from_snapshots(
            snapshot,
            snapshot,
            peak(0, 0, 0),
            pool(0, 101, 100, 0),
        )
        .is_err()
    );
    assert!(
        DynamicMemoryReport::from_snapshots(snapshot, snapshot, peak(0, 0, 0), pool(0, 0, 100, 1),)
            .is_err()
    );
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
#[test]
fn active_dynamic_shapes_require_pool_growth() {
    assert!(validate_pool_growth("multi_tcp", 0, 1).is_ok());
    assert!(validate_pool_growth("multi_tcp", 1, 1).is_err());
    assert!(validate_pool_growth("idle_hot", 1, 0).is_err());
}
