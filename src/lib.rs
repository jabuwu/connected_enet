//! An extension for [rusty_enet](https://crates.io/crates/rusty_enet) allowing easier use with
//! connection based networking (such as TCP, WebRTC, etc).
//!
//! This crate provides a very similar interface to that of the connectionless version with the main
//! difference being how interfaces are defined (using [`Connection`] instead of [`enet::Socket`]
//! and how connections are made (see [`ConnectionHost::add_connection`]).
//!
//! The main purpose of this crate is to handle the awkward state where both sides are already
//! technically connected, but haven't communicated yet, as well as juggling between multiple
//! sockets instead of just the one (as you would with a connectionless socket).

#![warn(
    missing_docs,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::manual_assert,
    clippy::ptr_cast_constness,
    clippy::ptr_as_ptr,
    clippy::default_trait_access,
    clippy::explicit_iter_loop,
    clippy::explicit_into_iter_loop,
    clippy::needless_pass_by_value,
    clippy::option_if_let_else,
    clippy::redundant_feature_names,
    clippy::semicolon_if_nothing_returned,
    clippy::must_use_candidate,
    clippy::borrow_as_ptr,
    clippy::items_after_statements,
    clippy::single_match_else,
    clippy::bool_to_int_with_if,
    clippy::unnecessary_cast
)]

use std::{
    io,
    mem::replace,
    net::{SocketAddr, TcpStream},
    time::Duration,
};

pub use enet;

/// Error types.
pub mod error {
    use crate::Connection;

    /// Errors for [`ConnectionHost::add_connection`](`crate::ConnectionHost::add_connection`).
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub enum AddConnectionError<C: Connection> {
        /// Failed to add connection because there were no available ENet connection slots.
        NoAvailablePeers,
        /// Failed to initialize the connection ([`Connection::init`] failed).
        FailedToInitializeConnection(C::Error),
    }

    impl<C: Connection> core::fmt::Debug for AddConnectionError<C> {
        fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
            match self {
                AddConnectionError::NoAvailablePeers => f.write_str("NoAvailablePeers"),
                AddConnectionError::FailedToInitializeConnection(err) => f
                    .debug_tuple("FailedToInitializeConnection")
                    .field(&err)
                    .finish(),
            }
        }
    }

    impl<C: Connection> core::fmt::Display for AddConnectionError<C> {
        fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
            match self {
                AddConnectionError::NoAvailablePeers => f.write_str(
                    "Failed to add connection because there were no available ENet connection slots.",
                ),
                AddConnectionError::FailedToInitializeConnection(_) => {
                    f.write_str("Failed to initialize the connection.")
                }
            }
        }
    }
}

#[derive(Clone)]
struct ConnectionAddress<A: enet::Address> {
    address: A,
    id: ConnectionID,
    generation: usize,
}

impl<A: enet::Address> ConnectionAddress<A> {
    fn new(address: A, index: ConnectionID, generation: usize) -> Self {
        Self {
            address,
            id: index,
            generation,
        }
    }

    fn connection_id(&self) -> ConnectionID {
        self.id
    }
}

impl<A: enet::Address> enet::Address for ConnectionAddress<A> {
    fn same(&self, other: &Self) -> bool {
        self.id == other.id
            && self.generation == other.generation
            && A::same(&self.address, &other.address)
    }

    fn same_host(&self, other: &Self) -> bool {
        self.id == other.id
            && self.generation == other.generation
            && A::same_host(&self.address, &other.address)
    }

    fn is_broadcast(&self) -> bool {
        A::is_broadcast(&self.address)
    }
}

/// A trait for implementing connection based sockets, similar to [`enet::Socket`].
///
/// An implementation for [`std::net::TcpStream`] is provided out of the box.
///
/// An implementation of [`ReadWrite`](`enet::ReadWrite`) is provided with an address type of `()`.
#[allow(clippy::missing_errors_doc)]
pub trait Connection {
    /// The address type to use, which must implement [`enet::Address`].
    /// [`std::net::UdpSocket`].
    type Address: enet::Address;
    /// Errors returned by this connection.
    type Error: enet::SocketError;

    /// Initialize the socket with options passed down by ENet.
    ///
    /// Called in [`ConnectionHost::add_connection`]. If this function returns an error, it is
    /// bubbled up through [`ConnectionHost::add_connection`].
    fn init(&mut self) -> Result<Self::Address, Self::Error>;

    /// Try to send data. Should return the number of bytes successfully sent, or an error.
    fn send(&mut self, buffer: &[u8]) -> Result<usize, Self::Error>;

