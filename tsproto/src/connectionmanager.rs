use std::cell::RefCell;
use std::mem;
use std::net::SocketAddr;
use std::rc::{Rc, Weak};

use futures::{future, Future, Sink};
use slog::Logger;
use tokio_core::reactor::Handle;

use {Error, Map};
use connection::Connection;
use handler_data::Data;
use packets::{PacketType, UdpPacket};
use resend::{DefaultResender, ResendConfig, ResendFuture};

/// Implementers of this trait store all connections for a specific socket.
///
/// The unique identification of a connection is handled by the implementation.
pub trait ConnectionManager: Sized {
    /// The type that is used to send and resend command packets.
    ///
    /// A default implementation is provided as [`resend::DefaultResender`].
    ///
    /// [`resend::DefaultResender`]:
    type Resend: Resender;

    /// A unique identifier for each connection.
    ///
    /// It should be lightweight enough to be cloned.
    type ConnectionsKey: ::std::hash::Hash + Clone;

    /// Create a new resender that will be put into a new connection.
    fn create_resender(&self, logger: Logger) -> Self::Resend;

    /// Add a new connection to the list of connections.
    ///
    /// In this method, the manager can start e. g. the other part of the
    /// resender ([`DefaultResenderFuture`] in the case of [`DefaultResender`]).
    ///
    /// [`DefaultResenderFuture`]:
    /// [`DefaultResender`]:
    fn add_connection(&mut self, con: Rc<RefCell<Connection<Self>>>,
        handle: &Handle) -> Self::ConnectionsKey;
    /// Remove a connection.
    ///
    /// Returns the removed connection or `None` if there was no such
    /// connection.
    fn remove_connection(&mut self, key: Self::ConnectionsKey)
        -> Option<Rc<RefCell<Connection<Self>>>>;

    /// Get the connection object for a given key.
    fn get_connection(&self, key: Self::ConnectionsKey)
        -> Option<Rc<RefCell<Connection<Self>>>>;

    /// Find the connection for an incoming udp packet.
    fn get_connection_for_udp_packet(&self, src_addr: SocketAddr,
        udp_packet: &UdpPacket) -> Option<Self::ConnectionsKey>;
}

/// A connection manager, that allows to attach a custom data object to each
/// connection.
pub trait AttachedDataConnectionManager<T: Default>: ConnectionManager {
    /// Sets the associated data for a connection.
    ///
    /// Returns the old data if the connection exists.
    fn set_data(&mut self, key: Self::ConnectionsKey, t: T) -> Option<T>;

    /// Get the associated data for a connection.
    fn get_data(&mut self, key: Self::ConnectionsKey) -> Option<&T>;

    /// Get the associated data for a connection.
    fn get_mut_data(&mut self, key: Self::ConnectionsKey) -> Option<&mut T>;
}

/// Events to inform a resender of the current state of a connection.
#[derive(PartialEq, Eq, Debug, Hash)]
pub enum ResenderEvent {
    /// The connection is starting
    Connecting,
    /// The handshake is completed, this is the normal operation mode
    Connected,
    /// The connection is tearing down
    Disconnecting,
}

/// For each connection, a resender is created, which is responsible for sending
/// command packets and ensure, that they are delivered.
///
/// This is accomplished by implementing a sink, which takes the packet type, id
/// and the packet itself. The id must be [`Command`] or [`CommandLow`].
///
/// You should note that the resending should be implemented independant of the
/// sink, so it is possible to put two packets into the sink while no ack has
/// been received.
///
/// [`Command`]:
/// [`CommandLow`]:
pub trait Resender: Sink<SinkItem = (PacketType, u16, UdpPacket),
    SinkError = Error> {
    /// Called for a received ack packet.
    ///
    /// The packet type must be [`Command`] or [`CommandLow`].
    ///
    /// [`Command`]:
    /// [`CommandLow`]:
    fn ack_packet(&mut self, p_type: PacketType, p_id: u16);

    /// The resender can block outgoing voice packets.
    ///
    /// Return `true` to allow sending and `false` to block packets.
    fn send_voice_packets(&self, p_type: PacketType) -> bool;

    /// If there are packets in the queue which were not acknowledged.
    fn is_empty(&self) -> bool;

    /// This method informs the resender of state changes of the connection.
    fn handle_event(&mut self, event: ResenderEvent);

    /// Called for received udp packets.
    fn udp_packet_received(&mut self, packet: &UdpPacket);
}

