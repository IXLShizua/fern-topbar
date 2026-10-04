use crate::features::availability::{self, Availability, AvailabilityPublisher, UnavailableReason};
mod dbus;
mod model;

pub use model::{
    ConnectionKind, DeviceInfo, DeviceState, Event, Password, Security, Snapshot, WifiNetwork,
    WifiProfile,
};

use crate::{
    backend::dbus::{Service, availability_from_error, probe},
    runtime::Task,
};
use futures_util::{FutureExt, StreamExt};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{sync::mpsc, time::timeout};

#[derive(Clone, Debug)]
pub enum Command {
    SetWifi(bool),
    SetWired(bool),
    Scan,
    Connect(WifiNetwork, Option<Password>),
    Disconnect(String),
    Forget(String),
}

/// Cloneable network controller. Its final owner stops the background task.
#[derive(Clone)]
pub struct Backend {
    commands: mpsc::UnboundedSender<Command>,
    pending: Arc<AtomicBool>,
    _task: Arc<Task>,
}

impl Backend {
    /// Starts the controller and returns its single ordered event stream.
    pub fn start(availability: AvailabilityPublisher) -> (Self, mpsc::UnboundedReceiver<Event>) {
        let (sender, commands) = mpsc::unbounded_channel();
        let (events, receiver) = mpsc::unbounded_channel();
        let pending = Arc::new(AtomicBool::new(false));
        let task = Task::spawn(Self::run(commands, events, pending.clone(), availability));

        (
            Self {
                commands: sender,
                pending,
                _task: Arc::new(task),
            },
            receiver,
        )
    }

    /// Reports whether a request is queued or running.
    pub fn is_busy(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }

    /// Enables or disables Wi-Fi; returns false if another request is pending.
    pub fn set_wifi(&self, enabled: bool) -> bool {
        self.submit(Command::SetWifi(enabled))
    }

    /// Changes Ethernet autoconnect and activates or disconnects its devices.
    pub fn set_wired(&self, enabled: bool) -> bool {
        self.submit(Command::SetWired(enabled))
    }

    /// Requests a Wi-Fi scan, rejecting competing requests while busy.
    pub fn scan(&self) -> bool {
        self.submit(Command::Scan)
    }

    /// Activates a network using saved credentials or the supplied password.
    pub fn connect(&self, network: WifiNetwork, password: Option<Password>) -> bool {
        self.submit(Command::Connect(network, password))
    }

    /// Disconnects an interface without removing its saved profile.
    pub fn disconnect(&self, device: &str) -> bool {
        self.submit(Command::Disconnect(device.into()))
    }

    /// Deletes a saved profile; the caller is responsible for confirmation.
    pub fn forget(&self, profile: &str) -> bool {
        self.submit(Command::Forget(profile.into()))
    }

    fn submit(&self, command: Command) -> bool {
        if self
            .pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }

        if self.commands.send(command).is_err() {
            self.pending.store(false, Ordering::Release);
            return false;
        }

