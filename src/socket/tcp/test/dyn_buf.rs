use super::*;

fn dyn_socket(cfg: DynamicBufferConfig, pool: Option<MemoryPool>) -> TestSocket {
    let (iface, _, _) = crate::tests::setup(crate::phy::Medium::Ip);
    let mut socket = Socket::new_dynamic(cfg, pool);
    socket.set_ack_delay(None);
    TestSocket {
        socket,
        cx: iface.inner,
    }
}

fn dyn_socket_listen(cfg: DynamicBufferConfig, pool: Option<MemoryPool>) -> TestSocket {
    let mut s = dyn_socket(cfg, pool);
    s.state = State::Listen;
    s.listen_endpoint = LISTEN_END;
    s
}

// Bring the SYN-RECEIVED + Established path into a dynamic socket
// without going through the full handshake macros (which assume
// legacy buffer sizing). Mirrors socket_established_with_buffer_sizes.
fn dyn_socket_established(cfg: DynamicBufferConfig, pool: Option<MemoryPool>) -> TestSocket {
    let mut s = dyn_socket(cfg, pool);
    s.state = State::Established;
    s.tuple = Some(TUPLE);
    s.local_seq_no = LOCAL_SEQ + 1;
    s.remote_seq_no = REMOTE_SEQ + 1;
    s.remote_last_seq = LOCAL_SEQ + 1;
    s.remote_last_ack = Some(REMOTE_SEQ + 1);
    s.remote_win_len = 65535;
    s.remote_last_win = s.scaled_window();
    s
}

#[derive(Debug, PartialEq, Eq)]
struct TcpSnapshot {
    repr: TcpRepr<'static>,
    payload: std::vec::Vec<u8>,
}

impl TcpSnapshot {
    fn from_repr(repr: TcpRepr<'_>) -> Self {
        Self {
            repr: TcpRepr {
                src_port: repr.src_port,
                dst_port: repr.dst_port,
                control: repr.control,
                seq_number: repr.seq_number,
                ack_number: repr.ack_number,
                window_len: repr.window_len,
                window_scale: repr.window_scale,
                max_seg_size: repr.max_seg_size,
                sack_permitted: repr.sack_permitted,
                sack_ranges: repr.sack_ranges,
                timestamp: repr.timestamp,
                payload: &[],
            },
            payload: repr.payload.to_vec(),
        }
    }
}

fn fixed_equivalent_config(tx_len: usize, rx_len: usize) -> DynamicBufferConfig {
    DynamicBufferConfig {
        rx_initial: rx_len as u32,
        rx_max: rx_len as u32,
        tx_initial: tx_len as u32,
        tx_max: tx_len as u32,
        grow_chunk: 1,
    }
}

fn dyn_socket_established_fixed_capacity(tx_len: usize, rx_len: usize) -> TestSocket {
    let mut s = dyn_socket(fixed_equivalent_config(tx_len, rx_len), None);
    s.state = State::Established;
    s.tuple = Some(TUPLE);
    s.local_seq_no = LOCAL_SEQ + 1;
    s.remote_seq_no = REMOTE_SEQ + 1;
    s.remote_last_seq = LOCAL_SEQ + 1;
    s.remote_last_ack = Some(REMOTE_SEQ + 1);
    s.remote_win_len = 256;
    s.remote_last_win = s.scaled_window();
    s
}

#[track_caller]
fn process_snapshot(
    socket: &mut TestSocket,
    timestamp: Instant,
    repr: &TcpRepr,
) -> Option<TcpSnapshot> {
    socket.cx.set_now(timestamp);
    let ip_repr = IpReprIpvX(IpvXRepr {
        src_addr: REMOTE_ADDR,
        dst_addr: LOCAL_ADDR,
        next_header: IpProtocol::Tcp,
        payload_len: repr.buffer_len(),
        hop_limit: 64,
    });

    assert!(socket.socket.accepts(&mut socket.cx, &ip_repr, repr));
    socket
        .socket
        .process(&mut socket.cx, &ip_repr, repr)
        .map(|(_, repr)| TcpSnapshot::from_repr(repr))
}

#[track_caller]
fn dispatch_snapshot(socket: &mut TestSocket, timestamp: Instant) -> Option<TcpSnapshot> {
    socket.cx.set_now(timestamp);
    let mut sent = 0;
    let mut snapshot = None;
    let result: Result<(), ()> = socket.socket.dispatch(&mut socket.cx, |_, (ip, repr)| {
        assert_eq!(ip.next_header(), IpProtocol::Tcp);
        assert_eq!(ip.src_addr(), LOCAL_ADDR.into());
        assert_eq!(ip.dst_addr(), REMOTE_ADDR.into());
        assert_eq!(ip.payload_len(), repr.buffer_len());
        sent += 1;
        snapshot = Some(TcpSnapshot::from_repr(repr));
        Ok(())
    });

    assert_eq!(result, Ok(()));
    assert!(sent <= 1, "test helper expects at most one emitted packet");
    snapshot
}

#[track_caller]
fn assert_fixed_capacity_equivalent(fixed: &TestSocket, dynamic: &TestSocket) {
    sanity!(fixed, dynamic);
    assert_eq!(
        fixed.recv_capacity(),
        dynamic.recv_capacity(),
        "rx capacity"
    );
    assert_eq!(
        fixed.send_capacity(),
        dynamic.send_capacity(),
        "tx capacity"
    );
    assert_eq!(fixed.recv_queue(), dynamic.recv_queue(), "rx queue");
    assert_eq!(fixed.send_queue(), dynamic.send_queue(), "tx queue");
    assert_eq!(fixed.may_send(), dynamic.may_send(), "may_send");
    assert_eq!(fixed.can_send(), dynamic.can_send(), "can_send");
    assert_eq!(fixed.may_recv(), dynamic.may_recv(), "may_recv");
    assert_eq!(fixed.can_recv(), dynamic.can_recv(), "can_recv");
}