    /// Try to receive data from the socket into a buffer of size [`enet::MTU_MAX`].
    ///
    /// A received packet should be written into the provided buffer. If a packet is received that
    /// is larger than [`enet::MTU_MAX`], it should simply be discarded. ENet will never send a
    /// packet that is larger than this maximum, so if one is received, it was not sent by ENet.
    ///
    /// The return value should be `Ok(None)` if no packet was received. If a packet was received,
    /// the amount of bytes received should be returned. Packets received may be complete or
    /// partial. See [`enet::PacketReceived`] for more info.
    fn receive(
        &mut self,
        buffer: &mut [u8; enet::MTU_MAX],
    ) -> Result<Option<enet::PacketReceived>, Self::Error>;
}

impl<E: enet::SocketError> Connection for enet::ReadWrite<(), E> {
    type Address = ();
    type Error = E;

    fn init(&mut self) -> Result<Self::Address, Self::Error> {
        Ok(())
    }

    fn send(&mut self, buffer: &[u8]) -> Result<usize, Self::Error> {
        enet::Socket::send(self, (), buffer)
    }

    fn receive(
        &mut self,
        buffer: &mut [u8; enet::MTU_MAX],
    ) -> Result<Option<enet::PacketReceived>, Self::Error> {
        Ok(enet::Socket::receive(self, buffer)?.map(|(_, received)| received))
    }
}

type SocketInterface<C> =
    enet::ReadWrite<ConnectionAddress<<C as Connection>::Address>, <C as Connection>::Error>;

/// In ENet, one peer must establish a connection to the other peer. This can be a little confusing
/// in a connection based environment, where this has already happened. Still, one side needs to
/// initiate what is essentially a handshake, and the other needs to receive it.
///
/// The initiator provides details for [`enet::Host::connect`] to start the handshake, while the
/// receiver (which simply awaits this handshake) provides a timeout in case that handshake never
/// arrives. If the receiver times out, an [`Event::Disconnect`] is fired for that connection.
pub enum ConnectionKind<C: Connection> {
    /// Initiate a connection. This is usually from the TCP connector or WebRTC offerer.
    Initiator {
        /// The connection object to send to and receive from.
        connection: C,
        /// The number of ENet channels to allow.
        channel_count: usize,
        /// Data sent to the remote peer on connect.
        data: u32,
    },
    /// Receive a connection. This is usually from the TCP listener or WebRTC answerer.
    Receiver {
        /// The connection object to send to and receive from.
        connection: C,
        /// The amount of time to wait for the initiator before giving up.
        timeout: Duration,
    },
}

/// A newtype around a `usize`, representing a unique identifier for a connection based peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnectionID(pub usize);

#[derive(Default)]
struct ConnectionPeerSettings {
    timeout: Option<(u32, u32, u32)>,
    ping_interval: Option<u32>,
    throttle: Option<(u32, u32, u32)>,
    mtu: Option<u16>,
}

impl ConnectionPeerSettings {
    fn apply<C: Connection + 'static>(&self, peer: &mut enet::Peer<SocketInterface<C>>) {
        if let Some((limit, minimum, maximum)) = self.timeout {
            peer.set_timeout(limit, minimum, maximum);
        }
        if let Some(ping_interval) = self.ping_interval {
            peer.set_ping_interval(ping_interval);
        }
        if let Some((interval, acceleration, deceleration)) = self.throttle {
            peer.set_throttle(interval, acceleration, deceleration);
        }
        if let Some(mtu) = self.mtu {
            _ = peer.set_mtu(mtu);
        }
    }
}

enum ConnectionPeerState<C: Connection + 'static> {
    Disconnected {
        last_peer_ptr: Option<*mut enet::Peer<SocketInterface<C>>>,
    },
    AwaitingPeer {
        connection: C,
        address: ConnectionAddress<C::Address>,
        since: Duration,
        timeout: Duration,
        settings: ConnectionPeerSettings,
    },
    HasPeer {
        connection: C,
        address: ConnectionAddress<C::Address>,
        peer_ptr: *mut enet::Peer<SocketInterface<C>>,
    },
    Disconnecting {
        connection: C,
        address: ConnectionAddress<C::Address>,
        last_peer_ptr: Option<*mut enet::Peer<SocketInterface<C>>>,
        last_send: Duration,
    },
}

/// A peer, associated with a [`ConnectionHost`], which may or may not be connected.
///
/// To check on the connectivity of a peer, see [`ConnectionPeer::state`].
pub struct ConnectionPeer<C: Connection + 'static> {
    id: ConnectionID,
    state: ConnectionPeerState<C>,
}

impl<C: Connection> ConnectionPeer<C> {
    fn peer(&self) -> Option<&mut enet::Peer<SocketInterface<C>>> {
        match self.state {
            ConnectionPeerState::HasPeer { peer_ptr, .. } => unsafe { Some(&mut *peer_ptr) },
            _ => None,
        }
    }

