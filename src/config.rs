use crate::{features::FeatureId, ui::core::UiScale};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum FeatureMode {
    Switch(bool),
    Auto(Auto),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Auto {
    Auto,
}

/// Placement and ordering of configured features. Omitted groups are empty.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Features {
    #[serde(default)]
    pub start: Vec<FeatureOptions>,

    #[serde(default)]
    pub center: Vec<FeatureOptions>,

    #[serde(default)]
    pub end: Vec<FeatureOptions>,
}

/// One named feature; its options are interpreted by the owning feature.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureOptions {
    pub name: FeatureId,
    pub mode: FeatureMode,
    #[serde(default)]
    pub options: HashMap<String, serde_json::Value>,
}

impl Features {
    fn validate(&self) -> Result<(), String> {
        let mut seen = HashSet::new();

        for feature in self.start.iter().chain(&self.center).chain(&self.end) {
            if !seen.insert(feature.name) {
                return Err(format!(
                    "feature '{}' occurs more than once in features",
                    feature.name.as_str(),
                ));
            }
        }

        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Reveal {
    pub on_niri_overview: bool,
    pub on_hover: Hover,
}

#[derive(Clone, Copy, Debug)]
pub struct Hover {
    pub enabled: bool,
    pub on_fullscreen: bool,
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub scale: UiScale,
    pub monitor: Option<String>,
    pub features: Features,
    pub reveal: Reveal,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RevealConfig {
    on_niri_overview: bool,
    on_hover: HoverConfig,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct HoverConfig {
    enabled: bool,
    on_fullscreen: Option<bool>,
}

impl Default for HoverConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            on_fullscreen: None,
        }
    }
}

impl Default for RevealConfig {
    fn default() -> Self {
        Self {
            on_niri_overview: true,
            on_hover: HoverConfig::default(),
        }
    }
}

impl Default for Features {
    fn default() -> Self {
        let enabled = FeatureMode::Switch(true);
        let auto = FeatureMode::Auto(Auto::Auto);
        let option = |name, mode| FeatureOptions {
            name,
            mode,
            options: HashMap::new(),
        };

        Self {
            start: vec![option(FeatureId::Workspaces, enabled)],
            center: vec![option(FeatureId::Clock, enabled)],
            end: vec![
                option(FeatureId::Audio, enabled),
                option(FeatureId::Microphone, enabled),
                option(FeatureId::KeyboardLayout, enabled),
                option(FeatureId::Brightness, auto),
                option(FeatureId::Tray, auto),
                option(FeatureId::Network, auto),
                option(FeatureId::Battery, auto),
                option(FeatureId::Notifications, auto),
            ],
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    scale: f64,
    monitor: Option<String>,
    features: Features,
    reveal: RevealConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            scale: 1.0,
            monitor: None,
            features: Features::default(),
            reveal: RevealConfig::default(),
        }
    }
}

impl Config {
    fn into_settings(self) -> Result<Settings, String> {
        self.features.validate()?;

        let monitor = self.monitor.map(|monitor| monitor.trim().to_owned());

        if monitor.as_deref() == Some("") {
            return Err("monitor cannot be empty; use null for automatic selection".into());
        }

        let on_fullscreen = self
            .reveal
            .on_hover
            .on_fullscreen
            .unwrap_or(self.reveal.on_hover.enabled);

        if !self.reveal.on_hover.enabled && on_fullscreen {
            return Err("reveal.on_hover.on_fullscreen requires reveal.on_hover.enabled".into());
        }

        Ok(Settings {
            scale: UiScale::new(self.scale)?,
            monitor,
            features: self.features,
            reveal: Reveal {
                on_niri_overview: self.reveal.on_niri_overview,
                on_hover: Hover {
                    enabled: self.reveal.on_hover.enabled,
                    on_fullscreen,
                },
            },
        })
    }
}

pub fn load() -> Result<Settings, String> {
    let path = path();

    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && env::var_os("FERN_TOPBAR_CONFIG").is_none() =>
        {
            return Config::default().into_settings();
        }
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };

    let config = serde_json::from_str::<Config>(&contents)
        .map_err(|error| format!("invalid {}: {error}", path.display()))?;

