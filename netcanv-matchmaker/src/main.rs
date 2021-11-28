//! The NetCanv matchmaker server.
//! Keeps track of open rooms and relays packets between peers.

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use nanorand::Rng;
use netcanv_protocol::matchmaker::{self as mm, Packet, PeerId, RoomId, DEFAULT_PORT};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

struct Rooms {
   occupied_room_ids: HashSet<RoomId>,
   client_rooms: HashMap<PeerId, RoomId>,
   room_clients: HashMap<RoomId, Vec<PeerId>>,
   room_hosts: HashMap<RoomId, PeerId>,
}

impl Rooms {
   /// The room ID character set. Room IDs are composed of characters picked at random from
   /// this string.
   ///
   /// This is _almost_ base32, with `I`, `0`, and `O` omitted to avoid confusion.
   /// Some fonts render `0` and `O` in a very similar way, and people often confuse the capital
   /// `I` for the lowercase `l`, even if it's not a part of a code.
   const ID_CHARSET: &'static str = "123456789ABCDEFGHJKLMNPQRSTUVWXYZ";

   fn new() -> Self {
      Self {
         occupied_room_ids: HashSet::new(),
         client_rooms: HashMap::new(),
         room_clients: HashMap::new(),
         room_hosts: HashMap::new(),
      }
   }

   /// Generates a pseudo-random room ID.
   fn generate_room_id() -> RoomId {
      let mut rng = nanorand::tls_rng();
      RoomId([(); 6].map(|_| {
         let index = rng.generate_range(0..Self::ID_CHARSET.len());
         Self::ID_CHARSET.as_bytes()[index]
      }))
   }

   /// Allocates a new, free room ID.
   ///
   /// Returns `None` if all attempts to find a free ID have failed.
   fn find_room_id(&mut self) -> Option<RoomId> {
      for _attempt in 0..50 {
         let id = Self::generate_room_id();
         if self.occupied_room_ids.insert(id) {
            self.room_clients.insert(id, Vec::new());
            return Some(id);
         }
      }
      None
   }

   /// Makes the peer with the given ID the host of this room.
   fn make_host(&mut self, room_id: RoomId, peer_id: PeerId) {
      self.room_hosts.insert(room_id, peer_id);
   }

   /// Makes the peer join the room with the given ID.
   fn join_room(&mut self, peer_id: PeerId, room_id: RoomId) {
      if let Some(room_clients) = self.room_clients.get_mut(&room_id) {
         self.client_rooms.insert(peer_id, room_id);
         room_clients.push(peer_id);
      }
   }

   /// Makes the peer quit the room with the given ID.
   fn quit_room(&mut self, peer_id: PeerId) {
      if let Some(room_id) = self.client_rooms.remove(&peer_id) {
         if let Some(room_clients) = self.room_clients.get_mut(&room_id) {
            if let Some(index) = room_clients.iter().position(|&id| id == peer_id) {
               room_clients.swap_remove(index);
            }
         }
      }
   }

   /// Returns the ID of the given room's host, or `None` if the room doesn't exist.
   fn host_id(&self, room_id: RoomId) -> Option<PeerId> {
      self.room_hosts.get(&room_id).cloned()
   }

   /// Returns the ID of the given peer's room, or `None` if they haven't joined a room yet.
   fn room_id(&self, peer_id: PeerId) -> Option<RoomId> {
      self.client_rooms.get(&peer_id).cloned()
   }

   /// Returns an iterator over all the peers in a given room.
   fn peers_in_room<'r>(&'r self, room_id: RoomId) -> Option<impl Iterator<Item = PeerId> + 'r> {
      Some(self.room_clients.get(&room_id)?.iter().cloned())
   }
}

struct Peers {
   occupied_peer_ids: HashSet<PeerId>,
   peer_ids: HashMap<SocketAddr, PeerId>,
   peer_streams: HashMap<PeerId, Arc<Mutex<TcpStream>>>,
}