/// An implementation of a connectionmanager, that identifies a connection its
/// socket.
///
/// `T` contains associated data that will be saved for each connection.
pub struct SocketConnectionManager<T: Default + 'static> {
    /// We need the data for the resender, so that he can remove connections
    /// which time out.
    ///
    /// As this is a circular dependency, it has to be set after the data object
    /// is created.
    data: Option<Weak<RefCell<Data<SocketConnectionManager<T>>>>>,
    resend_config: ResendConfig,
    connections: Map<SocketAddr,
        (T, Rc<RefCell<Connection<SocketConnectionManager<T>>>>)>
}

impl<T: Default + 'static> Default for SocketConnectionManager<T> {
    fn default() -> Self {
        Self {
            data: None,
            resend_config: Default::default(),
            connections: Default::default(),
        }
    }
}

impl<T: Default + 'static> SocketConnectionManager<T> {
    /// Create a new connection manager.
    ///
    /// Remember to set the data object afterwards using [`set_data_ref`].
    ///
    /// [`set_data_ref`]:
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new connection manager with custom timeouts.
    ///
    /// Remember to set the data object afterwards using [`set_data_ref`].
    ///
    /// [`set_data_ref`]:
    pub fn with_resender_config(resend_config: ResendConfig)
        -> Self {
        Self {
            resend_config,
            .. Self::new()
        }
    }

    /// Sets the data reference in this connection manager.
    pub fn set_data_ref(&mut self, data: Weak<RefCell<Data<Self>>>) {
        self.data = Some(data);
    }
}

impl<T: Default + 'static> AttachedDataConnectionManager<T> for
    SocketConnectionManager<T> {
    /// Sets the associated data for a connection.
    ///
    /// Returns the old data if the connection exists.
    fn set_data(&mut self, key: SocketAddr, t: T) -> Option<T> {
        if let Some(&mut (ref mut t_old, _)) = self.connections.get_mut(&key) {
            Some(mem::replace(t_old, t))
        } else {
            None
        }
    }

    /// Get the associated data for a connection.
    fn get_data(&mut self, key: SocketAddr) -> Option<&T> {
        self.connections.get(&key).map(|&(ref t, _)| t)
    }

    /// Get the associated data for a connection.
    fn get_mut_data(&mut self, key: SocketAddr) -> Option<&mut T> {
        self.connections.get_mut(&key).map(|&mut (ref mut t, _)| t)
    }
}

impl<T: Default + 'static> ConnectionManager for SocketConnectionManager<T> {
    type Resend = DefaultResender;
    type ConnectionsKey = SocketAddr;

    fn create_resender(&self, logger: Logger) -> Self::Resend {
        DefaultResender::new(self.resend_config.clone(), logger)
    }

    fn add_connection(&mut self, con: Rc<RefCell<Connection<Self>>>,
        handle: &Handle) -> Self::ConnectionsKey {
        let key = con.borrow().address;
        self.connections.insert(key, (Default::default(), con));

        let data = self.data.as_ref().unwrap().clone();
        handle.spawn(future::lazy(move || {
            let data_tmp = data.upgrade().unwrap();
            let resend = ResendFuture::new(&data_tmp, key);

            // Start the actual resend future
            let logger = data_tmp.borrow().logger.clone();

            resend.map_err(move |e| {
                error!(logger, "Resender exited with error"; "error" => ?e);
                // Remove connection if it exists
                if let Some(data) = data.upgrade() {
                    let mut data = data.borrow_mut();
                    data.connection_manager.remove_connection(key);
                }
            })
        }));

        key
    }

    fn remove_connection(&mut self, key: Self::ConnectionsKey)
        -> Option<Rc<RefCell<Connection<Self>>>> {
        self.connections.remove(&key).map(|(_, c)| c)
    }

    fn get_connection(&self, key: Self::ConnectionsKey)
        -> Option<Rc<RefCell<Connection<Self>>>> {
        self.connections.get(&key).map(|&(_, ref c)| c.clone())
    }

    fn get_connection_for_udp_packet(&self, src_addr: SocketAddr,
        _: &UdpPacket) -> Option<Self::ConnectionsKey> {
        if self.connections.contains_key(&src_addr) {
            Some(src_addr)
        } else {
            None
        }
    }
}
