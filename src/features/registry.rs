//! Typed feature definitions and resolution of the configured groups.

use super::{
    FeatureMountContext, FeatureOptionsError, FeatureParameters, FeatureServices, MountedFeature,
    audio, availability::Availability, battery, brightness, clock, keyboard_layout, microphone,
    network, notifications, tray, workspaces,
};
use crate::config::{FeatureMode, FeatureOptions, Features};
use serde::Deserialize;

/// Feature names accepted by the configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureId {
    Workspaces,
    Clock,
    Audio,
    Microphone,
    KeyboardLayout,
    Brightness,
    Tray,
    Network,
    Battery,
    Notifications,
}

impl FeatureId {
    pub fn all() -> impl Iterator<Item = Self> {
        FEATURE_REGISTRY.iter().map(|feature| feature.id)
    }

    pub fn as_str(self) -> &'static str {
        self.registration().name
    }

    fn registration(self) -> &'static FeatureRegistration {
        FEATURE_REGISTRY
            .iter()
            .find(|feature| feature.id == self)
            .expect("every FeatureId must be registered")
    }
}

struct FeatureRegistration {
    id: FeatureId,
    name: &'static str,
    definition: fn() -> FeatureDefinition,
}

/// Shared registration list for configuration resolution, names and readiness channels.
/// Register each new FeatureId here; availability derives its keys from this list.
const FEATURE_REGISTRY: &[FeatureRegistration] = &[
    FeatureRegistration {
        id: FeatureId::Workspaces,
        name: "workspaces",
        definition: workspaces::definition,
    },
    FeatureRegistration {
        id: FeatureId::Clock,
        name: "clock",
        definition: clock::definition,
    },
    FeatureRegistration {
        id: FeatureId::Audio,
        name: "audio",
        definition: audio::definition,
    },
    FeatureRegistration {
        id: FeatureId::Microphone,
        name: "microphone",
        definition: microphone::definition,
    },
    FeatureRegistration {
        id: FeatureId::KeyboardLayout,
        name: "keyboard_layout",
        definition: keyboard_layout::definition,
    },
    FeatureRegistration {
        id: FeatureId::Brightness,
        name: "brightness",
        definition: brightness::definition,
    },
    FeatureRegistration {
        id: FeatureId::Tray,
        name: "tray",
        definition: tray::definition,
    },
    FeatureRegistration {
        id: FeatureId::Network,
        name: "network",
        definition: network::definition,
    },
    FeatureRegistration {
        id: FeatureId::Battery,
        name: "battery",
        definition: battery::definition,
    },
    FeatureRegistration {
        id: FeatureId::Notifications,
        name: "notifications",
        definition: notifications::definition,
    },
];

/// Invalid options prevent mounting; backend failures are reported through `Availability`.
pub type Mount = fn(FeatureMountContext) -> Result<MountedFeature, FeatureOptionsError>;

/// A feature's readiness subscription and UI constructor.
///
/// Valid options create a UI/backend owner even while services are missing, so
/// readiness transitions can reveal the same component after recovery.
pub struct FeatureDefinition {
    pub available: fn(&FeatureServices) -> tokio::sync::watch::Receiver<Availability>,
    pub mount: Mount,
}

/// Configured features remain registered while dependencies are unavailable.
pub struct EnabledFeature {
    pub name: FeatureId,
    pub mode: FeatureMode,
    pub options: FeatureParameters,
    pub definition: FeatureDefinition,
}

#[derive(Default)]
pub struct EnabledFeatures {
    pub start: Vec<EnabledFeature>,
    pub center: Vec<EnabledFeature>,
    pub end: Vec<EnabledFeature>,
}

pub fn resolve(config: Features) -> EnabledFeatures {
    EnabledFeatures {
        start: resolve_group(config.start),
        center: resolve_group(config.center),
        end: resolve_group(config.end),
    }
}

fn resolve_group(options: Vec<FeatureOptions>) -> Vec<EnabledFeature> {
    options.into_iter().filter_map(resolve_feature).collect()
}

