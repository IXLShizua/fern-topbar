//! Persistent configured slots and separators between visible features.

use super::Input;
use crate::{
    config::FeatureMode,
    features::{FeatureId, MountedFeature, availability::Availability},
    runtime::Task,
    ui::core::FeatureDivider,
};
use relm4::gtk::prelude::*;
use relm4::{WidgetTemplate, gtk};
use tokio::sync::watch;

pub struct FeatureSlot {
    name: FeatureId,
    mode: FeatureMode,
    availability: Availability,
    container: gtk::Box,
    divider: FeatureDivider,
    feature: MountedFeature,
    // Keep readiness observation alive while hidden, and cancel it on slot removal.
    _updates: Task,
}

impl FeatureSlot {
    pub fn new(
        name: FeatureId,
        mode: FeatureMode,
        feature: MountedFeature,
        mut updates: watch::Receiver<Availability>,
        input: relm4::Sender<Input>,
    ) -> Self {
        let container = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        container.append(feature.widget());
        container.set_visible(false);

        let visibility_input = input.clone();

        feature.widget().connect_visible_notify(move |_| {
            let _ = visibility_input.send(Input::ContentChanged);
        });

        let forwarder = Task::spawn(async move {
            loop {
                let state = *updates.borrow_and_update();

                if input.send(Input::AvailabilityChanged(name, state)).is_err() {
                    return;
                }

                if updates.changed().await.is_err() {
                    return;
                }
            }
        });

        Self {
            name,
            mode,
            availability: Availability::Checking,
            container,
            divider: FeatureDivider::init(()),
            feature,
            _updates: forwarder,
        }
    }

    fn update(&mut self, state: Availability) {
        if self.availability == state {
            return;
        }

        let previous = self.availability;

        self.availability = state;
        self.container.set_visible(state.is_available());

        let feature = self.name.as_str();

        match (self.mode, state) {
            (_, Availability::Checking) => {}
            (_, Availability::Available) if previous == Availability::Checking => {
                tracing::debug!(feature, "feature ready");
            }
            (_, Availability::Available) => tracing::info!(feature, "feature restored"),
            (FeatureMode::Switch(true), Availability::Unavailable(reason)) => {
                tracing::error!(feature, %reason, "feature unavailable");
            }
            (FeatureMode::Switch(true), Availability::Failed(error)) => {
                tracing::error!(feature, %error, "feature availability check failed");
            }
            (FeatureMode::Auto(_), Availability::Unavailable(reason)) => {
                tracing::info!(feature, %reason, "feature unavailable; waiting for recovery");
            }
            (FeatureMode::Auto(_), Availability::Failed(error)) => {
                tracing::warn!(feature, %error, "feature availability check failed");
            }
            (FeatureMode::Switch(false), _) => {}
        }
    }
}

pub struct FeatureGroup {
    widget: gtk::Box,
    features: Vec<FeatureSlot>,
}

impl FeatureGroup {
    pub fn new(features: Vec<FeatureSlot>) -> Self {
        let widget = gtk::Box::new(gtk::Orientation::Horizontal, 0);

        for (index, slot) in features.iter().enumerate() {
            if index != 0 {
                widget.append(slot.divider.widget());
            }

            widget.append(&slot.container);
        }

        let group = Self { widget, features };
        group.refresh();

        group
    }

    pub fn update(&mut self, name: FeatureId, state: Availability) {
        if let Some(slot) = self.features.iter_mut().find(|slot| slot.name == name) {
            slot.update(state);
            self.refresh();
        }
    }

    pub fn refresh(&self) {
        let mut preceding_visible = false;

        for slot in &self.features {
            let visible = slot.container.get_visible() && slot.feature.widget().get_visible();

            slot.divider
                .widget()
                .set_visible(visible && preceding_visible);
            preceding_visible |= visible;
        }

        self.widget.set_visible(preceding_visible);
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.widget
    }
}
