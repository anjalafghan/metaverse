use std::{
    collections::HashMap,
    env,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures_channel::mpsc::{UnboundedSender, unbounded};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{RwLock, broadcast},
    time::interval,
};
use tokio_tungstenite::{
    accept_async,
    tungstenite::{Error as WsError, protocol::Message},
};
use uuid::Uuid;

// Connection limits and configuration
const MAX_CONNECTIONS: usize = 10000;
const MAX_MESSAGE_SIZE: usize = 1024;
const BROADCAST_BUFFER_SIZE: usize = 1024;
const PING_INTERVAL: Duration = Duration::from_secs(30);
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: String,
    pub user_id: String,
    pub room_id: String,
    pub content: String,
    pub timestamp: u64,
}

#[derive(Debug, Clone)]
pub struct Connection {
    pub id: String,
    pub user_id: String,
    pub room_id: String,
    pub tx: UnboundedSender<Message>,
    pub last_pong: std::time::Instant,
}

pub struct ChatServer {
    // Use broadcast channels for efficient message distribution
    broadcast_tx: broadcast::Sender<ChatMessage>,
    // Connection management with RwLock for better concurrent access
    connections: Arc<RwLock<HashMap<String, Connection>>>,
    // Room-based connection mapping for targeted broadcasting
    rooms: Arc<RwLock<HashMap<String, Vec<String>>>>,
    // Connection counter with atomic operations
    connection_count: Arc<AtomicUsize>,
    // Rate limiting (simplified - use redis/database in production)
    rate_limits: Arc<RwLock<HashMap<String, (u32, std::time::Instant)>>>,
}

impl ChatServer {
    pub fn new() -> Self {
        let (broadcast_tx, _) = broadcast::channel(BROADCAST_BUFFER_SIZE);

        Self {
            broadcast_tx,
            connections: Arc::new(RwLock::new(HashMap::new())),
            rooms: Arc::new(RwLock::new(HashMap::new())),
            connection_count: Arc::new(AtomicUsize::new(0)),
            rate_limits: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn add_connection(&self, connection: Connection) -> Result<(), &'static str> {
        // Check connection limits
        if self.connection_count.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
            return Err("Server at capacity");
        }

        let mut connections = self.connections.write().await;
        let mut rooms = self.rooms.write().await;

        // Add to connections
        connections.insert(connection.id.clone(), connection.clone());

        // Add to room
        rooms
            .entry(connection.room_id.clone())
            .or_insert_with(Vec::new)
            .push(connection.id.clone());

        self.connection_count.fetch_add(1, Ordering::Relaxed);

        println!(
            "Connection {} joined room {}",
            connection.id, connection.room_id
        );
        Ok(())
    }

    pub async fn remove_connection(&self, connection_id: &str) {
        let mut connections = self.connections.write().await;
        let mut rooms = self.rooms.write().await;

        if let Some(connection) = connections.remove(connection_id) {
            // Remove from room
            if let Some(room_connections) = rooms.get_mut(&connection.room_id) {
                room_connections.retain(|id| id != connection_id);
                if room_connections.is_empty() {
                    rooms.remove(&connection.room_id);
                }
            }

            self.connection_count.fetch_sub(1, Ordering::Relaxed);
            println!(
                "Connection {} left room {}",
                connection_id, connection.room_id
            );
        }
    }

    pub async fn broadcast_to_room(
        &self,
        message: &ChatMessage,
    ) -> Result<usize, Box<dyn std::error::Error>> {
        // Send through broadcast channel for efficiency
        if let Err(_) = self.broadcast_tx.send(message.clone()) {
            return Err("Broadcast channel full".into());
        }

        // Get room connections count for metrics
        let rooms = self.rooms.read().await;
        let count = rooms
            .get(&message.room_id)
            .map(|conns| conns.len())
            .unwrap_or(0);

        Ok(count)
    }

    pub async fn check_rate_limit(&self, user_id: &str) -> bool {
        let mut rate_limits = self.rate_limits.write().await;
        let now = std::time::Instant::now();

        let (count, last_reset) = rate_limits.entry(user_id.to_string()).or_insert((0, now));

        // Reset counter every minute
        if now.duration_since(*last_reset) > Duration::from_secs(60) {
            *count = 0;
            *last_reset = now;
        }

        // Allow 60 messages per minute
        if *count >= 60 {
            false
        } else {
            *count += 1;
            true
        }
    }