        true
    }

    async fn run(
        mut commands: mpsc::UnboundedReceiver<Command>,
        events: mpsc::UnboundedSender<Event>,
        pending: Arc<AtomicBool>,
        availability: AvailabilityPublisher,
    ) {
        let mut service = Service::system(dbus::SERVICE, availability.clone());

        loop {
            let connection = tokio::select! {
                connection = service.connect() => connection,
                _ = events.closed() => return,
                command = commands.recv() => {
                    if command.is_none() {
                        return;
                    }

                    pending.store(false, Ordering::Release);
                    let _ = events.send(Event::Busy(false));
                    let _ = events.send(Event::Error("NetworkManager is unavailable".into()));

                    continue;
                }
            };
            let result = service
                .run(async {
                    let client = probe(dbus::Client::new(connection)).await?;

                    Self::listen(&client, &mut commands, &events, &pending, &availability).await
                })
                .await;

            if result.is_ok() {
                return;
            }

            while commands.try_recv().is_ok() {}

            pending.store(false, Ordering::Release);
            let _ = events.send(Event::Busy(false));
            let _ = events.send(Event::Updated(Snapshot::default()));
        }
    }

    async fn publish_snapshot(
        client: &dbus::Client,
        events: &mpsc::UnboundedSender<Event>,
        availability: &AvailabilityPublisher,
    ) -> Result<(), Availability> {
        let snapshot = probe(client.snapshot()).await?;

        availability.set(if snapshot.wifi_available || snapshot.wired_available {
            Availability::Available
        } else {
            Availability::Unavailable(UnavailableReason::DeviceMissing)
        });

        let _ = events.send(Event::Updated(snapshot));

        Ok(())
    }

    async fn listen(
        client: &dbus::Client,
        commands: &mut mpsc::UnboundedReceiver<Command>,
        events: &mpsc::UnboundedSender<Event>,
        pending: &AtomicBool,
        availability: &AvailabilityPublisher,
    ) -> Result<(), Availability> {
        let mut changes = probe(client.changes()).await?;

        Self::publish_snapshot(client, events, availability).await?;

        let closed = || Availability::Failed(availability::ProbeError::Connect);

        loop {
            tokio::select! {
                command = commands.recv() => {
                    let Some(command) = command else {
                        return Ok(());
                    };
                    let _ = events.send(Event::Busy(true));
                    let outcome = timeout(Duration::from_secs(60), client.apply(command)).await;
                    let snapshot = Self::publish_snapshot(client, events, availability).await;

                    match outcome {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            let _ = events.send(Event::Error(error.to_string()));
                        }
                        Err(_) => {
                            let _ = events.send(Event::Error(
                                "Network operation timed out; \
                                 check the connection state and retry".into(),
                            ));
                        }
                    }

                    pending.store(false, Ordering::Release);

                    let _ = events.send(Event::Busy(false));

                    snapshot?;
                }
                _ = events.closed() => return Ok(()),
                change = changes.next() => {
                    change.ok_or_else(closed)?.map_err(availability_from_error)?;

                    while let Some(change) = changes.next().now_or_never() {
                        change.ok_or_else(closed)?.map_err(availability_from_error)?;
                    }

                    Self::publish_snapshot(client, events, availability).await?;
                }
            }
        }
    }

    #[cfg(test)]
    pub fn test_channel() -> (Self, mpsc::UnboundedReceiver<Command>) {
        let (commands, receiver) = mpsc::unbounded_channel();
        let backend = Self {
            commands,
            pending: Arc::new(AtomicBool::new(false)),
            _task: Arc::new(Task::spawn(async {})),
        };

        (backend, receiver)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_methods_reject_competing_requests_and_preserve_order() {
        let (backend, mut commands) = Backend::test_channel();

        assert!(backend.set_wifi(true));
        assert!(backend.is_busy());
        assert!(!backend.scan());
        assert!(matches!(commands.try_recv(), Ok(Command::SetWifi(true))));
        assert!(commands.try_recv().is_err());

        backend.pending.store(false, Ordering::Release);

        assert!(backend.scan());
        assert!(matches!(commands.try_recv(), Ok(Command::Scan)));

        backend.pending.store(false, Ordering::Release);

        assert!(backend.disconnect("/device"));
        assert!(matches!(commands.try_recv(), Ok(Command::Disconnect(path)) if path == "/device"));
    }

    #[test]
    fn cloned_backends_share_busy_state_and_report_a_closed_receiver() {
        let (backend, commands) = Backend::test_channel();
        let clone = backend.clone();

        assert!(clone.forget("/profile"));
        assert!(backend.is_busy());
        assert!(!backend.set_wired(true));

        backend.pending.store(false, Ordering::Release);

        drop(commands);

        assert!(!clone.scan());
        assert!(!backend.is_busy());
    }

    #[tokio::test]
    async fn final_backend_owner_stops_the_task() {
        struct Completion(Option<tokio::sync::oneshot::Sender<()>>);

        impl Drop for Completion {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }

        let (started, ready) = tokio::sync::oneshot::channel();
        let (finished, mut stopped) = tokio::sync::oneshot::channel();
        let task = Task::spawn(async move {
            let _completion = Completion(Some(finished));
            let _ = started.send(());

            futures_util::future::pending::<()>().await;
        });

        let (commands, _receiver) = mpsc::unbounded_channel();
        let backend = Backend {
            commands,
            pending: Arc::new(AtomicBool::new(false)),
            _task: Arc::new(task),
        };

        let clone = backend.clone();

        ready.await.unwrap();

        drop(backend);

        assert!(
            timeout(Duration::from_millis(20), &mut stopped)
                .await
                .is_err()
        );

        drop(clone);
        timeout(Duration::from_secs(1), stopped)
            .await
            .unwrap()
            .unwrap();
    }
}
