//! Iroh transport adapter for the HHHS anti-entropy driver.
//!
//! Length-prefixed framing over one QUIC bidirectional stream, plus the
//! [`SyncStream`] impl that lets the application drive an HHHS session over it.
//!
//! Replica snapshots and budgets remain in the upstream sans-I/O driver.

use std::time::Duration;

use iroh::endpoint::{Connection, RecvStream, SendStream};
use thiserror::Error;

#[cfg(not(target_arch = "wasm32"))]
use super::SyncTimer;
use super::{SyncStream, TransportError};

/// The native runtime's clock for [`SyncTimer`].
///
/// The driver is runtime-neutral, so the deadline it enforces has to come from
/// whoever owns the runtime. On native that is tokio; the browser build
/// supplies [`super::browser::BrowserTimer`] instead.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioTimer;

#[cfg(not(target_arch = "wasm32"))]
impl SyncTimer for TokioTimer {
    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await
    }
}

/// Wire cap for one framed `SyncMessage`. Deliberately the *same* constant the
/// session budget uses, so the framing cap and the session cap cannot drift into
/// a state where a legal message is unsendable.
pub const MAX_REPAIR_FRAME_BYTES: usize = hhhs_sync::driver::DEFAULT_MAX_FRAME_BYTES;

/// Upper bound for waiting until the peer acknowledges every byte queued before
/// our stream FIN. `finish()` alone only schedules the FIN; dropping the owning
/// [`Connection`] immediately afterwards can discard the terminal sync frame.
// A repair can use an Iroh relay whose QUIC packet-loss recovery needs several
// PTO rounds. Two seconds was shorter than a healthy repair's observed FIN
// recovery under relay loss: the browser timed out, dropped the connection,
// and made the native peer report a false close failure after both HHHS sides
// had already reached terminal status. Ten seconds remains a strict carrier
// bound while allowing multiple retransmission rounds; it does not turn EOF or
// timeout into success.
const FIN_ACK_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Error)]
pub enum RepairError {
    #[error("repair stream I/O failed: {0}")]
    Io(String),
    #[error("repair frame is {actual} bytes; maximum is {max}")]
    FrameTooLarge { actual: usize, max: usize },
}

impl From<RepairError> for TransportError {
    fn from(value: RepairError) -> Self {
        match value {
            RepairError::FrameTooLarge { actual, max } => {
                TransportError::FrameTooLarge { actual, limit: max }
            }
            RepairError::Io(message) => TransportError::Backend(message),
        }
    }
}

/// One HHHS session's worth of iroh bidirectional stream.
///
/// Optionally owns the [`Connection`] it was opened on. Dropping the last
/// `Connection` handle closes the QUIC connection underneath its own streams, so
/// a dial site that builds the connection and hands back only the stream must
/// keep it alive here rather than in a caller-local variable.
#[derive(Debug)]
pub struct IrohSyncStream {
    send: SendStream,
    recv: RecvStream,
    connection: Option<Connection>,
}

impl IrohSyncStream {
    pub fn new(send: SendStream, recv: RecvStream) -> Self {
        Self {
            send,
            recv,
            connection: None,
        }
    }

    /// Keep `connection` alive for this stream's lifetime.
    pub fn owning(mut self, connection: Connection) -> Self {
        self.connection = Some(connection);
        self
    }

    /// Dial side: open a fresh bi-stream on an established connection.
    pub async fn open(connection: &Connection) -> Result<Self, TransportError> {
        let (send, recv) = connection
            .open_bi()
            .await
            .map_err(|error| TransportError::Backend(error.to_string()))?;
        Ok(Self::new(send, recv))
    }

    /// Accept side: take the bi-stream the initiator opened.
    pub async fn accept(connection: &Connection) -> Result<Self, TransportError> {
        let (send, recv) = connection
            .accept_bi()
            .await
            .map_err(|error| TransportError::Backend(error.to_string()))?;
        Ok(Self::new(send, recv))
    }
}