fn dynamic_charge(socket: &TestSocket) -> usize {
    socket.recv_capacity() + socket.send_capacity()
}

#[test]
fn fixed_capacity_dynamic_matches_fixed_listen_handshake() {
    let mut fixed = socket_with_buffer_sizes(64, 64);
    let mut dynamic = dyn_socket(fixed_equivalent_config(64, 64), None);
    fixed.listen(LISTEN_END).unwrap();
    dynamic.listen(LISTEN_END).unwrap();

    let syn = TcpRepr {
        control: TcpControl::Syn,
        seq_number: REMOTE_SEQ,
        ack_number: None,
        window_scale: Some(0),
        ..SEND_TEMPL
    };
    assert_eq!(
        process_snapshot(&mut fixed, Instant::from_millis(0), &syn),
        process_snapshot(&mut dynamic, Instant::from_millis(0), &syn)
    );
    assert_eq!(
        dispatch_snapshot(&mut fixed, Instant::from_millis(0)),
        dispatch_snapshot(&mut dynamic, Instant::from_millis(0))
    );
    assert_fixed_capacity_equivalent(&fixed, &dynamic);

    let ack = TcpRepr {
        seq_number: REMOTE_SEQ + 1,
        ack_number: Some(LOCAL_SEQ + 1),
        ..SEND_TEMPL
    };
    assert_eq!(
        process_snapshot(&mut fixed, Instant::from_millis(0), &ack),
        process_snapshot(&mut dynamic, Instant::from_millis(0), &ack)
    );
    assert_fixed_capacity_equivalent(&fixed, &dynamic);
}

#[test]
fn fixed_capacity_dynamic_matches_fixed_established_receive_ack() {
    let mut fixed = socket_established_with_buffer_sizes(64, 64);
    let mut dynamic = dyn_socket_established_fixed_capacity(64, 64);

    let data = TcpRepr {
        seq_number: REMOTE_SEQ + 1,
        ack_number: Some(LOCAL_SEQ + 1),
        payload: &b"abcdef"[..],
        ..SEND_TEMPL
    };
    assert_eq!(
        process_snapshot(&mut fixed, Instant::from_millis(0), &data),
        process_snapshot(&mut dynamic, Instant::from_millis(0), &data)
    );
    assert_fixed_capacity_equivalent(&fixed, &dynamic);
    assert_eq!(
        dispatch_snapshot(&mut fixed, Instant::from_millis(0)),
        dispatch_snapshot(&mut dynamic, Instant::from_millis(0))
    );
    assert_fixed_capacity_equivalent(&fixed, &dynamic);
}

#[test]
fn fixed_capacity_dynamic_matches_fixed_established_send_and_ack() {
    let mut fixed = socket_established_with_buffer_sizes(64, 64);
    let mut dynamic = dyn_socket_established_fixed_capacity(64, 64);

    assert_eq!(fixed.send_slice(b"abcdef"), dynamic.send_slice(b"abcdef"));
    assert_eq!(
        dispatch_snapshot(&mut fixed, Instant::from_millis(0)),
        dispatch_snapshot(&mut dynamic, Instant::from_millis(0))
    );
    assert_fixed_capacity_equivalent(&fixed, &dynamic);

    let ack = TcpRepr {
        seq_number: REMOTE_SEQ + 1,
        ack_number: Some(LOCAL_SEQ + 1 + 6),
        ..SEND_TEMPL
    };
    assert_eq!(
        process_snapshot(&mut fixed, Instant::from_millis(0), &ack),
        process_snapshot(&mut dynamic, Instant::from_millis(0), &ack)
    );
    assert_fixed_capacity_equivalent(&fixed, &dynamic);
}

#[test]
fn fixed_capacity_dynamic_matches_fixed_graceful_close_until_fin_ack() {
    let mut fixed = socket_established_with_buffer_sizes(64, 64);
    let mut dynamic = dyn_socket_established_fixed_capacity(64, 64);

    fixed.close();
    dynamic.close();
    assert_fixed_capacity_equivalent(&fixed, &dynamic);
    assert_eq!(
        dispatch_snapshot(&mut fixed, Instant::from_millis(0)),
        dispatch_snapshot(&mut dynamic, Instant::from_millis(0))
    );
    assert_fixed_capacity_equivalent(&fixed, &dynamic);

    let fin_ack = TcpRepr {
        seq_number: REMOTE_SEQ + 1,
        ack_number: Some(LOCAL_SEQ + 1 + 1),
        ..SEND_TEMPL
    };
    assert_eq!(
        process_snapshot(&mut fixed, Instant::from_millis(0), &fin_ack),
        process_snapshot(&mut dynamic, Instant::from_millis(0), &fin_ack)
    );
    assert_fixed_capacity_equivalent(&fixed, &dynamic);
}

#[test]
fn fixed_capacity_dynamic_matches_fixed_abort_rst_on_wire() {
    let mut fixed = socket_established_with_buffer_sizes(64, 64);
    let mut dynamic = dyn_socket_established_fixed_capacity(64, 64);

    fixed.abort();
    dynamic.abort();
    assert_eq!(
        dispatch_snapshot(&mut fixed, Instant::from_millis(0)),
        dispatch_snapshot(&mut dynamic, Instant::from_millis(0))
    );
    assert_eq!(fixed.state, dynamic.state);
    assert_eq!(fixed.tuple, dynamic.tuple);
    assert_eq!(fixed.remote_seq_no, dynamic.remote_seq_no);
    assert_eq!(fixed.remote_last_seq, dynamic.remote_last_seq);
    assert_eq!(dynamic.recv_capacity(), 0);
    assert_eq!(dynamic.send_capacity(), 0);
}

