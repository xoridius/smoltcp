use super::*;

fn ingest_sack_ranges(s: &mut TestSocket, ranges: &[Option<(u32, u32)>; 3]) {
    let snd_una = s.local_seq_no;
    let snd_nxt = s.local_seq_next;
    let tx_len = s.tx_buffer.len();
    s.sack_scoreboard.ingest(ranges, snd_una, snd_nxt, tx_len);
}

/// Build one wire-format SACK block from absolute sequence numbers.
fn sack_block(left: TcpSeqNumber, right: TcpSeqNumber) -> Option<(u32, u32)> {
    Some((left.0 as u32, right.0 as u32))
}

/// Queue two segments but emit only the first one.
fn socket_with_queued_unsent_suffix() -> TestSocket {
    let mut s = socket_established();
    s.set_congestion_control(CongestionControl::None);
    assert!(!s.congestion_controller.manages_window());
    s.flags.insert(Flags::LOCAL_HAS_SACK);
    s.set_nagle_enabled(false);
    s.remote_mss = 6;
    s.send_slice(b"xxxxxxyyyyyy").unwrap();
    recv!(s, time 1000, Ok(TcpRepr {
        seq_number: LOCAL_SEQ + 1,
        ack_number: Some(REMOTE_SEQ + 1),
        payload:    &b"xxxxxx"[..],
        ..RECV_TEMPL
    }));
    s
}

#[test]
fn reject_ack_for_queued_unsent_data() {
    let mut s = socket_with_queued_unsent_suffix();

    let challenge = send(
        &mut s,
        Instant::from_millis(1010),
        &TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1 + 12),
            ..SEND_TEMPL
        },
    );

    assert_eq!(s.tx_buffer.len(), 12, "queued-unsent data was acknowledged");
    assert_eq!(s.local_seq_no, LOCAL_SEQ + 1);
    assert_eq!(s.remote_last_seq, LOCAL_SEQ + 1 + 6);
    assert_eq!(
        challenge,
        Some(TcpRepr {
            seq_number: LOCAL_SEQ + 1 + 6,
            ack_number: Some(REMOTE_SEQ + 1),
            ..RECV_TEMPL
        })
    );

    // The exact transmitted prefix remains acceptable.
    send!(s, time 2020, TcpRepr {
        seq_number: REMOTE_SEQ + 1,
        ack_number: Some(LOCAL_SEQ + 1 + 6),
        ..SEND_TEMPL
    });
    assert_eq!(s.tx_buffer.len(), 6);
    recv!(s, time 2030, Ok(TcpRepr {
        seq_number: LOCAL_SEQ + 1 + 6,
        ack_number: Some(REMOTE_SEQ + 1),
        payload:    &b"yyyyyy"[..],
        ..RECV_TEMPL
    }));
}

#[test]
fn ignore_sack_for_queued_unsent_data() {
    let mut s = socket_with_queued_unsent_suffix();

    send!(s, time 1010, TcpRepr {
        seq_number: REMOTE_SEQ + 1,
        ack_number: Some(LOCAL_SEQ + 1),
        sack_ranges: [
            sack_block(LOCAL_SEQ + 1 + 6, LOCAL_SEQ + 1 + 12),
            None,
            None,
        ],
        ..SEND_TEMPL
    });

    assert!(s.sack_scoreboard.is_empty());
    assert_eq!(s.tx_buffer.len(), 12);
    assert_eq!(s.remote_last_seq, LOCAL_SEQ + 1 + 6);
    assert!(matches!(s.timer, Timer::Retransmit { .. }));
    recv!(s, time 1020, Ok(TcpRepr {
        seq_number: LOCAL_SEQ + 1 + 6,
        ack_number: Some(REMOTE_SEQ + 1),
        payload:    &b"yyyyyy"[..],
        ..RECV_TEMPL
    }));
}

#[test]
fn sack_straddling_snd_nxt_is_trimmed_to_transmitted_data() {
    let mut s = socket_with_queued_unsent_suffix();

    ingest_sack_ranges(
        &mut s,
        &[
            sack_block(LOCAL_SEQ + 1 + 3, LOCAL_SEQ + 1 + 12),
            None,
            None,
        ],
    );

    let ranges: std::vec::Vec<_> = s.sack_scoreboard.ranges().collect();
    assert_eq!(ranges, std::vec![(3, 6)]);
    assert_eq!(s.local_seq_next, LOCAL_SEQ + 1 + 6);
    assert_eq!(s.tx_buffer.len(), 12);
}