impl SyncStream for IrohSyncStream {
    async fn send_frame(&mut self, frame: &[u8]) -> Result<(), TransportError> {
        write_frame(&mut self.send, frame).await.map_err(Into::into)
    }

    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        read_frame(&mut self.recv).await.map_err(Into::into)
    }

    async fn close(mut self) -> Result<(), TransportError> {
        let stopped = self.send.stopped();
        self.send
            .finish()
            .map_err(|error| TransportError::Backend(error.to_string()))?;
        let send_confirm = async {
            match stopped
                .await
                .map_err(|error| TransportError::Backend(error.to_string()))?
            {
                Some(code) => Err(TransportError::Backend(format!(
                    "peer stopped the repair send stream with code {code}"
                ))),
                None => Ok(()),
            }
        };
        let receive_confirm = async {
            self.recv
                .read_to_end(0)
                .await
                .map_err(|error| TransportError::Backend(error.to_string()))?;
            Ok(())
        };
        // Both peers perform the same symmetric close. Reading the peer FIN
        // only after waiting for acknowledgement of our own FIN can deadlock:
        // each peer withholds the read which would let the other's `stopped`
        // future complete. Drive both halves concurrently under one deadline.
        let confirm = async {
            futures::try_join!(send_confirm, receive_confirm)?;
            Ok::<(), TransportError>(())
        };
        // Keep the owning connection alive until our FIN is acknowledged and
        // the peer's FIN is observed. One bounded deadline covers both halves.
        #[cfg(target_arch = "wasm32")]
        let result = n0_future::time::timeout(FIN_ACK_TIMEOUT, confirm)
            .await
            .map_err(|error| TransportError::Backend(format!("repair close timed out: {error}")))?;
        #[cfg(not(target_arch = "wasm32"))]
        let result = tokio::time::timeout(FIN_ACK_TIMEOUT, confirm)
            .await
            .map_err(|error| TransportError::Backend(format!("repair close timed out: {error}")))?;
        result
    }
}

/// Write one length-prefixed frame (`u32` big-endian length + payload).
pub async fn write_frame(stream: &mut SendStream, bytes: &[u8]) -> Result<(), RepairError> {
    if bytes.len() > MAX_REPAIR_FRAME_BYTES {
        return Err(RepairError::FrameTooLarge {
            actual: bytes.len(),
            max: MAX_REPAIR_FRAME_BYTES,
        });
    }
    stream
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .await
        .map_err(|error| RepairError::Io(error.to_string()))?;
    stream
        .write_all(bytes)
        .await
        .map_err(|error| RepairError::Io(error.to_string()))
}

/// Write several frames in order.
pub async fn write_sync_frames<'a>(
    stream: &mut SendStream,
    frames: impl IntoIterator<Item = &'a [u8]>,
) -> Result<(), RepairError> {
    for frame in frames {
        write_frame(stream, frame).await?;
    }
    Ok(())
}

/// Read one frame. `Ok(None)` is a clean end of stream; a partial length prefix
/// is an error. The length is validated *before* the buffer is allocated.
pub async fn read_frame(stream: &mut RecvStream) -> Result<Option<Vec<u8>>, RepairError> {
    let mut length = [0_u8; 4];
    let mut filled = 0;
    while filled < length.len() {
        match stream
            .read(&mut length[filled..])
            .await
            .map_err(|error| RepairError::Io(error.to_string()))?
        {
            None if filled == 0 => return Ok(None),
            None => {
                return Err(RepairError::Io(
                    "repair stream ended inside a frame length".into(),
                ));
            }
            Some(read) => filled += read,
        }
    }
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_REPAIR_FRAME_BYTES {
        return Err(RepairError::FrameTooLarge {
            actual: length,
            max: MAX_REPAIR_FRAME_BYTES,
        });
    }
    let mut bytes = vec![0_u8; length];
    stream
        .read_exact(&mut bytes)
        .await
        .map_err(|error| RepairError::Io(error.to_string()))?;
    Ok(Some(bytes))
}

/// Back-compat alias for [`read_frame`].
pub async fn read_sync_frame(stream: &mut RecvStream) -> Result<Option<Vec<u8>>, RepairError> {
    read_frame(stream).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_cap_matches_the_session_cap() {
        // A drift here would make a session-legal message unsendable.
        assert_eq!(
            MAX_REPAIR_FRAME_BYTES,
            hhhs_sync::driver::DEFAULT_MAX_FRAME_BYTES
        );
    }

    #[test]
    fn oversize_frames_map_to_a_transport_error() {
        let error: TransportError = RepairError::FrameTooLarge {
            actual: MAX_REPAIR_FRAME_BYTES + 1,
            max: MAX_REPAIR_FRAME_BYTES,
        }
        .into();
        assert!(matches!(error, TransportError::FrameTooLarge { .. }));
    }
}