#[test]
fn pool_used_matches_sum_of_dynamic_socket_capacities() {
    let pool = MemoryPool::new(64 * 1024);
    let cfg = DynamicBufferConfig::symmetric(0, 16 * 1024, 4 * 1024);
    let mut a = dyn_socket_established(cfg, Some(pool.clone()));
    let mut b = dyn_socket_established(cfg, Some(pool.clone()));

    assert_eq!(pool.used(), 0);
    assert!(a.try_grow_rx());
    assert!(a.try_grow_tx());
    assert!(b.try_grow_rx());
    assert_eq!(pool.used(), dynamic_charge(&a) + dynamic_charge(&b));

    a.abort();
    assert_eq!(pool.used(), dynamic_charge(&a) + dynamic_charge(&b));
    assert!(dispatch_snapshot(&mut a, Instant::from_millis(0)).is_some());
    assert_eq!(pool.used(), dynamic_charge(&b));

    b.reset();
    assert_eq!(pool.used(), 0);
}

#[test]
fn full_dynamic_tx_is_writable_only_when_growth_can_succeed() {
    let cfg = DynamicBufferConfig {
        rx_initial: 0,
        rx_max: 0,
        tx_initial: 4 * 1024,
        tx_max: 8 * 1024,
        grow_chunk: 4 * 1024,
    };

    let mut growable = dyn_socket_established(cfg, Some(MemoryPool::new(8 * 1024)));
    let payload = std::vec![b'x'; 4 * 1024];
    assert_eq!(growable.send_slice(&payload), Ok(payload.len()));
    assert!(growable.tx_buffer.is_full());
    assert!(growable.can_send());
    assert_eq!(growable.send_slice(b"y"), Ok(1));
    assert_eq!(growable.send_capacity(), 8 * 1024);

    let mut exhausted = dyn_socket_established(cfg, Some(MemoryPool::new(4 * 1024)));
    assert_eq!(exhausted.send_slice(&payload), Ok(payload.len()));
    assert!(exhausted.tx_buffer.is_full());
    assert!(!exhausted.can_send());
    assert_eq!(exhausted.send_slice(b"y"), Ok(0));
    assert_eq!(exhausted.send_capacity(), 4 * 1024);
}

#[test]
fn idle_socket_zero_allocation() {
    // rx_initial = tx_initial = 0 → no backing storage at all until
    // pressure forces it.
    let cfg = DynamicBufferConfig::symmetric(0, 64 * 1024, 4 * 1024);
    let s = dyn_socket(cfg, None);
    assert_eq!(s.rx_buffer.capacity(), 0);
    assert_eq!(s.tx_buffer.capacity(), 0);
    // Window-scale must still be sized for the *max*, since it's
    // fixed at SYN time and the connection may later grow.
    // 64 KiB = 2^16 → ceil(log2) = 17 → shift = 17 - 16 = 1.
    assert_eq!(s.remote_win_shift, 1);
}

#[test]
fn idle_socket_with_pool_no_charge() {
    // 1 MiB budget, 100 idle sockets at zero-initial → pool used == 0.
    let pool = MemoryPool::new(1024 * 1024);
    let cfg = DynamicBufferConfig::symmetric(0, 32 * 1024, 4 * 1024);
    let mut sockets = std::vec::Vec::new();
    for _ in 0..100 {
        sockets.push(Socket::new_dynamic(cfg, Some(pool.clone())));
    }
    assert_eq!(pool.used(), 0);
    // Dropping all sockets keeps pool clean.
    drop(sockets);
    assert_eq!(pool.used(), 0);
}

#[test]
fn nonzero_initial_charges_pool() {
    let pool = MemoryPool::new(64 * 1024);
    let cfg = DynamicBufferConfig::symmetric(4 * 1024, 32 * 1024, 4 * 1024);
    let s = Socket::new_dynamic(cfg, Some(pool.clone()));
    assert_eq!(s.rx_buffer.capacity(), 4 * 1024);
    assert_eq!(s.tx_buffer.capacity(), 4 * 1024);
    assert_eq!(pool.used(), 8 * 1024);
    drop(s);
    assert_eq!(pool.used(), 0);
}

#[test]
fn pool_overcommit_falls_back_to_zero() {
    // Pool budget too small for the requested initial → constructor
    // refuses the reservation and the socket comes up with zero
    // capacity, not a partial allocation.
    let pool = MemoryPool::new(1024);
    let cfg = DynamicBufferConfig::symmetric(8 * 1024, 32 * 1024, 4 * 1024);
    let s = Socket::new_dynamic(cfg, Some(pool.clone()));
    assert_eq!(s.rx_buffer.capacity(), 0);
    assert_eq!(s.tx_buffer.capacity(), 0);
    assert_eq!(pool.used(), 0);
}

