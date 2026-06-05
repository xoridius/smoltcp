//! Diagnostic: print sizes of key smoltcp types. Not a behavioural test —
//! run with `cargo test --release --test sizecheck -- --nocapture` to inspect
//! footprint changes after layout edits.

use core::mem::size_of;

#[test]
fn print_sizes() {
    use smoltcp::socket;
    use smoltcp::storage::*;
    use smoltcp::wire::*;

    macro_rules! row {
        ($t:ty) => {
            println!("  {:>5}  {}", size_of::<$t>(), stringify!($t));
        };
    }

    println!("\n--- smoltcp footprint ---");
    row!(socket::tcp::Socket<'static>);
    row!(socket::udp::Socket<'static>);
    row!(socket::icmp::Socket<'static>);
    row!(socket::raw::Socket<'static>);
    row!(IpAddress);
    row!(IpEndpoint);
    row!(IpRepr);
    row!(TcpRepr<'static>);
    row!(Assembler);
    row!(RingBuffer<'static, u8>);
}
