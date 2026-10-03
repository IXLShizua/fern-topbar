use super::{
    Command, ConnectionKind, DeviceInfo, DeviceState, Password, Security, Snapshot, WifiNetwork,
    WifiProfile,
};
use futures_util::{
    StreamExt,
    stream::{BoxStream, select_all},
};
use std::{collections::HashMap, time::Duration};
use tokio::time::timeout;
use zbus::{
    Connection, Error, MatchRule, MessageStream,
    message::Type,
    proxy::CacheProperties,
    zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Str, Value},
};

type Settings = HashMap<String, HashMap<String, OwnedValue>>;

pub const SERVICE: &str = "org.freedesktop.NetworkManager";
const ROOT: &str = "/org/freedesktop/NetworkManager";

fn connection_kind(kind: &str) -> Option<ConnectionKind> {
    match kind {
        "" => None,
        "802-11-wireless" => Some(ConnectionKind::Wifi),
        "802-3-ethernet" => Some(ConnectionKind::Ethernet),
        _ => Some(ConnectionKind::Other),
    }
}

fn device_state(state: u32) -> DeviceState {
    match state {
        10 => DeviceState::Unknown,
        20 | 30 => DeviceState::Disconnected,
        // NetworkManager's preparation, authentication and IP setup phases
        // describe one ongoing connection attempt for consumers of the backend.
        40 | 50 | 60 | 70 | 80 | 90 => DeviceState::Connecting,
        100 => DeviceState::Connected,
        110 => DeviceState::Disconnecting,
        120 => DeviceState::Failed,
        _ => DeviceState::Unknown,
    }
}

fn security_from_flags(flags: u32, security: u32) -> Security {
    if security & (0x200 | 0x2000) != 0 {
        Security::Unsupported
    } else if security & 0x100 != 0 {
        Security::Psk
    } else if security & 0x400 != 0 {
        Security::Sae
    } else if security & (0x800 | 0x1000) != 0 {
        Security::Owe
    } else if flags & 1 != 0 || security != 0 {
        Security::Unsupported
    } else {
        Security::Open
    }
}

pub struct Client {
    connection: Connection,
    manager: NetworkManagerProxy<'static>,
}

impl Client {
    pub async fn new(connection: Connection) -> zbus::Result<Self> {
        let manager = NetworkManagerProxy::builder(&connection)
            .cache_properties(CacheProperties::No)
            .build()
            .await?;

        Ok(Self {
            connection,
            manager,
        })
    }

    pub async fn changes(&self) -> zbus::Result<BoxStream<'static, zbus::Result<()>>> {
        // One namespace subscription also covers device/AP hotplug and property
        // changes; separate generated DeviceAdded/DeviceRemoved streams are unnecessary.
        let network = MatchRule::builder()
            .msg_type(Type::Signal)
            .sender(SERVICE)?
            .path_namespace(ROOT)?
            .build();

        let owner = MatchRule::builder()
            .msg_type(Type::Signal)
            .sender("org.freedesktop.DBus")?
            .interface("org.freedesktop.DBus")?
            .member("NameOwnerChanged")?
            .add_arg(SERVICE)?
            .build();

        let mut streams = Vec::new();

        for rule in [network, owner] {
            streams.push(MessageStream::for_match_rule(rule, &self.connection, Some(128)).await?);
        }

