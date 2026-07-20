use std::io;
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
use std::mem;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

use libc;

use super::bpf_records::{HeaderLayout, parse_record};
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
use super::{ifreq, ifreq_for};
use crate::phy::Medium;

/// set interface
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "freebsd"
))]
const BIOCSETIF: libc::c_ulong = 0x8020426c;
/// get buffer length
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "freebsd"
))]
const BIOCGBLEN: libc::c_ulong = 0x40044266;
/// set buffer length
#[cfg(any(target_os = "macos", target_os = "ios"))]
const BIOCSBLEN: libc::c_ulong = 0xc0044266;
/// set immediate/nonblocking read
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "freebsd"
))]
const BIOCIMMEDIATE: libc::c_ulong = 0x80044270;
/// get interface MTU
#[cfg(any(target_os = "macos", target_os = "ios"))]
const SIOCGIFMTU: libc::c_ulong = 0xc0206933;
// These BPF timestamp fields remain 32-bit on the supported targets.
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "openbsd"))]
const BPF_HEADER_LAYOUT: HeaderLayout = HeaderLayout::new(8, 12, 16, 4);

// NetBSD's BPF timestamp ABI is independent of libc::timeval.
#[cfg(all(target_os = "netbsd", target_pointer_width = "32"))]
const BPF_HEADER_LAYOUT: HeaderLayout = HeaderLayout::new(8, 12, 16, 4);

#[cfg(all(target_os = "netbsd", target_pointer_width = "64"))]
const BPF_HEADER_LAYOUT: HeaderLayout = HeaderLayout::new(16, 20, 24, 8);

// FreeBSD bpf_hdr starts with libc::timeval and BPF_WORDALIGN uses sizeof(long).
#[cfg(target_os = "freebsd")]
const BPF_HEADER_LAYOUT: HeaderLayout = {
    let timestamp_len = mem::size_of::<libc::timeval>();
    HeaderLayout::new(
        timestamp_len,
        timestamp_len + 4,
        timestamp_len + 8,
        mem::size_of::<libc::c_long>(),
    )
};

#[cfg(any(target_os = "macos", target_os = "ios"))]
type BpfIfreq = libc::ifreq;
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
type BpfIfreq = ifreq;

macro_rules! try_ioctl {
    ($fd:expr,$cmd:expr,$req:expr) => {
        unsafe {
            if libc::ioctl($fd, $cmd, $req) == -1 {
                return Err(io::Error::last_os_error());
            }
        }
    };
}

#[derive(Debug)]
pub struct BpfDevice {
    fd: OwnedFd,
    ifreq: BpfIfreq,
    read_len: usize,
    read_offset: usize,
}

impl AsFd for BpfDevice {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl AsRawFd for BpfDevice {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

fn open_device() -> io::Result<OwnedFd> {
    unsafe {
        for i in 0..256 {
            let dev = format!("/dev/bpf{}\0", i);
            match libc::open(
                dev.as_ptr() as *const libc::c_char,
                libc::O_RDWR | libc::O_NONBLOCK,
            ) {
                -1 => continue,
                fd => return Ok(OwnedFd::from_raw_fd(fd)),
            };
        }
    }
    // at this point, all 256 BPF devices were busy and we weren't able to open any
    Err(io::Error::last_os_error())
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn bpf_ifreq_for(name: &str) -> io::Result<BpfIfreq> {
    if name.len() >= libc::IF_NAMESIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "interface name is too long",
        ));
    }

    let mut ifreq = unsafe { mem::zeroed::<libc::ifreq>() };
    for (i, byte) in name.as_bytes().iter().enumerate() {
        ifreq.ifr_name[i] = *byte as libc::c_char
    }
    Ok(ifreq)
}

#[cfg(not(any(target_os = "macos", target_os = "ios")))]
fn bpf_ifreq_for(name: &str) -> io::Result<BpfIfreq> {
    ifreq_for(name)
}

impl BpfDevice {
    pub fn new(name: &str, _medium: Medium) -> io::Result<BpfDevice> {
        let ifreq = bpf_ifreq_for(name)?;
        Ok(BpfDevice {
            fd: open_device()?,
            ifreq,
            read_len: 0,
            read_offset: 0,
        })
    }

    pub fn bind_interface(&mut self) -> io::Result<()> {
        try_ioctl!(self.as_raw_fd(), BIOCSETIF, &mut self.ifreq);

        Ok(())
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    pub fn interface_mtu(&mut self) -> io::Result<usize> {
        let mut ifreq = unsafe { mem::zeroed::<libc::ifreq>() };
        ifreq.ifr_name = self.ifreq.ifr_name;
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
        if fd == -1 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };

        try_ioctl!(fd.as_raw_fd(), SIOCGIFMTU, &mut ifreq);

        Ok(unsafe { ifreq.ifr_ifru.ifru_mtu } as usize)
    }

    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    pub fn interface_mtu(&mut self) -> io::Result<usize> {
        self.bpf_buffer_len()
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    pub fn read_buffer_len(&mut self, frame_mtu: usize) -> io::Result<usize> {
        let needed = frame_mtu
            .checked_add(mem::size_of::<libc::bpf_hdr>())
            .and_then(|len| libc::c_uint::try_from(len).ok())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "interface MTU is too large")
            })?;
        let mut bufsize = needed;
        try_ioctl!(
            self.as_raw_fd(),
            BIOCSBLEN,
            &mut bufsize as *mut libc::c_uint
        );

        let actual = self.bpf_buffer_len()?;
        if actual < needed as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "BPF buffer is too small for interface MTU",
            ));
        }

        Ok(actual)
    }

    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    pub fn read_buffer_len(&mut self, _frame_mtu: usize) -> io::Result<usize> {
        self.bpf_buffer_len()
    }

    fn bpf_buffer_len(&mut self) -> io::Result<usize> {
        let mut immediate: libc::c_uint = 1;
        try_ioctl!(
            self.as_raw_fd(),
            BIOCIMMEDIATE,
            &mut immediate as *mut libc::c_uint
        );
        let mut bufsize: libc::c_uint = 0;
        try_ioctl!(
            self.as_raw_fd(),
            BIOCGBLEN,
            &mut bufsize as *mut libc::c_uint
        );

        Ok(bufsize as usize)
    }

    pub fn recv<'a>(&mut self, buffer: &'a mut [u8]) -> io::Result<&'a [u8]> {
        if self.read_offset == self.read_len {
            let len = unsafe {
                let len = libc::read(
                    self.as_raw_fd(),
                    buffer.as_mut_ptr() as *mut libc::c_void,
                    buffer.len(),
                );

                if len == -1 {
                    return Err(io::Error::last_os_error());
                }

                len as usize
            };
            self.read_len = len;
            self.read_offset = 0;
        }

        match parse_record(
            &buffer[..self.read_len],
            self.read_offset,
            BPF_HEADER_LAYOUT,
        ) {
            Ok(record) => {
                self.read_offset = record.next_offset;
                Ok(record.packet)
            }
            Err(_) => {
                self.read_offset = self.read_len;
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid BPF record",
                ))
            }
        }
    }

    pub fn send(&mut self, buffer: &[u8]) -> io::Result<usize> {
        unsafe {
            let len = libc::write(
                self.as_raw_fd(),
                buffer.as_ptr() as *const libc::c_void,
                buffer.len(),
            );

            if len == -1 {
                return Err(io::Error::last_os_error());
            }

            Ok(len as usize)
        }
    }
}
