mod backend;
mod control;
mod model;

use crate::{
    features::{
        FeatureId, FeatureMountContext, FeatureOptionsError, MountedFeature,
        availability::FeatureAvailability,
    },
    runtime::Task,
};
use relm4::{Component, ComponentController, Controller, gtk};
use std::{cell::RefCell, rc::Rc};
use tokio::sync::watch;

pub(crate) use model::{AudioDevice, Volume};

#[derive(Clone, Copy)]
pub(crate) struct VolumeControlOptions {
    pub device: AudioDevice,
    pub default_icon: &'static str,
    pub button_tooltip: &'static str,
    pub mute_tooltip: &'static str,
    pub scale_tooltip: &'static str,
    pub button_class: &'static str,
    pub menu_class: &'static str,
    pub row_class: &'static str,
    pub icon: fn(Volume) -> &'static str,
}

/// Lazily starts and shares the audio backend between volume controls.
/// This UI-side owner serves both audio and microphone; transport code stays in `backend`.
#[derive(Clone, Default)]
pub(crate) struct AudioService {
    inner: Rc<RefCell<Option<AudioRuntime>>>,
}

struct AudioRuntime {
    controls: backend::Controls,
    states: [watch::Sender<Option<Volume>>; 2],
    // Both audio controls share this owner; retain the PulseAudio connection.
    _backend: backend::Backend,
    _forwarder: Task,
}

struct VolumeSubscription {
    controls: backend::Controls,
    updates: watch::Receiver<Option<Volume>>,
}

impl AudioService {
    fn subscribe(
        &self,
        device: AudioDevice,
        availability: &FeatureAvailability,
    ) -> VolumeSubscription {
        let mut inner = self.inner.borrow_mut();
        let runtime = inner.get_or_insert_with(|| AudioRuntime::start(availability));

        VolumeSubscription {
            controls: runtime.controls.clone(),
            updates: runtime.states[device.index()].subscribe(),
        }
    }
}

impl AudioRuntime {
    fn start(availability: &FeatureAvailability) -> Self {
        let mut backend = backend::Backend::start([
            availability.publisher(FeatureId::Audio),
            availability.publisher(FeatureId::Microphone),
        ]);

        let controls = backend.controls();
        let mut events = backend.take_events();
        let (output, _) = watch::channel(None);
        let (input, _) = watch::channel(None);
        let states = [output, input];
        let publishers = states.clone();
        let forwarder = Task::spawn(async move {
            while let Some(event) = events.recv().await {
                publishers[event.device.index()].send_replace(event.volume);
            }
        });

        Self {
            controls,
            states,
            _backend: backend,
            _forwarder: forwarder,
        }
    }
}

/// Owns one volume component, its forwarding task and the shared audio backend.
pub struct Mounted {
    controller: Controller<control::VolumeControl>,
    _forwarder: Task,
    // Keeps the shared backend alive after the temporary mount context is dropped.
    _audio_service: AudioService,
}

impl Mounted {
    pub fn widget(&self) -> &gtk::Widget {
        self.controller.widget().as_ref()
    }
}

pub(crate) fn mount_control(
    context: FeatureMountContext,
    spec: VolumeControlOptions,
) -> Result<MountedFeature, FeatureOptionsError> {
    let percentage = context.options.boolean("percentage", false)?;
    let audio_service = context.audio.clone();
    let VolumeSubscription {
        controls,
        mut updates,
    } = audio_service.subscribe(spec.device, &context.availability);

    let controller = control::VolumeControl::builder()
        .launch(control::VolumeControlInit {
            spec,
            controls,
            popovers: context.popovers,
            percentage,
        })
        .detach();

    let sender = controller.sender().clone();
    let forwarder = Task::spawn(async move {
        loop {
            let sound = updates.borrow_and_update().map(|volume| control::Sound {
                level: volume.percent(),
                muted: volume.muted,
            });

            if sender.send(control::Input::Sound(sound)).is_err() {
                break;
            }

            if updates.changed().await.is_err() {
                break;
            }
        }
    });

    Ok(MountedFeature::Sound(Mounted {
        controller,
        _forwarder: forwarder,
        _audio_service: audio_service,
    }))
}
