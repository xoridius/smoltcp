#![allow(unsafe_code)]

use crate::time::Duration;
use std::os::unix::io::RawFd;
use std::{io, mem, ptr};

#[cfg(any(target_os = "linux", target_os = "android"))]
#[path = "linux.rs"]
mod imp;

#[cfg(all(
    feature = "phy-raw_socket",
    unix,
    any(
        test,
        target_os = "macos",
        target_os = "ios",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "freebsd"
    )
))]
mod bpf_records;

#[cfg(all(
    feature = "phy-raw_socket",
    not(any(target_os = "linux", target_os = "android")),
    unix
))]
pub mod bpf;
#[cfg(all(
    feature = "phy-raw_socket",
    any(target_os = "linux", target_os = "android")
))]
pub mod raw_socket;
#[cfg(all(
    feature = "phy-tuntap_interface",
    any(target_os = "linux", target_os = "android")
))]
pub mod tuntap_interface;

#[cfg(all(
    feature = "phy-raw_socket",
    not(any(target_os = "linux", target_os = "android")),
    unix
))]
pub use self::bpf::BpfDevice as RawSocketDesc;
#[cfg(all(
    feature = "phy-raw_socket",
    any(target_os = "linux", target_os = "android")
))]
pub use self::raw_socket::RawSocketDesc;
#[cfg(all(
    feature = "phy-tuntap_interface",
    any(target_os = "linux", target_os = "android")
))]
pub use self::tuntap_interface::TunTapInterfaceDesc;

/// Wait until given file descriptor becomes readable, but no longer than given timeout.
pub fn wait(fd: RawFd, duration: Option<Duration>) -> io::Result<()> {
    if fd < 0 || fd as usize >= libc::FD_SETSIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "file descriptor is outside fd_set bounds",
        ));
    }

    unsafe {
        let mut readfds = {
            let mut readfds = mem::MaybeUninit::<libc::fd_set>::uninit();
            libc::FD_ZERO(readfds.as_mut_ptr());
            libc::FD_SET(fd, readfds.as_mut_ptr());
            readfds.assume_init()
        };

        let mut writefds = {
            let mut writefds = mem::MaybeUninit::<libc::fd_set>::uninit();
            libc::FD_ZERO(writefds.as_mut_ptr());
            writefds.assume_init()
        };

        let mut exceptfds = {
            let mut exceptfds = mem::MaybeUninit::<libc::fd_set>::uninit();
            libc::FD_ZERO(exceptfds.as_mut_ptr());
            exceptfds.assume_init()
        };

        let mut timeout = libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        };
        let timeout_ptr = if let Some(duration) = duration {
            timeout.tv_sec = duration.secs() as libc::time_t;
            timeout.tv_usec = (duration.millis() * 1_000) as libc::suseconds_t;
            &mut timeout as *mut _
        } else {
            ptr::null_mut()
        };

        let res = libc::select(
            fd + 1,
            &mut readfds,
            &mut writefds,
            &mut exceptfds,
            timeout_ptr,
        );
        if res == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(all(
    any(feature = "phy-tuntap_interface", feature = "phy-raw_socket"),
    unix,
    not(any(target_os = "macos", target_os = "ios"))
))]
#[repr(C)]
#[derive(Debug)]
struct ifreq {
    ifr_name: [libc::c_char; libc::IF_NAMESIZE],
    ifr_data: libc::c_int, /* ifr_ifindex or ifr_mtu */
}

#[cfg(all(
    any(feature = "phy-tuntap_interface", feature = "phy-raw_socket"),
    unix,
    not(any(target_os = "macos", target_os = "ios"))
))]
fn ifreq_for(name: &str) -> io::Result<ifreq> {
    if name.len() >= libc::IF_NAMESIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "interface name is too long",
        ));
    }

    let mut ifreq = ifreq {
        ifr_name: [0; libc::IF_NAMESIZE],
        ifr_data: 0,
    };
    for (i, byte) in name.as_bytes().iter().enumerate() {
        ifreq.ifr_name[i] = *byte as libc::c_char
    }
    Ok(ifreq)
}

#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    any(feature = "phy-tuntap_interface", feature = "phy-raw_socket")
))]
fn ifreq_ioctl(
    lower: libc::c_int,
    ifreq: &mut ifreq,
    cmd: libc::c_ulong,
) -> io::Result<libc::c_int> {
    unsafe {
        let res = libc::ioctl(lower, cmd as _, ifreq as *mut ifreq);
        if res == -1 {
            return Err(io::Error::last_os_error());
        }
    }

    Ok(ifreq.ifr_data)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn wait_rejects_fd_outside_fd_set_bounds() {
        assert_eq!(
            wait(-1, None).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            wait(libc::FD_SETSIZE as RawFd, None).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    #[cfg(all(
        unix,
        any(feature = "phy-tuntap_interface", feature = "phy-raw_socket"),
        not(any(target_os = "macos", target_os = "ios"))
    ))]
    fn ifreq_for_rejects_names_without_nul_room() {
        let max_valid = "x".repeat(libc::IF_NAMESIZE - 1);
        assert!(ifreq_for(&max_valid).is_ok());

        let too_long = "x".repeat(libc::IF_NAMESIZE);
        assert_eq!(
            ifreq_for(&too_long).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
