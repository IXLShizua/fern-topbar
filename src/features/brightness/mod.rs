mod backend;
mod view;

use super::{
    FeatureDefinition, FeatureId, FeatureMountContext, FeatureOptionsError, MountedFeature,
};
use crate::runtime::Task;
use relm4::{Component, ComponentController, Controller, gtk};

/// Describes availability and mounting without starting the feature.
pub fn definition() -> FeatureDefinition {
    FeatureDefinition {
        available: |services| services.availability.subscribe(FeatureId::Brightness),
        mount,
    }
}

/// Owns the component and every resource required by this mounted feature.
pub struct Mounted {
    controller: Controller<view::Brightness>,
    // Retain the backend task while the mounted UI waits for service/device recovery.
    _backend: backend::Backend,
    // Aborted on unmount so the event subscription cannot outlive its controller.
    _forwarder: Task,
}

impl Mounted {
    pub fn widget(&self) -> &gtk::Widget {
        self.controller.widget().as_ref()
    }
}

fn mount(context: FeatureMountContext) -> Result<MountedFeature, FeatureOptionsError> {
    let percentage = context.options.boolean("percentage", false)?;
    let mut backend =
        backend::Backend::start(context.availability.publisher(FeatureId::Brightness));
    let commands = backend.controls();
    let mut receiver = backend.take_events();
    let controller = view::Brightness::builder()
        .launch(view::BrightnessInit {
            commands,
            popovers: context.popovers,
            percentage,
        })
        .detach();

    let sender = controller.sender().clone();

    let forwarder = Task::spawn(async move {
        while let Some(value) = receiver.recv().await {
            if sender.send(view::Input::Changed(value)).is_err() {
                break;
            }
        }
    });

    Ok(MountedFeature::Brightness(Mounted {
        controller,
        _backend: backend,
        _forwarder: forwarder,
    }))
}
