//! Apple Accessory Protocol over a raw L2CAP socket (PSM 0x1001).
//!
//! The handshake and the head-tracking opcode come from the reverse
//! engineering done by LibrePods. The head-tracking request is a small
//! protobuf message: a stream id and a sample interval in microseconds.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

const AF_BLUETOOTH: libc::c_int = 31;
const BTPROTO_L2CAP: libc::c_int = 0;
const SOL_BLUETOOTH: libc::c_int = 274;
const BT_SECURITY: libc::c_int = 4;
// Without at least "medium", the AirPods accept the channel and then stay silent.
const BT_SECURITY_MEDIUM: u8 = 2;
pub const PSM: u16 = 0x1001;

const HEADER: [u8; 4] = [0x04, 0x00, 0x04, 0x00];
const HANDSHAKE: [u8; 16] = [0, 0, 4, 0, 1, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0];
const SET_FEATURES: [u8; 10] = [0x4d, 0x00, 0xd7, 0, 0, 0, 0, 0, 0, 0];
const REQUEST_NOTIFICATIONS: [u8; 6] = [0x0f, 0x00, 0xff, 0xff, 0xff, 0xff];
const CONNECT_RESPONSE: [u8; 4] = [0x01, 0x00, 0x04, 0x00];
const DISCONNECT: [u8; 6] = [0x02, 0x00, 0x04, 0x00, 0x00, 0x00];
const DISCONNECT_RESPONSE: [u8; 4] = [0x03, 0x00, 0x04, 0x00];
const HANDSHAKE_TRIES: usize = 4;
const OPCODE_HEAD_TRACKING: u8 = 0x17;

/// Stream 16 carries the motion samples. The AirPods only serve it once
/// stream 14 has been asked for; that one returns small status messages, and
/// stopping it afterwards leaves stream 16 running.
pub const MOTION_STREAM: u8 = 0x10;
pub const ACTIVATION_STREAM: u8 = 0x0e;

#[repr(C)]
struct SockaddrL2 {
    l2_family: libc::sa_family_t,
    l2_psm: u16,
    l2_bdaddr: [u8; 6],
    l2_cid: u16,
    l2_bdaddr_type: u8,
}

#[repr(C)]
struct BtSecurity {
    level: u8,
    key_size: u8,
}

/// Parses "A4:C3:F0:12:34:56" into the byte order the kernel expects.
pub fn parse_address(s: &str) -> Result<[u8; 6]> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 6 {
        bail!("not a Bluetooth address: {s}");
    }
    let mut out = [0u8; 6];
    for (i, p) in parts.iter().enumerate() {
        out[5 - i] = u8::from_str_radix(p, 16).with_context(|| format!("not a Bluetooth address: {s}"))?;
    }
    Ok(out)
}

