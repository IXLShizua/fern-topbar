//! Sends application notifications to whichever desktop notification server is active.

use super::{ATTENTION_HINT, Urgency};
use crate::backend::dbus;
use futures_util::StreamExt;
use snafu::Snafu;
use std::collections::HashMap;
use zbus::{
    fdo::NameOwnerChangedStream, names::OwnedUniqueName, proxy::SignalStream, zvariant::OwnedValue,
};

const SERVICE: &str = "org.freedesktop.Notifications";

#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[allow(
        clippy::too_many_arguments,
        reason = "Notify's signature is defined by D-Bus"
    )]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, OwnedValue>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    fn close_notification(&self, id: u32) -> zbus::Result<()>;
}

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("notification connection failed"))]
    Connect,
    #[snafu(display("notification delivery failed"))]
    Notify,
    #[snafu(display("notification close failed"))]
    Close,
    #[snafu(display("notification subscription failed"))]
    Read,
}

/// IDs are valid only for this server owner; never replace an ID on a new owner.
pub struct NotificationClient {
    proxy: NotificationsProxy<'static>,
    owner: OwnedUniqueName,
    closed: SignalStream<'static>,
    owners: NameOwnerChangedStream,
}

impl NotificationClient {
    pub async fn connect() -> Result<Self, Error> {
        dbus::probe(async {
            let connection = dbus::session().await?;
            let bus = zbus::fdo::DBusProxy::new(&connection).await?;
            let owners = bus
                .receive_name_owner_changed_with_args(&[(0, SERVICE)])
                .await?;
            let owner = bus.get_name_owner(SERVICE.try_into()?).await?;
            let proxy = NotificationsProxy::builder(&connection)
                .destination(owner.clone())?
                .build()
                .await?;

            let closed = proxy.inner().receive_signal("NotificationClosed").await?;

            Ok(Self {
                proxy,
                owner,
                closed,
                owners,
            })
        })
        .await
        .map_err(|state| {
            tracing::debug!(?state, "cannot connect notification client");

            Error::Connect
        })
    }

    pub fn owner(&self) -> &OwnedUniqueName {
        &self.owner
    }

    pub async fn notify(
        &mut self,
        replaces: u32,
        icon: &str,
        summary: &str,
        body: &str,
        urgency: Urgency,
        request_attention: bool,
    ) -> Result<u32, Error> {
        let hints = HashMap::from([
            ("urgency", OwnedValue::from(urgency as u8)),
            (ATTENTION_HINT, OwnedValue::from(request_attention)),
        ]);

        let timeout = if urgency == Urgency::Critical { 0 } else { -1 };

        self.proxy
            .notify(
                "fern-topbar",
                replaces,
                icon,
                summary,
                body,
                &[],
                hints,
                timeout,
            )
            .await
            .map_err(|error| {
                tracing::warn!(%error, "cannot send application notification");

                Error::Notify
            })
    }

    pub async fn close(&mut self, id: u32) -> Result<(), Error> {
        self.proxy.close_notification(id).await.map_err(|error| {
            tracing::warn!(%error, id, "cannot close application notification");

            Error::Close
        })
    }

    /// Also wakes when the server or bus disappears, without polling a healthy session.
    pub async fn next_closed(&mut self) -> Result<u32, Error> {
        loop {
            tokio::select! {
                biased;
                changed = self.owners.next() => {
                    let changed = changed.ok_or(Error::Read)?;
                    let args = changed.args().map_err(|error| {
                        tracing::warn!(%error, "invalid notification server ownership signal");

                        Error::Read
                    })?;

                    // Consume the signal atomically, without an await after removal
                    // from the stream. Cancelling this read in select must not lose
                    // an ownership transition while a lookup is still in flight.
                    if args.old_owner().as_ref().is_some_and(|owner| owner.as_str() == self.owner.as_str()) {
                        return Err(Error::Connect);
                    }
                }
                signal = self.closed.next() => {
                    let Some(signal) = signal else {
                        return Err(Error::Read);
                    };
                    let (id, _reason) = signal.body().deserialize::<(u32, u32)>().map_err(|error| {
                        tracing::warn!(%error, "invalid NotificationClosed signal");

                        Error::Read
                    })?;

                    return Ok(id);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::notifications::{Event, server::Backend};
    use crate::features::availability::{Availability, AvailabilityPublisher, tests::wait_for};
    use std::time::Duration;

    #[tokio::test]
    async fn sends_typed_urgency_replaces_and_receives_closure_from_our_server() {
        let bus = dbus::tests::Bus::new().await;
        let publisher = AvailabilityPublisher::default();
        let mut readiness = publisher.subscribe();
        let mut backend = Backend::start(publisher);
        let mut events = backend.take_events();

        wait_for(&mut readiness, Availability::Available).await;

        let mut client = NotificationClient::connect().await.unwrap();
        let mut id = 0;

        for urgency in [Urgency::Low, Urgency::Normal, Urgency::Critical] {
            let next = client
                .notify(
                    id,
                    "battery-caution-symbolic",
                    "Battery",
                    "5%",
                    urgency,
                    true,
                )
                .await
                .unwrap();

            if id != 0 {
                assert_eq!(next, id);
            }

            id = next;

            let Event::Added(item, timeout) = events.recv().await.unwrap() else {
                panic!("expected notification");
            };

            assert_eq!(item.id, id);
            assert_eq!(item.urgency, urgency);
            assert!(item.request_attention);
            assert_eq!(timeout, if urgency == Urgency::Critical { 0 } else { -1 });
        }

        backend.controls().close(id, 2);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), client.next_closed())
                .await
                .unwrap()
                .unwrap(),
            id
        );

        // A replacement server may reuse IDs. The client must detect the owner
        // transition before attempting to replace any of the former server's IDs.
        drop(backend);

        let replacement = bus.connect().await;

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if replacement.request_name(SERVICE).await.is_ok() {
                    break;
                }

                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(2), client.next_closed())
                .await
                .unwrap()
                .is_err()
        );
    }
}