#[test]
fn win_shift_uses_max_not_current() {
    // The negotiated window scale must accommodate any future
    // growth, so it tracks rx_max, not the current (possibly tiny)
    // capacity. The remote sees this in the SYN-ACK.
    for (rx_max, expected_shift) in &[
        (1024usize, 0u8),
        (32 * 1024, 0),
        (64 * 1024, 1),
        (128 * 1024, 2),
        (256 * 1024, 3),
        (1024 * 1024, 5),
    ] {
        let cfg = DynamicBufferConfig {
            rx_initial: 0,
            rx_max: *rx_max as u32,
            tx_initial: 0,
            tx_max: *rx_max as u32,
            grow_chunk: 4 * 1024,
        };
        let s = Socket::new_dynamic(cfg, None);
        assert_eq!(
            s.remote_win_shift, *expected_shift,
            "rx_max={rx_max}: expected shift={expected_shift}, got {}",
            s.remote_win_shift
        );
    }
}

#[test]
fn syn_advertises_unscaled_window_from_initial() {
    // For a Listen → SYN-ACK exchange, the SYN-ACK carries an
    // unscaled window field. With rx_initial = 0, the advertised
    // window in the SYN-ACK is 0. Crucially, the window_scale
    // option still carries the rx_max-derived shift.
    let cfg = DynamicBufferConfig::symmetric(0, 128 * 1024, 8 * 1024);
    let mut s = dyn_socket_listen(cfg, None);
    // Capacity at this moment really is zero — no preallocation.
    assert_eq!(s.rx_buffer.capacity(), 0);
    send!(
        s,
        TcpRepr {
            control: TcpControl::Syn,
            seq_number: REMOTE_SEQ,
            ack_number: None,
            window_scale: Some(0),
            ..SEND_TEMPL
        }
    );
    // After processing the SYN, dispatch() will run; the rx growth
    // hook fires before scaled_window(), so we should see the
    // advertised window reflect a newly-grown buffer.
    recv!(
        s,
        Ok(TcpRepr {
            control: TcpControl::Syn,
            seq_number: LOCAL_SEQ,
            ack_number: Some(REMOTE_SEQ + 1),
            max_seg_size: Some(BASE_MSS),
            window_scale: Some(2), // 128 KiB → shift 2
            // Unscaled SYN window: should reflect the rx_buffer
            // capacity after the dispatch-time growth (8 KiB chunk).
            window_len: 8 * 1024,
            ..RECV_TEMPL
        })
    );
    // After SYN-ACK, the buffer has been grown at least once.
    assert!(s.rx_buffer.capacity() >= 8 * 1024);
}

#[test]
fn rx_grows_on_data_pressure() {
    // Established connection with tiny initial rx. Peer sends a
    // segment that fills the buffer; the next dispatch grows rx so
    // we can advertise a larger window.
    let cfg = DynamicBufferConfig::symmetric(4 * 1024, 32 * 1024, 4 * 1024);
    let mut s = dyn_socket_established(cfg, None);

    assert_eq!(s.rx_buffer.capacity(), 4 * 1024);
    // Peer sends data that fills our buffer.
    let payload = std::vec![b'x'; 4096];
    send!(
        s,
        TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1),
            payload: &payload,
            ..SEND_TEMPL
        }
    );
    // The dispatch path should grow rx so we can advertise some
    // window again. The ACK we emit reflects the new capacity.
    recv!(
        s,
        Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1 + 4096),
            // After growing by one 4 KiB chunk, window is 4 KiB.
            // The advertised value is scaled (shift = 0 here since
            // rx_max = 32 KiB).
            window_len: 4 * 1024,
            ..RECV_TEMPL
        })
    );
    assert!(s.rx_buffer.capacity() > 4 * 1024);
    assert!(s.rx_buffer.capacity() <= 32 * 1024);
}

#[test]
fn pool_exhaustion_collapses_window() {
    // Budget = 4 KiB only. First socket takes it; second cannot
    // grow at all. Second socket's advertised window stays 0.
    let pool = MemoryPool::new(4 * 1024);
    let cfg = DynamicBufferConfig::symmetric(0, 32 * 1024, 4 * 1024);
    let _winner = {
        // Force the winner to actually grow.
        let mut s = dyn_socket_established(cfg, Some(pool.clone()));
        let grew = s.try_grow_rx();
        assert!(grew);
        assert!(s.rx_buffer.capacity() >= 4 * 1024);
        s
    };
    // Budget should now be fully consumed.
    assert_eq!(pool.available(), 0);

    // A second dynamic socket on the same pool can't grow at all.
    let mut loser = dyn_socket_established(cfg, Some(pool.clone()));
    assert!(!loser.try_grow_rx());
    assert_eq!(loser.rx_buffer.capacity(), 0);

    // And the scaled_window must therefore be 0 — backpressure.
    assert_eq!(loser.scaled_window(), 0);
}

#[test]
fn pool_refunded_on_close() {
    let pool = MemoryPool::new(64 * 1024);
    let cfg = DynamicBufferConfig::symmetric(4 * 1024, 32 * 1024, 4 * 1024);
    {
        let mut s = dyn_socket_established(cfg, Some(pool.clone()));
        // Force more growth so we have something nontrivial to refund.
        while s.try_grow_rx() {}
        while s.try_grow_tx() {}
        assert!(pool.used() > 8 * 1024);
        // Close via the public abort path → set_state(Closed) →
        // release hook fires.
        s.abort();
    }
    assert_eq!(pool.used(), 0);
}

#[test]
fn pool_refunded_on_drop() {
    let pool = MemoryPool::new(64 * 1024);
    let cfg = DynamicBufferConfig::symmetric(4 * 1024, 32 * 1024, 4 * 1024);
    {
        let _s = Socket::new_dynamic(cfg, Some(pool.clone()));
        assert_eq!(pool.used(), 8 * 1024);
    } // Drop fires here.
    assert_eq!(pool.used(), 0);
}

