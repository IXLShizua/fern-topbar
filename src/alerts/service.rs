//! Serializes delivery, dismissal and cleanup for all internal alert producers.

use super::{Alert, AlertPublisher, Command};
use crate::{
    backend::{
        notifications::client::{Error, NotificationClient},
        reconnect::ReconnectBackoff,
    },
    runtime::Task,
};
use std::collections::HashMap;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use zbus::names::OwnedUniqueName;

pub struct AlertService {
    publisher: AlertPublisher,
    _task: Task,
}

impl AlertService {
    pub fn start() -> Self {
        let (commands, receiver) = mpsc::unbounded_channel();

        Self {
            publisher: AlertPublisher { commands },
            _task: Task::spawn(AlertDriver::new(receiver).run()),
        }
    }

    pub fn publisher(&self) -> AlertPublisher {
        self.publisher.clone()
    }
}

#[derive(Default)]
struct PendingAlert {
    target: Option<Alert>,
    delivered: Option<Alert>,
    active: Option<u32>,
    // A gap in closure signals makes quiet replacement unsafe even if the same
    // owner reconnects: the user could have dismissed the warning during that gap.
    can_update: bool,
    // Explicit attention survives connection failures until successfully delivered.
    // Quiet updates alone never create notifications, including after reconnecting.
    show: bool,
    removed: bool,
}

impl PendingAlert {
    fn change(&self) -> Option<Delivery<'_>> {
        match (&self.target, self.active) {
            (Some(alert), active)
                if self.show
                    || (active.is_some()
                        && self.can_update
                        && self.delivered.as_ref() != Some(alert)) =>
            {
                Some(Delivery::Notify(active.unwrap_or(0), alert))
            }
            (None, Some(id)) => Some(Delivery::Close(id)),
            _ => None,
        }
    }

    fn detach(&mut self) {
        self.active = None;
        self.delivered = None;
        self.can_update = false;
    }
}

enum Delivery<'a> {
    Notify(u32, &'a Alert),
    Close(u32),
}

struct AlertDriver {
    commands: UnboundedReceiver<Command>,
    alerts: HashMap<u64, PendingAlert>,
    owner: Option<OwnedUniqueName>,
    stopping: bool,
}

impl AlertDriver {
    fn new(commands: UnboundedReceiver<Command>) -> Self {
        Self {
            commands,
            alerts: HashMap::new(),
            owner: None,
            stopping: false,
        }
    }

    fn apply(&mut self, command: Command) {
        match command {
            Command::Show(id, alert) => {
                let pending = self.alerts.entry(id).or_default();
                pending.target = Some(alert);
                pending.show = true;
            }
            Command::Update(id, alert) => {
                self.alerts.entry(id).or_default().target = Some(alert);
            }
            Command::Clear(id) | Command::Remove(id) => {
                if let Some(pending) = self.alerts.get_mut(&id) {
                    pending.target = None;
                    pending.show = false;
                    pending.removed = matches!(command, Command::Remove(_));
                }
            }
        }

        self.collect_removed();
    }

    fn collect_removed(&mut self) {
        self.alerts
            .retain(|_, pending| !pending.removed || pending.active.is_some());
    }

    fn closed(&mut self, id: u32) {
        for pending in self.alerts.values_mut() {
            if pending.active == Some(id) {
                pending.detach();
                // Keep explicit attention queued after this older notification.
                // A closure may race with a newly requested danger level.
            }
        }

        self.collect_removed();
    }

    fn attach(&mut self, owner: &OwnedUniqueName) {
        if self.owner.as_ref() != Some(owner) {
            // IDs must never cross server ownership. Retain undelivered requests,
            // but replacing a vanished server does not request renewed attention.
            for pending in self.alerts.values_mut() {
                pending.detach();
            }

            self.collect_removed();
        }

        self.owner = Some(owner.clone());
    }

    fn disconnected(&mut self) {
        for pending in self.alerts.values_mut() {
            pending.can_update = false;
        }
    }

