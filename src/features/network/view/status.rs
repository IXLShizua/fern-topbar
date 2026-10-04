use super::super::backend::{ConnectionKind, DeviceInfo, Snapshot, WifiNetwork, WifiProfile};
use crate::ui::icon_names;

#[derive(Clone, Debug)]
pub struct View {
    pub visible: bool,
    pub icon: &'static str,
    pub summary: String,
    pub signal: Option<u8>,
    pub tooltip: String,
    pub wifi_enabled: bool,
    pub wifi_available: bool,
    pub wifi_hardware_enabled: bool,
    pub wired_enabled: bool,
    pub wired_connected: bool,
    pub wired_available: bool,
    pub error: Option<String>,
    pub busy: bool,
    pub networks: Vec<WifiNetwork>,
    pub profiles: Vec<WifiProfile>,
    pub devices: Vec<DeviceInfo>,
}

impl View {
    pub fn wifi_empty_message(&self) -> &'static str {
        if !self.wifi_available {
            "No Wi-Fi adapter detected"
        } else if !self.wifi_hardware_enabled {
            "Unblock Wi-Fi using the hardware switch"
        } else if !self.wifi_enabled {
            "Turn on Wi-Fi to see nearby networks"
        } else if self.busy {
            "Looking for networks…"
        } else {
            "No networks found. Try scanning again."
        }
    }

    pub fn wifi_subtitle(&self) -> &'static str {
        if !self.wifi_available {
            "No Wi-Fi device"
        } else if !self.wifi_hardware_enabled {
            "Blocked by hardware switch"
        } else if !self.wifi_enabled {
            "Off"
        } else if self.networks.iter().any(|network| network.active) {
            "Connected"
        } else {
            "Not connected"
        }
    }

    pub fn wired_subtitle(&self) -> &'static str {
        if self.wired_connected {
            "Connected"
        } else if !self.wired_available {
            "No Ethernet device"
        } else if !self.wired_enabled {
            "Off"
        } else {
            "Not connected"
        }
    }

    pub fn wifi_sensitive(&self) -> bool {
        !self.busy && self.wifi_available && self.wifi_hardware_enabled
    }

    pub fn from_status(status: Snapshot, error: Option<String>, visible: bool) -> Self {
        let wifi_enabled =
            status.wifi_enabled && status.wifi_available && status.wifi_hardware_enabled;
        let (icon, label) = match status.connection_kind {
            Some(ConnectionKind::Wifi) if wifi_enabled => (icon_names::NETWORK_WIFI, "Wi-Fi"),
            _ if status.wired_connected => (icon_names::NETWORK_WIRED, "Ethernet"),
            _ if status.wired_enabled => (icon_names::NETWORK_WIRED, "Connecting"),
            _ if wifi_enabled => (icon_names::NETWORK_WIFI, "Connecting"),
            _ => (icon_names::NETWORK_OFF, "Offline"),
        };

        let tooltip = if status.connection_name.is_empty() {
            label.to_string()
        } else {
            format!("Connected: {}", status.connection_name)
        };

        let signal = match status.connection_kind {
            Some(ConnectionKind::Wifi) if wifi_enabled => status
                .networks
                .iter()
                .find(|network| network.active)
                .map(|network| network.strength),
            _ => None,
        };

        Self {
            visible,
            icon,
            summary: label.into(),
            signal,
            tooltip,
            wifi_enabled,
            wifi_available: status.wifi_available,
            wifi_hardware_enabled: status.wifi_hardware_enabled,
            wired_enabled: status.wired_enabled,
            wired_connected: status.wired_connected,
            wired_available: status.wired_available,
            error,
            busy: false,
            networks: status.networks,
            profiles: status.profiles,
            devices: status.devices,
        }
    }
}

impl Default for View {
    fn default() -> Self {
        Self::from_status(Snapshot::default(), None, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_wifi_takes_precedence_unless_the_adapter_is_blocked() {
        let mut snapshot = Snapshot {
            wifi_enabled: true,
            wifi_available: true,
            wifi_hardware_enabled: true,
            wired_connected: true,
            connection_kind: Some(ConnectionKind::Wifi),
            ..Snapshot::default()
        };

        let view = View::from_status(snapshot.clone(), None, true);

        assert_eq!(view.icon, icon_names::NETWORK_WIFI);
        assert_eq!(view.summary, "Wi-Fi");

        snapshot.wifi_hardware_enabled = false;
        let view = View::from_status(snapshot, None, true);

        assert_eq!(view.icon, icon_names::NETWORK_WIRED);
        assert_eq!(view.summary, "Ethernet");
        assert!(!view.wifi_enabled);
    }
}
