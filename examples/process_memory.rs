pub(super) fn signed_delta(after: u64, before: u64) -> i128 {
    i128::from(after) - i128::from(before)
}

#[derive(Clone, Copy)]
pub(super) struct ProcessMemorySample {
    pub(super) current_bytes: u64,
    pub(super) lifetime_peak_bytes: Option<u64>,
}

pub(super) fn process_memory_sample() -> ProcessMemorySample {
    #[cfg(target_vendor = "apple")]
    {
        apple_process_memory_sample().expect("process-memory sampling is required")
    }
    #[cfg(all(not(target_vendor = "apple"), target_os = "linux"))]
    {
        linux_process_memory_sample().expect("process-memory sampling is required")
    }
    #[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
    {
        panic!("process-memory sampling is required")
    }
}

pub(super) fn process_memory_bytes() -> u64 {
    process_memory_sample().current_bytes
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
fn apple_process_memory_sample() -> Option<ProcessMemorySample> {
    let mut info = core::mem::MaybeUninit::<libc::rusage_info_v4>::uninit();
    // SAFETY: `getpid` has no preconditions, and `info` is writable storage for
    // the structure selected by `RUSAGE_INFO_V4`.
    let result = unsafe {
        libc::proc_pid_rusage(
            libc::getpid(),
            libc::RUSAGE_INFO_V4,
            info.as_mut_ptr().cast(),
        )
    };
    if result != 0 {
        return None;
    }
    // SAFETY: a successful `proc_pid_rusage` call initialized the result.
    let info = unsafe { info.assume_init() };
    Some(ProcessMemorySample {
        current_bytes: info.ri_phys_footprint,
        lifetime_peak_bytes: Some(info.ri_lifetime_max_phys_footprint),
    })
}

#[cfg(target_os = "linux")]
fn linux_rss_bytes() -> Option<u64> {
    use std::io::Read;

    let mut file = std::fs::File::open("/proc/self/statm").ok()?;
    let mut buffer = [0u8; 128];
    let len = file.read(&mut buffer).ok()?;
    let contents = core::str::from_utf8(&buffer[..len]).ok()?;
    let mut fields = contents.split_whitespace();
    let _size = fields.next()?;
    let resident_pages: u64 = fields.next()?.parse().ok()?;
    // SAFETY: `sysconf` is thread-safe and `_SC_PAGESIZE` requires no caller-owned memory.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        return None;
    }
    resident_pages.checked_mul(page_size as u64)
}

#[cfg(target_os = "linux")]
fn linux_lifetime_peak_bytes(current_bytes: u64, peak_kib: libc::c_long) -> Option<u64> {
    u64::try_from(peak_kib)
        .ok()?
        .checked_mul(1024)
        .map(|peak| peak.max(current_bytes))
}

#[cfg(target_os = "linux")]
fn linux_process_memory_sample() -> Option<ProcessMemorySample> {
    let current_bytes = linux_rss_bytes()?;
    let mut usage = core::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: `usage` points to writable storage for the complete result.
    let lifetime_peak_bytes =
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } == 0 {
            // SAFETY: a successful `getrusage` call initialized the result.
            let usage = unsafe { usage.assume_init() };
            linux_lifetime_peak_bytes(current_bytes, usage.ru_maxrss)
        } else {
            None
        };
    Some(ProcessMemorySample {
        current_bytes,
        lifetime_peak_bytes,
    })
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

    #[test]
    fn signed_delta_preserves_the_full_u64_domain() {
        assert_eq!(signed_delta(u64::MAX, 0), i128::from(u64::MAX));
        assert_eq!(signed_delta(0, u64::MAX), -i128::from(u64::MAX));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_peak_rss_converts_checked_signed_kibibytes() {
        assert_eq!(super::linux_lifetime_peak_bytes(2048, 1), Some(2048));
        assert_eq!(super::linux_lifetime_peak_bytes(1024, 2), Some(2048));
        assert_eq!(super::linux_lifetime_peak_bytes(2048, -1), None);

        let overflow = u64::MAX / 1024 + 1;
        if let Ok(overflow) = libc::c_long::try_from(overflow) {
            assert_eq!(super::linux_lifetime_peak_bytes(0, overflow), None);
        }
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