#[test]
fn sack_fin_sequence_space_does_not_become_a_data_range() {
    let mut s = socket_established();
    s.flags.insert(Flags::LOCAL_HAS_SACK);
    s.remote_mss = 6;
    s.send_slice(b"xxxxxx").unwrap();
    s.close();
    recv!(s, time 1000, Ok(TcpRepr {
        control:    TcpControl::Fin,
        seq_number: LOCAL_SEQ + 1,
        ack_number: Some(REMOTE_SEQ + 1),
        payload:    &b"xxxxxx"[..],
        ..RECV_TEMPL
    }));
    assert_eq!(s.local_seq_next, LOCAL_SEQ + 1 + 7);

    // The FIN consumes sequence space but has no byte in tx_buffer.
    ingest_sack_ranges(
        &mut s,
        &[sack_block(LOCAL_SEQ + 1 + 6, LOCAL_SEQ + 1 + 7), None, None],
    );
    assert!(s.sack_scoreboard.is_empty());

    // A block that includes the last data byte and FIN is capped at data.
    ingest_sack_ranges(
        &mut s,
        &[sack_block(LOCAL_SEQ + 1 + 5, LOCAL_SEQ + 1 + 7), None, None],
    );
    let ranges: std::vec::Vec<_> = s.sack_scoreboard.ranges().collect();
    assert_eq!(ranges, std::vec![(5, 6)]);
}

/// Send 4 × 6-byte segments on an established socket and emit them.
fn socket_with_four_segments_in_flight() -> TestSocket {
    let mut s = socket_established();
    s.flags.insert(Flags::LOCAL_HAS_SACK);
    s.remote_mss = 6;
    send!(s, time 0, TcpRepr {
        seq_number: REMOTE_SEQ + 1,
        ack_number: Some(LOCAL_SEQ + 1),
        ..SEND_TEMPL
    });
    s.send_slice(b"xxxxxxyyyyyywwwwwwzzzzzz").unwrap();
    for (i, payload) in [&b"xxxxxx"[..], b"yyyyyy", b"wwwwww", b"zzzzzz"]
        .into_iter()
        .enumerate()
    {
        recv!(s, time 1000 + i as i64 * 5, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1 + 6 * i,
            ack_number: Some(REMOTE_SEQ + 1),
            payload,
            ..RECV_TEMPL
        }));
    }
    s
}

/// Three duplicate ACKs reporting segments 2 and 4 as SACKed.
#[cfg(any(
    feature = "socket-tcp-cubic",
    not(any(feature = "socket-tcp-cubic", feature = "socket-tcp-reno"))
))]
fn send_three_dupacks_sacking_2_and_4(mut s: &mut TestSocket, times: [i64; 3]) {
    for t in times {
        send!(s, time t, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1),
            sack_ranges: [
                sack_block(LOCAL_SEQ + 1 + 6, LOCAL_SEQ + 1 + 12),
                sack_block(LOCAL_SEQ + 1 + 18, LOCAL_SEQ + 1 + 24),
                None,
            ],
            ..SEND_TEMPL
        });
    }
}

#[cfg(not(any(feature = "socket-tcp-cubic", feature = "socket-tcp-reno")))]
#[test]
fn test_sack_fast_retransmit_holes_first_then_redundant_pass() {
    // Under NoControl (no congestion window), recovery sends the holes
    // FIRST — the delivery-blocking bytes — and then, because nothing
    // bounds the pipe, one redundant in-order pass over the window as
    // extra insurance and ACK solicitation. Under Reno/Cubic the
    // redundant pass is skipped entirely (see the cubic-gated test).
    let mut s = socket_with_four_segments_in_flight();
    send_three_dupacks_sacking_2_and_4(&mut s, [1050, 1055, 1060]);

    // Phase 1: the two holes, in order, ahead of everything else.
    recv!(s, time 1100, Ok(TcpRepr {
        seq_number: LOCAL_SEQ + 1,
        ack_number: Some(REMOTE_SEQ + 1),
        payload:    &b"xxxxxx"[..],
        ..RECV_TEMPL
    }));
    recv!(s, time 1105, Ok(TcpRepr {
        seq_number: LOCAL_SEQ + 1 + 12,
        ack_number: Some(REMOTE_SEQ + 1),
        payload:    &b"wwwwww"[..],
        ..RECV_TEMPL
    }));
    // Phase 2: one redundant in-order pass, then quiescence.
    for (i, payload) in [&b"xxxxxx"[..], b"yyyyyy", b"wwwwww", b"zzzzzz"]
        .into_iter()
        .enumerate()
    {
        recv!(s, time 1110 + i as i64 * 5, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1 + 6 * i,
            ack_number: Some(REMOTE_SEQ + 1),
            payload,
            ..RECV_TEMPL
        }));
    }
    recv_nothing!(s, time 1140);
    assert!(matches!(s.timer, Timer::Retransmit { .. }));

    send!(s, time 1200, TcpRepr {
        seq_number: REMOTE_SEQ + 1,
        ack_number: Some(LOCAL_SEQ + 1 + 24),
        ..SEND_TEMPL
    });
    assert!(s.recovery_point().is_none());
    assert!(s.sack_scoreboard.is_empty());
    assert_eq!(s.tx_buffer.len(), 0);
}

