//! Transparent monitor-edge windows that report pointer entry to the panel.

use super::Action;
use gtk4_layer_shell::{Edge, Layer, LayerShell};
use relm4::gtk;
use relm4::gtk::prelude::*;
use relm4::prelude::*;

/// A transparent layer-shell hover target at the top edge of one monitor.
pub struct PanelTrigger {
    output: Option<String>,
    actions: tokio::sync::mpsc::UnboundedSender<Action>,
}

/// Target monitor and action channel for a hover trigger.
pub struct PanelTriggerInit {
    pub monitor: gtk::gdk::Monitor,
    pub actions: tokio::sync::mpsc::UnboundedSender<Action>,
}

/// Pointer-entry messages from the trigger's GTK motion controller.
#[derive(Debug)]
pub enum Input {
    Entered,
}

#[relm4::component(pub)]
impl SimpleComponent for PanelTrigger {
    type Init = PanelTriggerInit;
    type Input = Input;
    type Output = ();

    view! {
        #[root]
        trigger = gtk::ApplicationWindow {
            set_decorated: false,
            set_default_height: 2,
            add_css_class: "topbar-trigger",

            #[name = "area"]
            gtk::DrawingArea {
                set_size_request: (-1, 1),
                add_controller = gtk::EventControllerMotion {
                    connect_enter[sender] => move |_, _, _| {
                        sender.input(Input::Entered);
                    },
                },
            },
        }
    }

    /// Creates and presents a transparent edge window on the selected monitor.
    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let PanelTriggerInit { monitor, actions } = init;
        let model = Self {
            output: monitor.connector().map(|name| name.to_string()),
            actions,
        };

        let widgets = view_output!();

        root.set_application(Some(&relm4::main_application()));
        root.init_layer_shell();
        root.set_layer(Layer::Overlay);
        root.set_anchor(Edge::Top, true);
        root.set_anchor(Edge::Left, true);
        root.set_anchor(Edge::Right, true);
        root.set_exclusive_zone(0);
        root.set_namespace(Some("fern-topbar-trigger"));
        root.set_monitor(Some(&monitor));
        widgets.area.set_draw_func(|_, cr, _, _| {
            cr.set_operator(gtk::cairo::Operator::Source);
            cr.set_source_rgba(0.0, 0.0, 0.0, 0.0);

            let _ = cr.paint();
        });

        root.present();

        ComponentParts { model, widgets }
    }

    /// Reports pointer entry together with the trigger monitor's connector name.
    fn update(&mut self, message: Self::Input, _sender: ComponentSender<Self>) {
        match message {
            Input::Entered => {
                let _ = self
                    .actions
                    .send(Action::TriggerEntered(self.output.clone()));
            }
        }
    }
}
