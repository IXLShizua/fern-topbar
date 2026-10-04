mod view;

use super::{
    FeatureDefinition, FeatureId, FeatureMountContext, MountedFeature, availability::Availability,
};
use crate::runtime::Task;
use chrono::Local;
use relm4::{Component, ComponentController, Controller, gtk};
use std::time::Duration;

/// Describes availability and mounting without starting the feature.
pub fn definition() -> FeatureDefinition {
    FeatureDefinition {
        available: |services| services.availability.subscribe(FeatureId::Clock),
        mount: |context| Ok(mount(context)),
    }
}

/// Owns the component and every resource required by this mounted feature.
pub struct Mounted {
    controller: Controller<view::Clock>,
    // The source of clock updates follows the mounted feature's lifetime.
    _ticks: Task,
}

impl Mounted {
    pub fn widget(&self) -> &gtk::Widget {
        self.controller.widget().as_ref()
    }
}

fn mount(context: FeatureMountContext) -> MountedFeature {
    context
        .availability
        .publisher(FeatureId::Clock)
        .set(Availability::Available);

    let now = Local::now();
    let component = view::Clock::builder()
        .launch(view::ClockInit {
            popovers: context.popovers,
            now,
        })
        .detach();

    let input = component.sender().clone();
    let mut last_minute = now.format("%Y-%m-%d %H:%M").to_string();
    let ticks = Task::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;

            let current = Local::now();
            let minute = current.format("%Y-%m-%d %H:%M").to_string();

            if minute == last_minute {
                continue;
            }

            last_minute = minute;

            if input.send(view::Input::Tick(current)).is_err() {
                break;
            }
        }
    });

    MountedFeature::Clock(Mounted {
        controller: component,
        _ticks: ticks,
    })
}