    pub async fn start_health_check(&self) {
        let connections = self.connections.clone();
        let mut interval = interval(PING_INTERVAL);

        tokio::spawn(async move {
            loop {
                interval.tick().await;
                let mut to_remove = Vec::new();

                {
                    let conns = connections.read().await;
                    for (id, conn) in conns.iter() {
                        if conn.last_pong.elapsed() > CONNECTION_TIMEOUT {
                            to_remove.push(id.clone());
                        } else {
                            // Send ping
                            if let Err(_) = conn.tx.unbounded_send(Message::Ping(vec![].into())) {
                                to_remove.push(id.clone());
                            }
                        }
                    }
                }

                // Remove stale connections
                if !to_remove.is_empty() {
                    let mut conns = connections.write().await;
                    for id in to_remove {
                        conns.remove(&id);
                    }
                }
            }
        });
    }
}

async fn handle_connection(
    server: Arc<ChatServer>,
    raw_stream: TcpStream,
    addr: std::net::SocketAddr,
) -> Result<(), Box<dyn std::error::Error>> {
    // Upgrade to WebSocket
    let ws_stream = accept_async(raw_stream).await?;
    println!("WebSocket connection established: {}", addr);

    let (ws_sender, mut ws_receiver) = ws_stream.split();
    let (tx, rx) = unbounded::<Message>();

    // Generate unique connection ID
    let connection_id = Uuid::new_v4().to_string();
    let user_id = format!("user_{}", connection_id[..8].to_string());
    let room_id = "general".to_string(); // Default room

    // Create connection
    let connection = Connection {
        id: connection_id.clone(),
        user_id: user_id.clone(),
        room_id: room_id.clone(),
        tx,
        last_pong: std::time::Instant::now(),
    };

    // Add to server
    if let Err(e) = server.add_connection(connection).await {
        return Err(e.into());
    }

    // Subscribe to broadcast channel
    let mut broadcast_rx = server.broadcast_tx.subscribe();
    let server_clone = server.clone();
    let connection_id_clone = connection_id.clone();

    // Handle outgoing messages (to client)
    let outgoing_task = {
        let server = server_clone.clone();
        let connection_id = connection_id_clone.clone();
        let room_id = room_id.clone();
        let user_id = user_id.clone();

        tokio::spawn(async move {
            let mut rx = rx;
            let mut ws_sender = ws_sender;

            loop {
                tokio::select! {
                    // Direct messages from this connection's queue
                    msg = rx.next() => {
                        if let Some(msg) = msg {
                            if let Err(_) = ws_sender.send(msg).await {
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                    // Broadcast messages for this room
                    msg = broadcast_rx.recv() => {
                        if let Ok(chat_msg) = msg {
                            if chat_msg.room_id == room_id && chat_msg.user_id != user_id {
                                let ws_msg = Message::Text(serde_json::to_string(&chat_msg).unwrap().into());
                                if let Err(_) = ws_sender.send(ws_msg).await {
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            server.remove_connection(&connection_id).await;
        })
    };

    // Handle incoming messages (from client)
    let incoming_task = {
        let server = server_clone.clone();
        let user_id = user_id.clone();
        let room_id = room_id.clone();
        let connection_id = connection_id.clone();

        tokio::spawn(async move {
            while let Some(msg) = ws_receiver.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        // Rate limiting
                        if !server.check_rate_limit(&user_id).await {
                            continue;
                        }

                        // Size limiting
                        if text.len() > MAX_MESSAGE_SIZE {
                            continue;
                        }

                        // Create chat message
                        let chat_message = ChatMessage {
                            id: Uuid::new_v4().to_string(),
                            user_id: user_id.clone(),
                            room_id: room_id.clone(),
                            content: text.to_string(),
                            timestamp: std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap()
                                .as_secs(),
                        };

                        // Broadcast to room
                        if let Err(e) = server.broadcast_to_room(&chat_message).await {
                            println!("Broadcast error: {}", e);
                        }
                    }
                    Ok(Message::Pong(_)) => {
                        // Update last pong time
                        let mut connections = server.connections.write().await;
                        if let Some(conn) = connections.get_mut(&connection_id) {
                            conn.last_pong = std::time::Instant::now();
                        }
                    }
                    Ok(Message::Close(_)) => break,
                    Err(e) => {
                        println!("WebSocket error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }
        })
    };

    // Wait for either task to complete
    tokio::select! {
        _ = outgoing_task => {},
        _ = incoming_task => {},
    }

    server.remove_connection(&connection_id).await;
    println!("Connection {} disconnected", connection_id);
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:8080".to_string());

    let server = Arc::new(ChatServer::new());

    // Start health check
    server.start_health_check().await;

    // Create TCP listener
    let listener = TcpListener::bind(&addr).await?;
    println!("Scalable chat server listening on: {}", addr);

    // Accept connections
    while let Ok((stream, addr)) = listener.accept().await {
        let server = server.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(server, stream, addr).await {
                println!("Connection error: {}", e);
            }
        });
    }

    Ok(())
}