#[test]
fn pool_refunded_on_reset() {
    let pool = MemoryPool::new(64 * 1024);
    let cfg = DynamicBufferConfig::symmetric(4 * 1024, 32 * 1024, 4 * 1024);
    let mut s = Socket::new_dynamic(cfg, Some(pool.clone()));
    assert_eq!(pool.used(), 8 * 1024);
    // Internal reset (e.g. would happen at TIME-WAIT expiry).
    s.reset();
    assert_eq!(pool.used(), 0);
    // Re-using the socket post-reset works: window scale recomputed
    // from rx_max.
    assert_eq!(s.remote_win_shift, 0); // 32 KiB → shift 0
    assert_eq!(s.rx_buffer.capacity(), 0);
}

#[test]
fn zero_initial_tx_can_send_when_growth_possible() {
    let cfg = DynamicBufferConfig::symmetric(0, 32 * 1024, 4 * 1024);
    let s = dyn_socket_established(cfg, None);

    assert!(s.may_send());
    assert!(s.can_send());
}

#[test]
fn public_listen_preserves_nonzero_initial_capacity() {
    let pool = MemoryPool::new(64 * 1024);
    let cfg = DynamicBufferConfig::symmetric(4 * 1024, 32 * 1024, 4 * 1024);
    let mut s = dyn_socket(cfg, Some(pool.clone()));

    assert_eq!(pool.used(), 8 * 1024);
    assert_eq!(s.recv_capacity(), 4 * 1024);
    assert_eq!(s.send_capacity(), 4 * 1024);

    s.listen(LISTEN_END).unwrap();

    assert_eq!(pool.used(), 8 * 1024);
    assert_eq!(s.recv_capacity(), 4 * 1024);
    assert_eq!(s.send_capacity(), 4 * 1024);
}

#[test]
fn public_connect_preserves_nonzero_initial_capacity() {
    let pool = MemoryPool::new(64 * 1024);
    let cfg = DynamicBufferConfig::symmetric(4 * 1024, 32 * 1024, 4 * 1024);
    let mut s = dyn_socket(cfg, Some(pool.clone()));

    s.socket
        .connect(&mut s.cx, REMOTE_END, LOCAL_END.port)
        .unwrap();

    assert_eq!(pool.used(), 8 * 1024);
    assert_eq!(s.recv_capacity(), 4 * 1024);
    assert_eq!(s.send_capacity(), 4 * 1024);
}

#[test]
fn last_ack_data_fin_ack_dequeues_before_release() {
    let pool = MemoryPool::new(64 * 1024);
    let cfg = DynamicBufferConfig::symmetric(4 * 1024, 32 * 1024, 4 * 1024);
    let mut s = dyn_socket_established(cfg, Some(pool.clone()));
    s.state = State::LastAck;
    s.remote_seq_no = REMOTE_SEQ + 1 + 1;
    s.remote_last_ack = Some(REMOTE_SEQ + 1 + 1);
    assert_eq!(s.tx_buffer.enqueue_slice(b"x"), 1);
    s.remote_last_seq = LOCAL_SEQ + 1 + 1 + 1;

    send!(
        s,
        TcpRepr {
            seq_number: REMOTE_SEQ + 1 + 1,
            ack_number: Some(LOCAL_SEQ + 1 + 1 + 1),
            ..SEND_TEMPL
        }
    );

    assert_eq!(s.state, State::Closed);
    assert_eq!(s.send_queue(), 0);
    assert_eq!(pool.used(), 0);
}

#[test]
fn abort_with_unread_rx_sends_correct_rst_ack_then_refunds_pool() {
    let pool = MemoryPool::new(64 * 1024);
    let cfg = DynamicBufferConfig::symmetric(4 * 1024, 32 * 1024, 4 * 1024);
    let mut s = dyn_socket_established(cfg, Some(pool.clone()));
    let _ = send(
        &mut s,
        Instant::from_millis(0),
        &TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1),
            payload: &b"abcd"[..],
            ..SEND_TEMPL
        },
    );
    assert_eq!(s.recv_queue(), 4);
    assert_eq!(pool.used(), 8 * 1024);

    s.abort();
    assert_eq!(s.recv_queue(), 4);
    assert_eq!(pool.used(), 8 * 1024);

    recv!(
        s,
        Ok(TcpRepr {
            control: TcpControl::Rst,
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1 + 4),
            window_len: 4 * 1024 - 4,
            ..RECV_TEMPL
        })
    );
    assert_eq!(pool.used(), 0);
    assert_eq!(s.recv_capacity(), 0);
}

#[test]
fn time_wait_releases_empty_dynamic_buffers_after_ack() {
    let pool = MemoryPool::new(64 * 1024);
    let cfg = DynamicBufferConfig::symmetric(4 * 1024, 32 * 1024, 4 * 1024);
    let mut s = dyn_socket_established(cfg, Some(pool.clone()));

    assert!(s.try_grow_rx());
    assert!(s.try_grow_tx());
    assert!(pool.used() > 8 * 1024);

    s.close();
    recv!(
        s,
        [TcpRepr {
            control: TcpControl::Fin,
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1),
            window_len: 8 * 1024,
            ..RECV_TEMPL
        }]
    );
    send!(
        s,
        TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1 + 1),
            ..SEND_TEMPL
        }
    );
    send!(
        s,
        TcpRepr {
            control: TcpControl::Fin,
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1 + 1),
            ..SEND_TEMPL
        }
    );
    recv!(
        s,
        [TcpRepr {
            seq_number: LOCAL_SEQ + 1 + 1,
            ack_number: Some(REMOTE_SEQ + 1 + 1),
            window_len: 8 * 1024,
            ..RECV_TEMPL
        }]
    );

    assert_eq!(s.state, State::TimeWait);
    assert_eq!(pool.used(), 0);
    assert_eq!(s.recv_capacity(), 0);
    assert_eq!(s.send_capacity(), 0);
}

