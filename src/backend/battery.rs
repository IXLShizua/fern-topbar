use crate::features::availability::AvailabilityPublisher;
use crate::{backend::dbus::Service, runtime::Task};
use tokio::sync::watch;

#[derive(Clone, Debug, PartialEq)]
pub struct Battery {
    pub percent: u8,
    pub watts: Option<f64>,
    pub power_state: PowerState,
    pub warning_level: WarningLevel,
    pub on_battery: bool,
}

/// The complete battery power state; an unknown state is not discharging.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PowerState {
    #[default]
    Unknown,
    Charging,
    Discharging,
    Empty,
    FullyCharged,
    PendingCharge,
    PendingDischarge,
}

impl PowerState {
    pub fn is_charging(self) -> bool {
        matches!(self, Self::Charging | Self::PendingCharge)
    }
}

/// UPower's system warning policy, separate from notification urgency.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum WarningLevel {
    #[default]
    Unknown,
    None,
    Discharging,
    Low,
    Critical,
    Action,
}

/// Shared latest snapshot for widgets and application-owned warning observation.
#[derive(Clone)]
pub struct BatteryState(watch::Receiver<Option<Battery>>);

impl Default for BatteryState {
    fn default() -> Self {
        Self(watch::channel(None).1)
    }
}

impl BatteryState {
    pub fn subscribe(&self) -> watch::Receiver<Option<Battery>> {
        self.0.clone()
    }
}

/// One UPower subscription owned by the application, independently of widget mounting.
pub struct Backend {
    pub state: BatteryState,
    _task: Task,
}

impl Backend {
    pub fn start(availability: AvailabilityPublisher) -> Self {
        let (event_sender, events) = watch::channel(None);

        Self {
            state: BatteryState(events),
            _task: Task::spawn(BatteryDriver::new(event_sender, availability).run()),
        }
    }
}

struct BatteryDriver {
    events: watch::Sender<Option<Battery>>,
    availability: AvailabilityPublisher,
}

impl BatteryDriver {
    fn new(events: watch::Sender<Option<Battery>>, availability: AvailabilityPublisher) -> Self {
        Self {
            events,
            availability,
        }
    }

    async fn run(self) {
        let mut service = Service::system("org.freedesktop.UPower", self.availability.clone());

        loop {
            let connection = tokio::select! {
                connection = service.connect() => connection,
                _ = self.events.closed() => return,
            };
            let result = tokio::select! {
                result = service.run(dbus::run(&connection, &self.events, &self.availability)) => result,
                _ = self.events.closed() => return,
            };

            if result.is_ok() {
                return;
            }

            publish(&self.events, None);
        }
    }
}

fn publish(events: &watch::Sender<Option<Battery>>, battery: Option<Battery>) {
    events.send_if_modified(|current| {
        if *current == battery {
            return false;
        }

        *current = battery;

        true
    });
}

mod dbus {
    use super::{Battery, PowerState, WarningLevel, publish};
    use crate::backend::dbus::probe;
    use crate::features::availability::{
        Availability, AvailabilityPublisher, PROBE_TIMEOUT, ProbeError, UnavailableReason,
    };
    use futures_util::StreamExt;
    use tokio::sync::watch;
    use zbus::Connection;

    const DISPLAY_DEVICE: &str = "/org/freedesktop/UPower/devices/DisplayDevice";

