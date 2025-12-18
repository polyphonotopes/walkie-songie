use std::sync::Arc;

use anyhow::Result;
use matchbox_socket::{PeerState, WebRtcSocket};
use tracing::info;
use walkie_songie::net;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let bootstrap_peer = std::env::args().nth(1).unwrap_or_default();

    info!("Creating iroh signaller...");
    let signaller = net::create_signaller().await?;

    info!("Iroh node ID: {}", signaller.iroh_id());
    info!("Matchbox peer ID: {}", signaller.matchbox_id());

    println!("\n=== Walkie Songie ===");
    println!("Share this ID to connect: {}", signaller.iroh_id());
    if !bootstrap_peer.is_empty() {
        println!("Connecting to bootstrap peer: {bootstrap_peer}");
    }
    println!("\nWaiting for peers...\n");

    // Create matchbox socket with our custom signaller
    let (socket, loop_fut) = WebRtcSocket::builder(&bootstrap_peer)
        .signaller_builder(Arc::new(signaller))
        .add_reliable_channel()
        .build();

    // Spawn the socket event loop
    let loop_handle = tokio::spawn(loop_fut);

    // Monitor peer connections
    let mut socket = socket;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("\nShutting down...");
                break;
            }
            _ = tokio::time::sleep(tokio::time::Duration::from_millis(100)) => {
                // Check for peer updates
                for (peer_id, state) in socket.update_peers() {
                    match state {
                        PeerState::Connected => {
                            info!("Peer connected: {peer_id}");
                        }
                        PeerState::Disconnected => {
                            info!("Peer disconnected: {peer_id}");
                        }
                    }
                }
            }
        }
    }

    loop_handle.abort();
    Ok(())
}