    fn peer_or_last_peer(&self) -> Option<&mut enet::Peer<SocketInterface<C>>> {
        match self.state {
            ConnectionPeerState::Disconnected { last_peer_ptr, .. } => unsafe {
                last_peer_ptr.map(|peer_ptr| &mut *peer_ptr)
            },
            ConnectionPeerState::HasPeer { peer_ptr, .. } => unsafe { Some(&mut *peer_ptr) },
            ConnectionPeerState::Disconnecting { last_peer_ptr, .. } => unsafe {
                last_peer_ptr.map(|peer_ptr| &mut *peer_ptr)
            },
            _ => None,
        }
    }

    fn settings(&mut self) -> Option<&mut ConnectionPeerSettings> {
        match &mut self.state {
            ConnectionPeerState::AwaitingPeer { settings, .. } => Some(settings),
            _ => None,
        }
    }

    /// Get the [`ConnectionID`] of this peer.
    #[must_use]
    pub fn id(&self) -> ConnectionID {
        self.id
    }

    /// Get a reference to the underlying connection.
    pub fn connection(&self) -> Option<&C> {
        match &self.state {
            ConnectionPeerState::Disconnected { .. } => None,
            ConnectionPeerState::AwaitingPeer { connection, .. } => Some(connection),
            ConnectionPeerState::HasPeer { connection, .. } => Some(connection),
            ConnectionPeerState::Disconnecting { connection, .. } => Some(connection),
        }
    }

    /// Get a mutable reference to the underlying connection.
    pub fn connection_mut(&mut self) -> Option<&mut C> {
        match &mut self.state {
            ConnectionPeerState::Disconnected { .. } => None,
            ConnectionPeerState::AwaitingPeer { connection, .. } => Some(connection),
            ConnectionPeerState::HasPeer { connection, .. } => Some(connection),
            ConnectionPeerState::Disconnecting { connection, .. } => Some(connection),
        }
    }

    /// See [`enet::Peer::ping`].
    pub fn ping(&mut self) {
        if let Some(peer) = self.peer() {
            peer.ping();
        }
    }

    /// See [`enet::Peer::send`].
    ///
    /// # Errors
    ///
    /// May return any of the [`enet::error::PeerSendError`] variants on failure.
    pub fn send(
        &mut self,
        channel_id: u8,
        packet: &enet::Packet,
    ) -> Result<(), enet::error::PeerSendError> {
        self.peer()
            .map_or(Err(enet::error::PeerSendError::NotConnected), |peer| {
                peer.send(channel_id, packet)
            })
    }

    /// See [`enet::Peer::disconnect`].
    pub fn disconnect(&mut self, data: u32) {
        if let Some(peer) = self.peer() {
            peer.disconnect(data);
        } else {
            self.state = ConnectionPeerState::Disconnected {
                last_peer_ptr: None,
            };
        }
    }

    /// See [`enet::Peer::disconnect_now`].
    pub fn disconnect_now(&mut self, data: u32) {
        if let Some(peer) = self.peer() {
            peer.disconnect_now(data);
        } else {
            self.state = ConnectionPeerState::Disconnected {
                last_peer_ptr: None,
            };
        }
    }

    /// See [`enet::Peer::disconnect_later`].
    pub fn disconnect_later(&mut self, data: u32) {
        if let Some(peer) = self.peer() {
            peer.disconnect_later(data);
        } else {
            self.state = ConnectionPeerState::Disconnected {
                last_peer_ptr: None,
            };
        }
    }

    /// See [`enet::Peer::reset`].
    pub fn reset(&mut self) {
        if let Some(peer) = self.peer() {
            peer.reset();
        }
        self.state = ConnectionPeerState::Disconnected {
            last_peer_ptr: None,
        };
    }

    /// See [`enet::Peer::set_timeout`].
    pub fn set_timeout(&mut self, limit: u32, minimum: u32, maximum: u32) {
        if let Some(peer) = self.peer() {
            peer.set_timeout(limit, minimum, maximum);
        } else if let Some(settings) = self.settings() {
            settings.timeout = Some((limit, minimum, maximum));
        }
    }

    /// See [`enet::Peer::set_ping_interval`].
    pub fn set_ping_interval(&mut self, ping_interval: u32) {
        if let Some(peer) = self.peer() {
            peer.set_ping_interval(ping_interval);
        } else if let Some(settings) = self.settings() {
            settings.ping_interval = Some(ping_interval);
        }
    }

    /// See [`enet::Peer::set_throttle`].
    pub fn set_throttle(&mut self, interval: u32, acceleration: u32, deceleration: u32) {
        if let Some(peer) = self.peer() {
            peer.set_throttle(interval, acceleration, deceleration);
        } else if let Some(settings) = self.settings() {
            settings.throttle = Some((interval, acceleration, deceleration));
        }
    }

    /// See [`enet::Peer::mtu`].
    #[must_use]
    pub fn mtu(&self) -> u16 {
        self.peer_or_last_peer().map_or(0, |peer| peer.mtu())
    }

