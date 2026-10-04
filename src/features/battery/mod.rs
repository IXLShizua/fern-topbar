mod view;

use super::{
    FeatureDefinition, FeatureId, FeatureMountContext, FeatureOptionsError, MountedFeature,
};
use crate::runtime::Task;
use relm4::{Component, ComponentController, Controller, gtk};

/// Describes availability and mounting without starting the feature.
pub fn definition() -> FeatureDefinition {
    FeatureDefinition {
        available: |services| services.availability.subscribe(FeatureId::Battery),
        mount,
    }
}

/// Owns the component and every resource required by this mounted feature.
pub struct Mounted {
    controller: Controller<view::Battery>,
    // Aborted on unmount so the event subscription cannot outlive its controller.
    _forwarder: Task,
}

impl Mounted {
    pub fn widget(&self) -> &gtk::Widget {
        self.controller.widget().as_ref()
    }
}

fn mount(context: FeatureMountContext) -> Result<MountedFeature, FeatureOptionsError> {
    let percentage = context.options.boolean("percentage", true)?;
    let component = view::Battery::builder().launch(()).detach();
    let input = component.sender().clone();
    let mut receiver = context.battery.subscribe();
    let forwarder = Task::spawn(async move {
        loop {
            let status = receiver.borrow_and_update().clone();
            let view = status.map(|status| view::View::from_status(status, percentage));

            if input.send(view::Input::Changed(view)).is_err() || receiver.changed().await.is_err()
            {
                break;
            }
        }
    });

    Ok(MountedFeature::Battery(Mounted {
        controller: component,
        _forwarder: forwarder,
    }))
}
