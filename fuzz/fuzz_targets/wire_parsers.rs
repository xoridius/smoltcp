#![no_main]
//! Coverage-guided fuzzing of the parsers that recently had audit findings:
//! IPv4/IPv6, TCP, UDP, IPsec AH, 6LoWPAN NHC ExtHeader. Each branch consumes
//! the entire input as the protocol named by the first byte's low nibble so
//! libFuzzer can mutate freely without burning energy on a discriminator.
use libfuzzer_sys::fuzz_target;
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::*;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    let body = &data[1..];
    match data[0] & 0x07 {
        0 => {
            if let Ok(pkt) = Ipv4Packet::new_checked(body) {
                let _ = Ipv4Repr::parse(&pkt, &ChecksumCapabilities::default());
            }
        }
        1 => {
            if let Ok(pkt) = Ipv6Packet::new_checked(body) {
                let _ = Ipv6Repr::parse(&pkt);
            }
        }
        2 => {
            if let Ok(pkt) = TcpPacket::new_checked(body) {
                let src = IpAddress::v4(10, 0, 0, 1);
                let dst = IpAddress::v4(10, 0, 0, 2);
                let _ = TcpRepr::parse(&pkt, &src, &dst, &ChecksumCapabilities::default());
            }
        }
        3 => {
            if let Ok(pkt) = UdpPacket::new_checked(body) {
                let src = IpAddress::v4(10, 0, 0, 1);
                let dst = IpAddress::v4(10, 0, 0, 2);
                let _ = UdpRepr::parse(&pkt, &src, &dst, &ChecksumCapabilities::default());
            }
        }
        4 => {
            #[cfg(feature = "proto-ipsec-ah")]
            if let Ok(pkt) = IpSecAuthHeaderPacket::new_checked(body) {
                let _ = IpSecAuthHeaderRepr::parse(&pkt);
            }
        }
        5 => {
            #[cfg(feature = "proto-sixlowpan")]
            if let Ok(pkt) = SixlowpanExtHeaderPacket::new_checked(body) {
                let _ = SixlowpanExtHeaderRepr::parse(&pkt);
            }
        }
        6 => {
            if let Ok(pkt) = Icmpv4Packet::new_checked(body) {
                let _ = Icmpv4Repr::parse(&pkt, &ChecksumCapabilities::default());
            }
        }
        _ => {
            if let Ok(pkt) = Icmpv6Packet::new_checked(body) {
                let src = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
                let dst = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 2);
                let _ = Icmpv6Repr::parse(&src, &dst, &pkt, &ChecksumCapabilities::default());
            }
        }
    }
});
