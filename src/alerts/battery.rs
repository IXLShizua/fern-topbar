//! UPower warning rules; delivery and acknowledgments belong to the alert service.

use super::{Alert, AlertHandle};
use crate::backend::{
    battery::{Battery, WarningLevel},
    notifications::Urgency,
};
use tokio::sync::watch;

/// Application-owned observer; warning lifetime does not depend on widget mounting.
pub struct BatteryAlerts {
    updates: watch::Receiver<Option<Battery>>,
    warning: AlertHandle,
    policy: WarningPolicy,
}

impl BatteryAlerts {
    pub fn new(updates: watch::Receiver<Option<Battery>>, warning: AlertHandle) -> Self {
        Self {
            updates,
            warning,
            policy: WarningPolicy::default(),
        }
    }

    pub async fn run(mut self) {
        loop {
            let change = self
                .policy
                .observe(self.updates.borrow_and_update().as_ref());

            match change {
                Some(Change::Show(warning)) => self.warning.show(warning.alert()),
                Some(Change::Update(warning)) => self.warning.update(warning.alert()),
                Some(Change::Clear) => self.warning.clear(),
                None => {}
            }

            if self.updates.changed().await.is_err() {
                return;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Warning {
    level: WarningLevel,
    percent: u8,
}

impl Warning {
    fn alert(self) -> Alert {
        Alert {
            icon: "battery-caution-symbolic".into(),
            summary: match self.level {
                WarningLevel::Low => "Low battery",
                WarningLevel::Critical => "Critically low battery",
                _ => "Battery almost empty",
            }
            .into(),
            body: format!("{}% remaining. Connect a power supply.", self.percent),
            urgency: if self.level == WarningLevel::Low {
                Urgency::Normal
            } else {
                Urgency::Critical
            },
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Change {
    Show(Warning),
    Update(Warning),
    Clear,
}

#[derive(Default)]
struct WarningPolicy {
    target: Option<Warning>,
    // Highest requested level in this discharge cycle. The generic service retains
    // pending delivery and user dismissal; only external power rearms this policy.
    announced: WarningLevel,
}

impl WarningPolicy {
    fn observe(&mut self, battery: Option<&Battery>) -> Option<Change> {
        // Missing data during UPower recovery is not evidence of restored power.
        let battery = battery?;
        let next = if !battery.on_battery {
            self.announced = WarningLevel::None;

            None
        } else {
            match battery.warning_level {
                WarningLevel::Unknown => return None,
                WarningLevel::Low | WarningLevel::Critical | WarningLevel::Action => {
                    Some(Warning {
                        level: battery.warning_level,
                        percent: battery.percent,
                    })
                }
                WarningLevel::None | WarningLevel::Discharging => None,
            }
        };

        if self.target == next {
            return None;
        }

        self.target = next;

        match next {
            Some(warning) if warning.level > self.announced => {
                self.announced = warning.level;

                Some(Change::Show(warning))
            }
            Some(warning) => Some(Change::Update(warning)),
            None => Some(Change::Clear),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::battery::PowerState;

    fn battery(level: WarningLevel, percent: u8, on_battery: bool) -> Battery {
        Battery {
            percent,
            watts: Some(8.0),
            power_state: PowerState::Discharging,
            warning_level: level,
            on_battery,
        }
    }

    #[test]
    fn updates_charge_quietly_and_requests_attention_only_for_new_danger_levels() {
        let mut policy = WarningPolicy::default();
        let mut status = battery(WarningLevel::Low, 15, true);

        assert!(matches!(
            policy.observe(Some(&status)),
            Some(Change::Show(_))
        ));
        assert_eq!(policy.observe(Some(&status)), None);
        status.watts = Some(25.0);
        assert_eq!(policy.observe(Some(&status)), None);
        status.percent = 14;
        assert!(matches!(
            policy.observe(Some(&status)),
            Some(Change::Update(Warning { percent: 14, .. }))
        ));
        status.warning_level = WarningLevel::Critical;
        assert!(matches!(
            policy.observe(Some(&status)),
            Some(Change::Show(_))
        ));
        status.warning_level = WarningLevel::Action;
        assert!(matches!(
            policy.observe(Some(&status)),
            Some(Change::Show(_))
        ));
    }

    #[test]
    fn unknown_snapshots_and_fluctuations_do_not_rearm_a_discharge_cycle() {
        let mut policy = WarningPolicy::default();
        policy.observe(Some(&battery(WarningLevel::Low, 15, true)));
        assert_eq!(policy.observe(None), None);
        assert_eq!(
            policy.observe(Some(&battery(WarningLevel::Unknown, 0, true))),
            None
        );
        assert_eq!(
            policy.observe(Some(&battery(WarningLevel::None, 16, true))),
            Some(Change::Clear)
        );
        assert!(matches!(
            policy.observe(Some(&battery(WarningLevel::Low, 15, true))),
            Some(Change::Update(_))
        ));
        assert!(matches!(
            policy.observe(Some(&battery(WarningLevel::Critical, 5, true))),
            Some(Change::Show(_))
        ));
    }

    #[test]
    fn external_power_resolves_and_rearms_even_when_charging_is_paused() {
        let mut policy = WarningPolicy::default();

        assert_eq!(
            policy.observe(Some(&battery(WarningLevel::Low, 15, false))),
            None
        );
        policy.observe(Some(&battery(WarningLevel::Critical, 5, true)));
        assert_eq!(
            policy.observe(Some(&battery(WarningLevel::Critical, 5, false))),
            Some(Change::Clear)
        );
        assert_eq!(
            policy.observe(Some(&battery(WarningLevel::Critical, 5, false))),
            None
        );
        assert!(matches!(
            policy.observe(Some(&battery(WarningLevel::Low, 15, true))),
            Some(Change::Show(_))
        ));
    }

    #[derive(Debug)]
    enum Recorded {
        Notification {
            id: u32,
            replaces: u32,
            urgency: u8,
            timeout: i32,
            body: String,
        },
        Close(u32),
    }

    struct DesktopServer {
        events: tokio::sync::mpsc::UnboundedSender<Recorded>,
        next_id: std::sync::atomic::AtomicU32,
    }

    #[zbus::interface(name = "org.freedesktop.Notifications")]
    impl DesktopServer {
        #[allow(
            clippy::too_many_arguments,
            reason = "Notify's signature is defined by D-Bus"
        )]
        fn notify(
            &self,
            _app_name: &str,
            replaces: u32,
            _icon: &str,
            _summary: &str,
            body: &str,
            _actions: Vec<String>,
            hints: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
            timeout: i32,
        ) -> u32 {
            let id = if replaces != 0 {
                replaces
            } else {
                self.next_id
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            };

            self.events
                .send(Recorded::Notification {
                    id,
                    replaces,
                    urgency: u8::try_from(hints.get("urgency").unwrap()).unwrap(),
                    timeout,
                    body: body.into(),
                })
                .unwrap();

            id
        }

        async fn close_notification(
            &self,
            id: u32,
            #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        ) {
            self.events.send(Recorded::Close(id)).unwrap();
            Self::notification_closed(emitter, id, 3).await.unwrap();
        }

        #[zbus(signal)]
        async fn notification_closed(
            emitter: zbus::object_server::SignalEmitter<'_>,
            id: u32,
            reason: u32,
        ) -> zbus::Result<()>;
    }

    async fn recorded(events: &mut tokio::sync::mpsc::UnboundedReceiver<Recorded>) -> Recorded {
        tokio::time::timeout(std::time::Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn warnings_work_without_widgets_and_respect_external_server_dismissals() {
        let bus = crate::backend::dbus::tests::Bus::new().await;
        let (updates, receiver) = watch::channel(Some(battery(WarningLevel::Low, 15, true)));
        let alerts = crate::alerts::service::AlertService::start();
        let worker =
            tokio::spawn(BatteryAlerts::new(receiver, alerts.publisher().register()).run());
        let connection = bus.connect().await;
        let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
        let path = "/org/freedesktop/Notifications";

        connection
            .object_server()
            .at(
                path,
                DesktopServer {
                    events,
                    next_id: std::sync::atomic::AtomicU32::new(1),
                },
            )
            .await
            .unwrap();
        connection
            .request_name("org.freedesktop.Notifications")
            .await
            .unwrap();

        assert!(
            matches!(recorded(&mut received).await, Recorded::Notification { id: 1, replaces: 0, urgency: 1, timeout: -1, body } if body.contains("15%"))
        );
        updates.send_replace(Some(battery(WarningLevel::Low, 14, true)));
        assert!(
            matches!(recorded(&mut received).await, Recorded::Notification { id: 1, replaces: 1, body, .. } if body.contains("14%"))
        );

        let emitter = zbus::object_server::SignalEmitter::new(&connection, path).unwrap();

        DesktopServer::notification_closed(emitter.clone(), 1, 2)
            .await
            .unwrap();
        // Signal emission completes before the receiving task consumes it.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(150), received.recv())
                .await
                .is_err()
        );
        updates.send_replace(Some(battery(WarningLevel::Low, 13, true)));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(150), received.recv())
                .await
                .is_err()
        );

        updates.send_replace(Some(battery(WarningLevel::Critical, 5, true)));
        assert!(matches!(
            recorded(&mut received).await,
            Recorded::Notification {
                id: 2,
                replaces: 0,
                urgency: 2,
                timeout: 0,
                ..
            }
        ));
        updates.send_replace(Some(battery(WarningLevel::Action, 2, true)));
        assert!(matches!(
            recorded(&mut received).await,
            Recorded::Notification {
                id: 2,
                replaces: 2,
                urgency: 2,
                timeout: 0,
                ..
            }
        ));

        updates.send_replace(Some(battery(WarningLevel::Action, 2, false)));
        assert!(matches!(recorded(&mut received).await, Recorded::Close(2)));
        updates.send_replace(Some(battery(WarningLevel::Low, 15, true)));
        assert!(matches!(
            recorded(&mut received).await,
            Recorded::Notification {
                id: 3,
                replaces: 0,
                urgency: 1,
                ..
            }
        ));

        drop(updates);
        tokio::time::timeout(std::time::Duration::from_secs(2), worker)
            .await
            .unwrap()
            .unwrap();
    }
}
