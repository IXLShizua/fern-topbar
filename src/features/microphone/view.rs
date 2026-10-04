use crate::{
    features::{FeatureMountContext, FeatureOptionsError, MountedFeature, sound},
    ui::icon_names,
};
use sound::{AudioDevice, Volume, VolumeControlOptions};

const CONTROL: VolumeControlOptions = VolumeControlOptions {
    device: AudioDevice::Input,
    default_icon: icon_names::MIC,
    button_tooltip: "Microphone volume · scroll to adjust",
    mute_tooltip: "Toggle microphone mute",
    scale_tooltip: "Microphone volume",
    button_class: "topbar-microphone-button",
    menu_class: "topbar-microphone-menu",
    row_class: "topbar-microphone-row",
    icon: microphone_icon,
};

pub fn mount(context: FeatureMountContext) -> Result<MountedFeature, FeatureOptionsError> {
    sound::mount_control(context, CONTROL)
}

fn microphone_icon(volume: Volume) -> &'static str {
    if volume.is_muted() {
        icon_names::MIC_MUTED
    } else {
        icon_names::MIC
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_microphone_volume_uses_the_muted_icon() {
        for level in [0.0, 0.0049] {
            assert_eq!(
                microphone_icon(Volume {
                    level,
                    muted: false
                }),
                icon_names::MIC_MUTED
            );
        }

        assert_eq!(
            microphone_icon(Volume {
                level: 0.5,
                muted: true
            }),
            icon_names::MIC_MUTED
        );
        assert_eq!(
            microphone_icon(Volume {
                level: 0.5,
                muted: false
            }),
            icon_names::MIC
        );
    }
}
