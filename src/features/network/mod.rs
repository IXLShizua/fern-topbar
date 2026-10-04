mod backend;
mod view;

use super::{
    FeatureDefinition, FeatureId, FeatureMountContext, FeatureOptionsError, MountedFeature,
};
use crate::runtime::Task;
use backend::Backend;
use relm4::{Component, ComponentController, Controller, gtk};

/// Describes availability and mounting without starting the feature.
pub fn definition() -> FeatureDefinition {
    FeatureDefinition {
        available: |services| services.availability.subscribe(FeatureId::Network),
        mount,
    }
}

/// Owns the component and every resource required by this mounted feature.
pub struct Mounted {
    controller: Controller<view::Network>,
    // The component owns the backend; this task owns its ordered UI event stream.
    _forwarder: Task,
}

impl Mounted {
    pub fn widget(&self) -> &gtk::Widget {
        self.controller.widget().as_ref()
    }
}

fn mount(context: FeatureMountContext) -> Result<MountedFeature, FeatureOptionsError> {
    let percentage = context.options.boolean("percentage", false)?;
    let (backend, mut receiver) =
        Backend::start(context.availability.publisher(FeatureId::Network));
    let component = view::Network::builder()
        .launch(view::NetworkInit {
            popovers: context.popovers,
            status: view::View::default(),
            backend,
            percentage,
        })
        .detach();

    let input = component.sender().clone();
    let forwarder = Task::spawn(async move {
        while let Some(event) = receiver.recv().await {
            if input.send(view::Input::BackendEvent(event)).is_err() {
                break;
            }
        }
    });

    Ok(MountedFeature::Network(Mounted {
        controller: component,
        _forwarder: forwarder,
    }))
}