fn resolve_feature(options: FeatureOptions) -> Option<EnabledFeature> {
    if options.mode == FeatureMode::Switch(false) {
        return None;
    }

    let definition = (options.name.registration().definition)();

    Some(EnabledFeature {
        name: options.name,
        mode: options.mode,
        options: options.options.into(),
        definition,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::core::PopoverScope;
    use crate::{config::Auto, features::availability::UnavailableReason};
    use serde_json::Value;
    use std::collections::HashMap;

    #[test]
    fn invalid_percentage_is_rejected_before_starting_feature_resources() {
        let services = FeatureServices::default();

        for name in [
            FeatureId::Audio,
            FeatureId::Microphone,
            FeatureId::Brightness,
            FeatureId::Battery,
            FeatureId::Network,
        ] {
            let feature = resolve_feature(FeatureOptions {
                name,
                mode: FeatureMode::Switch(true),
                options: HashMap::from([("percentage".into(), Value::String("true".into()))]),
            })
            .unwrap();
            let context =
                FeatureMountContext::new(&services, PopoverScope::default(), feature.options);

            assert!(matches!(
                (feature.definition.mount)(context),
                Err(FeatureOptionsError::InvalidType { name, .. }) if name == "percentage"
            ));
        }
    }

    #[test]
    fn resolution_passes_each_features_parameters_to_its_mount_context() {
        let config = Features {
            end: [(FeatureId::Audio, true), (FeatureId::Microphone, false)]
                .into_iter()
                .map(|(name, value)| FeatureOptions {
                    name,
                    mode: FeatureMode::Switch(true),
                    options: HashMap::from([
                        ("percentage".into(), Value::Bool(value)),
                        ("other_parameter".into(), Value::Bool(!value)),
                    ]),
                })
                .collect(),
            start: Vec::new(),
            center: Vec::new(),
        };
        let services = FeatureServices::default();

        for feature in resolve(config).end {
            let expected = feature.name == FeatureId::Audio;
            let context =
                FeatureMountContext::new(&services, PopoverScope::default(), feature.options);

            assert_eq!(
                context.options.boolean("percentage", !expected).unwrap(),
                expected
            );
            assert_eq!(
                context
                    .options
                    .boolean("other_parameter", expected)
                    .unwrap(),
                !expected
            );
        }
    }

    #[test]
    fn registered_definitions_observe_their_own_readiness() {
        let services = FeatureServices::default();

        for name in FeatureId::all() {
            let feature = resolve_feature(FeatureOptions {
                name,
                mode: FeatureMode::Switch(true),
                options: Default::default(),
            })
            .unwrap();

            let updates = (feature.definition.available)(&services);

            assert_eq!(*updates.borrow(), Availability::Checking, "{name:?}");

            let publisher = services.availability.publisher(name);
            publisher.set(Availability::Available);

            assert_eq!(*updates.borrow(), Availability::Available, "{name:?}");

            publisher.set(Availability::Checking);
        }
    }

    #[test]
    fn resolution_preserves_groups_and_order_after_filtering() {
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
                    mode: FeatureMode::Auto(Auto::Auto),
                    options: Default::default(),
                },
            ],
            center: vec![FeatureOptions {
                name: FeatureId::Workspaces,
                mode: FeatureMode::Switch(true),
                options: Default::default(),
            }],
            end: vec![
                FeatureOptions {
                    name: FeatureId::Audio,
                    mode: FeatureMode::Switch(true),
                    options: Default::default(),
                },
                FeatureOptions {
                    name: FeatureId::Microphone,
                    mode: FeatureMode::Switch(false),
                    options: Default::default(),
                },
            ],
        };

        let features = resolve(config);
        let names = |group: Vec<EnabledFeature>| {
            group
                .into_iter()
                .map(|feature| feature.name)
                .collect::<Vec<_>>()
        };

        assert_eq!(
            names(features.start),
            [FeatureId::KeyboardLayout, FeatureId::Clock]
        );
        assert_eq!(names(features.center), [FeatureId::Workspaces]);
        assert_eq!(names(features.end), [FeatureId::Audio]);
    }

    #[test]
    fn resolution_retains_unavailable_features_in_both_enabled_modes() {
        let config = Features {
            start: Vec::new(),
            center: Vec::new(),
            end: vec![
                FeatureOptions {
                    name: FeatureId::Battery,
                    mode: FeatureMode::Auto(Auto::Auto),
                    options: Default::default(),
                },
                FeatureOptions {
                    name: FeatureId::Notifications,
                    mode: FeatureMode::Switch(true),
                    options: Default::default(),
                },
                FeatureOptions {
                    name: FeatureId::Network,
                    mode: FeatureMode::Switch(false),
                    options: Default::default(),
                },
            ],
        };

        let group = resolve(config).end;

        assert_eq!(
            group.iter().map(|feature| feature.name).collect::<Vec<_>>(),
            [FeatureId::Battery, FeatureId::Notifications]
        );

        let services = FeatureServices::default();

        for feature in group {
            let missing = Availability::Unavailable(UnavailableReason::ServiceMissing);

            services.availability.publisher(feature.name).set(missing);

            assert_eq!(*(feature.definition.available)(&services).borrow(), missing);
        }
    }
}