#[cfg(feature = "socket-tcp-cubic")]
#[test]
fn test_sack_fast_retransmit_selective_only_with_congestion_control() {
    // With a real congestion controller, the redundant pass is skipped:
    // recovery retransmits ONLY the holes. The cwnd budget must go to
    // useful bytes — this is the bandwidth-constrained product path.
    let mut s = socket_with_four_segments_in_flight();
    s.set_congestion_control(CongestionControl::Cubic);
    s.congestion_controller.set_mss(6);
    send_three_dupacks_sacking_2_and_4(&mut s, [1050, 1055, 1060]);

    recv!(s, time 1100, Ok(TcpRepr {
        seq_number: LOCAL_SEQ + 1,
        ack_number: Some(REMOTE_SEQ + 1),
        payload:    &b"xxxxxx"[..],
        ..RECV_TEMPL
    }));
    recv!(s, time 1105, Ok(TcpRepr {
        seq_number: LOCAL_SEQ + 1 + 12,
        ack_number: Some(REMOTE_SEQ + 1),
        payload:    &b"wwwwww"[..],
        ..RECV_TEMPL
    }));
    recv_nothing!(s, time 1110);

    send!(s, time 1200, TcpRepr {
        seq_number: REMOTE_SEQ + 1,
        ack_number: Some(LOCAL_SEQ + 1 + 24),
        ..SEND_TEMPL
    });
    assert!(s.sack_scoreboard.is_empty());
    assert_eq!(s.tx_buffer.len(), 0);
}

#[test]
fn test_sack_flight_size_excludes_peer_sacked_bytes() {
    let mut s = socket_with_four_segments_in_flight();
    assert_eq!(s.flight_size(), 24);

    s.sack_scoreboard.insert(6, 6);
    s.sack_scoreboard.insert(18, 6);
    assert_eq!(s.flight_size(), 12);

    s.flags.insert(Flags::SACK_REDUNDANT_PASS);
    assert_eq!(s.flight_size(), 24);
}

#[cfg(feature = "socket-tcp-cubic")]
#[test]
fn test_sack_relost_hole_retransmits_on_partial_ack() {
    // RFC 6582/6675: a partial ACK during recovery proves the next
    // hole's retransmission was lost again; it is retransmitted
    // immediately, without three fresh dupacks or the backed-off RTO.
    // Cubic build: selective walk only, no redundant pass.
    let mut s = socket_with_four_segments_in_flight();
    s.set_congestion_control(CongestionControl::Cubic);
    s.congestion_controller.set_mss(6);
    send_three_dupacks_sacking_2_and_4(&mut s, [1050, 1055, 1060]);

    recv!(s, time 1100, Ok(TcpRepr {
        seq_number: LOCAL_SEQ + 1,
        ack_number: Some(REMOTE_SEQ + 1),
        payload:    &b"xxxxxx"[..],
        ..RECV_TEMPL
    }));
    recv!(s, time 1105, Ok(TcpRepr {
        seq_number: LOCAL_SEQ + 1 + 12,
        ack_number: Some(REMOTE_SEQ + 1),
        payload:    &b"wwwwww"[..],
        ..RECV_TEMPL
    }));
    recv_nothing!(s, time 1110);

    // Partial ACK below the recovery point, segment 4 still SACKed:
    // hole 3's retransmission was lost again — resend it now.
    let cwnd_before_partial_ack = s.congestion_controller.window();
    send!(s, time 1210, TcpRepr {
        seq_number: REMOTE_SEQ + 1,
        ack_number: Some(LOCAL_SEQ + 1 + 12),
        sack_ranges: [
            sack_block(LOCAL_SEQ + 1 + 18, LOCAL_SEQ + 1 + 24),
            None,
            None,
        ],
        ..SEND_TEMPL
    });
    assert_eq!(s.congestion_controller.window(), cwnd_before_partial_ack);
    recv!(s, time 1215, Ok(TcpRepr {
        seq_number: LOCAL_SEQ + 1 + 12,
        ack_number: Some(REMOTE_SEQ + 1),
        payload:    &b"wwwwww"[..],
        ..RECV_TEMPL
    }));
    recv_nothing!(s, time 1220);

    send!(s, time 1300, TcpRepr {
        seq_number: REMOTE_SEQ + 1,
        ack_number: Some(LOCAL_SEQ + 1 + 24),
        ..SEND_TEMPL
    });
    assert!(s.recovery_point().is_none());
    assert!(s.sack_scoreboard.is_empty());
    assert_eq!(s.tx_buffer.len(), 0);
}