#[test]
fn time_wait_retransmitted_fin_ack_survives_buffer_release() {
    let pool = MemoryPool::new(64 * 1024);
    let cfg = DynamicBufferConfig::symmetric(4 * 1024, 32 * 1024, 4 * 1024);
    let mut s = dyn_socket_established(cfg, Some(pool.clone()));

    assert!(s.try_grow_rx());
    assert!(s.try_grow_tx());

    s.close();
    recv!(
        s,
        [TcpRepr {
            control: TcpControl::Fin,
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1),
            window_len: 8 * 1024,
            ..RECV_TEMPL
        }]
    );
    send!(
        s,
        TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1 + 1),
            ..SEND_TEMPL
        }
    );
    send!(
        s,
        TcpRepr {
            control: TcpControl::Fin,
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1 + 1),
            ..SEND_TEMPL
        }
    );
    recv!(
        s,
        [TcpRepr {
            seq_number: LOCAL_SEQ + 1 + 1,
            ack_number: Some(REMOTE_SEQ + 1 + 1),
            window_len: 8 * 1024,
            ..RECV_TEMPL
        }]
    );

    assert_eq!(s.state, State::TimeWait);
    assert_eq!(pool.used(), 0);
    assert_eq!(s.recv_capacity(), 0);

    send!(s, time 5_000, TcpRepr {
        control: TcpControl::Fin,
        seq_number: REMOTE_SEQ + 1,
        ack_number: Some(LOCAL_SEQ + 1 + 1),
        ..SEND_TEMPL
    }, Some(TcpRepr {
        seq_number: LOCAL_SEQ + 1 + 1,
        ack_number: Some(REMOTE_SEQ + 1 + 1),
        window_len: 0,
        ..RECV_TEMPL
    }));

    assert_eq!(
        s.timer,
        Timer::Close {
            expires_at: Instant::from_secs(5) + CLOSE_DELAY
        }
    );
    assert_eq!(pool.used(), 0);
}

#[test]
fn established_remote_rst_releases_dynamic_buffers() {
    let pool = MemoryPool::new(64 * 1024);
    let cfg = DynamicBufferConfig::symmetric(4 * 1024, 32 * 1024, 4 * 1024);
    let mut s = dyn_socket_established(cfg, Some(pool.clone()));

    assert!(s.try_grow_rx());
    assert!(s.try_grow_tx());
    assert!(pool.used() > 8 * 1024);

    send!(
        s,
        TcpRepr {
            control: TcpControl::Rst,
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1),
            ..SEND_TEMPL
        }
    );

    assert_eq!(s.state, State::Closed);
    assert_eq!(s.tuple, None);
    assert_eq!(pool.used(), 0);
    assert_eq!(s.recv_capacity(), 0);
    assert_eq!(s.send_capacity(), 0);
}

#[test]
fn send_data_survives_until_ack_with_dynamic_tx() {
    // Standard correctness: pre-ACK tx data is not freed by growth.
    let cfg = DynamicBufferConfig::symmetric(4 * 1024, 32 * 1024, 4 * 1024);
    let mut s = dyn_socket_established(cfg, None);
    assert_eq!(s.send_slice(b"hello").unwrap(), 5);
    assert_eq!(s.tx_buffer.len(), 5);
    // Force a growth (we have plenty of headroom in tx_max).
    s.try_grow_tx();
    // Data must still be present.
    assert_eq!(s.tx_buffer.len(), 5);
    let mut buf = std::vec![0u8; 5];
    let copied = s.tx_buffer.read_allocated(0, &mut buf);
    assert_eq!(copied, 5);
    assert_eq!(&buf, b"hello");
}

#[test]
fn never_advertise_more_than_backing_capacity() {
    // Capacity = ground truth for the advertised window. After any
    // sequence of grows + len changes, scaled_window << shift never
    // exceeds rx_buffer.window().
    let cfg = DynamicBufferConfig::symmetric(0, 128 * 1024, 8 * 1024);
    let mut s = dyn_socket_established(cfg, None);
    for _ in 0..3 {
        s.try_grow_rx();
        let scaled = s.scaled_window() as usize;
        let backed = s.rx_buffer.window();
        assert!(
            scaled << s.remote_win_shift <= backed,
            "scaled={scaled} shift={} backed={backed}",
            s.remote_win_shift,
        );
    }
}

#[test]
fn growth_preserves_buffered_payload_ordering() {
    // Critical correctness: RingBuffer::try_grow must preserve
    // logical byte order even when read_at != 0 (wrapped state).
    // Direct test on the storage primitive.
    let mut ring: SocketBuffer = SocketBuffer::new(std::vec![0u8; 8]);
    // Fill, then wrap by dequeuing 4 + enqueuing 4.
    assert_eq!(ring.enqueue_slice(b"abcdefgh"), 8);
    assert_eq!(ring.dequeue_many(4), b"abcd");
    assert_eq!(ring.enqueue_slice(b"ijkl"), 4);
    // Logical contents now "efghijkl", but read_at = 4, length = 8.
    assert!(ring.try_grow(16));
    assert_eq!(ring.capacity(), 16);
    let mut out = std::vec![0u8; 8];
    assert_eq!(ring.read_allocated(0, &mut out), 8);
    assert_eq!(&out, b"efghijkl");
}

