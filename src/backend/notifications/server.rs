use crate::features::availability::{Availability, AvailabilityPublisher};
use crate::runtime::Task;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use super::{Event, Notification, Urgency};

#[derive(Clone)]
pub struct Controls {
    commands: UnboundedSender<Command>,
    active: Arc<Mutex<HashSet<u32>>>,
}

impl Controls {
    pub fn close(&self, id: u32, reason: u32) {
        if !self.active.lock().unwrap().remove(&id) {
            return;
        }

        let _ = self.commands.send(Command::Close { id, reason });
    }

    pub fn invoke_default(&self, id: u32, close_after: bool) {
        let mut active = self.active.lock().unwrap();

        if !active.contains(&id) {
            return;
        }

        if close_after {
            active.remove(&id);
        }

        let _ = self
            .commands
            .send(Command::InvokeDefault { id, close_after });
    }
}

pub struct Backend {
    controls: Controls,
    events: Option<UnboundedReceiver<Event>>,
    _task: Task,
}

impl Backend {
    pub fn start(availability: AvailabilityPublisher) -> Self {
        let active = Arc::new(Mutex::new(HashSet::new()));
        let (event_sender, events) = tokio::sync::mpsc::unbounded_channel();
        let (commands, command_receiver) = tokio::sync::mpsc::unbounded_channel();
        let task = Task::spawn(dbus::serve(
            event_sender,
            active.clone(),
            command_receiver,
            availability,
        ));

        Self {
            controls: Controls { commands, active },
            events: Some(events),
            _task: task,
        }
    }

    pub fn controls(&self) -> Controls {
        self.controls.clone()
    }

    pub fn take_events(&mut self) -> UnboundedReceiver<Event> {
        self.events.take().expect("notification events taken once")
    }
}

enum Command {
    Close { id: u32, reason: u32 },
    InvokeDefault { id: u32, close_after: bool },
}

mod dbus {
    use super::{Availability, AvailabilityPublisher, Command, Event, Notification, Urgency};
    use crate::backend::dbus::{
        ServiceChanges, availability_from_error, probe, service_session, session,
    };
    use crate::backend::notifications::ATTENTION_HINT;
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
    use zbus::Connection;
    use zbus::fdo::{RequestNameFlags, RequestNameReply};
    use zbus::names::BusName;
    use zbus::object_server::SignalEmitter;
    use zbus::zvariant::OwnedValue;

    const SERVICE_NAME: &str = "org.freedesktop.Notifications";
    const OBJECT_PATH: &str = "/org/freedesktop/Notifications";

