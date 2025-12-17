//! Direct message protocol for exchanging WebRTC signalling data between specific peers.

use anyhow::Result;
use iroh::{endpoint::Connection, protocol::ProtocolHandler, Endpoint, PublicKey};
use matchbox_socket::PeerEvent;

/// ALPN identifier for our direct message protocol.
pub const DIRECT_MESSAGE_ALPN: &[u8] = b"/walkie-songie/dm/0";

/// Protocol handler for receiving direct messages from other peers.
#[derive(Debug, Clone)]
pub struct DirectMessageHandler {
    pub sender: async_broadcast::Sender<(PublicKey, PeerEvent)>,
}

impl DirectMessageHandler {
    pub fn new(sender: async_broadcast::Sender<(PublicKey, PeerEvent)>) -> Self {
        Self { sender }
    }

    async fn handle_connection(self, conn: Connection) -> Result<()> {
        let remote_id = conn.remote_node_id()?;

        // Accept incoming unidirectional stream
        let mut recv_stream = conn.accept_uni().await?;
        let data = recv_stream.read_to_end(64 * 1024).await?;
        conn.close(0u8.into(), b"done");

        // Deserialize the peer event
        let event: PeerEvent = serde_json::from_slice(&data)?;

        // Only accept Signal events via direct message
        if matches!(&event, PeerEvent::Signal { .. }) {
            let _ = self.sender.broadcast((remote_id, event)).await;
        }

        Ok(())
    }
}

impl ProtocolHandler for DirectMessageHandler {
    fn accept(&self, conn: Connection) -> n0_future::boxed::BoxFuture<Result<()>> {
        Box::pin(self.clone().handle_connection(conn))
    }
}

/// Send a direct message to a specific peer.
pub async fn send_direct_message(
    endpoint: &Endpoint,
    target: PublicKey,
    event: PeerEvent,
) -> Result<()> {
    let conn = endpoint.connect(target, DIRECT_MESSAGE_ALPN).await?;
    let payload = serde_json::to_vec(&event)?;

    let mut send_stream = conn.open_uni().await?;
    send_stream.write_all(&payload).await?;
    send_stream.finish()?;

    conn.closed().await;
    Ok(())
}
