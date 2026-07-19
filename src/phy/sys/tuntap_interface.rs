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

    pub fn from_fd(fd: RawFd, mtu: usize) -> io::Result<TunTapInterfaceDesc> {
        let fd = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if fd == -1 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        Self::from_owned_fd(fd, mtu)
    }

    pub fn from_owned_fd(fd: OwnedFd, mtu: usize) -> io::Result<TunTapInterfaceDesc> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phy::TunTapInterface;
    use std::io::Read;
    use std::os::unix::net::UnixStream;

    fn fd_flags(fd: RawFd) -> io::Result<libc::c_int> {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(flags)
        }
    }

    #[test]
    fn descriptor_constructor_signatures_are_stable() {
        let _: fn(RawFd, Medium, usize) -> io::Result<TunTapInterface> = TunTapInterface::from_fd;
        let _: fn(OwnedFd, Medium, usize) -> io::Result<TunTapInterface> =
            TunTapInterface::from_owned_fd;
        let _: fn(RawFd, usize) -> io::Result<TunTapInterfaceDesc> = TunTapInterfaceDesc::from_fd;
        let _: fn(OwnedFd, usize) -> io::Result<TunTapInterfaceDesc> =
            TunTapInterfaceDesc::from_owned_fd;
    }

    #[test]
    fn raw_fd_constructor_duplicates_with_cloexec_and_survives_original_close() {
        let (original, mut peer) = UnixStream::pair().unwrap();
        let interface = TunTapInterface::from_fd(original.as_raw_fd(), Medium::Ip, 1500).unwrap();

        assert_ne!(
            fd_flags(interface.as_raw_fd()).unwrap() & libc::FD_CLOEXEC,
            0
        );
        drop(original);

        let byte = [0x5a_u8];
        let written = unsafe {
            libc::write(
                interface.as_raw_fd(),
                byte.as_ptr().cast::<libc::c_void>(),
                byte.len(),
            )
        };
        assert_eq!(written, byte.len() as libc::ssize_t);
        let mut received = [0];
        peer.read_exact(&mut received).unwrap();
        assert_eq!(received, byte);
    }

    #[test]
    fn dropping_raw_fd_interface_does_not_close_original() {
        let (original, mut peer) = UnixStream::pair().unwrap();
        let original_fd = original.as_raw_fd();
        let interface = TunTapInterface::from_fd(original_fd, Medium::Ip, 1500).unwrap();

        drop(interface);

        assert!(fd_flags(original_fd).is_ok());
        let byte = [0xa5_u8];
        let written = unsafe {
            libc::write(
                original.as_raw_fd(),
                byte.as_ptr().cast::<libc::c_void>(),
                byte.len(),
            )
        };
        assert_eq!(written, byte.len() as libc::ssize_t);
        let mut received = [0];
        peer.read_exact(&mut received).unwrap();
        assert_eq!(received, byte);
    }

    #[test]
    fn owned_fd_constructor_consumes_and_closes_descriptor() {
        let (original, mut peer) = UnixStream::pair().unwrap();
        let original_fd = original.as_raw_fd();
        let owned_fd: OwnedFd = original.into();
        let interface = TunTapInterface::from_owned_fd(owned_fd, Medium::Ip, 1500).unwrap();

        assert_eq!(interface.as_raw_fd(), original_fd);
        let byte = [0x3c_u8];
        let written = unsafe {
            libc::write(
                interface.as_raw_fd(),
                byte.as_ptr().cast::<libc::c_void>(),
                byte.len(),
            )
        };
        assert_eq!(written, byte.len() as libc::ssize_t);
        let mut received = [0];
        peer.read_exact(&mut received).unwrap();
        assert_eq!(received, byte);

        drop(interface);

        let error = fd_flags(original_fd).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EBADF));
    }

    #[test]
    fn raw_fd_constructor_rejects_invalid_descriptor() {
        let error = TunTapInterface::from_fd(-1, Medium::Ip, 1500).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EBADF));
    }
}