fn varint(mut v: u32, out: &mut Vec<u8>) {
    while v >= 0x80 {
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Builds a head-tracking request. An interval of 0 stops the stream.
pub fn head_tracking_request(seq: u32, stream: u8, interval_us: u32) -> Vec<u8> {
    let mut inner = vec![0x08, stream, 0x10, 0x02, 0x1a, 0x05, 0x01];
    inner.extend_from_slice(&interval_us.to_le_bytes());
    let mut body = vec![0x08];
    varint(seq, &mut body);
    body.push(0x42);
    body.push(inner.len() as u8);
    body.extend_from_slice(&inner);

    let mut packet = HEADER.to_vec();
    packet.extend_from_slice(&[OPCODE_HEAD_TRACKING, 0x00, 0x00, 0x00, 0x10, 0x00]);
    packet.extend_from_slice(&(body.len() as u16).to_le_bytes());
    packet.extend_from_slice(&body);
    packet
}

pub struct Link {
    fd: OwnedFd,
    seq: u32,
}

impl Link {
    pub fn connect(address: &str, timeout: Duration) -> Result<Link> {
        let bdaddr = parse_address(address)?;
        // SAFETY: plain socket(2) call; the result is checked before use.
        let raw = unsafe { libc::socket(AF_BLUETOOTH, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, BTPROTO_L2CAP) };
        if raw < 0 {
            return Err(io::Error::last_os_error()).context("cannot open a Bluetooth L2CAP socket");
        }
        // SAFETY: `raw` is a fresh descriptor that nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };

        let sec = BtSecurity {
            level: BT_SECURITY_MEDIUM,
            key_size: 0,
        };
        setsockopt(&fd, SOL_BLUETOOTH, BT_SECURITY, &sec).context("cannot set L2CAP security")?;
        // The kernel waits for the L2CAP connection for the send timeout.
        let tv = libc::timeval {
            tv_sec: timeout.as_secs() as libc::time_t,
            tv_usec: 0,
        };
        setsockopt(&fd, libc::SOL_SOCKET, libc::SO_SNDTIMEO, &tv)?;

        let addr = SockaddrL2 {
            l2_family: AF_BLUETOOTH as libc::sa_family_t,
            l2_psm: PSM.to_le(),
            l2_bdaddr: bdaddr,
            l2_cid: 0,
            l2_bdaddr_type: 0, // BR/EDR
        };
        // SAFETY: `addr` is a valid sockaddr_l2 and the length matches it.
        let rc = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                (&addr as *const SockaddrL2).cast(),
                std::mem::size_of::<SockaddrL2>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("cannot reach the AirPods at {address}; are they connected?"));
        }
        Ok(Link { fd, seq: 0x40 })
    }

    pub fn send(&self, data: &[u8]) -> Result<()> {
        // SAFETY: the buffer is valid for `data.len()` bytes.
        let n = unsafe { libc::send(self.fd.as_raw_fd(), data.as_ptr().cast(), data.len(), 0) };
        if n < 0 {
            return Err(io::Error::last_os_error()).context("Bluetooth write failed");
        }
        Ok(())
    }

    /// Waits up to `timeout` for one packet. `Ok(None)` on timeout.
    pub fn recv(&self, buf: &mut [u8], timeout: Duration) -> Result<Option<usize>> {
        let mut pfd = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        let rc = unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as libc::c_int) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                return Ok(None);
            }
            return Err(err.into());
        }
        if rc == 0 {
            return Ok(None);
        }
        if pfd.revents & (libc::POLLHUP | libc::POLLERR) != 0 && pfd.revents & libc::POLLIN == 0 {
            bail!("the AirPods closed the connection");
        }
        // SAFETY: the buffer is valid for `buf.len()` bytes.
        let n = unsafe { libc::recv(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n < 0 {
            return Err(io::Error::last_os_error()).context("Bluetooth read failed");
        }
        if n == 0 {
            bail!("the AirPods closed the connection");
        }
        Ok(Some(n as usize))
    }

    /// Handshake, feature flags and notification request, as Apple devices do.
    ///
    /// When the previous client closed its channel without an AAP disconnect,
    /// the AirPods answer the first handshake on that dead channel, tear it
    /// down, and ignore everything else on the new one. Sending the handshake
    /// again until the AirPods answer it here gets past that.
    pub fn hello(&self) -> Result<()> {
        let mut buf = [0u8; 2048];
        let mut answered = false;
        for _ in 0..HANDSHAKE_TRIES {
            self.send(&HANDSHAKE)?;
            let until = Instant::now() + Duration::from_millis(700);
            while !answered && Instant::now() < until {
                if let Some(n) = self.recv(&mut buf, Duration::from_millis(100))? {
                    answered = buf[..n].starts_with(&CONNECT_RESPONSE);
                }
            }
            if answered {
                break;
            }
        }
        if !answered {
            bail!("the AirPods do not answer the AAP handshake");
        }
        self.send(&[&HEADER[..], &SET_FEATURES].concat())?;
        std::thread::sleep(Duration::from_millis(150));
        self.send(&[&HEADER[..], &REQUEST_NOTIFICATIONS].concat())?;
        std::thread::sleep(Duration::from_millis(150));
        Ok(())
    }

    pub fn start_tracking(&mut self, stream: u8, interval_us: u32) -> Result<()> {
        self.seq += 1;
        self.send(&head_tracking_request(self.seq, stream, interval_us))
    }

    pub fn stop_tracking(&mut self, stream: u8) -> Result<()> {
        self.start_tracking(stream, 0)
    }
}

impl Drop for Link {
    /// Ends the AAP session before the channel closes, so the AirPods do not
    /// keep it and answer the next client's handshake on this dead channel.
    /// The motion stream keeps running for the other connections.
    fn drop(&mut self) {
        if self.send(&DISCONNECT).is_err() {
            return;
        }
        let mut buf = [0u8; 2048];
        let until = Instant::now() + Duration::from_millis(300);
        while Instant::now() < until {
            match self.recv(&mut buf, Duration::from_millis(50)) {
                Ok(Some(n)) if buf[..n].starts_with(&DISCONNECT_RESPONSE) => return,
                Ok(_) => {}
                Err(_) => return,
            }
        }
    }
}

fn setsockopt<T>(fd: &OwnedFd, level: libc::c_int, name: libc::c_int, value: &T) -> Result<()> {
    // SAFETY: `value` points to a live T of the given size.
    let rc = unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            level,
            name,
            (value as *const T).cast(),
            std::mem::size_of::<T>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::tests::hex;

    #[test]
    fn address_is_reversed() {
        assert_eq!(
            parse_address("A4:C3:F0:12:34:56").unwrap(),
            [0x56, 0x34, 0x12, 0xf0, 0xc3, 0xa4]
        );
        assert!(parse_address("A4:C3:F0").is_err());
    }

    #[test]
    fn requests_match_known_packets() {
        // The two start packets and the "alternate" stop packet LibrePods sends.
        assert_eq!(
            head_tracking_request(0x121, 0x0e, 40_000),
            hex("04000400170000001000100008a102420b080e10021a0501409c0000")
        );
        assert_eq!(
            head_tracking_request(0x73, 0x10, 40_000),
            hex("040004001700000010000f000873420b081010021a0501409c0000")
        );
        assert_eq!(
            head_tracking_request(0x75, 0x10, 0),
            hex("040004001700000010000f000875420b081010021a050100000000")
        );
    }
}