impl Peers {
   fn new() -> Self {
      Self {
         occupied_peer_ids: HashSet::new(),
         peer_ids: HashMap::new(),
         peer_streams: HashMap::new(),
      }
   }

   /// Allocates a new peer ID for the given socket address.
   fn allocate_peer_id(
      &mut self,
      stream: Arc<Mutex<TcpStream>>,
      address: SocketAddr,
   ) -> Option<PeerId> {
      let mut rng = nanorand::tls_rng();
      for _attempt in 0..50 {
         let id = PeerId(rng.generate_range(PeerId::FIRST_PEER..=PeerId::LAST_PEER));
         if self.occupied_peer_ids.insert(id) {
            self.peer_ids.insert(address, id);
            self.peer_streams.insert(id, stream);
            return Some(id);
         }
      }
      None
   }

   /// Deallocates the peer with the given ID. New peers will be able to join with the same ID.
   fn free_peer_id(&mut self, address: SocketAddr) {
      if let Some(id) = self.peer_ids.remove(&address) {
         self.occupied_peer_ids.remove(&id);
      }
   }

   /// Returns the ID of the peer with the given socket address.
   fn peer_id(&self, address: SocketAddr) -> Option<PeerId> {
      self.peer_ids.get(&address).cloned()
   }
}

struct State {
   rooms: Rooms,
   peers: Peers,
}

impl State {
   fn new() -> Self {
      Self {
         rooms: Rooms::new(),
         peers: Peers::new(),
      }
   }
}

async fn send_packet(stream: &Mutex<TcpStream>, packet: Packet) -> anyhow::Result<()> {
   println!("-> {:?}", packet);
   let encoded = bincode::serialize(&packet)?;
   stream.lock().await.write_all(&encoded).await?;
   Ok(())
}

async fn host(
   stream: &Arc<Mutex<TcpStream>>,
   address: SocketAddr,
   state: &mut State,
) -> anyhow::Result<()> {
   let peer_id = if let Some(id) = state.peers.allocate_peer_id(Arc::clone(stream), address) {
      id
   } else {
      send_packet(&stream, Packet::Error(mm::Error::NoFreePeerIDs)).await?;
      anyhow::bail!("no more free peer IDs");
   };

   let room_id = if let Some(id) = state.rooms.find_room_id() {
      id
   } else {
      send_packet(&stream, Packet::Error(mm::Error::NoFreeRooms)).await?;
      anyhow::bail!("no more free room IDs");
   };

   state.rooms.make_host(room_id, peer_id);
   state.rooms.join_room(peer_id, room_id);
   send_packet(&stream, Packet::RoomCreated(room_id, peer_id)).await?;

   Ok(())
}

async fn join(
   stream: &Arc<Mutex<TcpStream>>,
   address: SocketAddr,
   state: &mut State,
   room_id: RoomId,
) -> anyhow::Result<()> {
   println!("joining room");

   let peer_id = if let Some(id) = state.peers.allocate_peer_id(Arc::clone(stream), address) {
      id
   } else {
      send_packet(&stream, Packet::Error(mm::Error::NoFreePeerIDs)).await?;
      anyhow::bail!("no more free peer IDs");
   };

   let host_id = if let Some(id) = state.rooms.host_id(room_id) {
      id
   } else {
      send_packet(&stream, Packet::Error(mm::Error::RoomDoesNotExist)).await?;
      anyhow::bail!("no room with the given ID");
   };

   state.rooms.join_room(peer_id, room_id);
   send_packet(&stream, Packet::HostId(host_id)).await?;

   Ok(())
}

