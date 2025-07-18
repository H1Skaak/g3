/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2023-2025 ByteDance and/or its affiliates.
 */

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::task::{Context, Poll, ready};

use g3_io_ext::{AsyncUdpRecv, UdpRelayRemoteError, UdpRelayRemoteRecv};
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "macos",
    target_os = "solaris",
))]
use g3_io_ext::{UdpRelayPacket, UdpRelayPacketMeta};
use g3_types::net::UpstreamAddr;

pub(crate) struct DirectUdpRelayRemoteRecv<T> {
    inner_v4: Option<T>,
    inner_v6: Option<T>,
    bind_v4: SocketAddr,
    bind_v6: SocketAddr,
}

impl<T> DirectUdpRelayRemoteRecv<T> {
    pub(crate) fn new() -> Self {
        DirectUdpRelayRemoteRecv {
            inner_v4: None,
            inner_v6: None,
            bind_v4: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
            bind_v6: SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
        }
    }
}

impl<T> DirectUdpRelayRemoteRecv<T>
where
    T: AsyncUdpRecv,
{
    pub(crate) fn enable_v4(&mut self, inner: T, bind: SocketAddr) {
        self.inner_v4 = Some(inner);
        self.bind_v4 = bind;
    }

    pub(crate) fn enable_v6(&mut self, inner: T, bind: SocketAddr) {
        self.inner_v6 = Some(inner);
        self.bind_v6 = bind;
    }

    fn poll_recv_packet(
        &mut self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<(usize, usize, SocketAddr), UdpRelayRemoteError>> {
        match (&mut self.inner_v4, &mut self.inner_v6) {
            (Some(inner_v4), Some(inner_v6)) => {
                let ret = match inner_v4.poll_recv_from(cx, buf) {
                    Poll::Ready(t) => {
                        let (nr, addr) =
                            t.map_err(|e| UdpRelayRemoteError::RecvFailed(self.bind_v4, e))?;
                        Ok((0, nr, addr))
                    }
                    Poll::Pending => {
                        let (nr, addr) = ready!(inner_v6.poll_recv_from(cx, buf))
                            .map_err(|e| UdpRelayRemoteError::RecvFailed(self.bind_v6, e))?;
                        Ok((0, nr, addr))
                    }
                };
                Poll::Ready(ret)
            }
            (Some(inner_v4), None) => {
                let (nr, addr) = ready!(inner_v4.poll_recv_from(cx, buf))
                    .map_err(|e| UdpRelayRemoteError::RecvFailed(self.bind_v4, e))?;
                Poll::Ready(Ok((0, nr, addr)))
            }
            (None, Some(inner_v6)) => {
                let (nr, addr) = ready!(inner_v6.poll_recv_from(cx, buf))
                    .map_err(|e| UdpRelayRemoteError::RecvFailed(self.bind_v6, e))?;
                Poll::Ready(Ok((0, nr, addr)))
            }
            (None, None) => Poll::Ready(Err(UdpRelayRemoteError::NoListenSocket)),
        }
    }

    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "macos",
        target_os = "solaris",
    ))]
    fn poll_recv_packets(
        inner: &mut T,
        bind_addr: SocketAddr,
        cx: &mut Context<'_>,
        packets: &mut [UdpRelayPacket],
    ) -> Poll<Result<usize, UdpRelayRemoteError>> {
        use g3_io_sys::udp::RecvMsgHdr;

        let mut hdr_v: Vec<RecvMsgHdr<1>> = packets
            .iter_mut()
            .map(|p| RecvMsgHdr::new([std::io::IoSliceMut::new(p.buf_mut())]))
            .collect();

        let count = ready!(inner.poll_batch_recvmsg(cx, &mut hdr_v))
            .map_err(|e| UdpRelayRemoteError::RecvFailed(bind_addr, e))?;

        let mut r = Vec::with_capacity(count);
        for h in hdr_v.into_iter().take(count) {
            let iov = &h.iov[0];
            let addr = h.src_addr().unwrap_or_else(|| match bind_addr {
                SocketAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
                SocketAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
            });
            let ups = UpstreamAddr::from(addr);
            r.push(UdpRelayPacketMeta::new(iov, 0, h.n_recv, ups))
        }
        for (m, p) in r.into_iter().zip(packets.iter_mut()) {
            m.set_packet(p);
        }

        Poll::Ready(Ok(count))
    }
}

impl<T> UdpRelayRemoteRecv for DirectUdpRelayRemoteRecv<T>
where
    T: AsyncUdpRecv + Send,
{
    fn max_hdr_len(&self) -> usize {
        0
    }

    fn poll_recv_packet(
        &mut self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<(usize, usize, UpstreamAddr), UdpRelayRemoteError>> {
        let (off, nr, addr) = ready!(self.poll_recv_packet(cx, buf))?;
        Poll::Ready(Ok((off, nr, UpstreamAddr::from(addr))))
    }

    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "macos",
        target_os = "solaris",
    ))]
    fn poll_recv_packets(
        &mut self,
        cx: &mut Context<'_>,
        packets: &mut [UdpRelayPacket],
    ) -> Poll<Result<usize, UdpRelayRemoteError>> {
        match (&mut self.inner_v4, &mut self.inner_v6) {
            (Some(inner_v4), Some(inner_v6)) => {
                match Self::poll_recv_packets(inner_v4, self.bind_v4, cx, packets) {
                    Poll::Ready(r) => Poll::Ready(r),
                    Poll::Pending => Self::poll_recv_packets(inner_v6, self.bind_v6, cx, packets),
                }
            }
            (Some(inner_v4), None) => Self::poll_recv_packets(inner_v4, self.bind_v4, cx, packets),
            (None, Some(inner_v6)) => Self::poll_recv_packets(inner_v6, self.bind_v6, cx, packets),
            (None, None) => Poll::Ready(Err(UdpRelayRemoteError::NoListenSocket)),
        }
    }
}
