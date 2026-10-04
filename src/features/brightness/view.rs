use super::backend;
use crate::ui::{
    core::{
        MenuButtonStyle, MenuPopover, PanelMenuButton, PercentScale, PercentValue, PopoverScope,
        PopoverStyle, PopupRegistration,
    },
    icon_names,
};
use relm4::gtk;
use relm4::gtk::prelude::*;
use relm4::prelude::*;

pub struct Brightness {
    _popup: Option<PopupRegistration>,
    value: Option<backend::Brightness>,
    commands: backend::Controls,
    percentage: bool,
}

pub struct BrightnessInit {
    pub popovers: PopoverScope,
    pub commands: backend::Controls,
    pub percentage: bool,
}

#[derive(Debug)]
pub enum Input {
    Changed(Option<backend::Brightness>),
    Slider(u8),
    Scroll(i8),
}

#[relm4::component(pub)]
impl Component for Brightness {
    type Init = BrightnessInit;
    type Input = Input;
    type Output = ();
    type CommandOutput = ();

    view! {
        #[root]
        #[template]
        PanelMenuButton(MenuButtonStyle::Icon) {
            set_class_active: ("topbar-labeled-button", model.percentage),
            #[watch]
            set_visible: model.value.is_some(),
            set_tooltip_text: Some("Brightness · scroll to adjust"),
            add_controller = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL) {
                connect_scroll[sender] => move |_, _, dy| {
                    if dy != 0.0 {
                        sender.input(Input::Scroll(if dy < 0.0 { 5 } else { -5 }));
                    }

                    gtk::glib::Propagation::Stop
                },
            },

            #[wrap(Some)]
            set_child = &gtk::Box {
                set_spacing: 4,
                set_halign: gtk::Align::Center,

                gtk::Image {
                    set_icon_name: Some(icon_names::BRIGHTNESS),
                },

                #[name = "percentage"]
                gtk::Label {
                    add_css_class: "topbar-percentage",
                    set_visible: model.percentage,
                    #[watch]
                    set_label: &model.value.as_ref().map_or(String::new(), |value| format!("{}%", value.percent)),
                },
            },

            #[wrap(Some)]
            #[template]
            set_popover = &MenuPopover(PopoverStyle::Menu) {
                gtk::Box {
                    add_css_class: "topbar-slider-row",
                    set_spacing: 8,
                    gtk::Image { set_icon_name: Some(icon_names::BRIGHTNESS) },
                    #[name = "scale"]
                    #[template]
                    PercentScale(1) {
                        connect_change_value[sender] => move |_, _, value| {
                            sender.input(Input::Slider(value.round().clamp(1.0, 100.0) as u8));

                            gtk::glib::Propagation::Proceed
                        },
                    },
                    #[template]
                    PercentValue {
                        #[watch]
                        set_label: &model.value.as_ref().map_or(String::new(), |value| format!("{}%", value.percent)),
                    },
                },
            },
        }
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let BrightnessInit {
            commands,
            popovers,
            percentage,
        } = init;
        let mut model = Self {
            _popup: None,
            value: None,
            commands,
            percentage,
        };

        let widgets = view_output!();

        model._popup = Some(popovers.register_button(root.widget()));

        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        message: Self::Input,
        sender: ComponentSender<Self>,
        _: &Self::Root,
    ) {
        match message {
            Input::Changed(value) => {
                if let Some(value) = &value {
                    widgets.scale.widget().set_value(f64::from(value.percent));
                }

                self.value = value;
            }
            Input::Slider(percent) => {
                if let Some(value) = &mut self.value
                    && value.percent != percent
                {
                    value.percent = percent;
                    self.commands.set(backend::Command::SetBrightness(percent));
                }
            }
            Input::Scroll(delta) => {
                self.commands.set(backend::Command::AdjustBrightness(delta));
            }
        }

        self.update_view(widgets, sender);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gtk::test]
    fn percentage_follows_snapshots_and_slider_changes_with_optional_visibility() {
        for percentage in [false, true] {
            let (commands, mut received) = backend::tests::controls();
            let component = Brightness::builder()
                .launch(BrightnessInit {
                    popovers: PopoverScope::default(),
                    commands,
                    percentage,
                })
                .detach();
            let label = component
                .widget()
                .widget()
                .child()
                .unwrap()
                .last_child()
                .unwrap()
                .downcast::<gtk::Label>()
                .unwrap();
            let context = gtk::glib::MainContext::default();
            let send = |input| {
                component.emit(input);

                while context.pending() {
                    context.iteration(false);
                }
            };

            send(Input::Changed(Some(backend::Brightness {
                percent: 73,
                device: "test-backlight".into(),
                max: 100,
            })));

            assert_eq!(label.label(), "73%");
            assert_eq!(label.get_visible(), percentage);
            assert_eq!(
                component
                    .widget()
                    .widget()
                    .has_css_class("topbar-labeled-button"),
                percentage
            );
            assert!(received.try_recv().is_err());

            send(Input::Slider(50));

            assert_eq!(label.label(), "50%");
            assert!(matches!(
                received.try_recv(),
                Ok(backend::Command::SetBrightness(50))
            ));

            send(Input::Changed(None));

            assert!(!component.widget().widget().get_visible());
            assert_eq!(label.label(), "");

            send(Input::Changed(Some(backend::Brightness {
                percent: 22,
                device: "test-backlight".into(),
                max: 100,
            })));

            assert!(component.widget().widget().get_visible());
            assert_eq!(label.label(), "22%");
            assert_eq!(label.get_visible(), percentage);
        }
    }
}
