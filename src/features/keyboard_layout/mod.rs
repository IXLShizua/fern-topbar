mod view;

use super::{FeatureDefinition, FeatureId, FeatureMountContext, MountedFeature};
use crate::runtime::Task;
use relm4::{Component, ComponentController, Controller, gtk};

/// Describes availability and mounting without starting the feature.
pub fn definition() -> FeatureDefinition {
    FeatureDefinition {
        available: |services| services.availability.subscribe(FeatureId::KeyboardLayout),
        mount: |context| Ok(mount(context)),
    }
}

/// Owns the component and every resource required by this mounted feature.
pub struct Mounted {
    controller: Controller<view::KeyboardLayout>,
    // Abort the WM subscription even while it is waiting for the next update.
    _forwarder: Task,
}

impl Mounted {
    pub fn widget(&self) -> &gtk::Widget {
        self.controller.widget().as_ref()
    }
}

fn mount(context: FeatureMountContext) -> MountedFeature {
    let mut updates = context.window_manager.subscribe_keyboard_layout();
    let initial = updates.borrow_and_update().clone();
    let component = view::KeyboardLayout::builder().launch(initial).detach();

    let input = component.sender().clone();
    let forwarder = Task::spawn(async move {
        while updates.changed().await.is_ok() {
            let layout = updates.borrow_and_update().clone();

            if input.send(layout).is_err() {
                break;
            }
        }
    });

    MountedFeature::KeyboardLayout(Mounted {
        controller: component,
        _forwarder: forwarder,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{features::FeatureServices, ui::core::PopoverScope};
    use relm4::gtk::prelude::*;
    use std::time::{Duration, Instant};

    #[gtk::test]
    fn mounting_reads_current_layout_and_forwards_updates_until_unmounted() {
        let services = FeatureServices::default();

        services
            .window_manager
            .set_keyboard_layout(Some("English".into()));

        let mount = || {
            let context =
                FeatureMountContext::new(&services, PopoverScope::default(), Default::default());
            let MountedFeature::KeyboardLayout(mounted) = super::mount(context) else {
                panic!("expected the keyboard layout feature");
            };

            mounted
        };

        let mounted = mount();
        let label = mounted.controller.widget();

        assert_eq!(label.label(), "English");
        assert!(label.get_visible());

        services.window_manager.set_keyboard_layout(None);

        let context = gtk::glib::MainContext::default();
        let deadline = Instant::now() + Duration::from_secs(2);

        while label.get_visible() {
            while context.pending() {
                context.iteration(false);
            }

            assert!(Instant::now() < deadline, "layout update timed out");
            std::thread::sleep(Duration::from_millis(1));
        }

        assert_eq!(label.label(), "");
        drop(mounted);
        services
            .window_manager
            .set_keyboard_layout(Some("Russian".into()));

        let remounted = mount();

        assert_eq!(remounted.controller.widget().label(), "Russian");
        assert!(remounted.controller.widget().get_visible());
    }
}
