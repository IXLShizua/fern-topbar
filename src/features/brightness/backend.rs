use crate::backend::reconnect::ReconnectBackoff;
use crate::runtime::Task;
use crate::{
    backend::dbus::{self, ServiceChanges, probe},
    features::availability::{Availability, AvailabilityPublisher, UnavailableReason},
};
use std::future::pending;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

#[derive(Clone, Debug, PartialEq)]
pub struct Brightness {
    pub percent: u8,
    pub device: String,
    pub max: u32,
}

pub enum Command {
    SetBrightness(u8),
    AdjustBrightness(i8),
}

#[derive(Clone)]
pub struct Controls {
    commands: UnboundedSender<Command>,
}

impl Controls {
    pub fn set(&self, command: Command) {
        let _ = self.commands.send(command);
    }
}

pub struct Backend {
    controls: Controls,
    events: Option<UnboundedReceiver<Option<Brightness>>>,
    _task: Task,
}

impl Backend {
    pub fn start(availability: AvailabilityPublisher) -> Self {
        let (command_sender, commands) = tokio::sync::mpsc::unbounded_channel();
        let (event_sender, events) = tokio::sync::mpsc::unbounded_channel();

        Self {
            controls: Controls {
                commands: command_sender,
            },
            events: Some(events),
            _task: Task::spawn(BrightnessDriver::new(commands, event_sender, availability).run()),
        }
    }

    pub fn controls(&self) -> Controls {
        self.controls.clone()
    }

    pub fn take_events(&mut self) -> UnboundedReceiver<Option<Brightness>> {
        self.events.take().expect("brightness events taken once")
    }
}

struct BrightnessDriver {
    commands: UnboundedReceiver<Command>,
    events: UnboundedSender<Option<Brightness>>,
    device: Option<system::BacklightDevice>,
    previous: Option<Brightness>,
    availability: AvailabilityPublisher,
}

impl BrightnessDriver {
    fn new(
        commands: UnboundedReceiver<Command>,
        events: UnboundedSender<Option<Brightness>>,
        availability: AvailabilityPublisher,
    ) -> Self {
        Self {
            commands,
            events,
            device: None,
            previous: None,
            availability,
        }
    }

    async fn run(mut self) {
        let mut events = None;
        let mut owners = None;
        let mut retry = ReconnectBackoff::default();

        loop {
            if events.is_none() {
                match system::BacklightEvents::connect() {
                    Ok(stream) => events = Some(stream),
                    Err(error) => tracing::debug!(%error, "cannot monitor backlight changes"),
                }
            }

            if owners.is_none()
                && let Ok(connection) = probe(dbus::system()).await
            {
                owners = probe(ServiceChanges::new(&connection, "org.freedesktop.login1"))
                    .await
                    .ok();
            }

            if !self.refresh().await {
                return;
            }

            let retrying =
                events.is_none() || owners.is_none() || !self.availability.current().is_available();

            if !retrying {
                retry.reset();
            }

            tokio::select! {
                command = self.commands.recv() => match command {
                    Some(command) => self.apply(command).await,
                    None => return,
                },
                event = async {
                    match events.as_ref() {
                        Some(events) => events.changed().await,
                        None => pending().await,
                    }
                } => {
                    if let Err(error) = event {
                        tracing::debug!(%error, "backlight event stream stopped");
                        events = None;
                    }
                }
                changed = async {
                    match owners.as_mut() {
                        Some(owners) => owners.changed().await,
                        None => pending().await,
                    }
                } => {
                    if changed.is_err() {
                        owners = None;
                    }

                    retry.reset();
                }
                _ = tokio::time::sleep(retry.next_delay()), if retrying => {}
                _ = self.events.closed() => return,
            }
        }
    }

    async fn refresh(&mut self) -> bool {
        let result = async {
            self.device = system::BacklightDevice::discover()?;

            let Some(device) = &self.device else {
                return Err(Availability::Unavailable(UnavailableReason::DeviceMissing));
            };
            let current = device.read()?;

            probe(system::login_available()).await?;

            Ok(current)
        }
        .await;

        let current = match result {
            Ok(current) => {
                self.availability.set(Availability::Available);

                Some(current)
            }
            Err(state) => {
                self.availability.set(state);

                None
            }
        };

        if current == self.previous {
            return true;
        }

        self.previous = current.clone();

        self.events.send(current).is_ok()
    }