        Ok(select_all(streams)
            .map(|message| {
                let message = message?;

                if message
                    .header()
                    .interface()
                    .is_some_and(|name| name.as_str() == "org.freedesktop.DBus")
                {
                    let (_, _, owner): (String, String, String) = message.body().deserialize()?;

                    if owner.is_empty() {
                        return Err(zbus::fdo::Error::NameHasNoOwner(
                            "NetworkManager disconnected".into(),
                        )
                        .into());
                    }
                }

                Ok(())
            })
            .boxed())
    }

    pub async fn snapshot(&self) -> zbus::Result<Snapshot> {
        let mut snapshot = Snapshot {
            wifi_enabled: self.manager.wireless_enabled().await?,
            wifi_hardware_enabled: self.manager.wireless_hardware_enabled().await?,
            connection_kind: connection_kind(&self.manager.primary_connection_type().await?),
            ..Snapshot::default()
        };

        self.read_profiles(&mut snapshot).await?;

        for path in self.manager.get_devices().await? {
            self.read_device(&path, &mut snapshot).await?;
        }

        let active = self.manager.primary_connection().await?;

        if active.as_str() != "/" {
            snapshot.connection_name = ActiveConnectionProxy::builder(&self.connection)
                .path(active)?
                .build()
                .await?
                .id()
                .await?;
        }

        snapshot.normalize();

        Ok(snapshot)
    }

    pub async fn apply(&self, command: Command) -> zbus::Result<()> {
        match command {
            Command::SetWifi(enabled) => self.manager.set_wireless_enabled(enabled).await,
            Command::SetWired(enabled) => self.set_wired(enabled).await,
            Command::Scan => self.scan().await,
            Command::Connect(network, password) => self.connect_wifi(network, password).await,
            Command::Disconnect(path) => self.disconnect(&path).await,
            Command::Forget(path) => {
                SavedProfileProxy::builder(&self.connection)
                    .path(path)?
                    .build()
                    .await?
                    .delete()
                    .await
            }
        }
    }

    async fn set_wired(&self, enabled: bool) -> zbus::Result<()> {
        let mut first = None;
        let mut connected = false;

        for path in self.manager.get_devices().await? {
            let device = NetworkDeviceProxy::builder(&self.connection)
                .path(path.clone())?
                .build()
                .await?;

            if device.device_type().await? != 1 {
                continue;
            }

            first.get_or_insert(path);

            let active = device_state(device.state().await?) == DeviceState::Connected;

            connected |= active;
            device.set_autoconnect(enabled).await?;

            if !enabled && active {
                device.disconnect().await?;
            }
        }

        let first = first.ok_or_else(|| Error::Failure("No wired device is available".into()))?;

        if enabled && !connected {
            let root = ObjectPath::try_from("/")?;

            self.manager
                .activate_connection(root.clone(), first.as_ref(), root)
                .await?;
        }

        Ok(())
    }

    async fn read_profiles(&self, snapshot: &mut Snapshot) -> zbus::Result<()> {
        let settings = SavedSettingsProxy::new(&self.connection).await?;

        for path in settings.list_connections().await? {
            let saved = SavedProfileProxy::builder(&self.connection)
                .path(path.clone())?
                .build()
                .await?;

            let Ok(data) = saved.get_settings().await else {
                continue;
            };

            if let Some(profile) = Self::profile(path.to_string(), &data) {
                snapshot.profiles.push(profile);
            }
        }

        Ok(())
    }

    async fn read_device(
        &self,
        path: &OwnedObjectPath,
        snapshot: &mut Snapshot,
    ) -> zbus::Result<()> {
        let device = NetworkDeviceProxy::builder(&self.connection)
            .path(path.clone())?
            .build()
            .await?;

        let kind = device.device_type().await?;

        if kind != 1 && kind != 2 {
            return Ok(());
        }

        let state = device_state(device.state().await?);

        match kind {
            1 => {
                snapshot.wired_available = true;
                snapshot.wired_enabled |= device.autoconnect().await?;
                snapshot.wired_connected |= state == DeviceState::Connected;
            }
            2 => snapshot.wifi_available = true,
            _ => {}
        }

        snapshot.devices.push(DeviceInfo {
            path: path.to_string(),
            interface: device.interface().await?,
            wireless: kind == 2,
            state,
            addresses: self.read_addresses(&device).await?,
        });

        if kind == 2 && snapshot.wifi_enabled {
            self.read_networks(path, &device, snapshot).await?;
        }

        Ok(())
    }

    async fn read_addresses(&self, device: &NetworkDeviceProxy<'_>) -> zbus::Result<Vec<String>> {
        let mut addresses = Vec::new();
        let ipv4 = device.ip4_config().await?;

        if ipv4.as_str() != "/" {
            let config = Ip4ConfigProxy::builder(&self.connection)
                .path(ipv4)?
                .build()
                .await?;

            Self::append_addresses(&mut addresses, config.address_data().await?);
        }

        let ipv6 = device.ip6_config().await?;

        if ipv6.as_str() != "/" {
            let config = Ip6ConfigProxy::builder(&self.connection)
                .path(ipv6)?
                .build()
                .await?;

            Self::append_addresses(&mut addresses, config.address_data().await?);
        }

        Ok(addresses)
    }

    fn append_addresses(addresses: &mut Vec<String>, data: Vec<HashMap<String, OwnedValue>>) {
        addresses.extend(data.into_iter().filter_map(|mut address| {
            address
                .remove("address")
                .and_then(|value| String::try_from(value).ok())
        }));
    }

    async fn read_networks(
        &self,
        path: &OwnedObjectPath,
        device: &NetworkDeviceProxy<'_>,
        snapshot: &mut Snapshot,
    ) -> zbus::Result<()> {
        let wireless = WirelessProxy::builder(&self.connection)
            .path(path.clone())?
            .build()
            .await?;

        let active = wireless.active_access_point().await?;
        let available = device.available_connections().await?;

        for access_point in wireless.get_all_access_points().await? {
            let Ok(Some(mut network)) = self.read_access_point(path, &access_point).await else {
                continue;
            };

            network.active = active == access_point;
            network.profile = snapshot
                .profiles
                .iter()
                .find(|profile| {
                    profile.ssid == network.ssid
                        && profile.security == network.security
                        && available.iter().any(|path| path.as_str() == profile.path)
                })
                .map(|profile| profile.path.clone());

            snapshot.networks.push(network);
        }

        Ok(())
    }

    async fn read_access_point(
        &self,
        device: &OwnedObjectPath,
        path: &OwnedObjectPath,
    ) -> zbus::Result<Option<WifiNetwork>> {
        let ap = AccessPointProxy::builder(&self.connection)
            .path(path.clone())?
            .build()
            .await?;

        let ssid = ap.ssid().await?;

        if ssid.is_empty() {
            return Ok(None);
        }

        let security = security_from_flags(
            ap.flags().await?,
            ap.wpa_flags().await? | ap.rsn_flags().await?,
        );

        Ok(Some(WifiNetwork {
            name: String::from_utf8_lossy(&ssid).into_owned(),
            ssid,
            security,
            strength: ap.strength().await?,
            device: device.to_string(),
            access_point: path.to_string(),
            active: false,
            profile: None,
        }))
    }

    async fn scan(&self) -> zbus::Result<()> {
        let manager = &self.manager;

        for path in manager.get_devices().await? {
            let device = NetworkDeviceProxy::builder(&self.connection)
                .path(path.clone())?
                .build()
                .await?;

            if device.device_type().await? != 2 {
                continue;
            }

            let wireless = WirelessProxy::builder(&self.connection)
                .path(path)?
                .build()
                .await?;

            let mut changes = wireless.receive_last_scan_changed().await;
            let previous = wireless.last_scan().await?;

            wireless.request_scan(HashMap::new()).await?;

            while wireless.last_scan().await? == previous {
                if changes.next().await.is_none() {
                    return Err(Error::Failure("Scan subscription closed".into()));
                }
            }
        }

        Ok(())
    }

    async fn connect_wifi(
        &self,
        network: WifiNetwork,
        password: Option<Password>,
    ) -> zbus::Result<()> {
        let manager = &self.manager;
        let device = OwnedObjectPath::try_from(network.device.as_str())?;
        let ap = OwnedObjectPath::try_from(network.access_point.as_str())?;

        let active = match network.profile.as_deref() {
            Some(path) => {
                let profile = OwnedObjectPath::try_from(path)?;

                if let Some(password) = password.as_ref() {
                    self.update_password(&profile, &network, password).await?;
                }

                manager
                    .activate_connection(profile.as_ref(), device.as_ref(), ap.as_ref())
                    .await?
            }
            None => {
                let settings = Self::connection_settings(
                    &network,
                    password.as_ref().map(|password| password.0.as_str()),
                )
                .map_err(Error::Failure)?;

                let (_, active) = manager
                    .add_and_activate_connection(settings, device.as_ref(), ap.as_ref())
                    .await?;

                active
            }
        };

        match timeout(Duration::from_secs(45), self.wait_for_activation(active)).await {
            Ok(result) => result,
            Err(_) => {
                self.disconnect(&network.device).await?;

                Err(Error::Failure(
                    "Connection timed out and was cancelled; check the password and signal".into(),
                ))
            }
        }
    }

    async fn update_password(
        &self,
        profile: &OwnedObjectPath,
        network: &WifiNetwork,
        password: &Password,
    ) -> zbus::Result<()> {
        let saved = SavedProfileProxy::builder(&self.connection)
            .path(profile.clone())?
            .build()
            .await?;

        let mut original = saved.get_settings().await?;
        let mut replacement =
            Self::connection_settings(network, Some(&password.0)).map_err(Error::Failure)?;
        let security = replacement
            .remove("802-11-wireless-security")
            .ok_or_else(|| Error::Failure("This network does not use a password".into()))?;

        original.insert("802-11-wireless-security".into(), security);

        saved.update(original).await
    }

    async fn wait_for_activation(&self, path: OwnedObjectPath) -> zbus::Result<()> {
        let active = ActiveConnectionProxy::builder(&self.connection)
            .path(path)?
            .build()
            .await?;

        let mut changes = active.receive_state_changed().await;

        loop {
            match active.state().await? {
                2 => return Ok(()),
                3 | 4 => {
                    return Err(Error::Failure(
                        "Connection failed; check the password and signal".into(),
                    ));
                }
                _ => {}
            }

            if changes.next().await.is_none() {
                return Err(Error::Failure("Connection subscription closed".into()));
            }
        }
    }

    async fn disconnect(&self, path: &str) -> zbus::Result<()> {
        NetworkDeviceProxy::builder(&self.connection)
            .path(path)?
            .build()
            .await?
            .disconnect()
            .await
    }

    fn profile(path: String, settings: &Settings) -> Option<WifiProfile> {
        if string(settings, "connection", "type")?.as_str() != "802-11-wireless" {
            return None;
        }

        let ssid = settings
            .get("802-11-wireless")?
            .get("ssid")?
            .try_clone()
            .ok()?
            .try_into()
            .ok()?;

        let security = match string(settings, "802-11-wireless-security", "key-mgmt").as_deref() {
            None => Security::Open,
            Some("wpa-psk") => Security::Psk,
            Some("sae") => Security::Sae,
            Some("owe") => Security::Owe,
            _ => Security::Unsupported,
        };

        Some(WifiProfile {
            path,
            name: string(settings, "connection", "id").unwrap_or_default(),
            ssid,
            security,
        })
    }

    fn connection_settings(
        network: &WifiNetwork,
        password: Option<&str>,
    ) -> Result<Settings, String> {
        let mut settings = Settings::new();
        settings.insert(
            "connection".into(),
            HashMap::from([
                ("id".into(), text(&network.name)),
                ("type".into(), text("802-11-wireless")),
                ("autoconnect".into(), OwnedValue::from(true)),
            ]),
        );

        settings.insert(
            "802-11-wireless".into(),
            HashMap::from([
                (
                    "ssid".into(),
                    Value::from(network.ssid.clone())
                        .try_into()
                        .map_err(|error| format!("Invalid SSID: {error}"))?,
                ),
                ("mode".into(), text("infrastructure")),
            ]),
        );

        if network.security != Security::Open {
            let key = match network.security {
                Security::Psk => "wpa-psk",
                Security::Sae => "sae",
                Security::Owe => "owe",
                _ => return Err("This security type is not supported".into()),
            };

            let mut security = HashMap::from([("key-mgmt".into(), text(key))]);

            if network.security.needs_password() {
                let password = password.ok_or("A password is required")?;
                let valid = match network.security {
                    Security::Psk => {
                        (8..=63).contains(&password.len()) && password.is_ascii()
                            || password.len() == 64
                                && password.bytes().all(|byte| byte.is_ascii_hexdigit())
                    }
                    _ => !password.is_empty(),
                };

                if !valid {
                    return Err("Invalid password: WPA/WPA2 requires 8–63 ASCII characters \
                         or 64 hexadecimal digits"
                        .into());
                }

                security.insert("psk".into(), text(password));
                security.insert("psk-flags".into(), OwnedValue::from(0u32));
            }

            settings
                .get_mut("802-11-wireless")
                .unwrap()
                .insert("security".into(), text("802-11-wireless-security"));

            settings.insert("802-11-wireless-security".into(), security);
        }

        for section in ["ipv4", "ipv6"] {
            settings.insert(
                section.into(),
                HashMap::from([("method".into(), text("auto"))]),
            );
        }

        Ok(settings)
    }
}

