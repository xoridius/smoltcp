#![no_main]
use libfuzzer_sys::fuzz_target;
use smoltcp::wire::*;

fuzz_target!(|data: &[u8]| {
    let _ = format!(
        "{}",
        PrettyPrinter::<EthernetFrame<&'static [u8]>>::new("", &data)
    );
});
