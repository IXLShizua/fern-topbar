use crate::{
    backend::battery::Battery as BatteryStatus,
    ui::{core::StatusBox, icon_names},
};
use relm4::gtk;
use relm4::gtk::prelude::*;
use relm4::prelude::*;

#[derive(Debug)]
pub struct View {
    pub icon: &'static str,
    pub label: String,
    pub tooltip: String,
}

impl View {
    pub fn from_status(status: BatteryStatus, percentage: bool) -> Self {
        let icon = if status.power_state.is_charging() {
            icon_names::BATTERY_CHARGING
        } else if status.percent < 15 {
            icon_names::BATTERY_EMPTY
        } else {
            icon_names::BATTERY
        };

        let power = status
            .watts
            .map_or_else(|| "— W".into(), |watts| format!("{watts:.1} W"));
        let label = if percentage {
            format!("{}% · {power}", status.percent)
        } else {
            power.clone()
        };
        let tooltip = match status.watts {
            Some(_) if status.power_state.is_charging() => {
                format!("Battery charging power: {power}")
            }
            Some(_) => format!("Battery power draw: {power}"),
            None => "Battery power data unavailable".into(),
        };

        Self {
            icon,
            label,
            tooltip,
        }
    }
}

#[derive(Debug)]
pub enum Input {
    Changed(Option<View>),
}

pub struct Battery {
    status: Option<View>,
}

#[relm4::component(pub)]
impl Component for Battery {
    type Init = ();
    type Input = Input;
    type Output = ();
    type CommandOutput = ();

    view! {
        #[root]
        #[template]
        StatusBox {
            #[watch]
            set_visible: model.status.is_some(),
            #[watch]
            set_tooltip_text: Some(&model.status.as_ref().map_or(String::new(), |status| status.tooltip.clone())),
            gtk::Image {
                #[watch]
                set_icon_name: model.status.as_ref().map(|status| status.icon),
            },
            gtk::Label {
                #[watch]
                set_label: model.status.as_ref().map_or("", |status| status.label.as_str()),
            },
        }
    }

    fn init(_: (), root: Self::Root, _sender: ComponentSender<Self>) -> ComponentParts<Self> {
        let model = Self { status: None };
        let widgets = view_output!();

        ComponentParts { model, widgets }
    }

    fn update(&mut self, input: Self::Input, _: ComponentSender<Self>, _: &Self::Root) {
        match input {
            Input::Changed(status) => self.status = status,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::battery::{PowerState, WarningLevel};

    #[test]
    fn percentage_visibility_preserves_power_text_and_battery_state() {
        for (power_state, watts, power, icon) in [
            (
                PowerState::Discharging,
                Some(4.2),
                "4.2 W",
                icon_names::BATTERY,
            ),
            (
                PowerState::Charging,
                Some(12.0),
                "12.0 W",
                icon_names::BATTERY_CHARGING,
            ),
            (PowerState::Unknown, None, "— W", icon_names::BATTERY),
        ] {
            let status = BatteryStatus {
                percent: 55,
                watts,
                power_state,
                warning_level: WarningLevel::None,
                on_battery: power_state == PowerState::Discharging,
            };
            let with_percentage = View::from_status(status.clone(), true);
            let without_percentage = View::from_status(status, false);

            assert_eq!(with_percentage.label, format!("55% · {power}"));
            assert_eq!(without_percentage.label, power);
            assert_eq!(with_percentage.icon, icon);
            assert_eq!(without_percentage.icon, icon);
            assert_eq!(with_percentage.tooltip, without_percentage.tooltip);
        }
    }
}