#[cfg(feature = "socket-tcp-cubic")]
#[test]
fn partial_ack_below_recovery_point_with_empty_scoreboard_stays_in_recovery() {
    let mut s = socket_with_four_segments_in_flight();
    s.set_congestion_control(CongestionControl::Cubic);
    s.congestion_controller.set_mss(6);

    // Only segment 2 is SACKed. The selective recovery walk retransmits
    // segments 1 and 3 before a cumulative ACK consumes that sole block.
    for t in [1050, 1055, 1060] {
        send!(s, time t, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1),
            sack_ranges: [
                sack_block(LOCAL_SEQ + 1 + 6, LOCAL_SEQ + 1 + 12),
                None,
                None,
            ],
            ..SEND_TEMPL
        });
    }
    recv!(s, time 1100, Ok(TcpRepr {
        seq_number: LOCAL_SEQ + 1,
        ack_number: Some(REMOTE_SEQ + 1),
        payload:    &b"xxxxxx"[..],
        ..RECV_TEMPL
    }));
    recv!(s, time 1105, Ok(TcpRepr {
        seq_number: LOCAL_SEQ + 1 + 12,
        ack_number: Some(REMOTE_SEQ + 1),
        payload:    &b"wwwwww"[..],
        ..RECV_TEMPL
    }));

    let recovery_point = s.recovery_point();
    let cwnd_before_partial_ack = s.congestion_controller.window();
    send!(s, time 1210, TcpRepr {
        seq_number: REMOTE_SEQ + 1,
        ack_number: Some(LOCAL_SEQ + 1 + 12),
        ..SEND_TEMPL
    });

    assert!(s.sack_scoreboard.is_empty());
    assert_eq!(s.recovery_point(), recovery_point);
    assert_eq!(s.congestion_controller.window(), cwnd_before_partial_ack);

    recv!(s, time 1215, Ok(TcpRepr {
        seq_number: LOCAL_SEQ + 1 + 12,
        ack_number: Some(REMOTE_SEQ + 1),
        payload:    &b"wwwwww"[..],
        ..RECV_TEMPL
    }));

    send!(s, time 1300, TcpRepr {
        seq_number: REMOTE_SEQ + 1,
        ack_number: Some(LOCAL_SEQ + 1 + 24),
        ..SEND_TEMPL
    });
    assert!(s.recovery_point().is_none());
    let cwnd_after_full_ack = s.congestion_controller.window();

    // A duplicate of the full ACK cannot deflate recovery a second time.
    send!(s, time 1310, TcpRepr {
        seq_number: REMOTE_SEQ + 1,
        ack_number: Some(LOCAL_SEQ + 1 + 24),
        ..SEND_TEMPL
    });
    assert_eq!(s.congestion_controller.window(), cwnd_after_full_ack);
}

