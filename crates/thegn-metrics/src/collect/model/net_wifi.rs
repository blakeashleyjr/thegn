//! Bounded Wi-Fi interface and connected-station rows.
use super::super::frame::Part;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InlineBytes<const N: usize> {
    pub bytes: [u8; N],
    pub len: u8,
}

impl<const N: usize> Default for InlineBytes<N> {
    fn default() -> Self {
        Self {
            bytes: [0; N],
            len: 0,
        }
    }
}

impl<const N: usize> InlineBytes<N> {
    pub fn set(&mut self, value: &[u8]) {
        let len = value.len().min(N);
        self.bytes[..len].copy_from_slice(&value[..len]);
        self.len = u8::try_from(len).unwrap_or(N as u8);
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WifiRow {
    pub ifindex: u32,
    pub name: InlineBytes<16>,
    pub ssid: Option<InlineBytes<32>>,
    pub bssid_masked: Option<&'static str>,
    pub freq_mhz: Option<u32>,
    pub width_mhz: Option<u32>,
    pub signal_dbm: Option<i8>,
    pub signal_avg_dbm: Option<i8>,
    pub tx_bitrate_100kbps: Option<u32>,
    pub rx_bitrate_100kbps: Option<u32>,
    pub tx_retries: Option<u32>,
    pub tx_failed: Option<u32>,
    pub connected_secs: Option<u32>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct NetWifiPart {
    pub ifaces: Vec<WifiRow>,
    pub truncated: bool,
}

impl Part for NetWifiPart {
    fn clear(&mut self) {
        self.ifaces.clear();
        self.truncated = false;
    }
}
