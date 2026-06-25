use super::*;
use crate::phy::Medium;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

#[derive(Debug)]
pub struct TunTapInterfaceDesc {
    lower: OwnedFd,
    mtu: usize,
}

impl AsFd for TunTapInterfaceDesc {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.lower.as_fd()
    }
}

impl AsRawFd for TunTapInterfaceDesc {
    fn as_raw_fd(&self) -> RawFd {
        self.lower.as_raw_fd()
    }
}

impl TunTapInterfaceDesc {
    pub fn new(name: &str, medium: Medium) -> io::Result<TunTapInterfaceDesc> {
        let mut ifreq = ifreq_for(name)?;
        let lower = unsafe {
            let fd = libc::open(c"/dev/net/tun".as_ptr(), libc::O_RDWR | libc::O_NONBLOCK);
            if fd == -1 {
                return Err(io::Error::last_os_error());
            }
            OwnedFd::from_raw_fd(fd)
        };

        Self::attach_interface_ifreq(lower.as_raw_fd(), medium, &mut ifreq)?;
        let mtu = Self::mtu_ifreq(medium, &mut ifreq)?;

        Ok(TunTapInterfaceDesc { lower, mtu })
    }

    pub fn from_fd(fd: OwnedFd, mtu: usize) -> io::Result<TunTapInterfaceDesc> {
        Ok(TunTapInterfaceDesc { lower: fd, mtu })
    }

    fn attach_interface_ifreq(
        lower: libc::c_int,
        medium: Medium,
        ifr: &mut ifreq,
    ) -> io::Result<()> {
        let mode = match medium {
            #[cfg(feature = "medium-ip")]
            Medium::Ip => imp::IFF_TUN,
            #[cfg(feature = "medium-ethernet")]
            Medium::Ethernet => imp::IFF_TAP,
            #[cfg(feature = "medium-ieee802154")]
            Medium::Ieee802154 => todo!(),
        };
        ifr.ifr_data = mode | imp::IFF_NO_PI;
        ifreq_ioctl(lower, ifr, imp::TUNSETIFF).map(|_| ())
    }

    fn mtu_ifreq(medium: Medium, ifr: &mut ifreq) -> io::Result<usize> {
        let lower = unsafe {
            let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, libc::IPPROTO_IP);
            if fd == -1 {
                return Err(io::Error::last_os_error());
            }
            OwnedFd::from_raw_fd(fd)
        };

        let ip_mtu =
            ifreq_ioctl(lower.as_raw_fd(), ifr, imp::SIOCGIFMTU).map(|mtu| mtu as usize)?;

        // SIOCGIFMTU returns the IP MTU (typically 1500 bytes.)
        // smoltcp counts the entire Ethernet packet in the MTU, so add the Ethernet header size to it.
        let mtu = match medium {
            #[cfg(feature = "medium-ip")]
            Medium::Ip => ip_mtu,
            #[cfg(feature = "medium-ethernet")]
            Medium::Ethernet => ip_mtu + crate::wire::EthernetFrame::<&[u8]>::header_len(),
            #[cfg(feature = "medium-ieee802154")]
            Medium::Ieee802154 => todo!(),
        };

        Ok(mtu)
    }

    pub fn interface_mtu(&self) -> io::Result<usize> {
        Ok(self.mtu)
    }

    pub fn recv(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        unsafe {
            let len = libc::read(
                self.as_raw_fd(),
                buffer.as_mut_ptr() as *mut libc::c_void,
                buffer.len(),
            );
            if len == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(len as usize)
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
