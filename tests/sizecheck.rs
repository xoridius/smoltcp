//! Print sizes of key smoltcp types and enforce the shipping default/iOS TCP
//! footprint gates. Run with `cargo test --release --test sizecheck -- --nocapture`
//! to inspect other feature-shape changes.

use core::mem::size_of;

#[test]
fn print_sizes() {
    use smoltcp::storage::*;
    use smoltcp::wire::*;

    macro_rules! row {
        ($t:ty) => {
            println!("  {:>5}  {}", size_of::<$t>(), stringify!($t));
        };
    }

    println!("\n--- smoltcp footprint ---");
    #[cfg(feature = "socket-tcp")]
    {
        row!(smoltcp::socket::tcp::Socket<'static>);
        // Default release shape: async wakers, fixed buffers, no controller.
        #[cfg(all(
            feature = "async",
            not(feature = "socket-tcp-dynamic-buffer"),
            not(feature = "socket-tcp-cubic"),
            not(feature = "socket-tcp-reno")
        ))]
        assert_eq!(
            size_of::<smoltcp::socket::tcp::Socket<'static>>(),
            536,
            "default TCP Socket grew"
        );

        // Shipping constrained Apple shape: dynamic buffers + CUBIC, no async wakers.
        #[cfg(all(
            not(feature = "async"),
            feature = "socket-tcp-dynamic-buffer",
            feature = "socket-tcp-cubic"
        ))]
        assert_eq!(
            size_of::<smoltcp::socket::tcp::Socket<'static>>(),
            592,
            "constrained iOS TCP Socket grew"
        );
    }
    #[cfg(feature = "socket-udp")]
    row!(smoltcp::socket::udp::Socket<'static>);
    #[cfg(feature = "socket-icmp")]
    row!(smoltcp::socket::icmp::Socket<'static>);
    #[cfg(feature = "socket-raw")]
    row!(smoltcp::socket::raw::Socket<'static>);
    row!(IpAddress);
    row!(IpEndpoint);
    row!(IpRepr);
    row!(TcpRepr<'static>);
    row!(Assembler);
    row!(RingBuffer<'static, u8>);
}
