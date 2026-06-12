//! Diagnostic: print sizes of key smoltcp types. Not a behavioural test —
//! run with `cargo test --release --test sizecheck -- --nocapture` to inspect
//! footprint changes after layout edits.

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
    row!(smoltcp::socket::tcp::Socket<'static>);
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
