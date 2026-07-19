pub(super) fn signed_delta(after: u64, before: u64) -> i128 {
    i128::from(after) - i128::from(before)
}

pub(super) fn process_memory_bytes() -> u64 {
    #[cfg(target_vendor = "apple")]
    {
        apple_phys_footprint_bytes().unwrap_or(0)
    }
    #[cfg(all(not(target_vendor = "apple"), target_os = "linux"))]
    {
        linux_rss_bytes().unwrap_or(0)
    }
    #[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
    {
        0
    }
}

pub(super) const fn process_memory_label() -> &'static str {
    #[cfg(target_vendor = "apple")]
    {
        "apple_phys_footprint"
    }
    #[cfg(all(not(target_vendor = "apple"), target_os = "linux"))]
    {
        "linux_rss"
    }
    #[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
    {
        "process_memory"
    }
}

#[cfg(target_vendor = "apple")]
#[repr(C)]
#[derive(Default)]
struct TaskVmInfo {
    virtual_size: u64,
    region_count: libc::integer_t,
    page_size: libc::integer_t,
    resident_size: u64,
    resident_size_peak: u64,
    device: u64,
    device_peak: u64,
    internal: u64,
    internal_peak: u64,
    external: u64,
    external_peak: u64,
    reusable: u64,
    reusable_peak: u64,
    purgeable_volatile_pmap: u64,
    purgeable_volatile_resident: u64,
    purgeable_volatile_virtual: u64,
    compressed: u64,
    compressed_peak: u64,
    compressed_lifetime: u64,
    phys_footprint: u64,
}

#[cfg(target_vendor = "apple")]
fn apple_phys_footprint_bytes() -> Option<u64> {
    const TASK_VM_INFO: libc::task_flavor_t = 22;
    // Mach reports the buffer length in `natural_t` words through the last field read.
    const TASK_VM_INFO_REV1_COUNT: libc::mach_msg_type_number_t =
        ((core::mem::offset_of!(TaskVmInfo, phys_footprint) + core::mem::size_of::<u64>())
            / core::mem::size_of::<libc::natural_t>()) as libc::mach_msg_type_number_t;

    let mut info = TaskVmInfo::default();
    let mut count = TASK_VM_INFO_REV1_COUNT;
    // SAFETY: reading the current task port has no preconditions.
    #[allow(deprecated)]
    let task = unsafe { libc::mach_task_self() };
    // SAFETY: `info` is a writable repr(C) buffer and `count` matches its populated prefix.
    let result = unsafe {
        libc::task_info(
            task,
            TASK_VM_INFO,
            (&mut info as *mut TaskVmInfo).cast::<libc::integer_t>(),
            &mut count,
        )
    };
    if result != libc::KERN_SUCCESS || count < TASK_VM_INFO_REV1_COUNT {
        return None;
    }
    Some(info.phys_footprint)
}

#[cfg(target_os = "linux")]
fn linux_rss_bytes() -> Option<u64> {
    let contents = std::fs::read_to_string("/proc/self/statm").ok()?;
    let mut fields = contents.split_whitespace();
    let _size = fields.next()?;
    let resident_pages: u64 = fields.next()?.parse().ok()?;
    // SAFETY: `sysconf` is thread-safe and `_SC_PAGESIZE` requires no caller-owned memory.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        return None;
    }
    Some(resident_pages.saturating_mul(page_size as u64))
}

#[cfg(test)]
mod tests {
    use super::{process_memory_bytes, process_memory_label, signed_delta};

    #[test]
    fn signed_delta_preserves_direction() {
        assert_eq!(signed_delta(8, 4), 4);
        assert_eq!(signed_delta(4, 4), 0);
        assert_eq!(signed_delta(4, 8), -4);
    }

    #[cfg(any(target_vendor = "apple", target_os = "linux"))]
    #[test]
    fn process_memory_sample_is_nonzero() {
        assert_ne!(process_memory_bytes(), 0);
    }

    #[test]
    fn process_memory_label_names_the_platform_metric() {
        #[cfg(target_vendor = "apple")]
        let expected = "apple_phys_footprint";
        #[cfg(all(not(target_vendor = "apple"), target_os = "linux"))]
        let expected = "linux_rss";
        #[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
        let expected = "process_memory";

        assert_eq!(process_memory_label(), expected);
    }
}
