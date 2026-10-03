// SPDX-License-Identifier: GPL-3.0-or-later
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Cell {
    pub percent: Option<u8>,
    pub charging: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Battery {
    pub left: Cell,
    pub right: Cell,
    pub case: Cell,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Off,
    Anc,
    Transparency,
    Adaptive,
}

impl Mode {
    pub fn value(self) -> u8 {
        match self {
            Self::Off => 1,
            Self::Anc => 2,
            Self::Transparency => 3,
            Self::Adaptive => 4,
        }
    }
    pub fn from_value(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Off),
            2 => Some(Self::Anc),
            3 => Some(Self::Transparency),
            4 => Some(Self::Adaptive),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PairedDevice {
    pub address: String,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Snapshot {
    pub schema: u8,
    pub configured: bool,
    pub status: String,
    pub name: String,
    pub connected: bool,
    // Physical left/right from BLE while idle, or AAP battery-entry ordering
    // during a live session. Unknown until that session establishes ordering.
    pub in_ear: [Option<bool>; 2],
    pub battery: Battery,
    pub mode: Option<Mode>,
    pub error: Option<String>,
    pub paired_devices: Vec<PairedDevice>,
    /// The running backend's package version. The plugin compares it with its
    /// manifest version, because `omarchy plugin update` never rebuilds the
    /// daemon. Absent from pre-0.1.0 backends, which read as "".
    #[serde(default)]
    pub version: String,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            schema: 1,
            configured: false,
            status: "setup_required".into(),
            name: "AirPods".into(),
            connected: false,
            in_ear: [None; 2],
            battery: Battery::default(),
            mode: None,
            error: None,
            paired_devices: Vec::new(),
            version: env!("CARGO_PKG_VERSION").into(),
        }
    }
}

pub fn wearing(ears: [Option<bool>; 2]) -> bool {
    ears.contains(&Some(true))
}