#[test]
fn no_control_partial_ack_rewinds_to_first_sack_hole() {
    let mut s = socket_with_four_segments_in_flight();
    s.set_congestion_control(CongestionControl::None);
    let una = s.local_seq_no;
    let recovery_point = s.local_seq_next;
    s.set_recovery_point(Some(recovery_point));

    send!(s, time 1100, TcpRepr {
        seq_number: REMOTE_SEQ + 1,
        ack_number: Some(una + 6),
        sack_ranges: [
            sack_block(una + 6, una + 12),
            sack_block(una + 18, una + 24),
            None,
        ],
        ..SEND_TEMPL
    });

    let scoreboard = std::vec![(0, 6), (12, 18)];
    assert_eq!(
        s.sack_scoreboard.ranges().collect::<std::vec::Vec<_>>(),
        scoreboard
    );
    assert_eq!(s.remote_last_seq, una + 12);
    assert_eq!(s.recovery_point(), Some(recovery_point));

    recv!(s, time 1101, Ok(TcpRepr {
        seq_number: una + 12,
        ack_number: Some(REMOTE_SEQ + 1),
        payload:    &b"wwwwww"[..],
        ..RECV_TEMPL
    }));
    assert_eq!(
        s.sack_scoreboard.ranges().collect::<std::vec::Vec<_>>(),
        scoreboard
    );
    assert_eq!(s.recovery_point(), Some(recovery_point));
}

#[test]
fn test_sack_ignored_when_not_locally_advertised() {
    let mut s = socket_with_four_segments_in_flight();
    s.flags.remove(Flags::LOCAL_HAS_SACK);

    send!(s, time 1050, TcpRepr {
        seq_number: REMOTE_SEQ + 1,
        ack_number: Some(LOCAL_SEQ + 1),
        sack_ranges: [
            sack_block(LOCAL_SEQ + 1 + 6, LOCAL_SEQ + 1 + 12),
            None,
            None,
        ],
        ..SEND_TEMPL
    });

    assert!(s.sack_scoreboard.is_empty());
    recv!(s, time 5000, Ok(TcpRepr {
        seq_number: LOCAL_SEQ + 1,
        ack_number: Some(REMOTE_SEQ + 1),
        payload:    &b"xxxxxx"[..],
        ..RECV_TEMPL
    }));
}

#[test]
fn test_sack_all_outstanding_sacked_solicits_ack_and_rearms_rto() {
    let mut s = socket_established();
    s.flags.insert(Flags::LOCAL_HAS_SACK);
    s.remote_mss = 6;
    send!(s, time 0, TcpRepr {
        seq_number: REMOTE_SEQ + 1,
        ack_number: Some(LOCAL_SEQ + 1),
        ..SEND_TEMPL
    });
    s.send_slice(b"xxxxxxyyyyyy").unwrap();
    recv!(s, time 1000, Ok(TcpRepr {
        seq_number: LOCAL_SEQ + 1,
        ack_number: Some(REMOTE_SEQ + 1),
        payload:    &b"xxxxxx"[..],
        ..RECV_TEMPL
    }));
    recv!(s, time 1005, Ok(TcpRepr {
        seq_number: LOCAL_SEQ + 1 + 6,
        ack_number: Some(REMOTE_SEQ + 1),
        payload:    &b"yyyyyy"[..],
        ..RECV_TEMPL
    }));

    // The peer reports holding ALL outstanding data; only its
    // cumulative ACK is missing.
    for t in [1050, 1055, 1060] {
        send!(s, time t, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1),
            sack_ranges: [
                sack_block(LOCAL_SEQ + 1, LOCAL_SEQ + 1 + 12),
                None,
                None,
            ],
            ..SEND_TEMPL
        });
    }

    // Nothing selective to send. Under NoControl, the redundant pass
    // solicits a fresh ACK by resending in order; with a congestion
    // controller, the socket stays quiet and relies on the RTO. In
    // both cases the RTO must stay armed as the backstop.
    #[cfg(not(any(feature = "socket-tcp-cubic", feature = "socket-tcp-reno")))]
    {
        recv!(s, time 1100, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1),
            payload:    &b"xxxxxx"[..],
            ..RECV_TEMPL
        }));
        recv!(s, time 1105, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1 + 6,
            ack_number: Some(REMOTE_SEQ + 1),
            payload:    &b"yyyyyy"[..],
            ..RECV_TEMPL
        }));
    }
    recv_nothing!(s, time 1110);
    assert!(matches!(s.timer, Timer::Retransmit { .. }));

    // If even that is lost, the (backed-off) RTO clears the scoreboard
    // and recovery degrades to the conservative full rewind.
    recv!(s, time 5000, Ok(TcpRepr {
        seq_number: LOCAL_SEQ + 1,
        ack_number: Some(REMOTE_SEQ + 1),
        payload:    &b"xxxxxx"[..],
        ..RECV_TEMPL
    }));
    assert!(s.sack_scoreboard.is_empty());
    recv!(s, time 5005, Ok(TcpRepr {
        seq_number: LOCAL_SEQ + 1 + 6,
        ack_number: Some(REMOTE_SEQ + 1),
        payload:    &b"yyyyyy"[..],
        ..RECV_TEMPL
    }));
}