#[test]
fn pool_capacity_floor_300_sockets_24mib_budget() {
    // The iOS target: ~300 sockets with 24 MiB total budget.
    // With rx_initial = tx_initial = 0 and a 24 MiB pool, all
    // sockets fit idle, and a subset can grow to a per-flow max.
    let pool = MemoryPool::new(24 * 1024 * 1024);
    let cfg = DynamicBufferConfig::symmetric(0, 64 * 1024, 8 * 1024);
    let mut sockets = std::vec::Vec::new();
    for _ in 0..300 {
        sockets.push(Socket::new_dynamic(cfg, Some(pool.clone())));
    }
    assert_eq!(
        pool.used(),
        0,
        "idle sockets should not reserve any pool memory"
    );

    // Now grow the first 50 to full rx_max + tx_max = 128 KiB each.
    // 50 * 128 KiB = 6.4 MiB, well within 24 MiB budget.
    for s in &mut sockets[..50] {
        // Open with a tuple so the socket is in a state where
        // growth makes sense. We just exercise the grow API.
        while s.try_grow_rx() {}
        while s.try_grow_tx() {}
        assert_eq!(s.recv_capacity(), 64 * 1024);
        assert_eq!(s.send_capacity(), 64 * 1024);
    }
    assert_eq!(pool.used(), 50 * 128 * 1024);

    // Drop everything; pool should be fully refunded.
    drop(sockets);
    assert_eq!(pool.used(), 0);
}

#[test]
fn growth_is_geometric() {
    // Without pool pressure, each grow at minimum doubles, so a
    // buffer goes 0 → chunk → 2·chunk → 4·chunk → ... → rx_max in
    // O(log) steps. The previous linear (`+chunk`) growth required
    // rx_max / chunk steps, each copying len bytes (O(n²) total
    // memcpy). Geometric matches Linux `tcp_rcv_space_adjust`
    // (`copied << 1`) and XNU `tcp_sbrcv_grow` (×2 / ×4 of the
    // RTT byte rate).
    let cfg = DynamicBufferConfig::symmetric(0, 64 * 1024, 4 * 1024);
    let mut s = dyn_socket_established(cfg, None);
    let mut caps = std::vec::Vec::new();
    while s.try_grow_rx() {
        caps.push(s.rx_buffer.capacity());
        if caps.len() > 32 {
            panic!("grow loop did not converge: {caps:?}");
        }
    }
    // Expected geometric progression: 4K, 8K, 16K, 32K, 64K.
    assert_eq!(
        caps,
        std::vec![4 * 1024, 8 * 1024, 16 * 1024, 32 * 1024, 64 * 1024]
    );
}

#[test]
fn growth_throttles_under_pool_pressure() {
    // When the pool is past 75% used, sockets fall back to linear
    // `+chunk` growth so a single greedy socket can't drain the
    // budget. Same conceptual gate as Linux's
    // `tcp_under_memory_pressure(sk)`.
    let pool = MemoryPool::new(64 * 1024);
    // Pre-charge to 75% to put the pool immediately under pressure.
    assert!(pool.try_charge(48 * 1024));
    assert!(pool.under_pressure());

    let cfg = DynamicBufferConfig::symmetric(0, 32 * 1024, 4 * 1024);
    let mut s = dyn_socket_established(cfg, Some(pool.clone()));
    // First grow: 0 → 4K (single chunk, geometric or linear same).
    assert!(s.try_grow_rx());
    assert_eq!(s.rx_buffer.capacity(), 4 * 1024);
    // Second grow: under pressure → linear, so 4K → 8K, not 4K → 8K.
    // (Both equal here; check the next step:)
    assert!(s.try_grow_rx());
    assert_eq!(s.rx_buffer.capacity(), 8 * 1024);
    // Third: under pressure → +4K → 12K (NOT doubled to 16K).
    assert!(s.try_grow_rx());
    assert_eq!(
        s.rx_buffer.capacity(),
        12 * 1024,
        "pressure should force linear growth, not geometric"
    );
}

#[test]
fn next_capacity_math() {
    // Geometric, no pressure.
    assert_eq!(Socket::next_capacity(0, 4096, 65536, false), 4096);
    assert_eq!(Socket::next_capacity(4096, 4096, 65536, false), 8192);
    assert_eq!(Socket::next_capacity(8192, 4096, 65536, false), 16384);
    assert_eq!(Socket::next_capacity(32768, 4096, 65536, false), 65536);
    // At max → no grow.
    assert_eq!(Socket::next_capacity(65536, 4096, 65536, false), 65536);
    // Pressure throttle → linear.
    assert_eq!(Socket::next_capacity(8192, 4096, 65536, true), 12288);
    assert_eq!(Socket::next_capacity(32768, 4096, 65536, true), 36864);
    // Clamp at max even with linear.
    assert_eq!(Socket::next_capacity(60000, 4096, 65536, true), 64096);
}

#[test]
fn release_is_idempotent() {
    // Defensive: calling release twice (via abort then drop, or
    // via close-then-reset, etc.) must not double-refund the pool.
    let pool = MemoryPool::new(64 * 1024);
    let cfg = DynamicBufferConfig::symmetric(4 * 1024, 32 * 1024, 4 * 1024);
    let mut s = Socket::new_dynamic(cfg, Some(pool.clone()));
    assert_eq!(pool.used(), 8 * 1024);
    // Force more growth.
    while s.try_grow_rx() {}
    let charged = pool.used();
    assert!(charged > 8 * 1024);
    // First release via abort.
    s.abort();
    assert_eq!(pool.used(), 0);
    // Re-set the state to simulate stale-state code paths.
    s.set_state(State::Closed);
    assert_eq!(pool.used(), 0);
    // And Drop.
    drop(s);
    assert_eq!(pool.used(), 0);
}

