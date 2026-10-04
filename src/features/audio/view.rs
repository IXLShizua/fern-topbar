use crate::{
    features::{FeatureMountContext, FeatureOptionsError, MountedFeature, sound},
    ui::icon_names,
};
use sound::{AudioDevice, Volume, VolumeControlOptions};

const CONTROL: VolumeControlOptions = VolumeControlOptions {
    device: AudioDevice::Output,
    default_icon: icon_names::SPEAKER_MAX,
    button_tooltip: "Output volume · scroll to adjust",
    mute_tooltip: "Toggle output mute",
    scale_tooltip: "Output volume",
    button_class: "topbar-audio-button",
    menu_class: "topbar-audio-menu",
    row_class: "topbar-audio-row",
    icon: output_icon,
};

pub fn mount(context: FeatureMountContext) -> Result<MountedFeature, FeatureOptionsError> {
    sound::mount_control(context, CONTROL)
}

fn output_icon(volume: Volume) -> &'static str {
    if volume.is_muted() {
        icon_names::SPEAKER_CROSS
    } else if volume.level < 0.01 {
        icon_names::SPEAKER_MIN
    } else if volume.level < 0.5 {
        icon_names::SPEAKER_MID
    } else {
        icon_names::SPEAKER_MAX
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_output_volume_uses_the_muted_icon() {
        for level in [0.0, 0.0049] {
            assert_eq!(
                output_icon(Volume {
                    level,
                    muted: false
                }),
                icon_names::SPEAKER_CROSS
            );
        }

        assert_eq!(
            output_icon(Volume {
                level: 0.5,
                muted: true
            }),
            icon_names::SPEAKER_CROSS
        );
        assert_eq!(
            output_icon(Volume {
                level: 0.5,
                muted: false
            }),
            icon_names::SPEAKER_MAX
        );
    }
}