    config
        .into_settings()
        .map_err(|error| format!("invalid {}: {error}", path.display()))
}

fn path() -> PathBuf {
    if let Some(path) = env::var_os("FERN_TOPBAR_CONFIG") {
        return PathBuf::from(path);
    }

    let directory = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".config"));

    directory.join("fern-topbar/config.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_feature_groups_can_be_empty() {
        let config = Config {
            features: Features {
                start: Vec::new(),
                center: Vec::new(),
                end: Vec::new(),
            },
            ..Config::default()
        };

        let features = config.into_settings().unwrap().features;

        assert!(features.start.is_empty());
        assert!(features.center.is_empty());
        assert!(features.end.is_empty());
    }

    #[test]
    fn repeated_features_are_rejected_within_and_between_groups() {
        let enabled = FeatureOptions {
            name: FeatureId::Clock,
            mode: FeatureMode::Switch(true),
            options: Default::default(),
        };

        let disabled = FeatureOptions {
            name: FeatureId::Clock,
            mode: FeatureMode::Switch(false),
            options: Default::default(),
        };

        for features in [
            Features {
                start: vec![enabled.clone(), disabled.clone()],
                center: Vec::new(),
                end: Vec::new(),
            },
            Features {
                start: vec![enabled],
                center: vec![disabled],
                end: Vec::new(),
            },
        ] {
            let config = Config {
                features,
                ..Config::default()
            };

            let error = config.into_settings().unwrap_err();

            assert!(error.contains("clock"));
            assert!(error.contains("more than once"));
        }
    }

    #[test]
    fn default_settings_enable_overview_and_hover() {
        let settings = Config::default().into_settings().unwrap();

        assert!(settings.reveal.on_niri_overview);
        assert!(settings.reveal.on_hover.enabled);
        assert!(settings.reveal.on_hover.on_fullscreen);
        assert_eq!(settings.monitor, None);
        assert_eq!(settings.scale, UiScale::default());
    }

    #[test]
    fn hover_on_fullscreen_is_disabled_with_hover() {
        let config = Config {
            reveal: RevealConfig {
                on_hover: HoverConfig {
                    enabled: false,
                    ..HoverConfig::default()
                },
                ..RevealConfig::default()
            },
            ..Config::default()
        };

        let settings = config.into_settings().unwrap();

        assert!(!settings.reveal.on_hover.enabled);
        assert!(!settings.reveal.on_hover.on_fullscreen);
    }

    #[test]
    fn hover_on_fullscreen_cannot_be_enabled_without_hover() {
        let config = Config {
            reveal: RevealConfig {
                on_hover: HoverConfig {
                    enabled: false,
                    on_fullscreen: Some(true),
                },
                ..RevealConfig::default()
            },
            ..Config::default()
        };

        assert!(config.into_settings().is_err());
    }

    #[test]
    fn monitor_is_optional_and_cannot_be_empty() {
        assert_eq!(Config::default().into_settings().unwrap().monitor, None);

        let pinned = Config {
            monitor: Some("  HDMI-A-1  ".into()),
            ..Config::default()
        };

        assert_eq!(
            pinned.into_settings().unwrap().monitor.as_deref(),
            Some("HDMI-A-1")
        );

        let empty = Config {
            monitor: Some("  ".into()),
            ..Config::default()
        };

        assert!(empty.into_settings().is_err());
    }

    #[test]
    fn scale_defaults_to_one_and_accepts_integer_and_fractional_values() {
        assert_eq!(
            Config::default().into_settings().unwrap().scale.factor(),
            1.0
        );

        for factor in [0.25, 0.75, 1.0, 1.25, 1.5, 2.0, 4.0] {
            let config = Config {
                scale: factor,
                ..Config::default()
            };

            assert_eq!(config.into_settings().unwrap().scale.factor(), factor);
        }
    }

    #[test]
    fn invalid_scales_are_rejected_before_starting_the_ui() {
        for factor in [0.0, -1.0, 0.24, 4.01, f64::NAN, f64::INFINITY] {
            let config = Config {
                scale: factor,
                ..Config::default()
            };

            assert!(config.into_settings().unwrap_err().contains("scale"));
        }
    }
}