    fn stop(&mut self) {
        self.stopping = true;

        for pending in self.alerts.values_mut() {
            pending.target = None;
            pending.show = false;
            pending.removed = true;
        }

        self.collect_removed();
    }

    fn drain_commands(&mut self) {
        loop {
            match self.commands.try_recv() {
                Ok(command) => self.apply(command),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    self.stop();
                    break;
                }
            }
        }
    }

    fn next(&self) -> Option<(u64, Delivery<'_>)> {
        self.alerts
            .iter()
            .find_map(|(&id, pending)| pending.change().map(|change| (id, change)))
    }

    async fn deliver(&mut self, client: &mut NotificationClient) -> Result<(), Error> {
        let Some((key, change)) = self.next() else {
            return Ok(());
        };

        match change {
            Delivery::Notify(replaces, alert) => {
                let id = client
                    .notify(
                        replaces,
                        &alert.icon,
                        &alert.summary,
                        &alert.body,
                        alert.urgency,
                        self.alerts[&key].show,
                    )
                    .await?;

                let delivered = alert.clone();
                let pending = self.alerts.get_mut(&key).unwrap();
                pending.active = Some(id);
                pending.delivered = Some(delivered);
                pending.show = false;
                pending.can_update = true;
            }
            Delivery::Close(id) => {
                client.close(id).await?;
                self.alerts.get_mut(&key).unwrap().detach();
                self.collect_removed();
            }
        }

        Ok(())
    }

    async fn run(mut self) {
        let mut retry = ReconnectBackoff::default();

        loop {
            self.drain_commands();

            if self.stopping && self.next().is_none() {
                return;
            }

            if self.next().is_none() {
                match self.commands.recv().await {
                    Some(command) => self.apply(command),
                    None => self.stop(),
                }

                continue;
            }

            let connection = tokio::select! {
                result = NotificationClient::connect() => result,
                command = self.commands.recv(), if !self.stopping => {
                    match command { Some(command) => self.apply(command), None => self.stop() }

                    continue;
                }
            };

            if let Ok(mut client) = connection {
                retry.reset();
                self.attach(client.owner());

                if self.run_session(&mut client).await.is_ok() {
                    return;
                }

                self.disconnected();
            }

            if self.stopping {
                return;
            }
            // Process clear/drop/latest content without resetting the retry deadline.
            let delay = tokio::time::sleep(retry.next_delay());

            tokio::pin!(delay);

            loop {
                tokio::select! {
                    _ = &mut delay => break,
                    command = self.commands.recv() => {
                        match command {
                            Some(command) => self.apply(command),
                            None => {
                                self.stop();

                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    async fn run_session(&mut self, client: &mut NotificationClient) -> Result<(), Error> {
        loop {
            self.drain_commands();

            if self.stopping && self.next().is_none() {
                return Ok(());
            }

            tokio::select! {
                biased;
                closed = client.next_closed() => self.closed(closed?),
                command = self.commands.recv(), if !self.stopping => {
                    match command { Some(command) => self.apply(command), None => self.stop() }
                }
                // Poll closures before delivery so an already dismissed notification
                // cannot be resurrected by a queued percentage/content update.
                _ = std::future::ready(()), if self.next().is_some() => self.deliver(client).await?,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        backend::dbus,
        backend::notifications::{Event, Urgency, server::Backend},
        features::availability::{Availability, AvailabilityPublisher, tests::wait_for},
    };
    use std::time::Duration;

    fn alert(body: &str) -> Alert {
        Alert {
            icon: "dialog-warning-symbolic".into(),
            summary: "Warning".into(),
            body: body.into(),
            urgency: Urgency::Critical,
        }
    }

    fn delivered(body: &str) -> PendingAlert {
        PendingAlert {
            target: Some(alert(body)),
            delivered: Some(alert(body)),
            active: Some(7),
            can_update: true,
            ..PendingAlert::default()
        }
    }

    #[test]
    fn closure_of_the_previous_warning_does_not_cancel_queued_attention() {
        let (commands, receiver) = mpsc::unbounded_channel();
        let publisher = AlertPublisher { commands };
        let handle = publisher.register();
        let mut driver = AlertDriver::new(receiver);
        driver.alerts.insert(handle.id, delivered("critical"));

        handle.show(alert("action required"));
        driver.drain_commands();
        driver.closed(7);
        assert!(
            matches!(driver.next(), Some((_, Delivery::Notify(0, alert))) if alert.body == "action required")
        );
    }

    #[test]
    fn closure_gaps_suppress_quiet_updates_but_allow_cleanup_only_on_the_same_owner() {
        let (commands, receiver) = mpsc::unbounded_channel();
        let publisher = AlertPublisher { commands };
        let handle = publisher.register();
        let mut driver = AlertDriver::new(receiver);
        let owner = OwnedUniqueName::try_from(":1.1").unwrap();

        driver.attach(&owner);
        driver.alerts.insert(handle.id, delivered("warning"));

        driver.disconnected();
        handle.update(alert("might have been dismissed offline"));
        driver.drain_commands();
        driver.attach(&owner);
        assert!(driver.next().is_none());

        handle.clear();
        driver.drain_commands();
        assert!(matches!(driver.next(), Some((_, Delivery::Close(7)))));
        driver.attach(&OwnedUniqueName::try_from(":1.2").unwrap());
        assert!(driver.next().is_none());
        drop(handle);
        driver.drain_commands();
        assert!(driver.alerts.is_empty());
    }

    async fn server() -> (Backend, UnboundedReceiver<Event>) {
        let availability = AvailabilityPublisher::default();
        let mut readiness = availability.subscribe();
        let mut backend = Backend::start(availability);
        let events = backend.take_events();

        wait_for(&mut readiness, Availability::Available).await;

        (backend, events)
    }

    async fn event(events: &mut UnboundedReceiver<Event>) -> Event {
        tokio::time::timeout(Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap()
    }

    async fn added(events: &mut UnboundedReceiver<Event>, body: &str) -> u32 {
        let Event::Added(item, _) = event(events).await else {
            panic!("expected notification");
        };

        assert_eq!(item.body, body);

        item.id
    }

    async fn no_event(events: &mut UnboundedReceiver<Event>) {
        assert!(
            tokio::time::timeout(Duration::from_millis(150), events.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn explicit_shows_and_quiet_updates_have_distinct_attention_without_changing_id() {
        let _bus = dbus::tests::Bus::new().await;
        let (_backend, mut events) = server().await;
        let service = AlertService::start();
        let handle = service.publisher().register();

        handle.show(alert("first"));

        let Event::Added(first, _) = event(&mut events).await else {
            panic!("expected notification");
        };

        assert!(first.request_attention);

        handle.update(alert("quiet update"));

        let Event::Added(update, _) = event(&mut events).await else {
            panic!("expected notification");
        };

        assert_eq!(update.id, first.id);
        assert!(!update.request_attention);

        handle.show(alert("renewed attention"));

        let Event::Added(renewed, _) = event(&mut events).await else {
            panic!("expected notification");
        };

        assert_eq!(renewed.id, first.id);
        assert!(renewed.request_attention);
    }

    #[tokio::test]
    async fn handles_update_independently_and_drop_withdraws_only_their_notification() {
        let _bus = dbus::tests::Bus::new().await;
        let (_backend, mut events) = server().await;
        let service = AlertService::start();
        let first = service.publisher().register();
        let second = service.publisher().register();

        first.show(alert("first"));

        let first_id = added(&mut events, "first").await;

        second.show(alert("second"));

        let second_id = added(&mut events, "second").await;

        assert_ne!(first_id, second_id);

        first.update(alert("first updated"));
        assert_eq!(added(&mut events, "first updated").await, first_id);
        drop(first);
        assert!(matches!(event(&mut events).await, Event::Closed(id) if id == first_id));

        second.update(alert("second updated"));
        assert_eq!(added(&mut events, "second updated").await, second_id);
        second.clear();
        assert!(matches!(event(&mut events).await, Event::Closed(id) if id == second_id));
        second.show(alert("new condition"));

        let new_id = added(&mut events, "new condition").await;

        assert_ne!(new_id, second_id);
        drop(second);
        assert!(matches!(event(&mut events).await, Event::Closed(id) if id == new_id));
    }

    #[tokio::test]
    async fn quiet_updates_respect_dismissal_but_explicit_show_can_reopen_at_same_urgency() {
        let _bus = dbus::tests::Bus::new().await;
        let (backend, mut events) = server().await;
        let service = AlertService::start();
        let handle = service.publisher().register();

        handle.update(alert("not yet shown"));
        no_event(&mut events).await;
        handle.show(alert("critical"));

        let id = added(&mut events, "critical").await;

        backend.controls().close(id, 2);
        // UI controls remove the card locally and emit a D-Bus closure only.
        // Allow the asynchronous signal to reach the subscriber before updating.
        no_event(&mut events).await;
        handle.update(alert("still critical"));
        no_event(&mut events).await;

        handle.show(alert("action required"));

        let next_id = added(&mut events, "action required").await;

        assert_ne!(id, next_id);
        handle.clear();
        assert!(matches!(event(&mut events).await, Event::Closed(closed) if closed == next_id));
        handle.update(alert("resolved warning stays hidden"));
        no_event(&mut events).await;
    }

    #[tokio::test]
    async fn offline_delivery_uses_latest_content_and_honors_clear_and_drop() {
        let _bus = dbus::tests::Bus::new().await;
        let service = AlertService::start();
        let publisher = service.publisher();
        let kept = publisher.register();
        let cleared = publisher.register();
        let removed = publisher.register();

        kept.show(alert("obsolete"));
        cleared.show(alert("cleared"));
        removed.show(alert("removed"));
        // Let connection failure happen before resolving/changing the queued alerts.
        tokio::time::sleep(Duration::from_millis(100)).await;
        kept.update(alert("latest"));
        cleared.clear();
        drop(removed);

        let (_backend, mut events) = server().await;

        added(&mut events, "latest").await;
        no_event(&mut events).await;
    }

    #[tokio::test]
    async fn replacing_a_server_preserves_dismissal_and_never_reuses_its_ids() {
        let _bus = dbus::tests::Bus::new().await;
        let (backend, mut old_events) = server().await;
        let service = AlertService::start();
        let first = service.publisher().register();
        first.show(alert("old owner"));

        let old_id = added(&mut old_events, "old owner").await;

        drop(backend);

        first.update(alert("quiet after owner loss"));

        let second = service.publisher().register();
        second.show(alert("new owner"));

        let (_replacement, mut events) = server().await;
        let reused_id = added(&mut events, "new owner").await;

        assert_eq!(
            old_id, reused_id,
            "server fixture starts its ID counter from one"
        );
        no_event(&mut events).await;

        first.clear();
        no_event(&mut events).await;
        first.show(alert("explicit new warning"));

        let fresh_id = added(&mut events, "explicit new warning").await;

        assert_ne!(fresh_id, reused_id);
        drop(first);
        assert!(matches!(event(&mut events).await, Event::Closed(id) if id == fresh_id));
        second.update(alert("second still active"));
        assert_eq!(added(&mut events, "second still active").await, reused_id);
    }

    #[tokio::test]
    async fn closing_the_command_channel_withdraws_alerts_and_stops_the_driver() {
        let _bus = dbus::tests::Bus::new().await;
        let (_backend, mut events) = server().await;
        let (commands, receiver) = mpsc::unbounded_channel();
        let publisher = AlertPublisher { commands };
        let worker = tokio::spawn(AlertDriver::new(receiver).run());
        let handle = publisher.register();
        handle.show(alert("owned"));

        let id = added(&mut events, "owned").await;

        drop(handle);
        drop(publisher);
        assert!(matches!(event(&mut events).await, Event::Closed(closed) if closed == id));
        tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .unwrap()
            .unwrap();
    }
}