    /// See [`enet::Peer::set_mtu`].
    ///
    /// # Errors
    ///
    /// Returns [`enet::error::BadParameter`] if `mtu` is greater than
    /// [`enet::consts::PROTOCOL_MAXIMUM_MTU`] or less than [`enet::consts::PROTOCOL_MINIMUM_MTU`].
    pub fn set_mtu(&mut self, mtu: u16) -> Result<(), enet::error::BadParameter> {
        if mtu > enet::consts::PROTOCOL_MAXIMUM_MTU as u16
            || mtu < enet::consts::PROTOCOL_MINIMUM_MTU as u16
        {
            return Err(enet::error::BadParameter {
                method: "ConnectedPeer::set_mtu",
                parameter: "mtu",
            });
        }
        #[allow(clippy::option_if_let_else)]
        if let Some(peer) = self.peer() {
            peer.set_mtu(mtu)
        } else if let Some(settings) = self.settings() {
            settings.mtu = Some(mtu);
            Ok(())
        } else {
            Ok(())
        }
    }

    /// See [`enet::Peer::state`].
    #[must_use]
    pub fn state(&self) -> enet::PeerState {
        self.peer_or_last_peer()
            .map_or(enet::PeerState::Disconnected, |peer| peer.state())
    }

    /// See [`enet::Peer::connected`].
    #[must_use]
    pub fn connected(&self) -> bool {
        self.state() == enet::PeerState::Connected
    }

    /// See [`enet::Peer::channel_count`].
    #[must_use]
    pub fn channel_count(&self) -> usize {
        self.peer_or_last_peer()
            .map_or(0, |peer| peer.channel_count())
    }

    /// See [`enet::Peer::incoming_bandwidth`].
    #[must_use]
    pub fn incoming_bandwidth(&self) -> u32 {
        self.peer_or_last_peer()
            .map_or(0, |peer| peer.incoming_bandwidth())
    }

    /// See [`enet::Peer::outgoing_bandwidth`].
    #[must_use]
    pub fn outgoing_bandwidth(&self) -> u32 {
        self.peer_or_last_peer()
            .map_or(0, |peer| peer.outgoing_bandwidth())
    }

    /// See [`enet::Peer::incoming_data_total`].
    #[must_use]
    pub fn incoming_data_total(&self) -> u32 {
        self.peer_or_last_peer()
            .map_or(0, |peer| peer.incoming_data_total())
    }

    /// See [`enet::Peer::outgoing_data_total`].
    #[must_use]
    pub fn outgoing_data_total(&self) -> u32 {
        self.peer_or_last_peer()
            .map_or(0, |peer| peer.outgoing_data_total())
    }

    /// See [`enet::Peer::packets_sent`].
    #[must_use]
    pub fn packets_sent(&self) -> u32 {
        self.peer_or_last_peer()
            .map_or(0, |peer| peer.packets_sent())
    }

    /// See [`enet::Peer::packets_lost`].
    #[must_use]
    pub fn packets_lost(&self) -> u32 {
        self.peer_or_last_peer()
            .map_or(0, |peer| peer.packets_lost())
    }

    /// See [`enet::Peer::packet_loss`].
    #[must_use]
    pub fn packet_loss(&self) -> u32 {
        self.peer_or_last_peer()
            .map_or(0, |peer| peer.packet_loss())
    }

    /// See [`enet::Peer::packet_loss_variance`].
    #[must_use]
    pub fn packet_loss_variance(&self) -> u32 {
        self.peer_or_last_peer()
            .map_or(0, |peer| peer.packet_loss_variance())
    }

    /// See [`enet::Peer::ping_interval`].
    #[must_use]
    pub fn ping_interval(&self) -> Duration {
        self.peer_or_last_peer()
            .map_or(Duration::ZERO, |peer| peer.ping_interval())
    }

    /// See [`enet::Peer::round_trip_time`].
    #[must_use]
    pub fn round_trip_time(&self) -> Duration {
        self.peer_or_last_peer()
            .map_or(Duration::ZERO, |peer| peer.round_trip_time())
    }

    /// See [`enet::Peer::round_trip_time_variance`].
    #[must_use]
    pub fn round_trip_time_variance(&self) -> Duration {
        self.peer_or_last_peer()
            .map_or(Duration::ZERO, |peer| peer.round_trip_time_variance())
    }

    /// See [`enet::Peer::address`].
    #[must_use]
    pub fn address(&self) -> Option<C::Address> {
        self.peer().map_or_else(
            || None,
            |peer| peer.address().map(|address| address.address),
        )
    }
}

