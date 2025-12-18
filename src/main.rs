//! Native CLI for walkie-songie P2P testing.
//! The web app uses wasm_bindgen(start) in src/web/app.rs instead.

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use anyhow::Result;
    use matchbox_socket::{PeerState, WebRtcSocket};
    use tracing::info;

    use walkie_songie::words::generate_room_name;

    /// Matchbox signaling server (same as web and plugin)
    const SIGNALING_SERVER: &str = "wss://matchbox.wondering.xyz";

    pub async fn run() -> Result<()> {
        tracing_subscriber::fmt::init();

        // Get room name from args or generate one
        let room_name = std::env::args().nth(1).unwrap_or_else(generate_room_name);

        info!("Creating matchbox connection to room: {}", room_name);

        // Build WebRTC socket with matchbox signaling
        let signaling_url = format!("{}/{}", SIGNALING_SERVER, room_name);
        let (socket, loop_fut) = WebRtcSocket::builder(&signaling_url)
            .add_reliable_channel()
            .build();

        // Spawn the socket event loop
        let loop_handle = tokio::spawn(loop_fut);

        println!("\n=== Walkie Songie ===");
        println!("Room: {}", room_name);
        println!("\nWaiting for peers...\n");

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
}

#[cfg(not(target_arch = "wasm32"))]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    native::run().await
}

#[cfg(target_arch = "wasm32")]
fn main() {
    // Web app entry point is in src/web/app.rs via wasm_bindgen(start)
}
