//! In-process [`Transport`] pair — `TransportMode::Loopback`.
//!
//! Exists so the sync driver and the room event loop are testable with no
//! sockets, no multicast, and no `#[ignore]`. Before this, the only end-to-end
//! exercise of gossip + anti-entropy needed real UDP and multicast, so CI never
//! ran it.
//!
//! Built on `async-channel`, which works on every target the crate supports
//! (including wasm), so this module is never cfg-gated.

use async_channel::{Receiver, Sender, unbounded};

use super::{
    LaneProtocol, PeerId, SyncStream, Transport, TransportError, TransportEvent, TransportMode,
};
use crate::client::{DiscoverySource, PeerPath};

/// One end of an in-process framed duplex.
#[derive(Debug)]
pub struct LoopbackStream {
    outbound: Sender<Vec<u8>>,
    inbound: Receiver<Vec<u8>>,
}

impl LoopbackStream {
    /// Two ends wired to each other.
    fn pair() -> (Self, Self) {
        let (left_tx, right_rx) = unbounded();
        let (right_tx, left_rx) = unbounded();
        (
            Self {
                outbound: left_tx,
                inbound: left_rx,
            },
            Self {
                outbound: right_tx,
                inbound: right_rx,
            },
        )
    }
}

impl SyncStream for LoopbackStream {
    async fn send_frame(&mut self, frame: &[u8]) -> Result<(), TransportError> {
        self.outbound
            .send(frame.to_vec())
            .await
            .map_err(|_| TransportError::Closed)
    }

    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        // A closed-and-drained channel is a clean end of stream, not an error.
        Ok(self.inbound.recv().await.ok())
    }

    async fn close(self) {
        self.outbound.close();
        self.inbound.close();
    }
}

/// One half of a connected pair.
#[derive(Debug)]
pub struct LoopbackTransport {
    me: PeerId,
    peer: PeerId,
    to_peer: Sender<TransportEvent<LoopbackStream>>,
    inbox: Receiver<TransportEvent<LoopbackStream>>,
}

impl LoopbackTransport {
    /// This end's own identity.
    pub fn peer_id(&self) -> PeerId {
        self.me
    }

    /// The identity of the peer at the other end.
    pub fn remote_id(&self) -> PeerId {
        self.peer
    }
}

/// Two transports wired to each other, each already holding a `PeerUp` for the
/// other so the driver's "a peer appeared, start anti-entropy" path is exercised.
pub fn loopback_pair() -> (LoopbackTransport, LoopbackTransport) {
    loopback_pair_with_ids(PeerId([0xa1; 32]), PeerId([0xb2; 32]))
}

pub fn loopback_pair_with_ids(
    left_id: PeerId,
    right_id: PeerId,
) -> (LoopbackTransport, LoopbackTransport) {
    let (left_tx, left_rx) = unbounded();
    let (right_tx, right_rx) = unbounded();

    // Seed each inbox with the other's arrival.
    let _ = left_tx.try_send(TransportEvent::PeerUp {
        peer: right_id,
        discovery: DiscoverySource::Gossip,
    });
    let _ = right_tx.try_send(TransportEvent::PeerUp {
        peer: left_id,
        discovery: DiscoverySource::Gossip,
    });

    (
        LoopbackTransport {
            me: left_id,
            peer: right_id,
            to_peer: right_tx,
            inbox: left_rx,
        },
        LoopbackTransport {
            me: right_id,
            peer: left_id,
            to_peer: left_tx,
            inbox: right_rx,
        },
    )
}

impl Transport for LoopbackTransport {
    type Stream = LoopbackStream;

    fn mode(&self) -> TransportMode {
        TransportMode::Loopback
    }

    fn max_broadcast_bytes(&self) -> usize {
        usize::MAX
    }

    async fn broadcast(&self, frame: Vec<u8>) -> Result<(), TransportError> {
        self.to_peer
            .send(TransportEvent::Message {
                from: self.me,
                bytes: frame,
            })
            .await
            .map_err(|_| TransportError::Closed)
    }

    async fn next_event(&mut self) -> Option<TransportEvent<Self::Stream>> {
        self.inbox.recv().await.ok()
    }

    async fn open_lane(
        &self,
        peer: PeerId,
        protocol: LaneProtocol,
    ) -> Result<Self::Stream, TransportError> {
        if peer != self.peer {
            return Err(TransportError::Unreachable(peer.to_hex()));
        }
        let (mine, theirs) = LoopbackStream::pair();
        self.to_peer
            .send(TransportEvent::LaneRequested {
                peer: self.me,
                protocol,
                stream: theirs,
            })
            .await
            .map_err(|_| TransportError::Closed)?;
        Ok(mine)
    }

    async fn peer_path(&self, peer: PeerId) -> PeerPath {
        if peer == self.peer {
            PeerPath::Direct
        } else {
            PeerPath::Disconnected
        }
    }

    async fn shutdown(self) -> Result<(), TransportError> {
        self.to_peer.close();
        self.inbox.close();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;

    #[test]
    fn pair_reports_each_other_up_then_carries_a_broadcast() {
        let (mut a, mut b) = loopback_pair();
        block_on(async {
            assert!(matches!(
                a.next_event().await,
                Some(TransportEvent::PeerUp { peer, .. }) if peer == b.peer_id()
            ));
            assert!(matches!(
                b.next_event().await,
                Some(TransportEvent::PeerUp { peer, .. }) if peer == a.peer_id()
            ));

            a.broadcast(b"hello".to_vec()).await.unwrap();
            match b.next_event().await {
                Some(TransportEvent::Message { from, bytes }) => {
                    assert_eq!(from, a.peer_id());
                    assert_eq!(bytes, b"hello");
                }
                other => panic!("expected Message, got {other:?}"),
            }
            assert_eq!(a.peer_path(b.peer_id()).await, PeerPath::Direct);
            assert_eq!(a.mode(), TransportMode::Loopback);
        });
    }

    #[test]
    fn open_lane_delivers_the_protocol_and_far_end_then_frames_round_trip() {
        let (a, mut b) = loopback_pair();
        block_on(async {
            let _ = b.next_event().await; // drain PeerUp
            let protocol = LaneProtocol::Repair(crate::room::v4::RoomLane::Music);
            let mut mine = a.open_lane(b.peer_id(), protocol).await.unwrap();
            let mut theirs = match b.next_event().await {
                Some(TransportEvent::LaneRequested {
                    protocol: requested,
                    stream,
                    ..
                }) => {
                    assert_eq!(requested, protocol);
                    stream
                }
                other => panic!("expected LaneRequested, got {other:?}"),
            };

            mine.send_frame(b"ping").await.unwrap();
            assert_eq!(
                theirs.recv_frame().await.unwrap().as_deref(),
                Some(&b"ping"[..])
            );
            theirs.send_frame(b"pong").await.unwrap();
            assert_eq!(
                mine.recv_frame().await.unwrap().as_deref(),
                Some(&b"pong"[..])
            );

            theirs.close().await;
            assert_eq!(mine.recv_frame().await.unwrap(), None, "clean EOF");
        });
    }

    #[test]
    fn open_lane_rejects_an_unknown_peer() {
        let (a, _b) = loopback_pair();
        block_on(async {
            assert!(matches!(
                a.open_lane(
                    PeerId([0xff; 32]),
                    LaneProtocol::Repair(crate::room::v4::RoomLane::Music),
                )
                .await,
                Err(TransportError::Unreachable(_))
            ));
        });
    }
}