    #[zbus::proxy(
        interface = "org.freedesktop.UPower",
        default_service = "org.freedesktop.UPower",
        default_path = "/org/freedesktop/UPower"
    )]
    trait UPower {
        #[zbus(property)]
        fn on_battery(&self) -> zbus::Result<bool>;
    }

    #[zbus::proxy(
        interface = "org.freedesktop.UPower.Device",
        default_service = "org.freedesktop.UPower"
    )]
    trait UPowerDevice {
        #[zbus(property)]
        fn percentage(&self) -> zbus::Result<f64>;

        #[zbus(property)]
        fn energy_rate(&self) -> zbus::Result<f64>;

        #[zbus(property)]
        fn state(&self) -> zbus::Result<u32>;

        #[zbus(property)]
        fn warning_level(&self) -> zbus::Result<u32>;

        #[zbus(property)]
        fn is_present(&self) -> zbus::Result<bool>;
    }

    pub async fn run(
        connection: &Connection,
        events: &watch::Sender<Option<Battery>>,
        availability: &AvailabilityPublisher,
    ) -> Result<(), Availability> {
        let mut session = probe(BatterySession::connect(connection)).await?;
        session.run(events, availability).await
    }

    struct BatterySession<'a> {
        device: UPowerDeviceProxy<'a>,
        power: UPowerProxy<'a>,
    }

    impl<'a> BatterySession<'a> {
        async fn connect(connection: &'a Connection) -> zbus::Result<Self> {
            // Probe without GetAll/cache tasks before constructing the live subscription.
            let probe = UPowerDeviceProxy::builder(connection)
                .path(DISPLAY_DEVICE)?
                .cache_properties(zbus::proxy::CacheProperties::No)
                .build()
                .await?;
            probe.is_present().await?;

            let power_probe = UPowerProxy::builder(connection)
                .cache_properties(zbus::proxy::CacheProperties::No)
                .build()
                .await?;
            power_probe.on_battery().await?;

            let device = UPowerDeviceProxy::builder(connection)
                .path(DISPLAY_DEVICE)?
                .build()
                .await?;

            let power = UPowerProxy::new(connection).await?;

            Ok(Self { device, power })
        }

        async fn run(
            &mut self,
            events: &watch::Sender<Option<Battery>>,
            availability: &AvailabilityPublisher,
        ) -> Result<(), Availability> {
            let (percentage, energy_rate, state, present, warning, on_battery) =
                tokio::time::timeout(PROBE_TIMEOUT, async {
                    (
                        self.device.receive_percentage_changed().await,
                        self.device.receive_energy_rate_changed().await,
                        self.device.receive_state_changed().await,
                        self.device.receive_is_present_changed().await,
                        self.device.receive_warning_level_changed().await,
                        self.power.receive_on_battery_changed().await,
                    )
                })
                .await
                .map_err(|_| Availability::Failed(ProbeError::Timeout))?;

            let device_changes = futures_util::stream::select(
                futures_util::stream::select(percentage.map(|_| ()), energy_rate.map(|_| ())),
                futures_util::stream::select(state.map(|_| ()), present.map(|_| ())),
            );

            let mut changes = futures_util::stream::select(
                device_changes,
                futures_util::stream::select(warning.map(|_| ()), on_battery.map(|_| ())),
            );

            loop {
                let battery = probe(self.snapshot()).await?;

                availability.set(if battery.is_some() {
                    Availability::Available
                } else {
                    Availability::Unavailable(UnavailableReason::DeviceMissing)
                });

                publish(events, battery);

                if events.is_closed() {
                    return Ok(());
                }

                if changes.next().await.is_none() {
                    return Err(Availability::Failed(ProbeError::Connect));
                }
            }
        }

        async fn snapshot(&self) -> zbus::Result<Option<Battery>> {
            if !self.device.is_present().await? {
                return Ok(None);
            }

            let percent = self.device.percentage().await?.round().clamp(0.0, 100.0) as u8;

            let watts = self
                .device
                .energy_rate()
                .await
                .ok()
                // UPower may use a negative rate for charging; zero means unset.
                .filter(|watts| watts.is_finite() && *watts != 0.0)
                .map(f64::abs);

            let power_state = match self.device.state().await? {
                1 => PowerState::Charging,
                2 => PowerState::Discharging,
                3 => PowerState::Empty,
                4 => PowerState::FullyCharged,
                5 => PowerState::PendingCharge,
                6 => PowerState::PendingDischarge,
                _ => PowerState::Unknown,
            };

            let warning_level = match self.device.warning_level().await? {
                1 => WarningLevel::None,
                2 => WarningLevel::Discharging,
                3 => WarningLevel::Low,
                4 => WarningLevel::Critical,
                5 => WarningLevel::Action,
                _ => WarningLevel::Unknown,
            };

            Ok(Some(Battery {
                percent,
                watts,
                power_state,
                warning_level,
                on_battery: self.power.on_battery().await?,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::availability::{Availability, UnavailableReason, tests::wait_for};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    };

    struct Device(Arc<AtomicBool>, Arc<AtomicU32>);

    struct Power(Arc<AtomicBool>);

    #[zbus::interface(name = "org.freedesktop.UPower")]
    impl Power {
        #[zbus(property)]
        fn on_battery(&self) -> bool {
            self.0.load(Ordering::Acquire)
        }
    }

    #[zbus::interface(name = "org.freedesktop.UPower.Device")]
    impl Device {
        #[zbus(property)]
        fn is_present(&self) -> bool {
            self.0.load(Ordering::Acquire)
        }
        #[zbus(property)]
        fn percentage(&self) -> f64 {
            76.0
        }
        #[zbus(property)]
        fn energy_rate(&self) -> f64 {
            8.4
        }
        #[zbus(property)]
        fn state(&self) -> u32 {
            2
        }

        #[zbus(property)]
        fn warning_level(&self) -> u32 {
            self.1.load(Ordering::Acquire)
        }
    }

    #[tokio::test]
    async fn appears_after_startup_tracks_hotplug_and_recovers_service_ownership() {
        let bus = crate::backend::dbus::tests::Bus::new().await;
        let publisher = AvailabilityPublisher::default();
        let mut readiness = publisher.subscribe();
        let backend = Backend::start(publisher);
        let mut events = backend.state.subscribe();

        wait_for(
            &mut readiness,
            Availability::Unavailable(UnavailableReason::ServiceMissing),
        )
        .await;

        let present = Arc::new(AtomicBool::new(false));
        let warning = Arc::new(AtomicU32::new(1));
        let on_battery = Arc::new(AtomicBool::new(true));
        let service = bus.connect().await;

        const PATH: &str = "/org/freedesktop/UPower/devices/DisplayDevice";
        service
            .object_server()
            .at(PATH, Device(present.clone(), warning.clone()))
            .await
            .unwrap();
        service
            .object_server()
            .at("/org/freedesktop/UPower", Power(on_battery.clone()))
            .await
            .unwrap();
        service
            .request_name("org.freedesktop.UPower")
            .await
            .unwrap();
        wait_for(
            &mut readiness,
            Availability::Unavailable(UnavailableReason::DeviceMissing),
        )
        .await;
        present.store(true, Ordering::Release);

        let interface = service
            .object_server()
            .interface::<_, Device>(PATH)
            .await
            .unwrap();

        interface
            .get()
            .await
            .is_present_changed(interface.signal_emitter())
            .await
            .unwrap();
        wait_for(&mut readiness, Availability::Available).await;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while events.borrow_and_update().is_none() {
                events.changed().await.unwrap();
            }
        })
        .await
        .unwrap();

        let battery = events.borrow_and_update().clone().unwrap();

        assert_eq!(battery.percent, 76);
        assert_eq!(battery.watts, Some(8.4));
        assert_eq!(battery.power_state, PowerState::Discharging);
        assert_eq!(battery.warning_level, WarningLevel::None);
        assert!(battery.on_battery);

        let mut other = backend.state.subscribe();

        assert_eq!(other.borrow_and_update().as_ref(), Some(&battery));

        warning.store(3, Ordering::Release);
        interface
            .get()
            .await
            .warning_level_changed(interface.signal_emitter())
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while events.borrow_and_update().as_ref().unwrap().warning_level != WarningLevel::Low {
                events.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        other.changed().await.unwrap();
        assert_eq!(
            other.borrow_and_update().as_ref().unwrap().warning_level,
            WarningLevel::Low
        );
        assert_eq!(
            backend
                .state
                .subscribe()
                .borrow()
                .as_ref()
                .unwrap()
                .warning_level,
            WarningLevel::Low
        );

        on_battery.store(false, Ordering::Release);

        let power = service
            .object_server()
            .interface::<_, Power>("/org/freedesktop/UPower")
            .await
            .unwrap();

        power
            .get()
            .await
            .on_battery_changed(power.signal_emitter())
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while events.borrow_and_update().as_ref().unwrap().on_battery {
                events.changed().await.unwrap();
            }
        })
        .await
        .unwrap();

        present.store(false, Ordering::Release);
        interface
            .get()
            .await
            .is_present_changed(interface.signal_emitter())
            .await
            .unwrap();
        wait_for(
            &mut readiness,
            Availability::Unavailable(UnavailableReason::DeviceMissing),
        )
        .await;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while events.borrow_and_update().is_some() {
                events.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        service
            .release_name("org.freedesktop.UPower")
            .await
            .unwrap();
        wait_for(
            &mut readiness,
            Availability::Unavailable(UnavailableReason::ServiceMissing),
        )
        .await;
        present.store(true, Ordering::Release);
        service
            .request_name("org.freedesktop.UPower")
            .await
            .unwrap();
        wait_for(&mut readiness, Availability::Available).await;
    }
}
