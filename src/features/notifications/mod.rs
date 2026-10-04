mod host;
mod icon;
mod menu;
mod state;
mod toasts;

use super::{FeatureDefinition, FeatureId, FeatureMountContext, MountedFeature};
use crate::backend::notifications::{Urgency, server as backend};
use crate::ui::monitor::MonitorSubscription;
use relm4::{Component, ComponentController, Controller, gtk};

fn urgency_class(urgency: Urgency) -> &'static str {
    match urgency {
        Urgency::Low => "low",
        Urgency::Normal => "normal",
        Urgency::Critical => "critical",
    }
}

/// Describes availability and mounting without starting the feature.
pub fn definition() -> FeatureDefinition {
    FeatureDefinition {
        available: |services| services.availability.subscribe(FeatureId::Notifications),
        mount: |context| Ok(mount(context)),
    }
}

/// Owns the component and every resource required by this mounted feature.
pub struct Mounted {
    controller: Controller<host::Host>,
    // Retain the backend task while the mounted UI waits for service/device recovery.
    _backend: backend::Backend,
    // Host inputs include a GTK monitor, so forwarding runs on the GTK executor.
    forwarder: gtk::glib::JoinHandle<()>,
    _monitor_subscription: MonitorSubscription,
}

impl Mounted {
    pub fn widget(&self) -> &gtk::Widget {
        self.controller.widget().as_ref()
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        // Dropping a GLib handle detaches its task; explicitly cancel the subscription.
        self.forwarder.abort();
    }
}

fn mount(context: FeatureMountContext) -> MountedFeature {
    let mut backend =
        backend::Backend::start(context.availability.publisher(FeatureId::Notifications));
    let controls = backend.controls();
    let mut events = backend.take_events();
    let host = host::Host::builder()
        .launch(host::HostInit {
            popovers: context.popovers,
            controls,
        })
        .detach();

    let monitor_subscription = context
        .monitor_selection
        .subscribe(host.sender().clone(), host::Input::Monitor);

    let input = host.sender().clone();
    let forwarder = relm4::spawn_local(async move {
        while let Some(event) = events.recv().await {
            if input.send(host::Input::ServerEvent(event)).is_err() {
                break;
            }
        }
    });

    MountedFeature::Notifications(Mounted {
        controller: host,
        _backend: backend,
        forwarder,
        _monitor_subscription: monitor_subscription,
    })
}
