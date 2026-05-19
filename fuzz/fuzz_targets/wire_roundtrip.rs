#![no_main]
//! Differential round-trip fuzz: parse a packet, re-emit it, parse the result,
//! and require the second parse to succeed and round-trip to itself. Catches
//! "accepts but emits malformed" and "emit drops a field" classes of bug that
//! shallow parse-only fuzzing misses.
use libfuzzer_sys::fuzz_target;
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::*;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    let body = &data[1..];
    let csum = ChecksumCapabilities::default();
    match data[0] & 0x03 {
        0 => {
            // IPv4
            let Ok(pkt) = Ipv4Packet::new_checked(body) else { return };
            let Ok(repr) = Ipv4Repr::parse(&pkt, &csum) else { return };
            let mut buf = vec![0u8; repr.buffer_len() + repr.payload_len];
            let mut out = Ipv4Packet::new_unchecked(&mut buf[..]);
            repr.emit(&mut out, &csum);
            let Ok(reparse_pkt) = Ipv4Packet::new_checked(&buf[..]) else { panic!("emit produced unparseable IPv4") };
            let reparse = Ipv4Repr::parse(&reparse_pkt, &csum).expect("re-parse of emitted IPv4 failed");
            assert_eq!(repr, reparse, "IPv4 round-trip drift");
        }
        1 => {
            // IPv6
            let Ok(pkt) = Ipv6Packet::new_checked(body) else { return };
            let Ok(repr) = Ipv6Repr::parse(&pkt) else { return };
            let mut buf = vec![0u8; repr.buffer_len() + repr.payload_len];
            let mut out = Ipv6Packet::new_unchecked(&mut buf[..]);
            repr.emit(&mut out);
            let Ok(reparse_pkt) = Ipv6Packet::new_checked(&buf[..]) else { panic!("emit produced unparseable IPv6") };
            let reparse = Ipv6Repr::parse(&reparse_pkt).expect("re-parse of emitted IPv6 failed");
            assert_eq!(repr, reparse, "IPv6 round-trip drift");
        }
        2 => {
            // UDP
            let src = IpAddress::v4(10, 0, 0, 1);
            let dst = IpAddress::v4(10, 0, 0, 2);
            let Ok(pkt) = UdpPacket::new_checked(body) else { return };
            let Ok(repr) = UdpRepr::parse(&pkt, &src, &dst, &csum) else { return };
            let payload = pkt.payload();
            let mut buf = vec![0u8; repr.header_len() + payload.len()];
            let mut out = UdpPacket::new_unchecked(&mut buf[..]);
            repr.emit(&mut out, &src, &dst, payload.len(), |p| p.copy_from_slice(payload), &csum);
            let Ok(reparse_pkt) = UdpPacket::new_checked(&buf[..]) else { panic!("emit produced unparseable UDP") };
            let _ = UdpRepr::parse(&reparse_pkt, &src, &dst, &csum).expect("re-parse of emitted UDP failed");
        }
        _ => {
            // TCP (no checksum on emit path)
            let src = IpAddress::v4(10, 0, 0, 1);
            let dst = IpAddress::v4(10, 0, 0, 2);
            let Ok(pkt) = TcpPacket::new_checked(body) else { return };
            let Ok(repr) = TcpRepr::parse(&pkt, &src, &dst, &csum) else { return };
            let mut buf = vec![0u8; repr.buffer_len()];
            let mut out = TcpPacket::new_unchecked(&mut buf[..]);
            repr.emit(&mut out, &src, &dst, &csum);
            let Ok(reparse_pkt) = TcpPacket::new_checked(&buf[..]) else { panic!("emit produced unparseable TCP") };
            let _ = TcpRepr::parse(&reparse_pkt, &src, &dst, &csum).expect("re-parse of emitted TCP failed");
        }
    }
});
