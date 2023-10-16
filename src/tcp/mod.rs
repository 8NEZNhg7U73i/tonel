//! A minimum, userspace TCP based datagram stack
//!
//! # Overview
//!
//! `tcp` module is a minimum TCP stack in
//! user space using the Tun interface. It allows programs to send datagrams
//! as if they are part of a TCP connection. `tcp` module has been tested to
//! be able to pass through a variety of NAT and stateful firewalls while
//! fully preserves certain desirable behavior such as out of order delivery
//! and no congestion/flow controls.
//!
//! # Core Concepts
//!
//! The core of the `tcp` module compose of two structures. [`Stack`] and
//! [`Socket`].
//!
//! ## [`Stack`]
//!
//! [`Stack`] represents a virtual TCP stack that operates at
//! Layer 3. It is responsible for:
//!
//! * TCP active and passive open and handshake
//! * `RST` handling
//! * Interact with the Tun interface at Layer 3
//! * Distribute incoming datagrams to corresponding [`Socket`]
//!
//! ## [`Socket`]
//!
//! [`Socket`] represents a TCP connection. It registers the identifying
//! tuple `(src_ip, src_port, dest_ip, dest_port)` inside the [`Stack`] so
//! so that incoming packets can be distributed to the right [`Socket`] with
//! using a channel. It is also what the client should use for
//! sending/receiving datagrams.

#![cfg_attr(feature = "benchmark", feature(test))]

pub mod packet;

use dashmap::{mapref::entry::Entry, DashMap, DashSet};
use fxhash::FxBuildHasher;
use log::{debug, error, info, trace, warn};
use packet::*;
use pnet::packet::{tcp, Packet};
use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::time;

const TIMEOUT: time::Duration = time::Duration::from_secs(3);
// const RETRIES: usize = 2;
const MPMC_BUFFER_LEN: usize = 1024 * 64;
const MPSC_BUFFER_LEN: usize = 1024 * 8;

type SocketAsyncSender = kanal::AsyncSender<(
    opool::RefGuard<'static, ObjectPoolAllocator, Box<[u8; MAX_PACKET_LEN]>>,
    usize,
)>;
type SocketAsyncReceiver = kanal::AsyncReceiver<(
    opool::RefGuard<'static, ObjectPoolAllocator, Box<[u8; MAX_PACKET_LEN]>>,
    usize,
)>;

#[derive(Hash, Eq, PartialEq, Clone, Debug)]
struct AddrTuple {
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
}

impl AddrTuple {
    fn new(local_addr: SocketAddr, remote_addr: SocketAddr) -> AddrTuple {
        AddrTuple {
            local_addr,
            remote_addr,
        }
    }
}

impl IntoIterator  for  AddrTuple {
    type Item = SocketAddr;
    type IntoIter = std::array::IntoIter<SocketAddr, 2>;

    fn into_iter(self) -> Self::IntoIter {
        std::array::IntoIter::new([self.local_addr, self.remote_addr])
    }
}

struct Shared {
    tuples: DashMap<AddrTuple, SocketAsyncSender, FxBuildHasher>,
    listening: DashSet<u16, FxBuildHasher>,
    tuns: Vec<Arc<tun::AsyncQueue>>,
    tun_index: AtomicUsize,
    ready: kanal::AsyncSender<(Socket, u16)>,
    tuples_purge: Arc<Vec<kanal::AsyncSender<AddrTuple>>>,
    deadline: Option<u64>,
}

pub struct Stack {
    shared: Arc<Shared>,
    local_ip: Ipv4Addr,
    local_ip6: Option<Ipv6Addr>,
    ready: kanal::AsyncReceiver<(Socket, u16)>,
}

pub enum State {
    Idle,
    SynSent,
    SynReceived,
    Established,
}

pub struct Socket {
    _reserved_socket: Option<UdpSocket>,
    shared: Arc<Shared>,
    tun: Arc<tun::AsyncQueue>,
    incoming: SocketAsyncReceiver,
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
    seq: AtomicU32,
    ack: AtomicU32,
    state: State,
    deadline: tokio::time::Instant,
}

