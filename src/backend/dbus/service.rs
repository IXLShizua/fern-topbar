//! Connection and ownership lifecycle shared by D-Bus backends.

use super::{ServiceChange, ServiceChanges, availability_from_error, probe, service_session};
use crate::{
    backend::reconnect::ReconnectBackoff,
    features::availability::{Availability, AvailabilityPublisher, ProbeError, UnavailableReason},
};
use tokio::time::Instant;
use zbus::{Connection, fdo, names::OwnedUniqueName};

enum Bus {
    Session,
    System,
    Exported,
}

enum Wait {
    Ready,
    Owner,
    Retry(Instant),
}

struct Session {
    changes: ServiceChanges,
    exported: Option<Connection>,
    owner: Option<OwnedUniqueName>,
}

/// Owns reconnection and readiness; consumers own their proxies, data and commands.
/// `connect` and its retry deadline survive cancellation in a consumer's select.
pub struct Service {
    bus: Bus,
    name: String,
    availability: AvailabilityPublisher,
    session: Option<Session>,
    retry: ReconnectBackoff,
    wait: Wait,
}

impl Service {
    pub fn system(name: impl Into<String>, availability: AvailabilityPublisher) -> Self {
        Self::new(Bus::System, name.into(), availability)
    }

    pub fn session(name: impl Into<String>, availability: AvailabilityPublisher) -> Self {
        Self::new(Bus::Session, name.into(), availability)
    }

    /// Gives the consumer a private connection while observing its name on the shared bus.
    pub fn exported(name: impl Into<String>, availability: AvailabilityPublisher) -> Self {
        Self::new(Bus::Exported, name.into(), availability)
    }

    pub fn owner(&self) -> &OwnedUniqueName {
        self.session
            .as_ref()
            .and_then(|session| session.owner.as_ref())
            .expect("connect must precede owner")
    }

    fn new(bus: Bus, name: String, availability: AvailabilityPublisher) -> Self {
        Self {
            bus,
            name,
            availability,
            session: None,
            retry: ReconnectBackoff::default(),
            wait: Wait::Ready,
        }
    }

    /// Waits for a service owner (or a free exported name) and a usable connection.
    /// Missing services wait for native events; transport failures use bounded backoff.
    pub async fn connect(&mut self) -> Connection {
        loop {
            self.wait().await;

            if self.session.is_none() {
                let result = probe(async {
                    let connection = match self.bus {
                        Bus::System => super::system().await?,
                        Bus::Session | Bus::Exported => super::session().await?,
                    };
                    let changes = ServiceChanges::new(&connection, &self.name).await?;

                    Ok(Session {
                        changes,
                        exported: None,
                        owner: None,
                    })
                })
                .await;

                match result {
                    Ok(session) => self.session = Some(session),
                    Err(state) => {
                        self.failed(state).await;
                        continue;
                    }
                }
            }

            let session = self.session.as_mut().unwrap();
            let result = probe(async {
                let proxy = &session.changes.proxy;
                match proxy.get_name_owner(self.name.as_str().try_into()?).await {
                    Ok(_) if matches!(self.bus, Bus::Exported) => Err(zbus::Error::NameTaken),
                    Ok(owner) => Ok((proxy.inner().connection().clone(), owner)),
                    Err(fdo::Error::NameHasNoOwner(_)) if matches!(self.bus, Bus::Exported) => {
                        let connection = service_session().await?;
                        let owner = connection.unique_name().unwrap().to_owned();

                        Ok((connection, owner))
                    }
                    Err(fdo::Error::NameHasNoOwner(_)) => {
                        proxy
                            .start_service_by_name(self.name.as_str().try_into()?, 0)
                            .await?;
                        let owner = proxy.get_name_owner(self.name.as_str().try_into()?).await?;

                        Ok((proxy.inner().connection().clone(), owner))
                    }
                    Err(error) => Err(error.into()),
                }
            })
            .await;

            match result {
                Ok((connection, owner)) => {
                    session.owner = Some(owner);
                    if matches!(self.bus, Bus::Exported) {
                        session.exported = Some(connection.clone());
                    }

                    return connection;
                }
                Err(state) => self.failed(state).await,
            }
        }
    }

    /// Runs one consumer session and invalidates it when its bus or owner changes.
    /// An error prepares the next `connect`; success returns the consumer's result.
    /// Commands are never replayed by this layer.
    pub async fn run<T>(
        &mut self,
        future: impl Future<Output = Result<T, Availability>>,
    ) -> Result<T, Availability> {
        let session = self.session.as_mut().expect("connect must precede run");
        tokio::pin!(future);

        let result = loop {
            tokio::select! {
                result = &mut future => break result,
                _ = async {
                    match session.exported.as_ref() {
                        Some(connection) => connection.closed().await,
                        None => std::future::pending().await,
                    }
                } => break Err(Availability::Failed(ProbeError::Connect)),
                change = session.changes.next() => match change {
                    Ok(ServiceChange::Activation) => {},
                    Ok(ServiceChange::Owner(owner)) if owner == session.owner => {},
                    Ok(ServiceChange::Owner(owner)) => {
                        let state = match (&self.bus, owner) {
                            (Bus::Exported, Some(_)) => Availability::Unavailable(UnavailableReason::NameOccupied),
                            (Bus::Session | Bus::System, None) => Availability::Unavailable(UnavailableReason::ServiceMissing),
                            _ => Availability::Checking,
                        };

                        break Err(state);
                    }
                    Err(error) => break Err(availability_from_error(error)),
                }
            }
        };

        if let Err(state) = result {
            self.failed(state).await;
        }

        result
    }