/// An ENet event returned by [`ConnectionHost::service`](`crate::ConnectionHost::service`).
pub enum Event<'a, C: Connection + 'static> {
    /// A new peer has connected.
    Connect {
        /// Peer that generated the event.
        peer: &'a mut ConnectionPeer<C>,
        /// Data associated with the event, sent by the peer on connect.
        data: u32,
    },
    /// A peer has disconnected.
    Disconnect {
        /// Peer that generated the event.
        peer: &'a mut ConnectionPeer<C>,
        /// Data associated with the event, sent by the peer on disconnect.
        data: u32,
    },
    /// A peer sent a packet to us.
    Receive {
        /// Peer that generated the event.
        peer: &'a mut ConnectionPeer<C>,
        /// Channel the peer sent the packet on.
        channel_id: u8,
        /// The actual packet data.
        packet: enet::Packet,
    },
}

impl<'a, C: Connection + 'static> Event<'a, C> {
    /// Remove the peer reference from this event, converting into an [`EventNoRef`].
    #[must_use]
    pub fn no_ref(self) -> EventNoRef {
        match self {
            Self::Connect { peer, data } => EventNoRef::Connect {
                peer: peer.id(),
                data,
            },
            Self::Disconnect { peer, data } => EventNoRef::Disconnect {
                peer: peer.id(),
                data,
            },
            Self::Receive {
                peer,
                channel_id,
                packet,
            } => EventNoRef::Receive {
                peer: peer.id(),
                channel_id,
                packet,
            },
        }
    }
}

/// An ENet event, like [`Event`], but without peer references.
///
/// Acquired with [`Event::no_ref`].
#[derive(Debug, Clone)]
pub enum EventNoRef {
    /// A new peer has connected.
    Connect {
        /// Peer that generated the event.
        peer: ConnectionID,
        /// Data associated with the event, sent by the peer on connect.
        data: u32,
    },
    /// A peer has disconnected.
    Disconnect {
        /// Peer that generated the event.
        peer: ConnectionID,
        /// Data associated with the event, sent by the peer on disconnect.
        data: u32,
    },
    /// A peer sent a packet to us.
    Receive {
        /// Peer that generated the event.
        peer: ConnectionID,
        /// Channel the peer sent the packet on.
        channel_id: u8,
        /// The actual packet data.
        packet: enet::Packet,
    },
}

/// A host for communicating with connection based peers.
///
/// Requires a [`Connection`] implementation.
pub struct ConnectionHost<C: Connection + 'static> {
    host: enet::Host<SocketInterface<C>>,
    peers: Vec<ConnectionPeer<C>>,
    next_generation: usize,
}

impl<C: Connection> ConnectionHost<C> {
    /// Creates a host for communicating to connection based peers.
    ///
    /// Supports [`std::net::TcpStream`] out of the box, but other transport protocols can be
    /// provided by implementing the [`Connection`] trait.
    ///
    /// # Errors
    ///
    /// Returns [`enet::error::BadParameter`] if one of the host settings was invalid:
    /// - If [`enet::HostSettings::channel_limit`] is equal to `0`.
    /// - If [`enet::HostSettings::incoming_bandwidth_limit`] is equal to `Some(0)`.
    /// - If [`enet::HostSettings::outgoing_bandwidth_limit`] is equal to `Some(0)`.
    /// - If [`enet::HostSettings::peer_limit`] is equal to `0` or greater than
    /// [`enet::consts::PROTOCOL_MAXIMUM_PEER_ID`].
    pub fn new(settings: enet::HostSettings) -> Result<Self, enet::error::BadParameter> {
        let mut peers = Vec::new();
        peers.reserve_exact(settings.peer_limit);
        for index in 0..settings.peer_limit {
            peers.push(ConnectionPeer {
                id: ConnectionID(index),
                state: ConnectionPeerState::Disconnected {
                    last_peer_ptr: None,
                },
            });
        }
        Ok(Self {
            host: enet::Host::new(enet::ReadWrite::new(), settings).map_err(|err| match err {
                enet::error::HostNewError::BadParameter(err) => err,
                enet::error::HostNewError::FailedToInitializeSocket(..) => unreachable!(),
            })?,
            peers,
            next_generation: 0,
        })
    }

