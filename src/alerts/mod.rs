//! Internal fern-topbar warnings, independent of widgets and notification transports.

pub mod battery;
pub mod service;

use crate::backend::notifications::Urgency;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc::{self, UnboundedSender};

/// Content of one ongoing warning. Its identity belongs to the handle, not its text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Alert {
    pub icon: String,
    pub summary: String,
    pub body: String,
    pub urgency: Urgency,
}

#[derive(Clone)]
pub struct AlertPublisher {
    commands: UnboundedSender<Command>,
}

impl Default for AlertPublisher {
    fn default() -> Self {
        // Inert dependency for contexts without an application-owned service.
        Self {
            commands: mpsc::unbounded_channel().0,
        }
    }
}

impl AlertPublisher {
    pub fn register(&self) -> AlertHandle {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);

        AlertHandle {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            commands: self.commands.clone(),
        }
    }
}

/// Owns one warning. Dropping it cancels queued delivery and withdraws the alert.
///
/// Delivery is asynchronous; the service retains the latest content while offline.
/// Handles are deliberately not clonable: the producer controls their lifetime.
pub struct AlertHandle {
    id: u64,
    commands: UnboundedSender<Command>,
}

impl AlertHandle {
    /// Requests attention, including after a previous dismissal.
    pub fn show(&self, alert: Alert) {
        let _ = self.commands.send(Command::Show(self.id, alert));
    }

    /// Updates content without reviving a dismissed or already delivered warning.
    pub fn update(&self, alert: Alert) {
        let _ = self.commands.send(Command::Update(self.id, alert));
    }

    /// Resolves the warning; this handle may later show a new warning.
    pub fn clear(&self) {
        let _ = self.commands.send(Command::Clear(self.id));
    }
}

impl Drop for AlertHandle {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Remove(self.id));
    }
}

enum Command {
    Show(u64, Alert),
    Update(u64, Alert),
    Clear(u64),
    Remove(u64),
}
