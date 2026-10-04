mod backend;
mod view;

use super::{FeatureDefinition, FeatureId, FeatureMountContext, MountedFeature};
use crate::runtime::Task;
use relm4::{Component, ComponentController, Controller, gtk};

/// Describes availability and mounting without starting the feature.
pub fn definition() -> FeatureDefinition {
    FeatureDefinition {
        available: |services| services.availability.subscribe(FeatureId::Tray),
        mount: |context| Ok(mount(context)),
    }
}

/// Owns the component and every resource required by this mounted feature.
pub struct Mounted {
    controller: Controller<view::Tray>,
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

fn mount(context: FeatureMountContext) -> MountedFeature {
    let mut backend = backend::Backend::start(context.availability.publisher(FeatureId::Tray));
    let commands = backend.controls();
    let mut receiver = backend.take_items();
    let controller = view::Tray::builder()
        .launch(view::TrayInit {
            commands,
            popovers: context.popovers,
        })
        .detach();

    let sender = controller.sender().clone();

    let forwarder = Task::spawn(async move {
        while let Some(items) = receiver.recv().await {
            if sender.send(view::TrayInput::Items(items)).is_err() {
                break;
            }
        }
    });

    MountedFeature::Tray(Mounted {
        controller,
        _backend: backend,
        _forwarder: forwarder,
    })
}