    #[zbus::proxy(
        interface = "org.freedesktop.DBus",
        default_service = "org.freedesktop.DBus",
        default_path = "/org/freedesktop/DBus"
    )]
    trait SessionBus {
        fn name_has_owner(&self, name: BusName<'_>) -> zbus::Result<bool>;
    }

    #[derive(Clone)]
    struct Service {
        next_id: Arc<AtomicU32>,
        events: UnboundedSender<Event>,
        active: Arc<Mutex<HashSet<u32>>>,
    }

    // These methods are called by notification clients over D-Bus, not by Rust
    // callers. Their names and signatures are the exported protocol contract.
    #[zbus::interface(name = "org.freedesktop.Notifications")]
    impl Service {
        fn get_capabilities(&self) -> Vec<&str> {
            vec!["actions", "body", "persistence"]
        }

        fn get_server_information(&self) -> (&str, &str, &str, &str) {
            ("Topbar", "Topbar", env!("CARGO_PKG_VERSION"), "1.2")
        }

        #[allow(
            clippy::too_many_arguments,
            reason = "Notify has eight arguments in the D-Bus specification"
        )]
        fn notify(
            &self,
            app_name: String,
            replaces_id: u32,
            app_icon: String,
            summary: String,
            body: String,
            actions: Vec<String>,
            hints: HashMap<String, OwnedValue>,
            expire_timeout: i32,
        ) -> u32 {
            let mut active = self.active.lock().unwrap();
            let id = if active.contains(&replaces_id) {
                replaces_id
            } else {
                self.next_id.fetch_add(1, Ordering::Relaxed)
            };

            active.insert(id);

            let _ = self.events.send(Event::Added(
                Notification {
                    id,
                    app: app_name,
                    icon: app_icon,
                    desktop_entry: string_hint(&hints, "desktop-entry"),
                    summary,
                    body,
                    default_action: has_action(&actions, "default"),
                    resident: bool_hint(&hints, "resident"),
                    urgency: urgency_hint(&hints),
                    request_attention: bool_hint(&hints, ATTENTION_HINT),
                },
                expire_timeout,
            ));

            id
        }

        async fn close_notification(
            &self,
            id: u32,
            #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        ) {
            let removed = self.active.lock().unwrap().remove(&id);

            if removed {
                let _ = self.events.send(Event::Closed(id));
                let _ = emitter.notification_closed(id, 3).await;
            }
        }

        #[zbus(signal)]
        async fn notification_closed(
            emitter: SignalEmitter<'_>,
            id: u32,
            reason: u32,
        ) -> zbus::Result<()>;

        #[zbus(signal)]
        async fn action_invoked(
            emitter: SignalEmitter<'_>,
            id: u32,
            action_key: &str,
        ) -> zbus::Result<()>;
    }

    pub fn serve(
        events: UnboundedSender<Event>,
        active: Arc<Mutex<HashSet<u32>>>,
        mut commands: UnboundedReceiver<Command>,
        availability: AvailabilityPublisher,
    ) -> impl std::future::Future<Output = ()> {
        let service = Service {
            next_id: Arc::new(AtomicU32::new(1)),
            events,
            active,
        };

        async move {
            use crate::{
                backend::reconnect::ReconnectBackoff, features::availability::Availability,
            };

            let mut retry = ReconnectBackoff::default();

            loop {
                let mut owners = None;
                let result = async {
                    let connection = probe(session()).await?;

                    owners = Some(probe(ServiceChanges::new(&connection, SERVICE_NAME)).await?);

                    let driver = NotificationDriver::connect(service.clone()).await?;

                    availability.set(Availability::Available);
                    retry.reset();

                    let result = driver.run(&mut commands, owners.as_mut().unwrap()).await;

                    driver.close().await;

                    result
                }
                .await;

                match result {
                    Ok(()) => return,
                    Err(state) => availability.set(state),
                }

                let delay = tokio::time::sleep(retry.next_delay());

                tokio::pin!(delay);

                loop {
                    tokio::select! {
                        _ = service.events.closed() => return,
                        _ = &mut delay => break,
                        _ = async {
                            match owners.as_mut() {
                                Some(owners) => {
                                    let _ = owners.changed().await;
                                }
                                None => std::future::pending().await,
                            }
                        } => {
                            retry.reset();

                            break;
                        }
                        command = commands.recv() => {
                            if command.is_none() {
                                return;
                            }
                        }
                    }
                }
            }
        }
    }

    struct NotificationDriver {
        connection: Connection,
    }

    impl NotificationDriver {
        async fn connect(service: Service) -> Result<Self, Availability> {
            use crate::features::availability::{Availability, UnavailableReason};

            let connection = probe(service_session()).await?;

            if !probe(Self::name_available(&connection)).await? {
                return Err(Availability::Unavailable(UnavailableReason::NameOccupied));
            }

            probe(connection.object_server().at(OBJECT_PATH, service)).await?;

            let reply = probe(
                connection
                    .request_name_with_flags(SERVICE_NAME, RequestNameFlags::DoNotQueue.into()),
            )
            .await?;

            match reply {
                RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner => {
                    Ok(Self { connection })
                }
                RequestNameReply::Exists | RequestNameReply::InQueue => {
                    Err(Availability::Unavailable(UnavailableReason::NameOccupied))
                }
            }
        }

        async fn name_available(connection: &Connection) -> zbus::Result<bool> {
            let bus = SessionBusProxy::new(connection).await?;
            let name = BusName::try_from(SERVICE_NAME)?;

            bus.name_has_owner(name).await.map(|owned| !owned)
        }

        async fn run(
            &self,
            commands: &mut UnboundedReceiver<Command>,
            owners: &mut ServiceChanges,
        ) -> Result<(), Availability> {
            use crate::features::availability::{Availability, UnavailableReason};

            loop {
                tokio::select! {
                    command = commands.recv() => {
                        let Some(command) = command else {
                            return Ok(());
                        };

                        if let Err(error) = self.emit(command).await {
                            tracing::debug!(%error, "cannot emit notification signal");
                            return Err(availability_from_error(&error));
                        }
                    }
                    changed = owners.changed() => {
                        changed.map_err(|error| availability_from_error(&error))?;

                        let owner = probe(async {
                            zbus::fdo::DBusProxy::new(&self.connection).await?
                                .get_name_owner(SERVICE_NAME.try_into()?).await.map_err(zbus::Error::from)
                        }).await?;

                        if Some(owner.as_str()) != self.connection.unique_name().map(|name| name.as_str()) {
                            return Err(Availability::Unavailable(UnavailableReason::NameOccupied));
                        }
                    }
                }
            }
        }

        async fn close(&self) {
            let _ = self.connection.release_name(SERVICE_NAME).await;
            let _ = self
                .connection
                .object_server()
                .remove::<Service, _>(OBJECT_PATH)
                .await;
        }

        async fn emit(&self, command: Command) -> zbus::Result<()> {
            let emitter = SignalEmitter::new(&self.connection, OBJECT_PATH)?;

            match command {
                Command::Close { id, reason } => {
                    Service::notification_closed(emitter, id, reason).await
                }
                Command::InvokeDefault { id, close_after } => {
                    Service::action_invoked(emitter.clone(), id, "default").await?;

                    if close_after {
                        Service::notification_closed(emitter, id, 2).await?;
                    }

                    Ok(())
                }
            }
        }
    }

    fn has_action(actions: &[String], key: &str) -> bool {
        actions
            .as_chunks::<2>()
            .0
            .iter()
            .any(|action| action.first().is_some_and(|candidate| candidate == key))
    }

    fn bool_hint(hints: &HashMap<String, OwnedValue>, key: &str) -> bool {
        hints
            .get(key)
            .and_then(|value| bool::try_from(value).ok())
            .unwrap_or(false)
    }

    fn string_hint(hints: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
        hints
            .get(key)
            .and_then(|value| <&str>::try_from(value).ok())
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    }

    fn urgency_hint(hints: &HashMap<String, OwnedValue>) -> Urgency {
        match hints
            .get("urgency")
            .and_then(|value| u8::try_from(value).ok())
        {
            Some(0) => Urgency::Low,
            Some(2) => Urgency::Critical,
            _ => Urgency::Normal,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn detects_only_action_keys() {
            let actions = vec![
                "open".to_string(),
                "Open".to_string(),
                "default".to_string(),
                "Open application".to_string(),
            ];

            assert!(has_action(&actions, "default"));
            assert!(!has_action(&actions, "Open application"));
        }

        #[test]
        fn missing_or_malformed_urgency_uses_normal_priority() {
            assert_eq!(urgency_hint(&HashMap::new()), Urgency::Normal);

            for (value, expected) in [
                (OwnedValue::from(0u8), Urgency::Low),
                (OwnedValue::from(1u8), Urgency::Normal),
                (OwnedValue::from(2u8), Urgency::Critical),
                (OwnedValue::from(99u8), Urgency::Normal),
                (OwnedValue::from(true), Urgency::Normal),
            ] {
                assert_eq!(
                    urgency_hint(&HashMap::from([("urgency".into(), value)])),
                    expected
                );
            }
        }
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use crate::backend::dbus::service_session;
    use crate::features::availability::{UnavailableReason, tests::wait_for};
    use futures_util::StreamExt;

    async fn notify(connection: &zbus::Connection, replaces: u32) -> u32 {
        let proxy = zbus::Proxy::new(
            connection,
            "org.freedesktop.Notifications",
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
        )
        .await
        .unwrap();

        proxy
            .call(
                "Notify",
                &(
                    "Test",
                    replaces,
                    "",
                    "Summary",
                    "Body",
                    Vec::<String>::new(),
                    std::collections::HashMap::<String, zbus::zvariant::OwnedValue>::new(),
                    0i32,
                ),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn waits_for_another_server_then_serves_replaces_and_closes_notifications() {
        let mut bus = crate::backend::dbus::tests::Bus::new().await;
        let competitor = bus.connect().await;

        competitor
            .request_name("org.freedesktop.Notifications")
            .await
            .unwrap();

        let publisher = AvailabilityPublisher::default();
        let mut readiness = publisher.subscribe();
        let mut backend = Backend::start(publisher);
        let controls = backend.controls();
        let mut events = backend.take_events();

        wait_for(
            &mut readiness,
            Availability::Unavailable(UnavailableReason::NameOccupied),
        )
        .await;
        competitor
            .release_name("org.freedesktop.Notifications")
            .await
            .unwrap();
        wait_for(&mut readiness, Availability::Available).await;

        let client = bus.connect().await;
        let id = notify(&client, 0).await;

        assert!(matches!(events.recv().await, Some(Event::Added(item, 0)) if item.id == id));
        assert_eq!(notify(&client, id).await, id);
        assert!(matches!(events.recv().await, Some(Event::Added(item, 0)) if item.id == id));

        let proxy = zbus::Proxy::new(
            &client,
            "org.freedesktop.Notifications",
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
        )
        .await
        .unwrap();

        let mut closed = proxy.receive_signal("NotificationClosed").await.unwrap();

        controls.close(id, 2);

        let signal = tokio::time::timeout(std::time::Duration::from_secs(2), closed.next())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(signal.body().deserialize::<(u32, u32)>().unwrap(), (id, 2));
        // The ownership event emitted by our own acquisition must not hide the feature.
        assert_eq!(*readiness.borrow(), Availability::Available);

        bus.restart().await;
        tokio::time::timeout(std::time::Duration::from_secs(4), async {
            while readiness.borrow_and_update().is_available() {
                readiness.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        wait_for(&mut readiness, Availability::Available).await;

        let client = bus.connect().await;
        let recovered_id = notify(&client, 0).await;

        assert!(recovered_id > id);
        assert!(
            matches!(events.recv().await, Some(Event::Added(item, 0)) if item.id == recovered_id)
        );

        drop(controls);
        drop(backend);

        let connection = service_session().await.unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while zbus::fdo::DBusProxy::new(&connection)
                .await
                .unwrap()
                .name_has_owner("org.freedesktop.Notifications".try_into().unwrap())
                .await
                .unwrap()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}
