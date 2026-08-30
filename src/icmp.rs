//! A UDP-shaped Quinn socket carried in ICMP echo messages.
//!
//! The encapsulated payload remains a QUIC packet. Consequently encryption,
//! authentication, reliability and congestion control are still provided by QUIC.

use std::{io, net::SocketAddr, sync::Arc};

const MAGIC: &[u8; 4] = b"RFLW";
const VERSION: u8 = 1;
const FRAME_HEADER_LEN: usize = 9;
const ICMP_HEADER_LEN: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Client,
    Server,
}

fn encode_frame(source_port: u16, destination_port: u16, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
    frame.extend_from_slice(MAGIC);
    frame.push(VERSION);
    frame.extend_from_slice(&source_port.to_be_bytes());
    frame.extend_from_slice(&destination_port.to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

fn decode_frame(frame: &[u8]) -> Option<(u16, u16, &[u8])> {
    if frame.len() < FRAME_HEADER_LEN || &frame[..4] != MAGIC || frame[4] != VERSION {
        return None;
    }
    Some((
        u16::from_be_bytes([frame[5], frame[6]]),
        u16::from_be_bytes([frame[7], frame[8]]),
        &frame[FRAME_HEADER_LEN..],
    ))
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use quinn::{
        AsyncUdpSocket, UdpPoller,
        udp::{RecvMeta, Transmit},
    };
    use socket2::{Domain, Protocol, SockAddr, Socket, Type};
    use std::{
        fmt,
        io::IoSliceMut,
        mem::MaybeUninit,
        net::{IpAddr, Ipv4Addr},
        pin::Pin,
        sync::atomic::{AtomicU16, Ordering},
        task::{Context, Poll, ready},
    };
    use tokio::io::unix::AsyncFd;

    const ICMP_ECHO_REPLY: u8 = 0;
    const ICMP_ECHO_REQUEST: u8 = 8;

    pub(super) struct IcmpSocket {
        io: AsyncFd<Socket>,
        local: SocketAddr,
        role: Role,
        sequence: AtomicU16,
    }

    #[derive(Debug)]
    struct IcmpPoller(Arc<IcmpSocket>);

    impl UdpPoller for IcmpPoller {
        fn poll_writable(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            match self.0.io.poll_write_ready(cx) {
                Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
                Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
                Poll::Pending => Poll::Pending,
            }
        }
    }

    impl fmt::Debug for IcmpSocket {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("IcmpSocket")
                .field("local", &self.local)
                .field("role", &self.role)
                .finish_non_exhaustive()
        }
    }

    impl IcmpSocket {
        pub(super) fn bind(bind: SocketAddr, role: Role) -> io::Result<Arc<Self>> {
            let SocketAddr::V4(bind) = bind else {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "ICMP transport currently supports IPv4 only",
                ));
            };
            let port = if bind.port() == 0 {
                reserve_ephemeral_port(bind.ip())?
            } else {
                bind.port()
            };
            let socket = Socket::new(Domain::IPV4, Type::RAW, Some(Protocol::ICMPV4))
                .map_err(permission_hint)?;
            socket.set_nonblocking(true)?;
            Ok(Arc::new(Self {
                io: AsyncFd::new(socket)?,
                local: SocketAddr::new(IpAddr::V4(*bind.ip()), port),
                role,
                sequence: AtomicU16::new(0),
            }))
        }

        fn packet(&self, destination_port: u16, payload: &[u8]) -> Vec<u8> {
            let frame = encode_frame(self.local.port(), destination_port, payload);
            let mut packet = Vec::with_capacity(ICMP_HEADER_LEN + frame.len());
            packet.push(match self.role {
                Role::Client => ICMP_ECHO_REQUEST,
                Role::Server => ICMP_ECHO_REPLY,
            });
            packet.push(0);
            packet.extend_from_slice(&[0, 0]);
            packet.extend_from_slice(&self.local.port().to_be_bytes());
            packet.extend_from_slice(&self.sequence.fetch_add(1, Ordering::Relaxed).to_be_bytes());
            packet.extend_from_slice(&frame);
            let checksum = checksum(&packet);
            packet[2..4].copy_from_slice(&checksum.to_be_bytes());
            packet
        }
    }

    fn reserve_ephemeral_port(ip: &Ipv4Addr) -> io::Result<u16> {
        let socket = std::net::UdpSocket::bind((*ip, 0))?;
        Ok(socket.local_addr()?.port())
    }

    fn permission_hint(error: io::Error) -> io::Error {
        if error.kind() == io::ErrorKind::PermissionDenied {
            io::Error::new(
                error.kind(),
                "open raw ICMP socket: grant CAP_NET_RAW to rflow or run it as root",
            )
        } else {
            error
        }
    }

    fn checksum(bytes: &[u8]) -> u16 {
        let mut sum = 0_u32;
        for chunk in bytes.chunks(2) {
            let word = if chunk.len() == 2 {
                u16::from_be_bytes([chunk[0], chunk[1]])
            } else {
                u16::from(chunk[0]) << 8
            };
            sum = sum.wrapping_add(u32::from(word));
        }
        while sum >> 16 != 0 {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        !(sum as u16)
    }

    impl AsyncUdpSocket for IcmpSocket {
        fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
            Box::pin(IcmpPoller(self))
        }

        fn try_send(&self, transmit: &Transmit<'_>) -> io::Result<()> {
            let SocketAddr::V4(destination) = transmit.destination else {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "ICMP transport currently supports IPv4 only",
                ));
            };
            let packet = self.packet(destination.port(), transmit.contents);
            match self.io.get_ref().send_to(
                &packet,
                &SockAddr::from(SocketAddr::new(IpAddr::V4(*destination.ip()), 0)),
            ) {
                Ok(written) if written == packet.len() => Ok(()),
                Ok(_) => Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "partial ICMP packet write",
                )),
                Err(error) => Err(error),
            }
        }

        fn poll_recv(
            &self,
            cx: &mut Context<'_>,
            bufs: &mut [IoSliceMut<'_>],
            meta: &mut [RecvMeta],
        ) -> Poll<io::Result<usize>> {
            if bufs.is_empty() || meta.is_empty() {
                return Poll::Ready(Ok(0));
            }
            loop {
                let mut guard = ready!(self.io.poll_read_ready(cx))?;
                let result = guard.try_io(|inner| {
                    let mut packet = [MaybeUninit::<u8>::uninit(); 65535];
                    let (len, source) = inner.get_ref().recv_from(&mut packet)?;
                    // `recv_from` initialized exactly the first `len` bytes.
                    let packet =
                        unsafe { std::slice::from_raw_parts(packet.as_ptr().cast::<u8>(), len) };
                    let source = source
                        .as_socket_ipv4()
                        .context("ICMP packet has no IPv4 source")?;
                    if packet.is_empty() {
                        return Ok(None);
                    }
                    let ip_header_len = usize::from(packet[0] & 0x0f) * 4;
                    if len < ip_header_len + ICMP_HEADER_LEN {
                        return Ok(None);
                    }
                    let icmp = &packet[ip_header_len..len];
                    let expected_type = match self.role {
                        Role::Client => ICMP_ECHO_REPLY,
                        Role::Server => ICMP_ECHO_REQUEST,
                    };
                    if icmp[0] != expected_type {
                        return Ok(None);
                    }
                    let Some((source_port, destination_port, payload)) =
                        decode_frame(&icmp[ICMP_HEADER_LEN..])
                    else {
                        return Ok(None);
                    };
                    if destination_port != self.local.port() {
                        return Ok(None);
                    }
                    if payload.len() > bufs[0].len() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "encapsulated QUIC datagram exceeds receive buffer",
                        ));
                    }
                    bufs[0][..payload.len()].copy_from_slice(payload);
                    meta[0] = RecvMeta {
                        addr: SocketAddr::new(IpAddr::V4(*source.ip()), source_port),
                        len: payload.len(),
                        stride: payload.len(),
                        ecn: None,
                        dst_ip: Some(self.local.ip()),
                    };
                    Ok(Some(()))
                });
                match result {
                    Ok(Ok(Some(()))) => return Poll::Ready(Ok(1)),
                    Ok(Ok(None)) => continue,
                    Ok(Err(error)) => return Poll::Ready(Err(error)),
                    Err(_) => continue,
                }
            }
        }

        fn local_addr(&self) -> io::Result<SocketAddr> {
            Ok(self.local)
        }
        fn may_fragment(&self) -> bool {
            true
        }
    }

    trait OptionContext<T> {
        fn context(self, message: &'static str) -> io::Result<T>;
    }
    impl<T> OptionContext<T> for Option<T> {
        fn context(self, message: &'static str) -> io::Result<T> {
            self.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, message))
        }
    }
}

pub fn socket(bind: SocketAddr, role: Role) -> io::Result<Arc<dyn quinn::AsyncUdpSocket>> {
    #[cfg(target_os = "linux")]
    {
        linux::IcmpSocket::bind(bind, role).map(|socket| socket as Arc<dyn quinn::AsyncUdpSocket>)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (bind, role);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "ICMP transport is currently available on Linux only",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip() {
        let frame = encode_frame(1234, 24801, b"quic packet");
        assert_eq!(
            decode_frame(&frame),
            Some((1234, 24801, &b"quic packet"[..]))
        );
    }

    #[test]
    fn rejects_unrelated_icmp_payload() {
        assert_eq!(decode_frame(b"not-rflow"), None);
    }
}
