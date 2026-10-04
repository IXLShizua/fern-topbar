//! Feature composition for the panel's start, center and end slots.

use super::{BAR_HEIGHT, core::PopoverScope};
use crate::features::{EnabledFeature, EnabledFeatures, FeatureMountContext, FeatureServices};
use crate::features::{FeatureId, availability::Availability};
use feature::{FeatureGroup, FeatureSlot};
use relm4::gtk;
use relm4::gtk::prelude::*;
use relm4::prelude::*;

mod feature;

/// Owns mounted feature groups and applies their availability changes.
pub struct PanelContent {
    start: FeatureGroup,
    center: FeatureGroup,
    end: FeatureGroup,
}

/// Mount configuration, services and the parent panel's shared popover scope.
pub struct PanelContentInit {
    pub features: EnabledFeatures,
    pub services: FeatureServices,
    pub popovers: PopoverScope,
}

/// Availability and layout messages received from mounted feature slots.
#[derive(Debug)]
pub enum Input {
    AvailabilityChanged(FeatureId, Availability),
    ContentChanged,
}

#[relm4::component(pub)]
impl Component for PanelContent {
    type Init = PanelContentInit;
    type Input = Input;
    type Output = ();
    type CommandOutput = ();

    view! {
        #[root]
        gtk::CenterBox {
            add_css_class: "topbar-content",
            set_size_request: (-1, BAR_HEIGHT),
            #[wrap(Some)]
            set_start_widget = &gtk::Box { #[local_ref] start -> gtk::Box {} },
            #[wrap(Some)]
            set_center_widget = &gtk::Box { #[local_ref] center -> gtk::Box {} },
            #[wrap(Some)]
            set_end_widget = &gtk::Box { #[local_ref] end -> gtk::Box {} },
        }
    }

    /// Creates every configured feature once; readiness controls its slot visibility.
    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let PanelContentInit {
            features,
            services,
            popovers,
        } = init;

        let mount_group = |features: Vec<EnabledFeature>| {
            let mut mounted = Vec::with_capacity(features.len());

            for feature in features {
                let EnabledFeature {
                    name,
                    mode,
                    options,
                    definition,
                } = feature;
                let context = FeatureMountContext::new(&services, popovers.clone(), options);

                let updates = (definition.available)(&services);
                let component = match (definition.mount)(context) {
                    Ok(component) => component,
                    Err(error) => {
                        tracing::error!(feature = name.as_str(), %error, "invalid feature options");

                        continue;
                    }
                };

                mounted.push(FeatureSlot::new(
                    name,
                    mode,
                    component,
                    updates,
                    sender.input_sender().clone(),
                ));
            }

            FeatureGroup::new(mounted)
        };

        let model = Self {
            start: mount_group(features.start),
            center: mount_group(features.center),
            end: mount_group(features.end),
        };

        let start = model.start.widget().clone();
        let center = model.center.widget().clone();
        let end = model.end.widget().clone();
        end.add_css_class("topbar-end");

        let widgets = view_output!();

        ComponentParts { model, widgets }
    }

    /// Updates feature visibility and group layout without interpreting feature data.
    fn update(&mut self, message: Self::Input, _sender: ComponentSender<Self>, _: &Self::Root) {
        match message {
            Input::AvailabilityChanged(name, state) => {
                self.start.update(name, state);
                self.center.update(name, state);
                self.end.update(name, state);
            }
            Input::ContentChanged => {
                self.start.refresh();
                self.center.refresh();
                self.end.refresh();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{backend::wm::state::WindowManagerState, ui::monitor::MonitorSelection};
    use crate::{
        config::{Auto, FeatureMode, FeatureOptions, Features},
        features,
    };
    use serde_json::Value;
    use std::collections::HashMap;

    fn children(widget: &gtk::Widget) -> Vec<gtk::Widget> {
        let mut children = Vec::new();
        let mut child = widget.first_child();

        while let Some(widget) = child {
            child = widget.next_sibling();
            children.push(widget);
        }

        children
    }

    #[gtk::test]
    fn invalid_options_skip_only_the_affected_feature() {
        let config = Features {
            start: vec![
                FeatureOptions {
                    name: FeatureId::Brightness,
                    mode: FeatureMode::Switch(true),
                    options: HashMap::from([("percentage".into(), Value::String("true".into()))]),
                },
                FeatureOptions {
                    name: FeatureId::Clock,
                    mode: FeatureMode::Switch(true),
                    options: HashMap::new(),
                },
            ],
            center: Vec::new(),
            end: Vec::new(),
        };
        let content = PanelContent::builder()
            .launch(PanelContentInit {
                features: features::resolve(config),
                services: FeatureServices::default(),
                popovers: PopoverScope::default(),
            })
            .detach();
        let slots = children(content.model().start.widget().upcast_ref());

        assert_eq!(slots.len(), 1);
        assert!(slots[0].first_child().unwrap().has_css_class("topbar-time"));
    }

    #[gtk::test]
    fn configured_groups_mount_components_in_order_with_dividers() {
        let config = Features {
            start: vec![
                FeatureOptions {
                    name: FeatureId::KeyboardLayout,
                    mode: FeatureMode::Switch(true),
                    options: Default::default(),
                },
                FeatureOptions {
                    name: FeatureId::Network,
                    mode: FeatureMode::Switch(false),
                    options: Default::default(),
                },
                FeatureOptions {
                    name: FeatureId::Clock,
                    mode: FeatureMode::Switch(true),
                    options: Default::default(),
                },
            ],
            center: vec![FeatureOptions {
                name: FeatureId::Workspaces,
                mode: FeatureMode::Switch(true),
                options: Default::default(),
            }],
            end: Vec::new(),
        };

        let content = PanelContent::builder()
            .launch(PanelContentInit {
                features: features::resolve(config),
                services: FeatureServices {
                    availability: features::availability::FeatureAvailability::default(),
                    audio: features::AudioService::default(),
                    battery: Default::default(),
                    alerts: Default::default(),
                    window_manager: WindowManagerState::default(),
                    wm_commands: None,
                    monitor_selection: MonitorSelection::default(),
                },
                popovers: PopoverScope::default(),
            })
            .detach();

        // Each slot wraps its feature group in a layout box.
        let start = children(
            &content
                .widget()
                .start_widget()
                .unwrap()
                .first_child()
                .unwrap(),
        );

        let center = children(
            &content
                .widget()
                .center_widget()
                .unwrap()
                .first_child()
                .unwrap(),
        );

        let end = children(
            &content
                .widget()
                .end_widget()
                .unwrap()
                .first_child()
                .unwrap(),
        );

        assert_eq!(start.len(), 3);
        assert!(
            start[0]
                .first_child()
                .unwrap()
                .has_css_class("topbar-layout")
        );
        assert!(start[1].is::<gtk::Separator>());
        assert!(start[2].first_child().unwrap().has_css_class("topbar-time"));
        assert_eq!(center.len(), 1);
        assert!(
            center[0]
                .first_child()
                .unwrap()
                .has_css_class("topbar-workspaces")
        );
        assert!(end.is_empty());
    }

    #[gtk::test]
    fn unavailable_slots_return_in_order_without_recreating_components() {
        use crate::features::availability::{Availability, UnavailableReason};

        let config = Features {
            start: vec![
                FeatureOptions {
                    name: FeatureId::Clock,
                    mode: FeatureMode::Switch(true),
                    options: Default::default(),
                },
                FeatureOptions {
                    name: FeatureId::KeyboardLayout,
                    mode: FeatureMode::Auto(Auto::Auto),
                    options: Default::default(),
                },
                FeatureOptions {
                    name: FeatureId::Workspaces,
                    mode: FeatureMode::Switch(true),
                    options: Default::default(),
                },
            ],
            center: Vec::new(),
            end: Vec::new(),
        };

        let services = FeatureServices::default();
        let availability = services.availability.clone();

        services
            .window_manager
            .set_keyboard_layout(Some("English".into()));

        let content = PanelContent::builder()
            .launch(PanelContentInit {
                features: features::resolve(config),
                services,
                popovers: PopoverScope::default(),
            })
            .detach();

        let group = content
            .widget()
            .start_widget()
            .unwrap()
            .first_child()
            .unwrap();

        let slots = children(&group);

        assert_eq!(slots.len(), 5);

        let layout = slots[2].first_child().unwrap();
        let main = gtk::glib::MainContext::default();
        let settle = |condition: &dyn Fn() -> bool| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);

            while !condition() {
                while main.pending() {
                    main.iteration(false);
                }

                assert!(
                    std::time::Instant::now() < deadline,
                    "UI state did not settle: slots {:?}, roots {:?}",
                    slots
                        .iter()
                        .map(|widget| widget.get_visible())
                        .collect::<Vec<_>>(),
                    slots
                        .iter()
                        .map(|widget| widget.first_child().map(|child| child.get_visible()))
                        .collect::<Vec<_>>()
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        };

        let missing = Availability::Unavailable(UnavailableReason::ServiceMissing);

        availability
            .publisher(FeatureId::KeyboardLayout)
            .set(missing);
        availability
            .publisher(FeatureId::Workspaces)
            .set(Availability::Available);
        settle(&|| {
            slots[0].get_visible()
                && slots[4].get_visible()
                && !slots[2].get_visible()
                && slots[3].get_visible()
        });
        assert!(!slots[1].get_visible());
        availability
            .publisher(FeatureId::KeyboardLayout)
            .set(Availability::Available);
        settle(&|| slots[2].get_visible() && slots[1].get_visible() && slots[3].get_visible());
        assert_eq!(slots[2].first_child().unwrap(), layout);
        availability
            .publisher(FeatureId::KeyboardLayout)
            .set(missing);
        settle(&|| !slots[2].get_visible() && !slots[1].get_visible());
        availability
            .publisher(FeatureId::KeyboardLayout)
            .set(Availability::Available);
        settle(&|| slots[2].get_visible() && slots[1].get_visible());
        assert_eq!(slots[2].first_child().unwrap(), layout);
    }
}