/// A socket that represents a unique TCP connection between a server and client.
///
/// The `Socket` object itself satisfies `Sync` and `Send`, which means it can
/// be safely called within an async future.
///
/// To close a TCP connection that is no longer needed, simply drop this object
/// out of scope.
impl Socket {
    fn new(
        _reserved_socket: Option<UdpSocket>,
        shared: Arc<Shared>,
        tun: Arc<tun::AsyncQueue>,
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
        seq: u32,
        ack: u32,
        state: State,
    ) -> (Socket, SocketAsyncSender) {
        let (incoming_tx, incoming_rx) = kanal::bounded_async(MPMC_BUFFER_LEN);

        let deadline = shared.deadline.map_or_else(
            || tokio::time::Instant::now() + Duration::from_secs(86400 * 365),
            |f| tokio::time::Instant::now() + Duration::from_secs(f),
        );
        (
            Socket {
                _reserved_socket,
                shared,
                tun,
                incoming: incoming_rx,
                local_addr,
                remote_addr,
                seq: AtomicU32::new(seq),
                ack: AtomicU32::new(ack),
                state,
                deadline,
            },
            incoming_tx,
        )
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn remote_addr(&self) -> SocketAddr {
        self.remote_addr
    }

    fn build_tcp_packet(
        &self,
        buf: &mut [u8],
        flags: u16,
        payload: Option<&[u8]>,
    ) -> Result<usize, String> {
        let ack = self.ack.load(Ordering::Relaxed);

        build_tcp_packet(
            buf,
            self.local_addr,
            self.remote_addr,
            self.seq.load(Ordering::Relaxed),
            ack,
            flags,
            payload,
        )
    }

    /// Sends a datagram to the other end.
    ///
    /// This method takes `&self`, and it can be called safely by multiple threads
    /// at the same time.
    ///
    /// A return of `None` means the Tun socket returned an error
    /// and this socket must be closed.
    pub async fn send(&self, buf: &mut [u8], payload: &[u8]) -> Option<()> {
        match self.state {
            State::Established => {
                let result = self.build_tcp_packet(buf, tcp::TcpFlags::ACK, Some(payload));
                let size = match result {
                    Ok(size) => size,
                    Err(err) => {
                        error!("Building TCP Packet error on {}: {err}", self.local_addr);
                        return None;
                    }
                };
                self.seq.fetch_add(payload.len() as u32, Ordering::Relaxed);
                self.tun.send(&buf[..size]).await.ok().and(Some(()))
            }
            _ => unreachable!(),
        }
    }

    // Sends keepalive packets
    async fn send_keepalive(&self, buf: &mut [u8], seq: u32) -> Option<()> {
        match self.state {
            State::Established => {
                let size = build_tcp_packet(
                    buf,
                    self.local_addr,
                    self.remote_addr,
                    seq,
                    0,
                    tcp::TcpFlags::ACK,
                    None,
                )
                .unwrap();
                self.tun.send(&buf[..size]).await.ok().and(Some(()))
            }
            _ => unreachable!(),
        }
    }

    /// Attempt to receive a datagram from the other end.
    ///
    /// This method takes `&self`, and it can be called safely by multiple threads
    /// at the same time.
    ///
    /// A return of `None` means the TCP connection is broken
    /// and this socket must be closed.
    pub async fn recv(&self, buf: &mut [u8]) -> Option<usize> {
        let deadline =
            tokio::time::sleep(self.deadline.duration_since(tokio::time::Instant::now()));
        tokio::pin!(deadline);

        for _ in 0..3 {
            let mut seq_sent = false;
            loop {
                match self.state {
                    State::Idle => {
                        trace!(" idle connection {} ", self)
                    }

                    State::SynSent => {
                        trace!(" SynSent connection {} ", self)
                    }

                    State::SynReceived => {
                        trace!(" SynReceived connection {}", self)
                    }

                    State::Established => {
                        let raw_buf = tokio::select! {
                            res = time::timeout(TIMEOUT, self.incoming.recv()) => {
                                match res {
                                    Ok(raw_buf) => match raw_buf {
                                        Ok(raw_buf) => raw_buf,
                                        Err(err) => {
                                            error!("Incoming channel recv error: {err}");
                                            return None;
                                        }
                                    },
                                    Err(err) => {
                                        if seq_sent {
                                            break;
                                        }
                                        trace!("Waiting for tcp {} recv timed out: {err}, sending ACK", self);
                                        if self.send_keepalive(buf, 0).await.is_none() {
                                            trace!("Connection {} unable to send idling ACK back", self);
                                            return None;
                                        }
                                        seq_sent = true;
                                        continue;
                                    }
                                    error!("channel {} recv error: {err}", self)
                                }

                            }
                            _ = &mut deadline => {
                                return None;
                            }
                        };

                        let (_v4_packet, tcp_packet) =
                            match parse_ip_packet(&raw_buf.0[..raw_buf.1]) {
                                Some(packet) => packet,
                                None => return None,
                            };

                        if (tcp_packet.get_flags() & tcp::TcpFlags::RST) != 0 {
                            info!("Connection {} reset by peer", self);
                            return None;
                        }

                        if tcp_packet.get_flags() == tcp::TcpFlags::ACK
                            && tcp_packet.get_acknowledgement() == 0
                            && tcp_packet.payload().is_empty()
                        {
                            if tcp_packet.get_sequence() == 1 && seq_sent {
                                trace!("Received final ACK {}", self);
                                seq_sent = false;
                                continue;
                            } else if tcp_packet.get_sequence() == 0 {
                                trace!("Received ACK, sending ACK {}", self);
                                if self.send_keepalive(buf, 1).await.is_none() {
                                    trace!("Connection {} unable to send idling ACK back", self);
                                    return None;
                                }
                                continue;
                            }
                        }

                        let payload = tcp_packet.payload();

                        let new_ack = tcp_packet.get_sequence().wrapping_add(payload.len() as u32);
                        self.ack.store(new_ack, Ordering::Relaxed);

                        buf[..payload.len()].copy_from_slice(payload);

                        return Some(payload.len());
                    }
                    _ => unreachable!(),
                }
            }
        }
        debug!("Waiting for tcp recv timed out on ACK, connection {} is broken", self);
        None
    }

    async fn accept(mut self, buf: &mut [u8], seq: u32) {
        loop {
            match self.state {
                State::Idle => {
                    let size = self
                        .build_tcp_packet(buf, tcp::TcpFlags::SYN | tcp::TcpFlags::ACK, None)
                        .unwrap();
                    // ACK set by constructor
                    if let Err(err) = self.tun.send(&buf[..size]).await {
                        trace!("Sent SYN + ACK error {}: {err}", self);
                        break;
                    }
                    self.state = State::SynReceived;
                    trace!("Sent SYN + ACK to client {}", self);
                }
                State::SynReceived => {
                    let res = time::timeout(TIMEOUT, self.incoming.recv()).await;
                    let buf = match res {
                        Ok(buf) => match buf {
                            Ok(buf) => buf,
                            Err(err) => {
                                error!("Incoming channel {} recv error: {err}", self);
                                break;
                            }
                        },
                        Err(err) => {
                            trace!("Waiting for client {} ACK timed out: {err}", self);
                            break;
                        }
                    };

                    let (_ip_packet, tcp_packet) = match parse_ip_packet(&buf.0[..buf.1]) {
                        Some(packet) => packet,
                        None => break,
                    };

                    if (tcp_packet.get_flags() & tcp::TcpFlags::RST) != 0 {
                        break;
                    }

                    let packet_ack = tcp_packet.get_acknowledgement();
                    if tcp_packet.get_flags() == tcp::TcpFlags::ACK
                        && self
                            .seq
                            .compare_exchange(
                                packet_ack - 1,
                                packet_ack,
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                            )
                            .is_ok()
                    {
                        // found our ACK
                        self.state = State::Established;

                        info!("Connection {} established", self);
                        let ready = self.shared.ready.clone();
                        if let Err(e) = ready.send((self, seq.try_into().unwrap_or(0))).await {
                            error!("Unable to send accepted socket to ready queue: {}", e);
                        }
                    }
                    return;
                }
                _ => unreachable!(),
            }
        }
        self.state = State::Idle;
    }

    async fn connect(&mut self, buf: &mut [u8]) -> Option<u16> {
        loop {
            match self.state {
                State::Idle => {
                    let size = self
                        .build_tcp_packet(buf, tcp::TcpFlags::SYN, None)
                        .unwrap();
                    if let Err(err) = self.tun.send(&buf[..size]).await {
                        trace!("Send SYN error {}: {err}", self);
                        return None;
                    }
                    self.state = State::SynSent;
                    trace!("Sent SYN to server {}", self);
                }
                State::SynSent => {
                    let res = time::timeout(TIMEOUT, self.incoming.recv()).await;
                    let packet_buf = match res {
                        Ok(packet_buf) => match packet_buf {
                            Ok(packet_buf) => packet_buf,
                            Err(err) => {
                                trace!("incoming channel {} error: {err}", self);
                                break;
                            }
                        },
                        Err(err) => {
                            trace!("Waiting for {} SYN + ACK timed out: {err}", self);
                            break;
                        }
                    };
                    let (_ip_packet, tcp_packet) =
                        match parse_ip_packet(&packet_buf.0[..packet_buf.1]) {
                            Some(packet) => packet,
                            None => break,
                        };

                    if (tcp_packet.get_flags() & tcp::TcpFlags::RST) != 0 {
                        break;
                    }

                    let packet_ack = tcp_packet.get_acknowledgement();
                    if tcp_packet.get_flags() == tcp::TcpFlags::SYN | tcp::TcpFlags::ACK
                        && self
                            .seq
                            .compare_exchange(
                                packet_ack - 1,
                                packet_ack,
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                            )
                            .is_ok()
                    {
                        // found our SYN + ACK
                        self.ack
                            .store(tcp_packet.get_sequence() + 1, Ordering::Release);

                        // send ACK to finish handshake
                        let size = self
                            .build_tcp_packet(buf, tcp::TcpFlags::ACK, None)
                            .unwrap();

                        if let Err(err) = self.tun.send(&buf[..size]).await {
                            trace!("Send ACK {} error: {err}", self);
                            break;
                        }

                        self.state = State::Established;

                        info!("Connection {} established", self);

                        return Some(tcp_packet.get_sequence().try_into().unwrap_or(0));
                    }

                    break;
                }
                _ => unreachable!(),
            }
        }
        self.state = State::Idle;
        None
    }
}

impl Drop for Socket {
    /// Drop the socket and close the TCP connection
    fn drop(&mut self) {
        let tuple = AddrTuple::new(self.local_addr, self.remote_addr);
        // dissociates ourself from the dispatch map
        assert!(self.shared.tuples.remove(&tuple).is_some());

        let tuples_purge = self.shared.tuples_purge.clone();
        let tun = self.tun.clone();
        let mut buf = [0u8; MAX_PACKET_LEN];
        let size = build_tcp_packet(
            &mut buf,
            self.local_addr,
            self.remote_addr,
            0,
            0,
            tcp::TcpFlags::RST,
            None,
        )
        .unwrap();
        tokio::spawn(async move {
            for tx in tuples_purge.iter() {
                if let Err(err) = tx.send(tuple.clone()).await {
                    error!("Send error in tuples_purge: {err} {:?}", tx);
                }
            }
            if let Err(e) = tun.send(&buf[..size]).await {
                warn!("Unable to send RST to remote end: {}", e);
            }
        });
        info!("TCP connection {} closed", self);
    }
}

impl fmt::Display for Socket {
    /// User-friendly string representation of the socket
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "(TCP connection from {} to {})",
            self.local_addr, self.remote_addr
        )
    }
}