#[test]
fn test_sack_rto_clears_scoreboard_and_resends_all() {
    let mut s = socket_with_four_segments_in_flight();

    // One SACK-bearing duplicate ACK (not enough for fast retransmit).
    send!(s, time 1050, TcpRepr {
        seq_number: REMOTE_SEQ + 1,
        ack_number: Some(LOCAL_SEQ + 1),
        sack_ranges: [
            sack_block(LOCAL_SEQ + 1 + 6, LOCAL_SEQ + 1 + 12),
            sack_block(LOCAL_SEQ + 1 + 18, LOCAL_SEQ + 1 + 24),
            None,
        ],
        ..SEND_TEMPL
    });
    assert!(!s.sack_scoreboard.is_empty());

    // RTO: conservative recovery discards the scoreboard and resends
    // the full window, including the previously-SACKed segments.
    for (i, payload) in [&b"xxxxxx"[..], b"yyyyyy", b"wwwwww", b"zzzzzz"]
        .into_iter()
        .enumerate()
    {
        recv!(s, time 5000 + i as i64 * 5, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1 + 6 * i,
            ack_number: Some(REMOTE_SEQ + 1),
            payload,
            ..RECV_TEMPL
        }));
    }
    assert!(s.sack_scoreboard.is_empty());
}

#[cfg(any(feature = "socket-tcp-reno", feature = "socket-tcp-cubic"))]
#[test]
fn partial_ack_after_rto_resumes_congestion_control() {
    for controller in managed_controllers() {
        let mut s = socket_with_four_segments_in_flight();
        s.set_congestion_control(controller);
        s.congestion_controller.set_mss(6);
        let una = s.local_seq_no;
        let recovery_point = s.local_seq_next;

        recv!(s, time 5000, Ok(TcpRepr {
            seq_number: una,
            ack_number: Some(REMOTE_SEQ + 1),
            payload:    &b"xxxxxx"[..],
            ..RECV_TEMPL
        }));
        assert_eq!(s.congestion_controller.window(), 6);
        assert!(s.flags.contains(Flags::RECOVERY_AFTER_RTO));

        send!(s, time 5010, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(una + 6),
            ..SEND_TEMPL
        });

        assert_eq!(s.recovery_point(), Some(recovery_point));
        assert_eq!(s.congestion_controller.window(), 12);

        for (time, offset, payload) in [(5020, 6, &b"yyyyyy"[..]), (5025, 12, &b"wwwwww"[..])] {
            recv!(s, time time, Ok(TcpRepr {
                seq_number: una + offset,
                ack_number: Some(REMOTE_SEQ + 1),
                payload,
                ..RECV_TEMPL
            }));
        }
        assert_eq!(s.remote_last_seq, una + 18);

        send!(s, time 5030, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(una + 12),
            ..SEND_TEMPL
        });
        assert!(s.congestion_controller.window() > 12);
        assert_eq!(s.remote_last_seq, una + 18);

        send!(s, time 5040, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(recovery_point),
            ..SEND_TEMPL
        });
        assert!(s.recovery_point().is_none());
        assert!(!s.flags.contains(Flags::RECOVERY_AFTER_RTO));
    }
}

