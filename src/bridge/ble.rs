//! Platform BLE host seam.
//!
//! Implementations own permissions, scanning, operating-system connection
//! objects, GATT subscription, and characteristic I/O. `tutti-ble` owns the
//! bytes sent through this seam.

use std::collections::VecDeque;

use thiserror::Error;
use tutti_ble::{FragmentCursor, Fragmenter};

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct BleAddress(pub String);

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BleScanResult {
    pub address: BleAddress,
    pub display_name: Option<String>,
    pub signal_dbm: Option<i16>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum BleHostEvent {
    ScanResult(BleScanResult),
    Connected {
        address: BleAddress,
        /// Negotiated characteristic-value bytes (ATT MTU minus protocol
        /// overhead), used in the host hello for responder fragmentation.
        max_fragment_value_bytes: u16,
    },
    Disconnected {
        address: BleAddress,
        reason: Option<String>,
    },
    /// Initial peer metadata. Platform hosts must emit `Connected` first so the
    /// transport has the negotiated fragmentation budget. The transport also
    /// buffers an early INFO defensively for imperfect platform callbacks.
    Info(Vec<u8>),
    /// GATT TX value. Platform hosts must emit `Connected` before notifications.
    Notification(Vec<u8>),
    /// Final GATT fragment of one complete bounded wire message was accepted
    /// by the peripheral. Queue admission alone is not this event.
    WriteComplete {
        message_id: u16,
    },
    /// Non-fatal platform detail. This must never be used for a condition that
    /// invalidates the installed connection or authenticated session.
    Diagnostic(String),
    PermissionDenied,
    EventsDropped(u64),
    Error(BleHostError),
}

#[derive(Clone, PartialEq, Eq, Debug, Error)]
pub enum BleHostError {
    #[error("Bluetooth permission was denied")]
    PermissionDenied,
    #[error("Bluetooth is unavailable: {0}")]
    Unavailable(String),
    #[error("Bluetooth operation failed: {0}")]
    Operation(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BleWritePriority {
    Control,
    Realtime,
    Repair,
}

/// One complete protocol wire message accepted by the platform TX owner.
/// Fragment values are materialized incrementally after this bounded object is
/// queued; callers never construct a `Vec<Vec<u8>>` fragment train.
pub struct BleWriteMessage {
    pub(crate) message_id: u16,
    pub(crate) wire: Vec<u8>,
    pub(crate) value_bytes: usize,
    pub(crate) priority: BleWritePriority,
}

impl BleWriteMessage {
    pub fn new(
        message_id: u16,
        wire: Vec<u8>,
        value_bytes: usize,
        priority: BleWritePriority,
    ) -> Result<Self, BleHostError> {
        Fragmenter::new(message_id, &wire, value_bytes)
            .map_err(|error| BleHostError::Operation(error.to_string()))?;
        Ok(Self {
            message_id,
            wire,
            value_bytes,
            priority,
        })
    }

    pub fn retained_wire_bytes(&self) -> usize {
        self.wire.len()
    }

    pub const fn message_id(&self) -> u16 {
        self.message_id
    }

    pub(crate) fn into_cursor(self) -> Result<(BleWritePriority, FragmentCursor), BleHostError> {
        let cursor = FragmentCursor::new(self.message_id, self.wire, self.value_bytes)
            .map_err(|error| BleHostError::Operation(error.to_string()))?;
        Ok((self.priority, cursor))
    }
}

/// Non-blocking interface driven only by the bridge background worker.
pub trait BleHost: Send + 'static {
    fn start_scan(&mut self) -> Result<(), BleHostError>;
    fn stop_scan(&mut self) -> Result<(), BleHostError>;
    fn connect(&mut self, address: &BleAddress) -> Result<(), BleHostError>;
    fn disconnect(&mut self) -> Result<(), BleHostError>;
    /// Atomically accept one complete, bounded protocol message. The platform
    /// owner fragments it incrementally and does not retire its storage until
    /// the final GATT write completes.
    fn write_rx_message(&mut self, message: BleWriteMessage) -> Result<(), BleHostError>;
    fn poll_event(&mut self) -> Option<BleHostEvent>;
}

/// Deterministic host used by conformance and bridge tests. It deliberately
/// models GATT callbacks as queued events instead of invoking application code
/// reentrantly.
#[derive(Default)]
pub struct InMemoryBleHost {
    events: VecDeque<BleHostEvent>,
    writes: VecDeque<Vec<u8>>,
    connected: Option<BleAddress>,
}

impl InMemoryBleHost {
    pub fn push_event(&mut self, event: BleHostEvent) {
        self.events.push_back(event);
    }

    pub fn pop_write(&mut self) -> Option<Vec<u8>> {
        self.writes.pop_front()
    }

    pub fn connected(&self) -> Option<&BleAddress> {
        self.connected.as_ref()
    }
}

impl BleHost for InMemoryBleHost {
    fn start_scan(&mut self) -> Result<(), BleHostError> {
        Ok(())
    }

    fn stop_scan(&mut self) -> Result<(), BleHostError> {
        Ok(())
    }

    fn connect(&mut self, address: &BleAddress) -> Result<(), BleHostError> {
        self.connected = Some(address.clone());
        self.events.push_back(BleHostEvent::Connected {
            address: address.clone(),
            max_fragment_value_bytes: 20,
        });
        Ok(())
    }

    fn disconnect(&mut self) -> Result<(), BleHostError> {
        if let Some(address) = self.connected.take() {
            self.events.push_back(BleHostEvent::Disconnected {
                address,
                reason: None,
            });
        }
        Ok(())
    }

    fn write_rx_message(&mut self, message: BleWriteMessage) -> Result<(), BleHostError> {
        if self.connected.is_none() {
            return Err(BleHostError::Operation("no board is connected".into()));
        }
        let message_id = message.message_id();
        let (_, mut cursor) = message.into_cursor()?;
        let mut value = vec![0; cursor.fragment_value_bytes()];
        while let Some(used) = cursor
            .encode_next(&mut value)
            .map_err(|error| BleHostError::Operation(error.to_string()))?
        {
            self.writes.push_back(value[..used].to_vec());
        }
        self.events
            .push_back(BleHostEvent::WriteComplete { message_id });
        Ok(())
    }

    fn poll_event(&mut self) -> Option<BleHostEvent> {
        self.events.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_host_models_connect_write_disconnect_order() {
        let address = BleAddress("board-a".into());
        let mut host = InMemoryBleHost::default();
        host.connect(&address).unwrap();
        assert_eq!(
            host.poll_event(),
            Some(BleHostEvent::Connected {
                address: address.clone(),
                max_fragment_value_bytes: 20,
            })
        );
        host.write_rx_message(
            BleWriteMessage::new(1, b"fragment".to_vec(), 20, BleWritePriority::Control).unwrap(),
        )
        .unwrap();
        assert!(host.pop_write().is_some());
        assert_eq!(
            host.poll_event(),
            Some(BleHostEvent::WriteComplete { message_id: 1 })
        );
        host.disconnect().unwrap();
        assert!(matches!(
            host.poll_event(),
            Some(BleHostEvent::Disconnected { reason: None, .. })
        ));
    }
}