    async fn apply(&self, command: Command) {
        let Some(device) = self.device.as_ref() else {
            return;
        };
        let current = match device.read() {
            Ok(current) => current,
            Err(state) => {
                self.availability.set(state);
                return;
            }
        };

        let percent = match command {
            Command::SetBrightness(value) => value,
            Command::AdjustBrightness(delta) => current.percent.saturating_add_signed(delta),
        }
        .clamp(1, 100);

        let result = probe(device.set_percent(percent)).await;

        if let Err(error) = result {
            tracing::warn!(state = ?error, "cannot set brightness");
        }
    }
}

mod system {
    use super::{Availability, Brightness, UnavailableReason};
    use crate::{backend::dbus, features::availability::ProbeError};
    use std::fs;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::path::{Path, PathBuf};
    use tokio::io::unix::AsyncFd;

    #[zbus::proxy(
        interface = "org.freedesktop.login1.Session",
        default_service = "org.freedesktop.login1",
        default_path = "/org/freedesktop/login1/session/auto"
    )]
    trait LoginSession {
        fn set_brightness(&self, subsystem: &str, name: &str, brightness: u32) -> zbus::Result<()>;

        #[zbus(property)]
        fn id(&self) -> zbus::Result<String>;
    }

    pub async fn login_available() -> zbus::Result<()> {
        let connection = dbus::system().await?;
        let session = LoginSessionProxy::builder(&connection)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await?;
        session.id().await?;

        Ok(())
    }

    pub struct BacklightEvents {
        socket: AsyncFd<OwnedFd>,
    }

    impl BacklightEvents {
        pub fn connect() -> io::Result<Self> {
            // SAFETY: socket returns a new descriptor, and every argument is a
            // valid Linux netlink constant. Ownership is transferred to OwnedFd
            // exactly once after the error check.
            let raw_fd = unsafe {
                libc::socket(
                    libc::AF_NETLINK,
                    libc::SOCK_RAW | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                    libc::NETLINK_KOBJECT_UEVENT,
                )
            };

            if raw_fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: raw_fd was created successfully above and is not owned by
            // any other Rust value.
            let descriptor = unsafe { OwnedFd::from_raw_fd(raw_fd) };
            // SAFETY: zero is a valid baseline for sockaddr_nl; the initialized
            // fields select the kernel-assigned port and kobject multicast group.
            let mut address: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
            address.nl_family = libc::AF_NETLINK as libc::sa_family_t;
            address.nl_pid = 0;
            address.nl_groups = 1;
            // SAFETY: address points to a fully initialized sockaddr_nl and the
            // descriptor remains alive for the entire call.
            let result = unsafe {
                libc::bind(
                    descriptor.as_raw_fd(),
                    (&raw const address).cast(),
                    std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
                )
            };

            if result < 0 {
                return Err(io::Error::last_os_error());
            }

            Ok(Self {
                socket: AsyncFd::new(descriptor)?,
            })
        }

        pub async fn changed(&self) -> io::Result<()> {
            let mut buffer = [0_u8; 8192];

            loop {
                let mut ready = self.socket.readable().await?;
                let received = ready.try_io(|descriptor| {
                    // SAFETY: buffer is valid for writes of its full length and
                    // recv does not retain either pointer after returning.
                    let size = unsafe {
                        libc::recv(
                            descriptor.get_ref().as_raw_fd(),
                            buffer.as_mut_ptr().cast(),
                            buffer.len(),
                            0,
                        )
                    };

                    if size < 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(size as usize)
                    }
                });

                let Ok(received) = received else {
                    continue;
                };
                let size = received?;

                if Self::is_backlight_change(&buffer[..size]) {
                    return Ok(());
                }
            }
        }

        fn is_backlight_change(event: &[u8]) -> bool {
            event
                .split(|byte| *byte == 0)
                .any(|field| field == b"SUBSYSTEM=backlight")
        }
    }

    pub struct BacklightDevice {
        path: PathBuf,
        name: String,
        max: u32,
    }

    impl BacklightDevice {
        pub fn discover() -> Result<Option<Self>, Availability> {
            Self::discover_at(Path::new("/sys/class/backlight"))
        }

        fn discover_at(root: &Path) -> Result<Option<Self>, Availability> {
            let entries = match fs::read_dir(root) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(io_error(root, error)),
            };

            let mut failure = None;

            for entry in entries {
                let entry = entry.map_err(|error| io_error(root, error))?;
                let path = entry.path();
                let max = match read_number(&path.join("max_brightness")) {
                    Ok(0) => continue,
                    Ok(max) => max,
                    Err(error) => {
                        failure = Some(error);
                        continue;
                    }
                };

                let max = u32::try_from(max).map_err(|error| {
                    tracing::debug!(%error, path = %path.display(), "invalid backlight maximum");

                    Availability::Failed(ProbeError::Protocol)
                })?;

                let name = entry.file_name().to_string_lossy().into_owned();

                return Ok(Some(Self { path, name, max }));
            }

            match failure {
                Some(error) => Err(error),
                None => Ok(None),
            }
        }

        pub fn read(&self) -> Result<Brightness, Availability> {
            let value = read_number(&self.path.join("brightness"))?;

            Ok(Brightness {
                percent: (value.saturating_mul(100) / u64::from(self.max)).min(100) as u8,
                device: self.name.clone(),
                max: self.max,
            })
        }

        pub async fn set_percent(&self, percent: u8) -> zbus::Result<()> {
            let value = u64::from(self.max) * u64::from(percent) / 100;
            let connection = dbus::system().await?;
            let session = LoginSessionProxy::new(&connection).await?;

            session
                .set_brightness("backlight", &self.name, value.max(1) as u32)
                .await
        }
    }

    fn read_number(path: &Path) -> Result<u64, Availability> {
        let value = fs::read_to_string(path).map_err(|error| io_error(path, error))?;
        value.trim().parse().map_err(|error| {
            tracing::debug!(%error, path = %path.display(), "invalid backlight value");

            Availability::Failed(ProbeError::Protocol)
        })
    }

    fn io_error(path: &Path, error: io::Error) -> Availability {
        tracing::debug!(%error, path = %path.display(), "cannot read backlight");

        match error.kind() {
            io::ErrorKind::NotFound => Availability::Unavailable(UnavailableReason::DeviceMissing),
            io::ErrorKind::PermissionDenied => Availability::Failed(ProbeError::PermissionDenied),
            _ => Availability::Failed(ProbeError::Read),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn distinguishes_missing_backlight_from_malformed_data_and_recovers_after_hotplug() {
            let root =
                std::env::temp_dir().join(format!("topbar-backlight-{}", std::process::id()));

            assert!(BacklightDevice::discover_at(&root).unwrap().is_none());

            let device_path = root.join("display");

            fs::create_dir_all(&device_path).unwrap();
            fs::write(device_path.join("max_brightness"), "invalid").unwrap();
            assert!(matches!(
                BacklightDevice::discover_at(&root),
                Err(Availability::Failed(ProbeError::Protocol))
            ));

            fs::write(device_path.join("max_brightness"), "200").unwrap();
            fs::write(device_path.join("brightness"), "100").unwrap();

            let device = BacklightDevice::discover_at(&root).unwrap().unwrap();

            assert_eq!(device.read().unwrap().percent, 50);
            fs::write(device_path.join("brightness"), "invalid").unwrap();
            assert_eq!(
                device.read(),
                Err(Availability::Failed(ProbeError::Protocol))
            );
            fs::remove_dir_all(&root).unwrap();
            assert_eq!(
                device.read(),
                Err(Availability::Unavailable(UnavailableReason::DeviceMissing))
            );
            assert_eq!(
                io_error(&root, io::ErrorKind::PermissionDenied.into()),
                Availability::Failed(ProbeError::PermissionDenied)
            );
        }

        #[test]
        fn backlight_events_include_hotplug_and_ignore_other_subsystems() {
            for action in ["change", "add", "remove"] {
                let event = format!("ACTION={action}\0SUBSYSTEM=backlight\0");

                assert!(BacklightEvents::is_backlight_change(event.as_bytes()));
            }

            assert!(!BacklightEvents::is_backlight_change(
                b"ACTION=change\0SUBSYSTEM=power_supply\0"
            ));
        }
    }
}
