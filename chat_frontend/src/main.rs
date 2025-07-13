use std::env;

use futures_util::{StreamExt, future, pin_mut};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: String,
    pub user_id: String,
    pub room_id: String,
    pub content: String,
    pub timestamp: u64,
}

#[tokio::main]
async fn main() {
    let url = env::args()
        .nth(1)
        .unwrap_or_else(|| "ws://127.0.0.1:8080".to_string());

    println!("Connecting to: {}", url);

    let (stdin_tx, stdin_rx) = futures_channel::mpsc::unbounded();
    tokio::spawn(read_stdin(stdin_tx));

    let (ws_stream, _) = connect_async(&url).await.expect("Failed to connect");
    println!("✅ Connected to chat server!");
    println!("Type messages and press Enter to send. Type 'quit' to exit.");

    let (write, read) = ws_stream.split();

    // Handle incoming messages from server
    let ws_to_stdout = {
        read.for_each(|message| async {
            match message {
                Ok(Message::Text(text)) => {
                    // Try to parse as ChatMessage
                    if let Ok(chat_msg) = serde_json::from_str::<ChatMessage>(&text) {
                        println!(
                            "\n[{}] {}: {}",
                            format_timestamp(chat_msg.timestamp),
                            chat_msg.user_id,
                            chat_msg.content
                        );
                    } else {
                        println!("📨 {}", text);
                    }
                }
                Ok(Message::Binary(data)) => {
                    if let Ok(text) = String::from_utf8(Vec::from(data)) {
                        println!("📨 {}", text);
                    }
                }
                Ok(Message::Ping(_)) => {
                    // Server sent ping, client should respond with pong
                    // (usually handled automatically by the library)
                }
                Ok(Message::Close(_)) => {
                    println!("🔌 Connection closed by server");
                }
                Err(e) => {
                    println!("❌ Error: {}", e);
                }
                _ => {}
            }
        })
    };

    // Handle outgoing messages to server
    let stdin_to_ws = stdin_rx.map(Ok).forward(write);

    pin_mut!(stdin_to_ws, ws_to_stdout);
    future::select(stdin_to_ws, ws_to_stdout).await;

    println!("👋 Goodbye!");
}

async fn read_stdin(tx: futures_channel::mpsc::UnboundedSender<Message>) {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();

    loop {
        line.clear();
        print!("💬 ");
        tokio::io::stdout().flush().await.unwrap();

        match reader.read_line(&mut line).await {
            Ok(0) => break, // EOF
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed == "quit" || trimmed == "exit" {
                    break;
                }
                if !trimmed.is_empty() {
                    if let Err(e) = tx.unbounded_send(Message::Text(trimmed.to_string().into())) {
                        println!("❌ Failed to send: {}", e);
                        break;
                    }
                }
            }
            Err(e) => {
                println!("❌ Error reading input: {}", e);
                break;
            }
        }
    }
}

fn format_timestamp(timestamp: u64) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let diff = now.saturating_sub(timestamp);

    if diff < 60 {
        "now".to_string()
    } else if diff < 3600 {
        format!("{}m ago", diff / 60)
    } else if diff < 86400 {
        format!("{}h ago", diff / 3600)
    } else {
        format!("{}d ago", diff / 86400)
    }
}