#[test]
fn pool_accounting_survives_u32_max_caps() {
    // Regression for the `charged: u32` + uncapped `tx_max` bug
    // (independent review #1). With charged widened to `usize` and
    // both per-direction caps clamped to RFC 1323's 2^30 ceiling,
    // pool accounting must reach 2 × 2^30 = 2 GiB without
    // saturation or truncation loss.
    //
    // We use the public DynamicBufferConfig fields (which are
    // `u32`); the constructor clamps both to 1<<30 internally.
    let cfg = DynamicBufferConfig {
        rx_initial: 0,
        rx_max: u32::MAX, // larger than the 2^30 cap; should clamp.
        tx_initial: 0,
        tx_max: u32::MAX, // ditto — the *bug* was this not clamping.
        grow_chunk: 4 * 1024,
    };
    let s = Socket::new_dynamic(cfg, None);
    // recv_capacity_max / send_capacity_max should both report
    // the clamped value, not the original u32::MAX.
    assert_eq!(s.recv_capacity_max(), 1 << 30);
    assert_eq!(s.send_capacity_max(), 1 << 30);
}

#[test]
fn pool_accounting_no_truncation_on_initial_charge() {
    // The bug was: `state.charge(charge as u32)` truncated when
    // rx_initial + tx_initial exceeded u32::MAX, but the Vec
    // allocations were full-size. Pool would undercount; physical
    // memory could exceed budget. Fixed by widening charge to
    // usize and using checked_add.
    //
    // Exercise the accounting layer directly instead of allocating
    // 1.5 GiB of Vec backing storage. The invariant is arithmetic,
    // not allocator behavior.
    let pool = MemoryPool::new(2 * 1024 * 1024 * 1024);
    let cfg = DynamicBufferConfig {
        rx_initial: 0,
        rx_max: 1 << 30,
        tx_initial: 0,
        tx_max: 1 << 30,
        grow_chunk: 8 * 1024,
    };
    let mut state = dynbuf::DynBufState::new(&cfg, Some(pool.clone()));
    let charge = 2 * 768 * 1024 * 1024;

    assert!(state.charge(charge));
    assert_eq!(state.charged, charge);
    assert_eq!(pool.used(), charge);

    state.refund(charge);
    assert_eq!(pool.used(), 0);
    assert_eq!(state.charged, 0);
}

#[test]
fn syn_received_rst_to_listen_releases_pool_charge() {
    // Regression for the listen-server pool-leak bug (independent
    // review #2). When a listen socket transitions through
    // SynReceived and then receives RST, smoltcp moves it back to
    // Listen (tcp.rs handler ~line 2162). Before the fix this
    // path skipped release_dyn_buffers, leaking pool charge on
    // every failed handshake.
    let pool = MemoryPool::new(64 * 1024);
    let cfg = DynamicBufferConfig::symmetric(0, 32 * 1024, 8 * 1024);
    let mut s = dyn_socket_listen(cfg, Some(pool.clone()));

    // Verify the baseline: pool is empty while listening.
    assert_eq!(pool.used(), 0);

    // Push the socket into SynReceived and grow its buffers (which
    // is what a real SYN-ACK dispatch would do).
    s.state = State::SynReceived;
    s.tuple = Some(TUPLE);
    s.local_seq_no = LOCAL_SEQ;
    s.remote_seq_no = REMOTE_SEQ + 1;
    s.remote_last_seq = LOCAL_SEQ;
    assert!(s.try_grow_rx());
    let charged = pool.used();
    assert!(charged > 0, "rx grow should have charged the pool");

    // Now simulate the smoltcp handler's response to RST in
    // SynReceived for a listen socket: return to Listen.
    s.tuple = None;
    s.set_state(State::Listen);

    // The fix: this transition now releases the dynamic buffers
    // so the listen socket goes back to the zero-charge state
    // it had before the SYN arrived.
    assert_eq!(
        pool.used(),
        0,
        "SynReceived → Listen on RST must refund the pool charge"
    );
    assert_eq!(s.rx_buffer.capacity(), 0);
    assert_eq!(s.tx_buffer.capacity(), 0);
    assert_eq!(s.state, State::Listen);
}

#[test]
fn syn_received_rst_to_listen_restores_nonzero_initial_capacity() {
    let pool = MemoryPool::new(64 * 1024);
    let cfg = DynamicBufferConfig {
        rx_initial: 4 * 1024,
        rx_max: 32 * 1024,
        tx_initial: 2 * 1024,
        tx_max: 16 * 1024,
        grow_chunk: 4 * 1024,
    };
    let mut s = dyn_socket_listen(cfg, Some(pool.clone()));

    assert_eq!(pool.used(), 6 * 1024);
    assert_eq!(s.rx_buffer.capacity(), 4 * 1024);
    assert_eq!(s.tx_buffer.capacity(), 2 * 1024);

    s.state = State::SynReceived;
    s.tuple = Some(TUPLE);
    s.local_seq_no = LOCAL_SEQ;
    s.remote_seq_no = REMOTE_SEQ + 1;
    s.remote_last_seq = LOCAL_SEQ;
    assert!(s.try_grow_rx());
    assert!(pool.used() > 6 * 1024);

    s.tuple = None;
    s.set_state(State::Listen);

    assert_eq!(s.state, State::Listen);
    assert_eq!(pool.used(), 6 * 1024);
    assert_eq!(s.rx_buffer.capacity(), 4 * 1024);
    assert_eq!(s.tx_buffer.capacity(), 2 * 1024);
}