use once_cell::sync::Lazy;
use opool::PoolAllocator;

use crate::utils::new_udp_reuseport;

struct ObjectPoolAllocator;
impl PoolAllocator<Box<[u8; MAX_PACKET_LEN]>> for ObjectPoolAllocator {
    #[inline]
    fn allocate(&self) -> Box<[u8; MAX_PACKET_LEN]> {
        Box::new([0u8; MAX_PACKET_LEN])
    }

    #[inline]
    fn reset(&self, _obj: &mut Box<[u8; MAX_PACKET_LEN]>) {}

    #[inline]
    fn is_valid(&self, _obj: &Box<[u8; MAX_PACKET_LEN]>) -> bool {
        true
    }
}

static GLOBAL_PACKET_POOL: Lazy<opool::Pool<ObjectPoolAllocator, Box<[u8; MAX_PACKET_LEN]>>> =
    Lazy::new(|| opool::Pool::new(MPMC_BUFFER_LEN, ObjectPoolAllocator));

/// A userspace TCP state machine
impl Stack {
    /// Create a new stack, `tun` is an array of [`Tun`](tokio_tun::Tun).
    /// When more than one [`Tun`](tokio_tun::Tun) object is passed in, same amount
    /// of reader will be spawned later. This allows user to utilize the performance
    /// benefit of Multiqueue Tun support on machines with SMP.
    pub fn new<T>(
        tuns: T,
        local_ip: Ipv4Addr,
        local_ip6: Option<Ipv6Addr>,
        timeout: Option<u64>,
    ) -> Stack
    where
        T: tun::Device<Queue = tun::platform::Queue>,
    {
        let tuns: Vec<Arc<tun::AsyncQueue>> = tuns
            .queues()
            .into_iter()
            .map(|x| Arc::new(tun::AsyncQueue::new(x).unwrap()))
            .collect();
        let (ready_tx, ready_rx) = kanal::bounded_async(MPSC_BUFFER_LEN);
        let (tuples_purge_tx, tuples_purge_rx) = {
            let mut senders = Vec::with_capacity(tuns.len());
            let mut receivers = Vec::with_capacity(tuns.len());
            for _ in 0..tuns.len() {
                let (tuples_purge_tx, tuples_purge_rx) = kanal::bounded_async(MPMC_BUFFER_LEN);
                senders.push(tuples_purge_tx);
                receivers.push(tuples_purge_rx);
            }
            (senders, receivers)
        };
        let shared = Arc::new(Shared {
            tuples: DashMap::default(),
            tuns: tuns.clone(),
            tun_index: AtomicUsize::new(0),
            listening: DashSet::default(),
            ready: ready_tx,
            tuples_purge: Arc::new(tuples_purge_tx),
            deadline: timeout,
        });

        for (index, rx) in tuples_purge_rx.into_iter().enumerate() {
            tokio::spawn(Stack::reader_task(tuns[index].clone(), shared.clone(), rx));
        }

        Stack {
            shared,
            local_ip,
            local_ip6,
            ready: ready_rx,
        }
    }

