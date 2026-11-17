//! Minimal generic-netlink client: just enough to resolve a family and run a dump, which is
//! all the nfsd family's `rpc-status-get` (in-flight RPCs, Linux 6.7+) needs.
//!
//! Message framing is plain byte handling and builds everywhere, so it is unit-tested on any
//! host; only the socket itself is Linux-only.

pub const NLMSG_HDRLEN: usize = 16;
pub const GENL_HDRLEN: usize = 4;
const NLA_HDRLEN: usize = 4;

pub const NLMSG_ERROR: u16 = 2;
pub const NLMSG_DONE: u16 = 3;
pub const NLM_F_REQUEST: u16 = 0x1;
pub const NLM_F_ACK: u16 = 0x4;
pub const NLM_F_DUMP: u16 = 0x300;

pub const GENL_ID_CTRL: u16 = 0x10;
pub const CTRL_CMD_GETFAMILY: u8 = 3;
pub const CTRL_ATTR_FAMILY_ID: u16 = 1;
pub const CTRL_ATTR_FAMILY_NAME: u16 = 2;

/// Attribute type bits that are flags, not part of the type number.
const NLA_TYPE_MASK: u16 = 0x3fff;

pub fn align4(n: usize) -> usize {
    (n + 3) & !3
}

/// One netlink message in a received datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Msg<'a> {
    pub ty: u16,
    pub flags: u16,
    pub seq: u32,
    /// Everything after the 16-byte netlink header.
    pub payload: &'a [u8],
}

fn u16_at(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_ne_bytes(b.get(i..i + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(b.get(i..i + 4)?.try_into().ok()?))
}

/// The netlink messages packed in `buf`. Stops at the first malformed header.
pub fn messages(buf: &[u8]) -> impl Iterator<Item = Msg<'_>> {
    let mut rest = buf;
    std::iter::from_fn(move || {
        let len = u32_at(rest, 0)? as usize;
        if len < NLMSG_HDRLEN || len > rest.len() {
            return None;
        }
        let m = Msg {
            ty: u16_at(rest, 4)?,
            flags: u16_at(rest, 6)?,
            seq: u32_at(rest, 8)?,
            payload: &rest[NLMSG_HDRLEN..len],
        };
        rest = rest.get(align4(len)..).unwrap_or(&[]);
        Some(m)
    })
}

/// `(type, payload)` of each attribute in `buf`, flag bits stripped from the type. Stops at the
/// first malformed attribute.
pub fn attrs(buf: &[u8]) -> impl Iterator<Item = (u16, &[u8])> {
    let mut rest = buf;
    std::iter::from_fn(move || {
        let len = u16_at(rest, 0)? as usize;
        if len < NLA_HDRLEN || len > rest.len() {
            return None;
        }
        let ty = u16_at(rest, 2)? & NLA_TYPE_MASK;
        let a = (ty, &rest[NLA_HDRLEN..len]);
        rest = rest.get(align4(len)..).unwrap_or(&[]);
        Some(a)
    })
}

/// The errno carried by an `NLMSG_ERROR` or `NLMSG_DONE` payload (0 = success / plain ACK).
pub fn msg_errno(payload: &[u8]) -> i32 {
    u32_at(payload, 0).map_or(0, |e| -(e as i32))
}

/// A generic-netlink request: netlink header, genetlink header, then `attrs`.
pub fn request(family: u16, flags: u16, seq: u32, cmd: u8, version: u8, attrs: &[(u16, &[u8])]) -> Vec<u8> {
    let mut b = vec![0u8; NLMSG_HDRLEN];
    b.extend_from_slice(&[cmd, version, 0, 0]);
    for (ty, data) in attrs {
        b.extend_from_slice(&((NLA_HDRLEN + data.len()) as u16).to_ne_bytes());
        b.extend_from_slice(&ty.to_ne_bytes());
        b.extend_from_slice(data);
        b.resize(align4(b.len()), 0);
    }
    let len = b.len() as u32;
    b[0..4].copy_from_slice(&len.to_ne_bytes());
    b[4..6].copy_from_slice(&family.to_ne_bytes());
    b[6..8].copy_from_slice(&flags.to_ne_bytes());
    b[8..12].copy_from_slice(&seq.to_ne_bytes());
    b
}

#[cfg(target_os = "linux")]
pub use linux::{GenlSocket, monotonic_us};

#[cfg(target_os = "linux")]
mod linux {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::time::Duration;

    use super::*;

    /// How long to wait for the kernel before giving up on a reply.
    const RECV_TIMEOUT: Duration = Duration::from_secs(2);

