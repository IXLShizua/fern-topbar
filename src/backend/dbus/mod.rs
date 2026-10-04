//! Shared D-Bus connections, service ownership subscriptions and readiness probes.

use crate::features::availability::{self, Availability, ProbeError, UnavailableReason};
use futures_util::{StreamExt, TryFutureExt};
use tokio::sync::Mutex;
use zbus::{
    Connection, Error,
    fdo::{DBusProxy, Error as FdoError, NameOwnerChangedStream},
    names::OwnedUniqueName,
    proxy::{CacheProperties, SignalStream},
};

mod service;

pub use service::Service;

static SESSION: Mutex<Option<Connection>> = Mutex::const_new(None);
static SYSTEM: Mutex<Option<Connection>> = Mutex::const_new(None);

pub async fn session() -> zbus::Result<Connection> {
    connection(&SESSION, service_session).await
}

pub async fn system() -> zbus::Result<Connection> {
    connection(&SYSTEM, open_system).await
}

/// Exported services own a connection, so dropping their session releases names and objects.
pub async fn service_session() -> zbus::Result<Connection> {
    #[cfg(test)]
    if let Some(address) = tests::address() {
        return zbus::connection::Builder::address(address.as_str())?
            .method_timeout(availability::PROBE_TIMEOUT)
            .build()
            .await;
    }

    zbus::connection::Builder::session()?
        .method_timeout(availability::PROBE_TIMEOUT)
        .build()
        .await
}

async fn open_system() -> zbus::Result<Connection> {
    // Network operations may wait for authorization; probes impose their own shorter limit.
    let method_timeout = std::time::Duration::from_secs(60);
    #[cfg(test)]
    if let Some(address) = tests::address() {
        return zbus::connection::Builder::address(address.as_str())?
            .method_timeout(method_timeout)
            .build()
            .await;
    }

    zbus::connection::Builder::system()?
        .method_timeout(method_timeout)
        .build()
        .await
}

/// Converts transport-specific failures into feature readiness.
pub fn availability_from_error(error: Error) -> Availability {
    tracing::debug!(%error, "D-Bus availability check failed");

    match error {
        Error::MethodError(..) | Error::FDO(_) => match FdoError::from(error) {
            FdoError::ServiceUnknown(_) | FdoError::NameHasNoOwner(_) => {
                Availability::Unavailable(UnavailableReason::ServiceMissing)
            }
            FdoError::AccessDenied(_) | FdoError::AuthFailed(_) => {
                Availability::Failed(ProbeError::PermissionDenied)
            }
            FdoError::UnknownObject(_) | FdoError::UnknownInterface(_) => {
                Availability::Unavailable(UnavailableReason::DeviceMissing)
            }
            FdoError::NoReply(_) | FdoError::Timeout(_) => {
                Availability::Failed(ProbeError::Timeout)
            }
            _ => Availability::Failed(ProbeError::Read),
        },
        Error::NameTaken => Availability::Unavailable(UnavailableReason::NameOccupied),
        Error::InputOutput(error) | Error::Connection(error, _) => {
            Availability::Failed(match error.kind() {
                std::io::ErrorKind::PermissionDenied => ProbeError::PermissionDenied,
                std::io::ErrorKind::TimedOut => ProbeError::Timeout,
                _ => ProbeError::Connect,
            })
        }
        Error::Handshake(_) => Availability::Failed(ProbeError::Connect),
        Error::InvalidReply | Error::Variant(_) | Error::InvalidField => {
            Availability::Failed(ProbeError::Protocol)
        }
        _ => Availability::Failed(ProbeError::Read),
    }
}

/// Applies the shared probe timeout and classifies D-Bus failures at the transport boundary.
pub async fn probe<T>(future: impl Future<Output = zbus::Result<T>>) -> Result<T, Availability> {
    availability::probe(future.map_err(availability_from_error)).await
}

/// Watches service ownership changes without periodic availability probes.
struct ServiceChanges {
    proxy: DBusProxy<'static>,
    owners: NameOwnerChangedStream,
    activations: SignalStream<'static>,
}

enum ServiceChange {
    Owner(Option<OwnedUniqueName>),
    Activation,
}

impl ServiceChanges {
    /// Subscribes before the caller attempts its initial service connection.
    pub async fn new(connection: &Connection, service: &str) -> zbus::Result<Self> {
        let proxy = DBusProxy::builder(connection)
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        let owners = proxy
            .receive_name_owner_changed_with_args(&[(0, service)])
            .await?;
        // Matching an optional signal is valid even on buses that never emit it.
        let activations = proxy
            .inner()
            .receive_signal("ActivatableServicesChanged")
            .await?;

        Ok(Self {
            proxy,
            owners,
            activations,
        })
    }

    async fn next(&mut self) -> zbus::Result<ServiceChange> {
        tokio::select! {
            _ = self.proxy.inner().connection().closed() => {
                Err(std::io::Error::from(std::io::ErrorKind::NotConnected).into())
            }
            signal = self.owners.next() => {
                let signal = signal.ok_or_else(|| Error::Failure("D-Bus ownership stream closed".into()))?;
                let args = signal.args()?;

                Ok(ServiceChange::Owner(args.new_owner().as_ref().map(|name| name.to_owned().into())))
            }
            signal = self.activations.next() => {
                let signal = signal.ok_or_else(|| Error::Failure("D-Bus activation stream closed".into()))?;
                signal.body().deserialize::<()>()?;

                Ok(ServiceChange::Activation)
            }
        }
    }
}

