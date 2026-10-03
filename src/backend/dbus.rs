//! Shared D-Bus connections, service ownership subscriptions and readiness probes.

use crate::features::availability::{self, Availability, ProbeError, UnavailableReason};
use futures_util::{StreamExt, stream::BoxStream};
use tokio::sync::Mutex;
use zbus::{Connection, Error, fdo::DBusProxy};

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
pub fn availability_from_error(error: &zbus::Error) -> Availability {
    tracing::debug!(%error, "D-Bus availability check failed");

    let name = match error {
        zbus::Error::MethodError(name, ..) => Some(name.as_str()),
        zbus::Error::FDO(error) => match error.as_ref() {
            zbus::fdo::Error::ServiceUnknown(_) | zbus::fdo::Error::NameHasNoOwner(_) => {
                return Availability::Unavailable(UnavailableReason::ServiceMissing);
            }
            zbus::fdo::Error::AccessDenied(_) | zbus::fdo::Error::AuthFailed(_) => {
                return Availability::Failed(ProbeError::PermissionDenied);
            }
            zbus::fdo::Error::UnknownObject(_) | zbus::fdo::Error::UnknownInterface(_) => {
                return Availability::Unavailable(UnavailableReason::DeviceMissing);
            }
            zbus::fdo::Error::NoReply(_) | zbus::fdo::Error::Timeout(_) => {
                return Availability::Failed(ProbeError::Timeout);
            }
            _ => None,
        },
        zbus::Error::NameTaken => {
            return Availability::Unavailable(UnavailableReason::NameOccupied);
        }
        zbus::Error::InputOutput(error) | zbus::Error::Connection(error, _) => {
            return Availability::Failed(match error.kind() {
                std::io::ErrorKind::PermissionDenied => ProbeError::PermissionDenied,
                std::io::ErrorKind::TimedOut => ProbeError::Timeout,
                _ => ProbeError::Connect,
            });
        }
        zbus::Error::Handshake(_) => {
            return Availability::Failed(ProbeError::Connect);
        }
        zbus::Error::InvalidReply | zbus::Error::Variant(_) | zbus::Error::InvalidField => {
            return Availability::Failed(ProbeError::Protocol);
        }
        _ => None,
    };

    match name {
        Some(
            "org.freedesktop.DBus.Error.ServiceUnknown"
            | "org.freedesktop.DBus.Error.NameHasNoOwner",
        ) => Availability::Unavailable(UnavailableReason::ServiceMissing),
        Some(
            "org.freedesktop.DBus.Error.AccessDenied" | "org.freedesktop.DBus.Error.AuthFailed",
        ) => Availability::Failed(ProbeError::PermissionDenied),
        Some(
            "org.freedesktop.DBus.Error.UnknownObject"
            | "org.freedesktop.DBus.Error.UnknownInterface",
        ) => Availability::Unavailable(UnavailableReason::DeviceMissing),
        Some("org.freedesktop.DBus.Error.NoReply" | "org.freedesktop.DBus.Error.Timeout") => {
            Availability::Failed(ProbeError::Timeout)
        }
        _ => Availability::Failed(ProbeError::Read),
    }
}

/// Applies the shared probe timeout and classifies D-Bus failures at the transport boundary.
pub async fn probe<T>(future: impl Future<Output = zbus::Result<T>>) -> Result<T, Availability> {
    availability::probe(async {
        future
            .await
            .map_err(|error| availability_from_error(&error))
    })
    .await
}

/// Watches service ownership changes without periodic availability probes.
pub struct ServiceChanges {
    changes: BoxStream<'static, zbus::Result<()>>,
}

impl ServiceChanges {
    /// Subscribes before the caller attempts its initial service connection.
    pub async fn new(connection: &Connection, service: &str) -> zbus::Result<Self> {
        let proxy = DBusProxy::new(connection).await?;
        let changes = proxy
            .receive_name_owner_changed_with_args(&[(0, service)])
            .await?
            .map(|signal| signal.args().map(|_| ()))
            .boxed();

        Ok(Self { changes })
    }

    /// Waits for the next owner transition, or reports a closed bus stream.
    pub async fn changed(&mut self) -> zbus::Result<()> {
        self.changes
            .next()
            .await
            .ok_or_else(|| Error::Failure("D-Bus service subscription closed".into()))?
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
    fn missing_services_and_denied_reads_have_different_results() {
        let missing = zbus::Error::from(zbus::fdo::Error::ServiceUnknown("absent".into()));
        let denied = zbus::Error::from(zbus::fdo::Error::AccessDenied("denied".into()));

        assert_eq!(
            availability_from_error(&missing),
            Availability::Unavailable(UnavailableReason::ServiceMissing)
        );
        assert_eq!(
            availability_from_error(&denied),
            Availability::Failed(ProbeError::PermissionDenied)
        );
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
            assert_eq!(availability_from_error(&error), expected);
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