    async fn failed(&mut self, mut state: Availability) {
        if self.availability.current().is_available() {
            self.retry.reset();
        }

        if self
            .session
            .as_ref()
            .is_some_and(|session| session.changes.proxy.inner().connection().is_closed())
        {
            self.session = None;
            state = Availability::Failed(ProbeError::Connect);
        }

        self.availability.set(state);
        self.wait = match state {
            Availability::Checking => Wait::Ready,
            Availability::Unavailable(
                UnavailableReason::ServiceMissing | UnavailableReason::NameOccupied,
            ) if self.session.is_some() => Wait::Owner,
            _ => Wait::Retry(Instant::now() + self.retry.next_delay()),
        };

        if let Some(connection) = self
            .session
            .as_mut()
            .and_then(|session| session.exported.take())
        {
            let _ = connection.close().await;
        }
    }

    async fn wait(&mut self) {
        let change = match self.wait {
            Wait::Ready => return,
            Wait::Owner => self.session.as_mut().unwrap().changes.next().await,
            Wait::Retry(deadline) => {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => {
                        self.wait = Wait::Ready;
                        return;
                    }
                    change = async {
                        match self.session.as_mut() {
                            Some(session) => session.changes.next().await,
                            None => std::future::pending().await,
                        }
                    } => change,
                }
            }
        };

        match change {
            Ok(_) => {
                // Record the consumed wake before another await, so cancellation cannot lose it.
                self.retry.reset();
                self.wait = Wait::Ready;
            }
            Err(error) => {
                self.session = None;
                self.failed(availability_from_error(error)).await;
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::dbus::tests::Bus;
    use std::time::Duration;
    use tokio::time::timeout;

    const NAME: &str = "org.fernTopbar.TestService";

    #[tokio::test]
    async fn waits_for_ownership_and_recovers_after_owner_loss_and_bus_restart() {
        let mut bus = Bus::new().await;
        let publisher = AvailabilityPublisher::default();
        let mut service = Service::session(NAME, publisher.clone());

        assert!(
            timeout(Duration::from_millis(350), service.connect())
                .await
                .is_err()
        );
        assert_eq!(
            publisher.current(),
            Availability::Unavailable(UnavailableReason::ServiceMissing)
        );

        let owner = bus.connect().await;
        owner.request_name(NAME).await.unwrap();
        let connection = timeout(Duration::from_secs(1), service.connect())
            .await
            .unwrap();
        assert_eq!(service.owner(), owner.unique_name().unwrap());
        publisher.set(Availability::Available);

        owner.release_name(NAME).await.unwrap();
        let result = timeout(
            Duration::from_secs(1),
            service.run(std::future::pending::<Result<(), Availability>>()),
        )
        .await
        .unwrap();
        assert_eq!(
            result,
            Err(Availability::Unavailable(UnavailableReason::ServiceMissing))
        );

        assert!(
            timeout(Duration::from_millis(350), service.connect())
                .await
                .is_err()
        );
        owner.request_name(NAME).await.unwrap();
        let _connection = timeout(Duration::from_secs(1), service.connect())
            .await
            .unwrap();
        publisher.set(Availability::Available);

        bus.restart().await;
        assert!(
            timeout(
                Duration::from_secs(1),
                service.run(std::future::pending::<Result<(), Availability>>())
            )
            .await
            .unwrap()
            .is_err()
        );
        assert!(connection.is_closed());

        let replacement = bus.connect().await;
        replacement.request_name(NAME).await.unwrap();
        let connection = timeout(Duration::from_secs(2), service.connect())
            .await
            .unwrap();
        assert!(!connection.is_closed());
        assert_eq!(service.owner(), replacement.unique_name().unwrap());
    }

    #[tokio::test]
    async fn cancelled_connects_preserve_the_retry_deadline() {
        let bus = Bus::new().await;
        let owner = bus.connect().await;
        owner.request_name(NAME).await.unwrap();
        let mut service = Service::session(NAME, AvailabilityPublisher::default());
        let _connection = service.connect().await;

        let _ = service
            .run(async { Err::<(), _>(Availability::Failed(ProbeError::Read)) })
            .await;

        for _ in 0..3 {
            assert!(
                timeout(Duration::from_millis(20), service.connect())
                    .await
                    .is_err()
            );
        }

        let _connection = timeout(Duration::from_millis(225), service.connect())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn exported_names_wait_for_release_and_failed_sessions_release_their_names() {
        let bus = Bus::new().await;
        let owner = bus.connect().await;
        owner.request_name(NAME).await.unwrap();
        let mut service = Service::exported(NAME, AvailabilityPublisher::default());

        assert!(
            timeout(Duration::from_millis(350), service.connect())
                .await
                .is_err()
        );
        owner.release_name(NAME).await.unwrap();
        let connection = timeout(Duration::from_secs(1), service.connect())
            .await
            .unwrap();
        connection.request_name(NAME).await.unwrap();

        assert!(
            timeout(
                Duration::from_millis(50),
                service.run(std::future::pending::<Result<(), Availability>>())
            )
            .await
            .is_err()
        );
        let _ = service
            .run(async { Err::<(), _>(Availability::Failed(ProbeError::Read)) })
            .await;
        assert!(connection.is_closed());

        timeout(Duration::from_secs(1), owner.request_name(NAME))
            .await
            .unwrap()
            .unwrap();
    }
}