async fn connection<F, Fut>(
    cache: &Mutex<Option<Connection>>,
    connect: F,
) -> zbus::Result<Connection>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = zbus::Result<Connection>>,
{
    let mut connection = cache.lock().await;

    if let Some(current) = connection.as_ref()
        && !current.is_closed()
    {
        return Ok(current.clone());
    }

    let current = connect().await?;

    *connection = Some(current.clone());

    Ok(current)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader},
        process::{Child, Command, Stdio},
    };
    static ADDRESS: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    static LOCK: Mutex<()> = Mutex::const_new(());

    #[test]
    fn classifies_wrapped_and_remote_service_errors_consistently() {
        use zbus::DBusError;

        let call = zbus::Message::method_call("/", "Probe")
            .unwrap()
            .build(&())
            .unwrap();

        for (error, expected) in [
            (
                FdoError::ServiceUnknown("absent".into()),
                Availability::Unavailable(UnavailableReason::ServiceMissing),
            ),
            (
                FdoError::NameHasNoOwner("absent".into()),
                Availability::Unavailable(UnavailableReason::ServiceMissing),
            ),
            (
                FdoError::AccessDenied("denied".into()),
                Availability::Failed(ProbeError::PermissionDenied),
            ),
            (
                FdoError::AuthFailed("denied".into()),
                Availability::Failed(ProbeError::PermissionDenied),
            ),
            (
                FdoError::UnknownObject("removed".into()),
                Availability::Unavailable(UnavailableReason::DeviceMissing),
            ),
            (
                FdoError::UnknownInterface("absent".into()),
                Availability::Unavailable(UnavailableReason::DeviceMissing),
            ),
            (
                FdoError::NoReply("timeout".into()),
                Availability::Failed(ProbeError::Timeout),
            ),
            (
                FdoError::Timeout("timeout".into()),
                Availability::Failed(ProbeError::Timeout),
            ),
            (
                FdoError::Failed("failure".into()),
                Availability::Failed(ProbeError::Read),
            ),
        ] {
            let reply = error.create_reply(&call.header()).unwrap();
            let remote = Error::from(reply);
            let wrapped = Error::from(error);

            assert_eq!(availability_from_error(remote), expected);
            assert_eq!(availability_from_error(wrapped), expected);
        }
    }

    #[test]
    fn classifies_timeout_device_and_protocol_failures() {
        for (error, expected) in [
            (
                zbus::fdo::Error::UnknownObject("removed".into()).into(),
                Availability::Unavailable(UnavailableReason::DeviceMissing),
            ),
            (
                zbus::fdo::Error::NoReply("timeout".into()).into(),
                Availability::Failed(ProbeError::Timeout),
            ),
            (
                zbus::Error::InvalidReply,
                Availability::Failed(ProbeError::Protocol),
            ),
            (
                std::io::Error::from(std::io::ErrorKind::PermissionDenied).into(),
                Availability::Failed(ProbeError::PermissionDenied),
            ),
        ] {
            assert_eq!(availability_from_error(error), expected);
        }
    }

    #[tokio::test]
    async fn probe_converts_dbus_failures_into_readiness() {
        let result =
            probe(async { Err::<(), _>(zbus::fdo::Error::ServiceUnknown("absent".into()).into()) })
                .await;

        assert_eq!(
            result,
            Err(Availability::Unavailable(UnavailableReason::ServiceMissing))
        );
        assert_eq!(probe(async { Ok(42) }).await, Ok(42));
    }

    pub fn address() -> Option<String> {
        ADDRESS.lock().unwrap().clone()
    }

    pub struct Bus {
        child: Child,
        address: String,
        _guard: tokio::sync::MutexGuard<'static, ()>,
    }

    impl Bus {
        pub async fn new() -> Self {
            let guard = LOCK.lock().await;
            let (child, address) = Self::start();

            *ADDRESS.lock().unwrap() = Some(address.clone());
            *SESSION.lock().await = None;
            *SYSTEM.lock().await = None;

            Self {
                child,
                address,
                _guard: guard,
            }
        }

        fn start() -> (Child, String) {
            let mut child = Command::new("dbus-daemon")
                .args(["--session", "--nofork", "--print-address"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();

            let mut address = String::new();

            BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut address)
                .unwrap();

            let address = address.trim().to_owned();

            (child, address)
        }

        pub async fn restart(&mut self) {
            self.child.kill().unwrap();
            self.child.wait().unwrap();
            (self.child, self.address) = Self::start();
            *ADDRESS.lock().unwrap() = Some(self.address.clone());
            // Retain cached connections: production recovery must detect that they closed.
        }

        pub async fn connect(&self) -> Connection {
            zbus::connection::Builder::address(self.address.as_str())
                .unwrap()
                .build()
                .await
                .unwrap()
        }
    }

    impl Drop for Bus {
        fn drop(&mut self) {
            *ADDRESS.lock().unwrap() = None;

            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