    /// Adds a connection for this host to manage. The connection should already be established on
    /// both sides.
    ///
    /// See [`ConnectionKind`] for more information on how connections are established.
    ///
    /// # Errors
    ///
    /// Returns [`error::AddConnectionError::NoAvailablePeers`] if the connection kind was
    /// [`ConnectionKind::Initiator`], but there were no peer slots available for the connection.
    ///
    /// Returns [`error::AddConnectionError::FailedToInitializeConnection`] with the connection
    /// error if the call to [`Connection::init`] fails.
    pub fn add_connection(
        &mut self,
        kind: ConnectionKind<C>,
    ) -> Result<&mut ConnectionPeer<C>, error::AddConnectionError<C>> {
        let mut connection_id = None;
        for (index, peer) in self.peers.iter().enumerate() {
            if let ConnectionPeerState::Disconnected { .. } = &peer.state {
                connection_id = Some(ConnectionID(index));
                break;
            }
        }
        let Some(connection_id) = connection_id else {
            return Err(error::AddConnectionError::NoAvailablePeers);
        };
        match kind {
            ConnectionKind::Initiator {
                mut connection,
                channel_count,
                data,
            } => {
                let generation = self.next_generation;
                self.next_generation += 1;
                let address = ConnectionAddress::new(
                    connection.init().map_err(|err| {
                        error::AddConnectionError::FailedToInitializeConnection(err)
                    })?,
                    connection_id,
                    generation,
                );
                if let Ok(peer_ptr) = self.host.connect(address.clone(), channel_count, data) {
                    self.peers[connection_id.0].state = ConnectionPeerState::HasPeer {
                        connection,
                        address,
                        peer_ptr,
                    };
                } else {
                    return Err(error::AddConnectionError::NoAvailablePeers);
                }
            }
            ConnectionKind::Receiver {
                mut connection,
                timeout,
            } => {
                let generation = self.next_generation;
                self.next_generation += 1;
                let address = ConnectionAddress::new(
                    connection.init().map_err(|err| {
                        error::AddConnectionError::FailedToInitializeConnection(err)
                    })?,
                    connection_id,
                    generation,
                );
                self.peers[connection_id.0].state = ConnectionPeerState::AwaitingPeer {
                    connection,
                    address,
                    since: self.host.now(),
                    timeout,
                    settings: ConnectionPeerSettings::default(),
                };
            }
        }
        Ok(self.peer_mut(connection_id))
    }

    fn handle_event(&mut self, event: enet::EventNoRef) -> Event<C> {
        let now = self.host.now();
        match event {
            enet::EventNoRef::Connect { peer, data } => {
                let peer = self.host.peer_mut(peer);
                let peer_ptr = peer as *mut _;
                let connection = peer
                    .address()
                    .expect("Peer should have an address.")
                    .connection_id();
                let connection_peer = self.peer_mut(connection);
                connection_peer.state = match replace(
                    &mut connection_peer.state,
                    ConnectionPeerState::Disconnected {
                        last_peer_ptr: None,
                    },
                ) {
                    ConnectionPeerState::AwaitingPeer {
                        connection,
                        address,
                        settings,
                        ..
                    } => {
                        unsafe { settings.apply::<C>(&mut *peer_ptr) };
                        ConnectionPeerState::HasPeer {
                            connection,
                            address,
                            peer_ptr,
                        }
                    }
                    ConnectionPeerState::HasPeer {
                        connection,
                        address,
                        peer_ptr,
                    } => ConnectionPeerState::HasPeer {
                        connection,
                        address,
                        peer_ptr,
                    },
                    _ => unreachable!(),
                };
                Event::Connect {
                    peer: self.peer_mut(connection),
                    data,
                }
            }
            enet::EventNoRef::Disconnect { peer, data } => {
                let peer = self.host.peer_mut(peer);
                let connection_id = peer
                    .address()
                    .expect("Peer should have an address.")
                    .connection_id();
                let peer = self.peer_mut(connection_id);
                peer.state = match replace(
                    &mut peer.state,
                    ConnectionPeerState::Disconnected {
                        last_peer_ptr: None,
                    },
                ) {
                    ConnectionPeerState::AwaitingPeer {
                        connection,
                        address,
                        ..
                    } => ConnectionPeerState::Disconnecting {
                        connection,
                        address,
                        last_peer_ptr: None,
                        last_send: now,
                    },
                    ConnectionPeerState::HasPeer {
                        connection,
                        address,
                        peer_ptr,
                        ..
                    } => ConnectionPeerState::Disconnecting {
                        connection,
                        address,
                        last_peer_ptr: Some(peer_ptr),
                        last_send: now,
                    },
                    _ => unreachable!(),
                };
                Event::Disconnect {
                    peer: self.peer_mut(connection_id),
                    data,
                }
            }
            enet::EventNoRef::Receive {
                peer,
                channel_id,
                packet,
            } => {
                let peer = self.host.peer_mut(peer);
                let connection = peer
                    .address()
                    .expect("Peer should have an address.")
                    .connection_id();
                Event::Receive {
                    peer: self.peer_mut(connection),
                    channel_id,
                    packet,
                }
            }
        }
    }

    /// Checks for any queued events on the host and dispatches one if available.
    pub fn check_events(&mut self) -> Option<Event<C>> {
        #[allow(clippy::option_if_let_else)]
        match self.host.check_events() {
            Some(event) => {
                let event = event.no_ref();
                Some(self.handle_event(event))
            }
            None => None,
        }
    }

