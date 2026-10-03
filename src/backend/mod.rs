//! Shared desktop data sources and the combined WM/fullscreen event stream.

pub mod battery;
pub mod dbus;
mod fullscreen;
pub mod notifications;
pub mod reconnect;
pub mod wm;

use crate::{features::availability::FeatureAvailability, runtime::Task};
use snafu::Snafu;
use std::collections::HashSet;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

pub use wm::{Command, Workspace};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    OverviewOpened(Option<String>),
    OverviewClosed,
    KeyboardLayoutChanged(Option<String>),
    WorkspacesChanged(Vec<Workspace>),
    FocusedFullscreenOutputsChanged(HashSet<String>),
}

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("cannot start fullscreen monitoring"))]
    FullscreenMonitoring,
}

/// Channels, initial state and forwarding task owned by the application controller.
pub struct Backend {
    pub events: UnboundedReceiver<Event>,
    pub wm_commands: Option<UnboundedSender<Command>>,
    pub initial_fullscreen_outputs: HashSet<String>,
    // Dropping the controller aborts forwarding even while the Wayland stream is idle.
    _fullscreen_forwarder: Option<Task>,
}

impl Backend {
    /// Starts fullscreen monitoring when required, then selects the WM for this session.
    pub fn start(watch_fullscreen: bool, availability: FeatureAvailability) -> Result<Self, Error> {
        let fullscreen = if watch_fullscreen {
            Some(fullscreen::connect().map_err(|error| {
                tracing::error!(%error, "cannot start fullscreen monitoring");

                Error::FullscreenMonitoring
            })?)
        } else {
            None
        };

        let (events, event_receiver) = mpsc::unbounded_channel();
        let wm_commands = wm::start(events.clone(), availability);
        let mut initial_fullscreen_outputs = HashSet::new();

        let fullscreen_forwarder = fullscreen.map(|watcher| {
            initial_fullscreen_outputs = watcher.initial;

            Task::spawn(
                FullscreenEventForwarder {
                    changes: watcher.events,
                    events,
                }
                .run(),
            )
        });

        Ok(Self {
            events: event_receiver,
            wm_commands,
            initial_fullscreen_outputs,
            _fullscreen_forwarder: fullscreen_forwarder,
        })
    }
}

/// Forwards fullscreen changes and stops when either side of the stream closes.
struct FullscreenEventForwarder {
    changes: UnboundedReceiver<HashSet<String>>,
    events: UnboundedSender<Event>,
}

impl FullscreenEventForwarder {
    async fn run(mut self) {
        loop {
            tokio::select! {
                _ = self.events.closed() => return,
                outputs = self.changes.recv() => {
                    let Some(outputs) = outputs else {
                        return;
                    };

                    if self.events.send(Event::FocusedFullscreenOutputsChanged(outputs)).is_err() {
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    #[tokio::test]
    async fn fullscreen_events_preserve_order_and_stop_when_the_source_closes() {
        let (changes, receiver) = mpsc::unbounded_channel();
        let (events, mut backend_events) = mpsc::unbounded_channel();
        let worker = tokio::spawn(
            FullscreenEventForwarder {
                changes: receiver,
                events,
            }
            .run(),
        );

        for outputs in [HashSet::from(["DP-1".into()]), HashSet::new()] {
            changes.send(outputs.clone()).unwrap();
            assert_eq!(
                timeout(Duration::from_secs(1), backend_events.recv())
                    .await
                    .unwrap(),
                Some(Event::FocusedFullscreenOutputsChanged(outputs)),
            );
        }

        drop(changes);
        timeout(Duration::from_secs(1), worker)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(backend_events.recv().await, None);
    }

    #[tokio::test]
    async fn fullscreen_forwarder_stops_when_unused_without_waiting_for_a_change() {
        let (changes, receiver) = mpsc::unbounded_channel();
        let (events, backend_events) = mpsc::unbounded_channel();
        let worker = tokio::spawn(
            FullscreenEventForwarder {
                changes: receiver,
                events,
            }
            .run(),
        );

        drop(backend_events);
        timeout(Duration::from_secs(1), worker)
            .await
            .unwrap()
            .unwrap();
        assert!(changes.is_closed());
    }
}
