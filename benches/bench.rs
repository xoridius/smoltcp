#![feature(test)]

mod wire {
    use smoltcp::phy::ChecksumCapabilities;
    use smoltcp::wire::{IpAddress, IpProtocol};
    #[cfg(feature = "proto-ipv4")]
    use smoltcp::wire::{Ipv4Address, Ipv4Packet, Ipv4Repr};
    #[cfg(feature = "proto-ipv6")]
    use smoltcp::wire::{Ipv6Address, Ipv6Packet, Ipv6Repr};
    use smoltcp::wire::{TcpControl, TcpPacket, TcpRepr, TcpSeqNumber};
    use smoltcp::wire::{UdpPacket, UdpRepr};

    extern crate test;

    #[cfg(feature = "proto-ipv6")]
    const SRC_ADDR: IpAddress = IpAddress::Ipv6(Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1));
    #[cfg(feature = "proto-ipv6")]
    const DST_ADDR: IpAddress = IpAddress::Ipv6(Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 2));

    #[cfg(all(not(feature = "proto-ipv6"), feature = "proto-ipv4"))]
    const SRC_ADDR: IpAddress = IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 1));
    #[cfg(all(not(feature = "proto-ipv6"), feature = "proto-ipv4"))]
    const DST_ADDR: IpAddress = IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 2));

    #[bench]
    #[cfg(any(feature = "proto-ipv6", feature = "proto-ipv4"))]
    fn bench_emit_tcp(b: &mut test::Bencher) {
        static PAYLOAD_BYTES: [u8; 400] = [0x2a; 400];
        let repr = TcpRepr {
            src_port: 48896,
            dst_port: 80,
            control: TcpControl::Syn,
            seq_number: TcpSeqNumber(0x01234567),
            ack_number: None,
            window_len: 0x0123,
            window_scale: None,
            max_seg_size: None,
            sack_permitted: false,
            sack_ranges: [None, None, None],
            payload: &PAYLOAD_BYTES,
            timestamp: None,
        };
        let mut bytes = vec![0xa5; repr.buffer_len()];

        b.iter(|| {
            let mut packet = TcpPacket::new_unchecked(&mut bytes);
            repr.emit(
                &mut packet,
                &SRC_ADDR,
                &DST_ADDR,
                &ChecksumCapabilities::default(),
            );
        });
    }

    #[bench]
    #[cfg(any(feature = "proto-ipv6", feature = "proto-ipv4"))]
    fn bench_emit_udp(b: &mut test::Bencher) {
        static PAYLOAD_BYTES: [u8; 400] = [0x2a; 400];
        let repr = UdpRepr {
            src_port: 48896,
            dst_port: 80,
        };
        let mut bytes = vec![0xa5; repr.header_len() + PAYLOAD_BYTES.len()];

        b.iter(|| {
            let mut packet = UdpPacket::new_unchecked(&mut bytes);
            repr.emit(
                &mut packet,
                &SRC_ADDR,
                &DST_ADDR,
                PAYLOAD_BYTES.len(),
                |buf| buf.copy_from_slice(&PAYLOAD_BYTES),
                &ChecksumCapabilities::default(),
            );
        });
    }

    #[bench]
    #[cfg(feature = "proto-ipv4")]
    fn bench_emit_ipv4(b: &mut test::Bencher) {
        let repr = Ipv4Repr {
            src_addr: Ipv4Address::new(192, 168, 1, 1),
            dst_addr: Ipv4Address::new(192, 168, 1, 2),
            next_header: IpProtocol::Tcp,
            payload_len: 100,
            hop_limit: 64,
        };
        let mut bytes = vec![0xa5; repr.buffer_len()];

        b.iter(|| {
            let mut packet = Ipv4Packet::new_unchecked(&mut bytes);
            repr.emit(&mut packet, &ChecksumCapabilities::default());
        });
    }

    #[bench]
    #[cfg(feature = "proto-ipv6")]
    fn bench_emit_ipv6(b: &mut test::Bencher) {
        let repr = Ipv6Repr {
            src_addr: Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1),
            dst_addr: Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 2),
            next_header: IpProtocol::Tcp,
            payload_len: 100,
            hop_limit: 64,
        };
        let mut bytes = vec![0xa5; repr.buffer_len()];

        b.iter(|| {
            let mut packet = Ipv6Packet::new_unchecked(&mut bytes);
            repr.emit(&mut packet);
        });
    }

    // --- RFC 1071 checksum sweep across realistic packet sizes ---
    // 64: small TCP ACKs. 576: legacy IPv4 minimum MTU. 1500: standard Ethernet MTU.
    // 9000: jumbo. 65535: maximum IP packet (worst case for tunnel reassembly).

    fn bench_checksum_size(b: &mut test::Bencher, size: usize) {
        let buf = vec![0xa5u8; size];
        b.bytes = size as u64;
        b.iter(|| test::black_box(smoltcp::wire::checksum::data(test::black_box(&buf))));
    }

    #[bench]
    fn bench_checksum_64(b: &mut test::Bencher) {
        bench_checksum_size(b, 64);
    }

    #[bench]
    fn bench_checksum_576(b: &mut test::Bencher) {
        bench_checksum_size(b, 576);
    }

    #[bench]
    fn bench_checksum_1500(b: &mut test::Bencher) {
        bench_checksum_size(b, 1500);
    }

    #[bench]
    fn bench_checksum_9000(b: &mut test::Bencher) {
        bench_checksum_size(b, 9000);
    }

    #[bench]
    fn bench_checksum_65535(b: &mut test::Bencher) {
        bench_checksum_size(b, 65535);
    }

    // Odd lengths exercise the trailing-byte path.
    #[bench]
    fn bench_checksum_1501(b: &mut test::Bencher) {
        bench_checksum_size(b, 1501);
    }

    // --- TCP RX hot path: parse + verify checksum on a 1460-byte segment ---
    #[bench]
    #[cfg(feature = "proto-ipv4")]
    fn bench_parse_verify_tcp(b: &mut test::Bencher) {
        const PAYLOAD_LEN: usize = 1460;
        let src = IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 1));
        let dst = IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 2));

        let repr = TcpRepr {
            src_port: 48896,
            dst_port: 80,
            control: TcpControl::None,
            seq_number: TcpSeqNumber(0x01234567),
            ack_number: Some(TcpSeqNumber(0x12abcdef)),
            window_len: 0x4000,
            window_scale: None,
            max_seg_size: None,
            sack_permitted: false,
            sack_ranges: [None, None, None],
            payload: &[0x2a; PAYLOAD_LEN],
            timestamp: None,
        };
        let mut bytes = vec![0u8; repr.buffer_len()];
        {
            let mut packet = TcpPacket::new_unchecked(&mut bytes);
            repr.emit(&mut packet, &src, &dst, &ChecksumCapabilities::default());
        }

        b.bytes = bytes.len() as u64;
        b.iter(|| {
            let packet = TcpPacket::new_unchecked(test::black_box(&bytes[..]));
            let repr = TcpRepr::parse(
                &packet,
                test::black_box(&src),
                test::black_box(&dst),
                &ChecksumCapabilities::default(),
            )
            .unwrap();
            test::black_box(repr);
        });
    }
}