    /// Checks for events on the host and shuttles packets between the host and its peers.
    ///
    /// Should be called fairly regularly for adequate performance.
    pub fn service(&mut self) -> Option<Event<C>> {
        let now = self.host.now();
        let mut disconnect_event = None;
        for peer in &mut self.peers {
            let mut disconnect = false;
            if let ConnectionPeerState::AwaitingPeer { since, timeout, .. } = &mut peer.state {
                if *since < now - *timeout {
                    peer.state = ConnectionPeerState::Disconnected {
                        last_peer_ptr: None,
                    };
                    disconnect_event = Some(peer.id);
                    break;
                }
            }
            if let ConnectionPeerState::Disconnecting {
                last_send,
                last_peer_ptr,
                ..
            } = &mut peer.state
            {
                if *last_send < now - Duration::from_secs(2) {
                    peer.state = ConnectionPeerState::Disconnected {
                        last_peer_ptr: last_peer_ptr.take(),
                    };
                }
            }
            match &mut peer.state {
                ConnectionPeerState::Disconnected { .. } => {}
                ConnectionPeerState::AwaitingPeer {
                    connection,
                    address,
                    ..
                }
                | ConnectionPeerState::HasPeer {
                    connection,
                    address,
                    ..
                }
                | ConnectionPeerState::Disconnecting {
                    connection,
                    address,
                    ..
                } => {
                    let mut buffer = [0; enet::MTU_MAX];
                    match connection.receive(&mut buffer) {
                        Ok(Some(enet::PacketReceived::Complete(size))) => {
                            self.host
                                .socket_mut()
                                .write(address.clone(), (buffer[0..size]).to_vec());
                        }
                        Err(_) => {
                            disconnect = true;
                        }
                        _ => {}
                    }
                }
            }
            if disconnect {
                peer.disconnect(0);
            }
        }
        if let Some(connection) = disconnect_event {
            return Some(Event::Disconnect {
                peer: self.peer_mut(connection),
                data: 0,
            });
        }
        while let Some((packet_address, packet)) = self.host.socket_mut().read() {
            if let Some(peer) = self.peers.get_mut(packet_address.id.0) {
                let mut disconnect = false;
                if let ConnectionPeer {
                    state:
                        ConnectionPeerState::AwaitingPeer {
                            connection,
                            address,
                            ..
                        }
                        | ConnectionPeerState::HasPeer {
                            connection,
                            address,
                            ..
                        },
                    ..
                } = peer
                {
                    if packet_address.generation == address.generation
                        && connection.send(&packet).is_err()
                    {
                        disconnect = true;
                    }
                }
                if let ConnectionPeer {
                    state:
                        ConnectionPeerState::Disconnecting {
                            connection,
                            address,
                            last_send,
                            ..
                        },
                    ..
                } = peer
                {
                    if packet_address.generation == address.generation
                        && connection.send(&packet).is_ok()
                    {
                        *last_send = now;
                    }
                }
                if disconnect {
                    peer.disconnect(0);
                }
            }
        }
        match self.host.service() {
            Ok(Some(event)) => {
                let event = event.no_ref();
                Some(self.handle_event(event))
            }
            Ok(None) => None,
            Err(_) => {
                unreachable!()
            }
        }
    }

    /// See [`enet::Host::flush`].
    pub fn flush(&mut self) {
        self.host.flush();
    }

    /// See [`enet::Host::peer_limit`].
    #[must_use]
    pub fn peer_limit(&self) -> usize {
        self.host.peer_limit()
    }

    /// Get a reference to a single peer, if it exists.
    ///
    /// # Note
    ///
    /// Acquires the peer object, even if the peer is not in a connected state.
    ///
    /// # Panics
    ///
    /// Panics if the connection ID is outside the bounds of peers allocated for this host. Use
    /// [`ConnectionHost::get_peer`] for a non-panicking version.
    #[must_use]
    pub fn peer(&self, connection: ConnectionID) -> &ConnectionPeer<C> {
        self.peers
            .get(connection.0)
            .expect("Expected the connection id to be in bounds.")
    }

    /// Get a reference to a single peer.
    ///
    /// # Note
    ///
    /// Acquires the peer object, even if the peer is not in a connected state.
    #[must_use]
    pub fn get_peer(&self, connection: ConnectionID) -> Option<&ConnectionPeer<C>> {
        self.peers.get(connection.0)
    }

    /// Get a mutable reference to a single peer.
    ///
    /// # Note
    ///
    /// Acquires the peer object, even if the peer is not in a connected state.
    ///
    /// # Panics
    ///
    /// Panics if the connection ID is outside the bounds of peers allocated for this host. Use
    /// [`ConnectionHost::get_peer_mut`] for a non-panicking version.
    #[must_use]
    pub fn peer_mut(&mut self, connection: ConnectionID) -> &mut ConnectionPeer<C> {
        self.peers
            .get_mut(connection.0)
            .expect("Expected the connection id to be in bounds.")
    }

