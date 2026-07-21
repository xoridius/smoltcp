#![cfg(feature = "std")]

#[test]
fn std_instant_conversion_preserves_pre_origin_order() {
    let later = std::time::Instant::now();
    let earlier = later - std::time::Duration::from_secs(1);

    assert!(smoltcp::time::Instant::from(earlier) < smoltcp::time::Instant::from(later));
}