/// Relays a packet to the peer with the given ID.
async fn relay(
   stream: &Mutex<TcpStream>,
   address: SocketAddr,
   state: &mut State,
   target_id: PeerId,
   data: Vec<u8>,
) -> anyhow::Result<()> {
   let sender_id =
      state.peers.peer_id(address).ok_or_else(|| anyhow::anyhow!("peer does not have an ID"))?;
   let room_id =
      state.rooms.room_id(sender_id).ok_or_else(|| anyhow::anyhow!("peer is not in a room"))?;

   let packet = bincode::serialize(&Packet::Relayed(sender_id, data))?;
   let mut result = Ok(());
   if target_id.is_broadcast() {
      let peers_in_room = state.rooms.peers_in_room(room_id);
      if let Some(iter) = peers_in_room {
         for peer_id in iter {
            if peer_id != sender_id {
               if let Some(stream) = state.peers.peer_streams.get(&peer_id) {
                  match stream.lock().await.write_all(&packet).await {
                     Ok(()) => (),
                     Err(error) => result = Err(error),
                  }
               }
            }
         }
      }
   } else {
      if let Some(stream) = state.peers.peer_streams.get(&target_id) {
         stream.lock().await.write_all(&packet).await?;
      } else {
         send_packet(stream, Packet::Error(mm::Error::NoSuchPeer)).await?;
      }
   }

   Ok(result?)
}

async fn handle_packet(
   stream: Arc<Mutex<TcpStream>>,
   address: SocketAddr,
   state: &Mutex<State>,
   packet: Packet,
) -> anyhow::Result<()> {
   match packet {
      Packet::Host => host(&stream, address, &mut *state.lock().await).await?,
      Packet::Join(room_id) => join(&stream, address, &mut *state.lock().await, room_id).await?,
      Packet::Relay(target_id, data) => {
         relay(&stream, address, &mut *state.lock().await, target_id, data).await?
      }

      // These ones shouldn't happen, ignore.
      Packet::RoomCreated(_room_id, _peer_id) => (),
      Packet::HostId(_host_id) => (),
      Packet::Relayed(_peer_id, _data) => (),
      Packet::Disconnected(_peer_id) => (),
      Packet::Error(_message) => (),
   }
   Ok(())
}

async fn read_packets(
   stream: Arc<Mutex<TcpStream>>,
   address: SocketAddr,
   state: &Mutex<State>,
) -> anyhow::Result<()> {
   loop {
      // This is a bit of a workaround because bincode can't read from async streams.
      let packet: Packet = {
         let mut stream = stream.lock().await;
         let packet_size = stream.read_u32().await?;
         let mut buffer = vec![0; packet_size as usize];
         stream.read_exact(&mut buffer).await?;
         drop(stream);
         bincode::deserialize(&buffer)?
      };
      println!("got packet {:?}", packet);
      handle_packet(Arc::clone(&stream), address, &state, packet).await?;
   }
}

async fn handle_connection(
   stream: TcpStream,
   address: SocketAddr,
   state: Arc<Mutex<State>>,
) -> anyhow::Result<()> {
   eprintln!("{} has connected", address);
   stream.set_nodelay(true)?;
   let stream = Arc::new(Mutex::new(stream));

   match read_packets(stream, address, &state).await {
      Ok(()) => (),
      Err(error) => eprintln!("[{}] connection error: {}", address, error),
   }

   eprintln!("tearing down {}'s connection", address);
   {
      let mut state = state.lock().await;
      let peer_id =
         state.peers.peer_id(address).ok_or_else(|| anyhow::anyhow!("peer had no ID"))?;
      state.rooms.quit_room(peer_id);
      state.peers.free_peer_id(address);
   }

   Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
   let listener = TcpListener::bind((Ipv4Addr::from([0, 0, 0, 0]), DEFAULT_PORT)).await?;
   let state = Arc::new(Mutex::new(State::new()));

   eprintln!("NetCanv Matchmaker server");
   eprintln!("listening on {}", listener.local_addr()?);

   loop {
      let (socket, address) = listener.accept().await?;
      let state = Arc::clone(&state);
      tokio::spawn(async move { handle_connection(socket, address, state).await });
   }
}