    /// Get a mutable reference to a single peer, if it exists.
    ///
    /// # Note
    ///
    /// Acquires the peer object, even if the peer is not in a connected state.
    #[must_use]
    pub fn get_peer_mut(&mut self, connection: ConnectionID) -> Option<&mut ConnectionPeer<C>> {
        self.peers.get_mut(connection.0)
    }

    /// Iterate over all peer objects.
    ///
    /// # Note
    ///
    /// Acquires the peer objects, even if the peers are not in a connected state. Use
    /// [`ConnectionHost::connected_peers`] for only connected peers.
    pub fn peers(&mut self) -> impl Iterator<Item = &ConnectionPeer<C>> {
        self.peers.iter()
    }

    /// Mutably iterate over all peer objects.
    ///
    /// # Note
    ///
    /// Acquires the peer objects, even if the peers are not in a connected state. Use
    /// [`ConnectionHost::connected_peers_mut`] for only connected peers.
    pub fn peers_mut(&mut self) -> impl Iterator<Item = &mut ConnectionPeer<C>> {
        self.peers.iter_mut()
    }

    /// Iterate over all connected peers.
    pub fn connected_peers(&mut self) -> impl Iterator<Item = &ConnectionPeer<C>> {
        self.peers.iter().filter(|peer| peer.connected())
    }

    /// Mutably iterate over all connected peers.
    pub fn connected_peers_mut(&mut self) -> impl Iterator<Item = &mut ConnectionPeer<C>> {
        self.peers.iter_mut().filter(|peer| peer.connected())
    }

    /// See [`enet::Host::broadcast`].
    pub fn broadcast(&mut self, channel_id: u8, packet: &enet::Packet) {
        self.host.broadcast(channel_id, packet);
    }

    /// See [`enet::Host::channel_limit`].
    #[must_use]
    pub fn channel_limit(&self) -> usize {
        self.host.channel_limit()
    }

    /// See [`enet::Host::set_channel_limit`].
    ///
    /// # Errors
    ///
    /// Returns [`enet::error::BadParameter`] if `channel_limit` is `0`.
    pub fn set_channel_limit(
        &mut self,
        channel_limit: usize,
    ) -> Result<(), enet::error::BadParameter> {
        self.host.set_channel_limit(channel_limit)
    }

    /// See [`enet::Host::bandwidth_limit`].
    #[must_use]
    pub fn bandwidth_limit(&self) -> (Option<u32>, Option<u32>) {
        self.host.bandwidth_limit()
    }

    /// See [`enet::Host::set_bandwidth_limit`].
    ///
    /// # Errors
    ///
    /// Returns [`enet::error::BadParameter`] if `incoming_bandwidth_limit` or
    /// `outgoing_bandwidth_limit` is `Some(0)`.
    pub fn set_bandwidth_limit(
        &mut self,
        incoming_bandwidth_limit: Option<u32>,
        outgoing_bandwidth_limit: Option<u32>,
    ) -> Result<(), enet::error::BadParameter> {
        self.host
            .set_bandwidth_limit(incoming_bandwidth_limit, outgoing_bandwidth_limit)
    }

    /// See [`enet::Host::mtu`].
    #[must_use]
    pub fn mtu(&self) -> u16 {
        self.host.mtu()
    }

    /// See [`enet::Host::set_mtu`].
    ///
    /// # Errors
    ///
    /// Returns [`enet::error::BadParameter`] if `mtu` is greater than
    /// [`enet::consts::PROTOCOL_MAXIMUM_MTU`] or less than [`enet::consts::PROTOCOL_MINIMUM_MTU`].
    pub fn set_mtu(&mut self, mtu: u16) -> Result<(), enet::error::BadParameter> {
        self.host.set_mtu(mtu)
    }
}

impl Connection for TcpStream {
    type Address = SocketAddr;
    type Error = io::Error;

    fn init(&mut self) -> Result<Self::Address, Self::Error> {
        self.set_nonblocking(true)?;
        self.set_nodelay(true)?;
        self.peer_addr()
    }

    fn send(&mut self, buffer: &[u8]) -> Result<usize, Self::Error> {
        use std::io::{ErrorKind, Write};
        match self.write(buffer) {
            Ok(sent_length) => Ok(sent_length),
            Err(err) if err.kind() == ErrorKind::WouldBlock => Ok(0),
            Err(err) => Err(err),
        }
    }

    fn receive(
        &mut self,
        buffer: &mut [u8; enet::MTU_MAX],
    ) -> Result<Option<enet::PacketReceived>, Self::Error> {
        use std::io::{ErrorKind, Read};
        match self.read(buffer) {
            Ok(recv_length) => Ok(Some(enet::PacketReceived::Complete(recv_length))),
            Err(err) if err.kind() == ErrorKind::WouldBlock => Ok(None),
            Err(err) => Err(err),
        }
    }
}
