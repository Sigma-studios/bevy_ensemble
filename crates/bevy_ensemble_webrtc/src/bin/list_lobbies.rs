use bevy_ensemble_webrtc::protocol::{ClientMessage, ServerMessage, decode, encode};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let url = std::env::var("SERVER_URL").unwrap_or_else(|_| "ws://localhost:9090/ws".into());
    eprintln!("Connecting to {url} ...");

    let (mut ws, _) = match tokio_tungstenite::connect_async(&url).await {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("Failed to connect to {url}: {e}");
            std::process::exit(1);
        }
    };

    let auth = encode(&ClientMessage::Authenticate {
        display_name: "lobby-lister".into(),
    })
    .expect("failed to encode");
    ws.send(Message::Binary(auth.into())).await.unwrap();

    // Wait for Welcome
    loop {
        let Some(Ok(msg)) = ws.next().await else {
            eprintln!("Connection closed before Welcome");
            std::process::exit(1);
        };
        let Message::Binary(bytes) = msg else { continue };
        match decode::<ServerMessage>(&bytes) {
            Ok(ServerMessage::Welcome { player_uuid }) => {
                eprintln!("Authenticated (uuid: {player_uuid})");
                break;
            }
            Ok(other) => {
                eprintln!("Unexpected message: {other:?}");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("Failed to decode: {e}");
                std::process::exit(1);
            }
        }
    }

    let list = encode(&ClientMessage::ListLobbies).expect("failed to encode");
    ws.send(Message::Binary(list.into())).await.unwrap();

    // Wait for LobbyList
    loop {
        let Some(Ok(msg)) = ws.next().await else {
            eprintln!("Connection closed before LobbyList");
            std::process::exit(1);
        };
        let Message::Binary(bytes) = msg else { continue };
        match decode::<ServerMessage>(&bytes) {
            Ok(ServerMessage::LobbyList { lobbies }) => {
                if lobbies.is_empty() {
                    println!("No lobbies.");
                } else {
                    println!("{:<6} {:<6} {:<20} {}", "ID", "CODE", "HOST", "PLAYERS");
                    for l in &lobbies {
                        println!(
                            "{:<6} {:<6} {:<20} {}/{}",
                            l.lobby_id, l.code, l.host_name, l.player_count, l.max_players
                        );
                    }
                }
                break;
            }
            Ok(other) => {
                eprintln!("Unexpected message while waiting for lobby list: {other:?}");
            }
            Err(e) => {
                eprintln!("Failed to decode: {e}");
            }
        }
    }

    ws.close(None).await.ok();
}