#[test]
fn test_sack_ingest_rejects_malformed_and_hostile_blocks() {
    let mut s = socket_with_four_segments_in_flight();

    // A pile of garbage: inverted, stale (below SND.UNA), entirely
    // beyond what we ever sent. None of it may panic or populate the
    // scoreboard.
    send!(s, time 1050, TcpRepr {
        seq_number: REMOTE_SEQ + 1,
        ack_number: Some(LOCAL_SEQ + 1),
        sack_ranges: [
            sack_block(LOCAL_SEQ + 1 + 12, LOCAL_SEQ + 1 + 6),
            sack_block(LOCAL_SEQ - 20, LOCAL_SEQ + 1),
            sack_block(LOCAL_SEQ + 1 + 5000, LOCAL_SEQ + 9000),
        ],
        ..SEND_TEMPL
    });
    assert!(s.sack_scoreboard.is_empty());

    // A block straddling SND.UNA must be trimmed, not rejected, and a
    // block overrunning the buffered data must be clamped to it.
    send!(s, time 1055, TcpRepr {
        seq_number: REMOTE_SEQ + 1,
        ack_number: Some(LOCAL_SEQ + 1),
        sack_ranges: [
            sack_block(LOCAL_SEQ - 5, LOCAL_SEQ + 1 + 6),
            sack_block(LOCAL_SEQ + 1 + 18, LOCAL_SEQ + 1 + 90),
            None,
        ],
        ..SEND_TEMPL
    });
    let ranges: std::vec::Vec<_> = s.sack_scoreboard.ranges().collect();
    assert_eq!(ranges, std::vec![(0, 6), (18, 24)]);

    // Scoreboard overflow: more disjoint ranges than the assembler can
    // track are silently dropped (advisory data), never an error.
    send!(s, time 1060, TcpRepr {
        seq_number: REMOTE_SEQ + 1,
        ack_number: Some(LOCAL_SEQ + 1),
        sack_ranges: [
            sack_block(LOCAL_SEQ + 1 + 8, LOCAL_SEQ + 1 + 9),
            sack_block(LOCAL_SEQ + 1 + 11, LOCAL_SEQ + 1 + 12),
            sack_block(LOCAL_SEQ + 1 + 14, LOCAL_SEQ + 1 + 15),
        ],
        ..SEND_TEMPL
    });
    assert!(!s.sack_scoreboard.is_empty());
}

#[test]
fn test_sack_recovery_across_sequence_wraparound() {
    let mut s = socket_established();
    s.flags.insert(Flags::LOCAL_HAS_SACK);
    s.remote_mss = 6;
    // Park the connection 13 bytes before the 2^32 wrap so the four
    // in-flight segments straddle it.
    let una = TcpSeqNumber(-13);
    s.local_seq_no = una;
    s.local_seq_next = una;
    s.remote_last_seq = una;
    s.local_rx_last_ack = Some(una);

    s.send_slice(b"xxxxxxyyyyyywwwwwwzzzzzz").unwrap();
    for (i, payload) in [&b"xxxxxx"[..], b"yyyyyy", b"wwwwww", b"zzzzzz"]
        .into_iter()
        .enumerate()
    {
        recv!(s, time 1000 + i as i64 * 5, Ok(TcpRepr {
            seq_number: una + 6 * i,
            ack_number: Some(REMOTE_SEQ + 1),
            payload,
            ..RECV_TEMPL
        }));
    }

    for t in [1050, 1055, 1060] {
        send!(s, time t, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(una),
            sack_ranges: [
                sack_block(una + 6, una + 12),
                sack_block(una + 18, una + 24),
                None,
            ],
            ..SEND_TEMPL
        });
    }

    // Holes first, straddling the wrap…
    recv!(s, time 1100, Ok(TcpRepr {
        seq_number: una,
        ack_number: Some(REMOTE_SEQ + 1),
        payload:    &b"xxxxxx"[..],
        ..RECV_TEMPL
    }));
    recv!(s, time 1105, Ok(TcpRepr {
        seq_number: una + 12,
        ack_number: Some(REMOTE_SEQ + 1),
        payload:    &b"wwwwww"[..],
        ..RECV_TEMPL
    }));
    // …then, under NoControl only, the redundant pass.
    #[cfg(not(any(feature = "socket-tcp-cubic", feature = "socket-tcp-reno")))]
    for (i, payload) in [&b"xxxxxx"[..], b"yyyyyy", b"wwwwww", b"zzzzzz"]
        .into_iter()
        .enumerate()
    {
        recv!(s, time 1110 + i as i64 * 5, Ok(TcpRepr {
            seq_number: una + 6 * i,
            ack_number: Some(REMOTE_SEQ + 1),
            payload,
            ..RECV_TEMPL
        }));
    }
    recv_nothing!(s, time 1140);

    send!(s, time 1200, TcpRepr {
        seq_number: REMOTE_SEQ + 1,
        ack_number: Some(una + 24),
        ..SEND_TEMPL
    });
    assert!(s.sack_scoreboard.is_empty());
    assert_eq!(s.tx_buffer.len(), 0);
}