    /// Listens for incoming connections on the given `port`.
    pub fn listen(&mut self, port: u16) {
        assert!(self.shared.listening.insert(port));
    }

    /// Accepts an incoming connection.
    pub async fn accept(&mut self) -> (Socket, u16) {
        self.ready.recv().await.unwrap()
    }

    /// Connects to the remote end. `None` returned means
    /// the connection attempt failed.
    pub async fn connect(
        &self,
        buf: &mut [u8],
        addr: SocketAddr,
        seq: u32,
    ) -> Option<(Socket, u16)> {
        let socket =
            match new_udp_reuseport(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0)) {
                Ok(socket) => socket,
                Err(err) => {
                    error!("failed creating new socket: {err}");
                    return None;
                }
            };
        let local_addr = SocketAddr::new(
            if addr.is_ipv4() {
                IpAddr::V4(self.local_ip)
            } else {
                IpAddr::V6(self.local_ip6.expect("IPv6 local address undefined"))
            },
            socket.local_addr().unwrap().port(),
        );
        let tuple = AddrTuple::new(local_addr, addr);
        let mut sock = match self.shared.tuples.entry(tuple) {
            Entry::Occupied(_) => return None,
            Entry::Vacant(v) => {
                let tun_index =
                    self.shared.tun_index.fetch_add(1, Ordering::AcqRel) % self.shared.tuns.len();
                let tun = self.shared.tuns[tun_index].clone();
                let (sock, incoming) = Socket::new(
                    Some(socket),
                    self.shared.clone(),
                    tun,
                    local_addr,
                    addr,
                    seq,
                    0,
                    State::Idle,
                );
                v.insert(incoming);
                sock
            }
        };
        sock.connect(buf).await.map(|port| (sock, port))
    }

    async fn reader_task(
        tun: Arc<tun::AsyncQueue>,
        shared: Arc<Shared>,
        tuples_purge: kanal::AsyncReceiver<AddrTuple>,
    ) {
        let mut tuples: HashMap<AddrTuple, SocketAsyncSender, FxBuildHasher> = HashMap::default();

        let mut send_buf = [0u8; MAX_PACKET_LEN];
        loop {
            let mut recv_buf = GLOBAL_PACKET_POOL.get();

            tokio::select! {
                biased;
                tuple = tuples_purge.recv() => {
                    let tuple = match tuple {
                        Ok(tuple) => tuple,
                        Err(err) => {
                            error!("tuples_purge recv error: {err}");
                            continue;
                        }
                    };
                    tuples.remove(&tuple);
                    trace!("Removed cached tuple: {:?}", tuple);
                    for i in tuples.iter() {
                        trace!("tuple: {:?}", i)
                    }
                },
                size = tun.recv(&mut recv_buf[..]) => {
                    let size = match size {
                        Ok(size) => size,
                        Err(err) => {
                            error!("Couldn't read tun buf: {err}");
                            continue;
                        }
                    };

                    let (ip_packet, tcp_packet) = match parse_ip_packet(&recv_buf[..size]) {
                        Some(data) => data,
                        None => continue,
                    };

                    let local_addr =
                        SocketAddr::new(ip_packet.get_destination(), tcp_packet.get_destination());
                    let remote_addr = SocketAddr::new(ip_packet.get_source(), tcp_packet.get_source());

                    let tuple = AddrTuple::new(local_addr, remote_addr);

                    if let Some(c) = tuples.get(&tuple) {
                        if c.send((recv_buf, size)).await.is_err() {
                            trace!("Cache hit, but receiver {:?} already closed, dropping packet", tuple);
                        }
                        continue;
                    }

                    if let Some(c) = shared.tuples.get(&tuple) {
                        tuples.insert(tuple.clone(), c.clone());
                        if let Err(err) = c.send((recv_buf, size)).await {
                            drop(c);
                            error!("Couldn't send to {:?} shared tuples channel: {err}", tuple);
                        }
                        continue;
                    }

                    if tcp_packet.get_flags() == tcp::TcpFlags::SYN
                        && shared.listening.contains(&tcp_packet.get_destination())
                    {
                        // SYN seen on listening socket
                        let (sock, incoming) = Socket::new(
                            None,
                            shared.clone(),
                            tun.clone(),
                            local_addr,
                            remote_addr,
                            remote_addr.port() as u32,
                            tcp_packet.get_sequence() + 1,
                            State::Idle,
                        );
                        assert!(shared.tuples.insert(tuple.clone(), incoming.clone()).is_none());
                        tuples.insert(tuple, incoming);
                        let seq = tcp_packet.get_sequence();
                        tokio::spawn(async move {
                            let mut buf = [0u8; MAX_PACKET_LEN];
                            sock.accept(&mut buf, seq).await
                        });
                    } else if (tcp_packet.get_flags() & tcp::TcpFlags::RST) == 0 {
                        trace!("Unknown TCP packet from {:?}, sending RST", tuple);
                        let size = build_tcp_packet(
                            &mut send_buf,
                            local_addr,
                            remote_addr,
                            tcp_packet.get_acknowledgement(),
                            tcp_packet.get_sequence() + tcp_packet.payload().len() as u32,
                            tcp::TcpFlags::RST | tcp::TcpFlags::ACK,
                            None,
                        ).unwrap();
                        let tun_index = shared.tun_index.fetch_add(1, Ordering::Relaxed) % shared.tuns.len();
                        let tun = shared.tuns[tun_index].clone();
                        if let Err(err) = tun.send(&send_buf[..size]).await {
                            error!("tun send {:?} error: {err}", tuple);
                        }
                    }
                }
            }
        }
    }
}