fn string(settings: &Settings, section: &str, key: &str) -> Option<String> {
    settings
        .get(section)?
        .get(key)?
        .try_clone()
        .ok()?
        .try_into()
        .ok()
}

fn text(value: &str) -> OwnedValue {
    OwnedValue::from(Str::from(value))
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager"
)]
trait NetworkManager {
    fn get_devices(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    fn activate_connection(
        &self,
        connection: ObjectPath<'_>,
        device: ObjectPath<'_>,
        specific_object: ObjectPath<'_>,
    ) -> zbus::Result<OwnedObjectPath>;

    fn add_and_activate_connection(
        &self,
        settings: Settings,
        device: ObjectPath<'_>,
        specific_object: ObjectPath<'_>,
    ) -> zbus::Result<(OwnedObjectPath, OwnedObjectPath)>;

    #[zbus(property)]
    fn primary_connection_type(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn primary_connection(&self) -> zbus::Result<OwnedObjectPath>;
    #[zbus(property)]
    fn wireless_enabled(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn wireless_hardware_enabled(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_wireless_enabled(&self, enabled: bool) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Device",
    default_service = "org.freedesktop.NetworkManager"
)]
trait NetworkDevice {
    fn disconnect(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn interface(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn available_connections(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    #[zbus(property)]
    fn ip4_config(&self) -> zbus::Result<OwnedObjectPath>;
    #[zbus(property)]
    fn ip6_config(&self) -> zbus::Result<OwnedObjectPath>;

    #[zbus(property)]
    fn device_type(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn autoconnect(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_autoconnect(&self, enabled: bool) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Connection.Active",
    default_service = "org.freedesktop.NetworkManager"
)]
trait ActiveConnection {
    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Device.Wireless",
    default_service = "org.freedesktop.NetworkManager"
)]
trait Wireless {
    fn request_scan(&self, options: HashMap<String, OwnedValue>) -> zbus::Result<()>;
    fn get_all_access_points(&self) -> zbus::Result<Vec<OwnedObjectPath>>;

    #[zbus(property)]
    fn active_access_point(&self) -> zbus::Result<OwnedObjectPath>;
    #[zbus(property)]
    fn last_scan(&self) -> zbus::Result<i64>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.AccessPoint",
    default_service = "org.freedesktop.NetworkManager"
)]
trait AccessPoint {
    #[zbus(property)]
    fn ssid(&self) -> zbus::Result<Vec<u8>>;
    #[zbus(property)]
    fn strength(&self) -> zbus::Result<u8>;
    #[zbus(property)]
    fn flags(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn wpa_flags(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn rsn_flags(&self) -> zbus::Result<u32>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Settings",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager/Settings"
)]
trait SavedSettings {
    fn list_connections(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Settings.Connection",
    default_service = "org.freedesktop.NetworkManager"
)]
trait SavedProfile {
    fn get_settings(&self) -> zbus::Result<Settings>;
    fn update(&self, settings: Settings) -> zbus::Result<()>;
    fn delete(&self) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.IP4Config",
    default_service = "org.freedesktop.NetworkManager"
)]
trait Ip4Config {
    #[zbus(property)]
    fn address_data(&self) -> zbus::Result<Vec<HashMap<String, OwnedValue>>>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.IP6Config",
    default_service = "org.freedesktop.NetworkManager"
)]
trait Ip6Config {
    #[zbus(property)]
    fn address_data(&self) -> zbus::Result<Vec<HashMap<String, OwnedValue>>>;
}

#[cfg(test)]
mod tests {
    use super::super::{Backend, Event};
    use super::*;
    use crate::backend::reconnect::ReconnectBackoff;
    use crate::{backend::dbus::ServiceChanges, features::availability::AvailabilityPublisher};
    use std::{
        collections::HashMap,
        sync::{
            Arc, Mutex,
            atomic::{AtomicI64, Ordering},
        },
    };
    use zbus::{
        object_server::SignalEmitter,
        zvariant::{OwnedObjectPath, OwnedValue},
    };

    const DEVICE: &str = "/org/freedesktop/NetworkManager/Devices/1";
    const AP: &str = "/org/freedesktop/NetworkManager/AccessPoint/1";
    const PROFILE: &str = "/org/freedesktop/NetworkManager/Settings/1";
    const ACTIVE: &str = "/org/freedesktop/NetworkManager/ActiveConnection/1";

    #[test]
    fn security_never_downgrades_unknown_encryption() {
        assert_eq!(security_from_flags(1, 0), Security::Unsupported);
        assert_eq!(security_from_flags(0, 0x200), Security::Unsupported);
        assert_eq!(security_from_flags(1, 0x100), Security::Psk);
        assert_eq!(security_from_flags(1, 0x400), Security::Sae);
        assert_eq!(security_from_flags(0, 0), Security::Open);
    }

    #[test]
    fn unknown_protocol_values_do_not_imply_a_connection() {
        assert_eq!(connection_kind(""), None);
        assert_eq!(connection_kind("vpn"), Some(ConnectionKind::Other));

        for state in [20, 30] {
            assert_eq!(device_state(state), DeviceState::Disconnected);
        }

        for state in [0, 10, 41, 99, 101, u32::MAX] {
            assert_eq!(device_state(state), DeviceState::Unknown);
        }
    }

    #[derive(Default)]
    struct Calls {
        wifi_enabled: bool,
        devices_absent: bool,
        activations: Vec<String>,
        created: Vec<Settings>,
        updated: Vec<Settings>,
        deny_activation: bool,
        disconnects: usize,
        deletes: usize,
    }

    struct Manager(Arc<Mutex<Calls>>);

    #[zbus::interface(name = "org.freedesktop.NetworkManager")]
    impl Manager {
        #[zbus(property)]
        fn wireless_enabled(&self) -> bool {
            self.0.lock().unwrap().wifi_enabled
        }
        #[zbus(property)]
        fn set_wireless_enabled(&self, enabled: bool) {
            self.0.lock().unwrap().wifi_enabled = enabled;
        }
        #[zbus(property)]
        fn wireless_hardware_enabled(&self) -> bool {
            true
        }
        #[zbus(property)]
        fn primary_connection_type(&self) -> &str {
            "802-11-wireless"
        }
        #[zbus(property)]
        fn primary_connection(&self) -> OwnedObjectPath {
            path(ACTIVE)
        }

        fn get_devices(&self) -> Vec<OwnedObjectPath> {
            if self.0.lock().unwrap().devices_absent {
                Vec::new()
            } else {
                vec![path(DEVICE)]
            }
        }
        #[zbus(signal)]
        async fn device_added(
            emitter: SignalEmitter<'_>,
            device: OwnedObjectPath,
        ) -> zbus::Result<()>;
        #[zbus(signal)]
        async fn device_removed(
            emitter: SignalEmitter<'_>,
            device: OwnedObjectPath,
        ) -> zbus::Result<()>;
        fn activate_connection(
            &self,
            connection: OwnedObjectPath,
            device: OwnedObjectPath,
            specific_object: OwnedObjectPath,
        ) -> zbus::fdo::Result<OwnedObjectPath> {
            assert_eq!(device.as_str(), DEVICE);
            assert_eq!(specific_object.as_str(), AP);

            if self.0.lock().unwrap().deny_activation {
                return Err(zbus::fdo::Error::AccessDenied(
                    "Test policy denied activation".into(),
                ));
            }

            self.0
                .lock()
                .unwrap()
                .activations
                .push(connection.to_string());

            Ok(path(ACTIVE))
        }

        fn add_and_activate_connection(
            &self,
            connection: Settings,
            device: OwnedObjectPath,
            specific_object: OwnedObjectPath,
        ) -> (OwnedObjectPath, OwnedObjectPath) {
            assert_eq!(device.as_str(), DEVICE);
            assert_eq!(specific_object.as_str(), AP);

            self.0.lock().unwrap().created.push(connection);

            (path(PROFILE), path(ACTIVE))
        }
    }

    struct Device(Arc<Mutex<Calls>>);

    #[zbus::interface(name = "org.freedesktop.NetworkManager.Device")]
    impl Device {
        fn disconnect(&self) {
            self.0.lock().unwrap().disconnects += 1;
        }
        #[zbus(property)]
        fn device_type(&self) -> u32 {
            2
        }
        #[zbus(property)]
        fn interface(&self) -> &str {
            "wlan-test"
        }
        #[zbus(property)]
        fn state(&self) -> u32 {
            100
        }
        #[zbus(property)]
        fn available_connections(&self) -> Vec<OwnedObjectPath> {
            vec![path(PROFILE)]
        }
        #[zbus(property)]
        fn ip4_config(&self) -> OwnedObjectPath {
            path("/org/freedesktop/NetworkManager/IP4Config/1")
        }
        #[zbus(property)]
        fn ip6_config(&self) -> OwnedObjectPath {
            path("/")
        }
    }

    struct Wireless(AtomicI64);

    #[zbus::interface(name = "org.freedesktop.NetworkManager.Device.Wireless")]
    impl Wireless {
        async fn request_scan(
            &self,
            _options: HashMap<String, OwnedValue>,
            #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        ) -> zbus::fdo::Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);

            self.last_scan_changed(&emitter)
                .await
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
        }

        fn get_all_access_points(&self) -> Vec<OwnedObjectPath> {
            vec![path(AP)]
        }
        #[zbus(property)]
        fn last_scan(&self) -> i64 {
            self.0.load(Ordering::SeqCst)
        }
        #[zbus(property)]
        fn active_access_point(&self) -> OwnedObjectPath {
            path(AP)
        }
    }

    struct AccessPoint;

    #[zbus::interface(name = "org.freedesktop.NetworkManager.AccessPoint")]
    impl AccessPoint {
        #[zbus(property)]
        fn ssid(&self) -> Vec<u8> {
            b"Test".to_vec()
        }
        #[zbus(property)]
        fn strength(&self) -> u8 {
            73
        }
        #[zbus(property)]
        fn flags(&self) -> u32 {
            1
        }
        #[zbus(property)]
        fn wpa_flags(&self) -> u32 {
            0x100
        }
        #[zbus(property)]
        fn rsn_flags(&self) -> u32 {
            0x100
        }
    }

    struct SavedSettings;

    struct IpConfig;

    #[zbus::interface(name = "org.freedesktop.NetworkManager.IP4Config")]
    impl IpConfig {
        #[zbus(property)]
        fn address_data(&self) -> Vec<HashMap<String, OwnedValue>> {
            vec![HashMap::from([
                ("address".into(), text("192.0.2.1")),
                ("prefix".into(), OwnedValue::from(24u32)),
            ])]
        }
    }

    #[zbus::interface(name = "org.freedesktop.NetworkManager.Settings")]
    impl SavedSettings {
        fn list_connections(&self) -> Vec<OwnedObjectPath> {
            vec![path(PROFILE)]
        }
    }

    struct SavedProfile(Arc<Mutex<Calls>>);

    #[zbus::interface(name = "org.freedesktop.NetworkManager.Settings.Connection")]
    impl SavedProfile {
        fn get_settings(&self) -> Settings {
            HashMap::from([
                (
                    "connection".into(),
                    HashMap::from([
                        ("id".into(), text("Test")),
                        ("type".into(), text("802-11-wireless")),
                        ("uuid".into(), text("f055e87d-8da3-4c74-bf22-892edb414030")),
                    ]),
                ),
                (
                    "802-11-wireless".into(),
                    HashMap::from([(
                        "ssid".into(),
                        Value::from(b"Test".to_vec()).try_into().unwrap(),
                    )]),
                ),
                (
                    "802-11-wireless-security".into(),
                    HashMap::from([("key-mgmt".into(), text("wpa-psk"))]),
                ),
            ])
        }

        fn update(&self, settings: Settings) {
            self.0.lock().unwrap().updated.push(settings);
        }

        fn delete(&self) {
            self.0.lock().unwrap().deletes += 1;
        }
    }

    struct Active;

    #[zbus::interface(name = "org.freedesktop.NetworkManager.Connection.Active")]
    impl Active {
        #[zbus(property)]
        fn id(&self) -> &str {
            "Test"
        }
        #[zbus(property)]
        fn state(&self) -> u32 {
            2
        }
    }

    fn path(value: &str) -> OwnedObjectPath {
        OwnedObjectPath::try_from(value).unwrap()
    }

    fn network() -> WifiNetwork {
        WifiNetwork {
            ssid: b"Test".to_vec(),
            name: "Test".into(),
            security: Security::Psk,
            strength: 73,
            device: DEVICE.into(),
            access_point: AP.into(),
            active: false,
            profile: None,
        }
    }

    #[tokio::test]
    async fn recovers_after_service_start_device_hotplug_and_owner_restart() {
        use crate::features::availability::{
            Availability, AvailabilityPublisher, UnavailableReason, tests::wait_for,
        };

        let bus = crate::backend::dbus::tests::Bus::new().await;
        let publisher = AvailabilityPublisher::default();
        let mut readiness = publisher.subscribe();
        let (backend, _events) = Backend::start(publisher);

        wait_for(
            &mut readiness,
            Availability::Unavailable(UnavailableReason::ServiceMissing),
        )
        .await;

        let calls = Arc::new(Mutex::new(Calls {
            wifi_enabled: true,
            devices_absent: true,
            ..Calls::default()
        }));

        let service = bus.connect().await;
        let server = service.object_server();
        server.at(ROOT, Manager(calls.clone())).await.unwrap();
        server.at(DEVICE, Device(calls.clone())).await.unwrap();
        server
            .at(DEVICE, Wireless(AtomicI64::new(1)))
            .await
            .unwrap();
        server.at(AP, AccessPoint).await.unwrap();
        server
            .at("/org/freedesktop/NetworkManager/IP4Config/1", IpConfig)
            .await
            .unwrap();
        server
            .at("/org/freedesktop/NetworkManager/Settings", SavedSettings)
            .await
            .unwrap();
        server
            .at(PROFILE, SavedProfile(calls.clone()))
            .await
            .unwrap();
        server.at(ACTIVE, Active).await.unwrap();
        service.request_name(SERVICE).await.unwrap();
        wait_for(
            &mut readiness,
            Availability::Unavailable(UnavailableReason::DeviceMissing),
        )
        .await;

        calls.lock().unwrap().devices_absent = false;

        let emitter = SignalEmitter::new(&service, ROOT).unwrap();

        Manager::device_added(emitter.clone(), path(DEVICE))
            .await
            .unwrap();
        wait_for(&mut readiness, Availability::Available).await;
        calls.lock().unwrap().devices_absent = true;
        Manager::device_removed(emitter.clone(), path(DEVICE))
            .await
            .unwrap();
        wait_for(
            &mut readiness,
            Availability::Unavailable(UnavailableReason::DeviceMissing),
        )
        .await;

        service.release_name(SERVICE).await.unwrap();
        wait_for(
            &mut readiness,
            Availability::Unavailable(UnavailableReason::ServiceMissing),
        )
        .await;
        calls.lock().unwrap().devices_absent = false;
        service.request_name(SERVICE).await.unwrap();
        wait_for(&mut readiness, Availability::Available).await;
        assert!(!backend.is_busy());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn scans_reads_profiles_and_dispatches_operations_on_a_private_bus() {
        let bus = crate::backend::dbus::tests::Bus::new().await;

        let calls = Arc::new(Mutex::new(Calls {
            wifi_enabled: true,
            ..Calls::default()
        }));

        let service = bus.connect().await;
        let server = service.object_server();
        server.at(ROOT, Manager(calls.clone())).await.unwrap();
        server.at(DEVICE, Device(calls.clone())).await.unwrap();
        server
            .at(DEVICE, Wireless(AtomicI64::new(1)))
            .await
            .unwrap();
        server.at(AP, AccessPoint).await.unwrap();
        server
            .at("/org/freedesktop/NetworkManager/IP4Config/1", IpConfig)
            .await
            .unwrap();
        server
            .at("/org/freedesktop/NetworkManager/Settings", SavedSettings)
            .await
            .unwrap();
        server
            .at(PROFILE, SavedProfile(calls.clone()))
            .await
            .unwrap();
        server.at(ACTIVE, Active).await.unwrap();
        service.request_name(SERVICE).await.unwrap();

        let client = bus.connect().await;

        let mut owners = ServiceChanges::new(&client, SERVICE).await.unwrap();
        let probe = client.clone();
        let client = Client::new(client).await.unwrap();
        let mut snapshot = client.snapshot().await.unwrap();

        assert_eq!(snapshot.connection_kind, Some(ConnectionKind::Wifi));
        assert_eq!(snapshot.devices[0].state, DeviceState::Connected);
        assert_eq!(snapshot.networks.len(), 1);
        assert!(snapshot.networks[0].active);
        assert_eq!(snapshot.networks[0].profile.as_deref(), Some(PROFILE));
        assert_eq!(snapshot.networks[0].strength, 73);
        assert_eq!(snapshot.devices[0].interface, "wlan-test");
        assert_eq!(snapshot.devices[0].addresses, ["192.0.2.1"]);
        assert_eq!(snapshot.profiles[0].ssid, b"Test");

        client.apply(Command::SetWifi(false)).await.unwrap();

        assert!(!client.snapshot().await.unwrap().wifi_enabled);

        client.apply(Command::SetWifi(true)).await.unwrap();

        assert!(
            client
                .apply(Command::SetWired(true))
                .await
                .unwrap_err()
                .to_string()
                .contains("No wired device")
        );

        client.apply(Command::Scan).await.unwrap();

        let mut saved_network = network();
        saved_network.profile = Some(PROFILE.into());
        client
            .apply(Command::Connect(
                saved_network.clone(),
                Some(Password("replacement-password".into())),
            ))
            .await
            .unwrap();

        calls.lock().unwrap().deny_activation = true;

        let denied = client
            .apply(Command::Connect(saved_network, None))
            .await
            .unwrap_err();

        assert!(denied.to_string().contains("AccessDenied"));

        calls.lock().unwrap().deny_activation = false;
        client
            .apply(Command::Connect(
                network(),
                Some(Password("test-password".into())),
            ))
            .await
            .unwrap();

        client
            .apply(Command::Connect(snapshot.networks.remove(0), None))
            .await
            .unwrap();

        client
            .apply(Command::Disconnect(DEVICE.into()))
            .await
            .unwrap();

        client.apply(Command::Forget(PROFILE.into())).await.unwrap();

        {
            let records = calls.lock().unwrap();

            assert_eq!(records.created.len(), 1);
            assert_eq!(
                string(&records.created[0], "802-11-wireless-security", "key-mgmt").as_deref(),
                Some("wpa-psk")
            );
            assert_eq!(records.activations, [PROFILE, PROFILE]);
            assert_eq!(records.updated.len(), 1);
            assert_eq!(
                string(&records.updated[0], "connection", "uuid").as_deref(),
                Some("f055e87d-8da3-4c74-bf22-892edb414030")
            );
            assert_eq!(
                string(&records.updated[0], "802-11-wireless-security", "psk").as_deref(),
                Some("replacement-password")
            );
            assert_eq!(records.disconnects, 1);
            assert_eq!(records.deletes, 1);
        }

        let (backend, mut commands) = Backend::test_channel();
        let pending = backend.pending.clone();
        let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let worker = tokio::spawn(async move {
            Backend::listen(
                &client,
                &mut commands,
                &events,
                &pending,
                &AvailabilityPublisher::default(),
                &mut ReconnectBackoff::default(),
            )
            .await
        });

        assert!(matches!(
            timeout(Duration::from_secs(3), receiver.recv())
                .await
                .unwrap(),
            Some(Event::Updated(_)),
        ));

        assert!(backend.scan());

        timeout(Duration::from_secs(3), async {
            while let Some(event) = receiver.recv().await {
                if matches!(event, Event::Busy(false)) {
                    return;
                }
            }

            panic!("Backend event stream closed");
        })
        .await
        .unwrap();

        assert!(!backend.is_busy());

        calls.lock().unwrap().deny_activation = true;

        let mut saved_network = network();
        saved_network.profile = Some(PROFILE.into());

        assert!(backend.connect(saved_network, None));

        let mut denied = false;

        timeout(Duration::from_secs(3), async {
            while let Some(event) = receiver.recv().await {
                match event {
                    Event::Error(message) => denied |= message.contains("AccessDenied"),
                    Event::Busy(false) => return,
                    _ => {}
                }
            }

            panic!("Backend event stream closed");
        })
        .await
        .unwrap();

        assert!(denied);
        assert!(!backend.is_busy());

        service.release_name(SERVICE).await.unwrap();

        let disconnected = timeout(Duration::from_secs(3), worker)
            .await
            .unwrap()
            .unwrap();

        assert!(disconnected.is_err());

        timeout(Duration::from_secs(3), owners.changed())
            .await
            .unwrap()
            .unwrap();

        service.request_name(SERVICE).await.unwrap();
        timeout(Duration::from_secs(3), owners.changed())
            .await
            .unwrap()
            .unwrap();

        assert!(Client::new(probe).await.unwrap().snapshot().await.is_ok());

        drop(service);
    }

    #[test]
    fn connection_settings_validate_passwords_and_omit_security_for_open_networks() {
        let mut network = network();

        for password in ["a".repeat(8), "a".repeat(63), "f".repeat(64)] {
            assert!(Client::connection_settings(&network, Some(&password)).is_ok());
        }

        for password in [
            "a".repeat(7),
            "a".repeat(65),
            "g".repeat(64),
            "пароль123".into(),
        ] {
            assert!(Client::connection_settings(&network, Some(&password)).is_err());
        }

        assert!(Client::connection_settings(&network, None).is_err());

        network.security = Security::Open;
        assert!(
            !Client::connection_settings(&network, None)
                .unwrap()
                .contains_key("802-11-wireless-security")
        );
    }
}