    /// The kernel's `CLOCK_MONOTONIC` in microseconds: the clock nfsd stamps requests with
    /// (`ktime_get()`).
    // `time_t` / `c_long` are i64 here but narrower on 32-bit targets.
    #[allow(clippy::useless_conversion)]
    pub fn monotonic_us() -> i64 {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: valid clock id and a valid out pointer.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        i64::from(ts.tv_sec) * 1_000_000 + i64::from(ts.tv_nsec) / 1_000
    }

    pub struct GenlSocket {
        fd: OwnedFd,
        seq: u32,
        buf: Vec<u8>,
    }

    impl GenlSocket {
        pub fn open() -> io::Result<Self> {
            // SAFETY: plain socket(2); the result is checked before it is wrapped.
            let fd =
                unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, libc::NETLINK_GENERIC) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: fd is a fresh descriptor we own.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            // SAFETY: zeroed sockaddr_nl is valid; the kernel assigns our port id.
            let mut sa: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
            sa.nl_family = libc::AF_NETLINK as libc::sa_family_t;
            let sa_len = std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t;
            // SAFETY: sa is a valid sockaddr_nl of the given length.
            if unsafe { libc::bind(fd.as_raw_fd(), &sa as *const _ as *const libc::sockaddr, sa_len) } < 0 {
                return Err(io::Error::last_os_error());
            }
            let tv = libc::timeval { tv_sec: RECV_TIMEOUT.as_secs() as _, tv_usec: RECV_TIMEOUT.subsec_micros() as _ };
            // SAFETY: tv is a valid timeval of the given length.
            let r = unsafe {
                libc::setsockopt(
                    fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_RCVTIMEO,
                    &tv as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::timeval>() as libc::socklen_t,
                )
            };
            if r < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { fd, seq: 0, buf: vec![0; 256 * 1024] })
        }

        /// Send `req` (with this call's sequence number patched in) and hand every reply
        /// message's genetlink attributes to `each`, until the kernel ends the exchange
        /// (`NLMSG_DONE` for a dump, an ACK otherwise). Kernel errors come back as `Err`.
        fn transact(&mut self, mut req: Vec<u8>, mut each: impl FnMut(&[u8])) -> io::Result<()> {
            self.seq = self.seq.wrapping_add(1);
            let seq = self.seq;
            req[8..12].copy_from_slice(&seq.to_ne_bytes());
            // SAFETY: req is a valid buffer of the given length.
            let n = unsafe { libc::send(self.fd.as_raw_fd(), req.as_ptr().cast(), req.len(), 0) };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            loop {
                // MSG_TRUNC makes recv report the real datagram size, so truncation is caught.
                // SAFETY: buf is a valid writable buffer of the given length.
                let n = unsafe {
                    libc::recv(self.fd.as_raw_fd(), self.buf.as_mut_ptr().cast(), self.buf.len(), libc::MSG_TRUNC)
                };
                if n < 0 {
                    let e = io::Error::last_os_error();
                    if e.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(e);
                }
                let n = n as usize;
                if n > self.buf.len() {
                    return Err(io::Error::other(format!("netlink reply truncated ({n} bytes)")));
                }
                for m in messages(&self.buf[..n]) {
                    // A late reply to an earlier, timed-out request.
                    if m.seq != seq {
                        continue;
                    }
                    match m.ty {
                        NLMSG_ERROR | NLMSG_DONE => {
                            return match msg_errno(m.payload) {
                                0 => Ok(()),
                                e => Err(io::Error::from_raw_os_error(e)),
                            };
                        }
                        _ => each(m.payload.get(GENL_HDRLEN..).unwrap_or(&[])),
                    }
                }
            }
        }

        /// The id of generic-netlink family `name`, or None if the kernel doesn't have it
        /// (module not loaded, or a kernel that predates it).
        pub fn family_id(&mut self, name: &str) -> io::Result<Option<u16>> {
            let mut cname = name.as_bytes().to_vec();
            cname.push(0);
            let req = request(
                GENL_ID_CTRL,
                NLM_F_REQUEST | NLM_F_ACK,
                0,
                CTRL_CMD_GETFAMILY,
                1,
                &[(CTRL_ATTR_FAMILY_NAME, &cname)],
            );
            let mut id = None;
            let r = self.transact(req, |a| {
                for (ty, v) in attrs(a) {
                    if ty == CTRL_ATTR_FAMILY_ID {
                        id = u16_at(v, 0);
                    }
                }
            });
            match r {
                Ok(()) => Ok(id),
                Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Ok(None),
                Err(e) => Err(e),
            }
        }

        /// Run dump command `cmd` of `family`, handing each reply message's attributes to `each`.
        pub fn dump(&mut self, family: u16, cmd: u8, version: u8, each: impl FnMut(&[u8])) -> io::Result<()> {
            self.transact(request(family, NLM_F_REQUEST | NLM_F_DUMP, 0, cmd, version, &[]), each)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Wrap genetlink attributes in a netlink message the way the kernel sends it.
    pub fn reply(ty: u16, seq: u32, cmd: u8, attrs: &[(u16, &[u8])]) -> Vec<u8> {
        request(ty, 0, seq, cmd, 1, attrs)
    }

    #[test]
    fn request_layout() {
        let r = request(0x1c, NLM_F_REQUEST | NLM_F_DUMP, 7, 1, 1, &[(2, b"nfsd\0")]);
        // 16 netlink + 4 genl + 4 attr header + 5 bytes padded to 8.
        assert_eq!(r.len(), 32);
        assert_eq!(u32_at(&r, 0), Some(32));
        assert_eq!(u16_at(&r, 4), Some(0x1c));
        assert_eq!(u16_at(&r, 6), Some(0x301));
        assert_eq!(u32_at(&r, 8), Some(7));
        assert_eq!(&r[16..20], &[1, 1, 0, 0]);
        assert_eq!(u16_at(&r, 20), Some(9), "attr length excludes padding");
        assert_eq!(&r[24..29], b"nfsd\0");
    }

    #[test]
    fn walks_messages_and_attrs() {
        let mut buf = reply(0x1c, 3, 1, &[(1, &[1, 2, 3, 4]), (12, &[0x03, 0x2b])]);
        buf.extend(reply(0x1c, 3, 1, &[(14 | 0x8000, &5u32.to_ne_bytes())]));
        let mut done = reply(NLMSG_DONE, 3, 0, &[]);
        done.truncate(NLMSG_HDRLEN);
        done.extend_from_slice(&0i32.to_ne_bytes());
        done[0..4].copy_from_slice(&20u32.to_ne_bytes());
        buf.extend(done);
        let msgs: Vec<Msg> = messages(&buf).collect();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[2].ty, NLMSG_DONE);
        assert_eq!(msg_errno(msgs[2].payload), 0);
        let a: Vec<(u16, &[u8])> = attrs(&msgs[0].payload[GENL_HDRLEN..]).collect();
        assert_eq!(a, vec![(1, &[1u8, 2, 3, 4][..]), (12, &[0x03, 0x2b][..])]);
        let a: Vec<(u16, &[u8])> = attrs(&msgs[1].payload[GENL_HDRLEN..]).collect();
        assert_eq!(a[0].0, 14, "flag bits stripped from the type");
    }

    /// A real round trip against the kernel's always-present `nlctrl` family: one lookup (ACK
    /// path), one dump (NLMSG_DONE path), one missing family (ENOENT path).
    #[cfg(target_os = "linux")]
    #[test]
    fn kernel_round_trip() {
        let mut s = GenlSocket::open().unwrap();
        assert_eq!(s.family_id("nlctrl").unwrap(), Some(GENL_ID_CTRL));
        let mut names = Vec::new();
        s.dump(GENL_ID_CTRL, CTRL_CMD_GETFAMILY, 1, |a| {
            for (ty, v) in attrs(a) {
                if ty == CTRL_ATTR_FAMILY_NAME {
                    names.push(String::from_utf8_lossy(v).trim_end_matches('\0').to_string());
                }
            }
        })
        .unwrap();
        assert!(names.iter().any(|n| n == "nlctrl"), "{names:?}");
        assert_eq!(s.family_id("nosuchfam").unwrap(), None);
        // The socket is still usable after an error reply.
        assert_eq!(s.family_id("nlctrl").unwrap(), Some(GENL_ID_CTRL));
    }

    #[test]
    fn malformed_input_stops_cleanly() {
        assert_eq!(messages(&[0xff; 10]).count(), 0);
        // Header claims more than is there.
        let mut b = reply(0x1c, 1, 1, &[]);
        b[0..4].copy_from_slice(&999u32.to_ne_bytes());
        assert_eq!(messages(&b).count(), 0);
        assert_eq!(attrs(&[2, 0, 1, 0]).count(), 0, "attr shorter than its header");
        let err = (-19i32).to_ne_bytes();
        assert_eq!(msg_errno(&err), 19);
    }
}
