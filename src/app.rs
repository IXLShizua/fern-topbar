use crate::{
    alerts::{battery::BatteryAlerts, service::AlertService},
    backend::{self, wm::state::WindowManagerState},
    config, features,
    features::FeatureId,
    runtime,
    ui::{self, core::PopupId, monitor::MonitorSelection},
};
use relm4::RelmApp;
use std::collections::HashSet;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

pub fn run() -> Result<(), String> {
    let settings = config::load()?;
    let panel = start_application_runtime(settings)?;

    launch_graphical_interface(panel);

    Ok(())
}

struct ApplicationState {
    window_manager: WindowManagerState,
    wm_commands: Option<UnboundedSender<backend::Command>>,
    reveal: config::Reveal,
    monitor: Option<String>,
    // Output selected for the panel, not data reported by the WM.
    panel_output: Option<String>,
    focused_fullscreen_outputs: HashSet<String>,
    hovered: bool,
    overview: bool,
    overview_dismissed: bool,
    popups: HashSet<PopupId>,
}

impl ApplicationState {
    fn new(
        window_manager: WindowManagerState,
        wm_commands: Option<UnboundedSender<backend::Command>>,
        reveal: config::Reveal,
        monitor: Option<String>,
        focused_fullscreen_outputs: HashSet<String>,
    ) -> Self {
        Self {
            window_manager,
            wm_commands,
            reveal,
            panel_output: monitor.clone(),
            monitor,
            focused_fullscreen_outputs,
            hovered: false,
            overview: false,
            overview_dismissed: false,
            popups: HashSet::new(),
        }
    }

    fn hover_allowed(&self, output: Option<&str>) -> bool {
        let fullscreen = match output {
            Some(output) => self.focused_fullscreen_outputs.contains(output),
            None => !self.focused_fullscreen_outputs.is_empty(),
        };

        self.reveal.on_hover.enabled && (self.reveal.on_hover.on_fullscreen || !fullscreen)
    }

    fn target_output(&self, event_output: Option<&str>) -> Option<String> {
        self.monitor
            .clone()
            .or_else(|| event_output.map(str::to_owned))
    }

    fn handle_wm(&mut self, event: backend::Event) {
        match event {
            backend::Event::OverviewOpened(output) => {
                self.send_to_ui(ui::Input::OverviewChanged(true));

                let output = self.target_output(output.as_deref());

                if output.is_some() {
                    self.panel_output = output.clone();
                }

                if !self.reveal.on_niri_overview {
                    return;
                }

                self.overview = true;
                self.overview_dismissed = false;
                self.send_to_ui(ui::Input::Show(output));
            }
            backend::Event::OverviewClosed => {
                self.send_to_ui(ui::Input::OverviewChanged(false));

                if !self.reveal.on_niri_overview {
                    return;
                }

                self.overview = false;
                self.hide_if_idle();
            }
            backend::Event::FocusedFullscreenOutputsChanged(outputs) => {
                self.focused_fullscreen_outputs = outputs;

                if !self.hover_allowed(self.panel_output.as_deref())
                    && !self.overview_visible()
                    && self.popups.is_empty()
                {
                    self.send_to_ui(ui::Input::Hide);
                }
            }
            backend::Event::KeyboardLayoutChanged(layout) => {
                self.window_manager.set_keyboard_layout(layout);
            }
            backend::Event::WorkspacesChanged(workspaces) => {
                self.window_manager.set_workspaces(workspaces);
            }
        }
    }

    fn handle_ui(&mut self, action: &ui::Action) {
        match action {
            ui::Action::TriggerEntered(output) => {
                let output = self.target_output(output.as_deref());

                if !self.overview_visible()
                    && self.popups.is_empty()
                    && self.hover_allowed(output.as_deref())
                {
                    self.overview_dismissed = false;
                    self.panel_output = output.clone();
                    self.send_to_ui(ui::Input::Show(output.clone()));
                }
            }
            ui::Action::PanelEntered => self.hovered = true,
            ui::Action::PanelLeft => {
                self.hovered = false;
                self.hide_if_idle();
            }
            ui::Action::Dismissed => {
                self.hovered = false;
                self.popups.clear();
                self.overview_dismissed = true;
                self.send_to_ui(ui::Input::Hide);
            }
            ui::Action::CloseOverview => {
                if let Some(commands) = &self.wm_commands {
                    let _ = commands.send(backend::Command::CloseOverview);
                }
            }
            ui::Action::PopupOpened(popup) => {
                self.popups.insert(*popup);
                self.send_to_ui(ui::Input::KeepOpen);
            }
            ui::Action::PopupClosed(popup) => {
                self.popups.remove(popup);
                self.hide_if_idle();
            }
        }
    }

    fn hide_if_idle(&self) {
        if !self.overview_visible() && !self.hovered && self.popups.is_empty() {
            self.send_to_ui(ui::Input::Hide);
        }
    }

    fn overview_visible(&self) -> bool {
        self.overview && !self.overview_dismissed
    }

    fn send_to_ui(&self, input: ui::Input) {
        ui::BROKER.send(input);
    }
}

struct ApplicationController {
    state: ApplicationState,
    backend: backend::Backend,
    ui_actions: UnboundedReceiver<ui::Action>,
    // Source and alert policy survive hiding or removing the battery widget.
    _battery: backend::battery::Backend,
    _battery_alerts: runtime::Task,
    _alerts: AlertService,
}

impl ApplicationController {
    async fn run(mut self) {
        loop {
            tokio::select! {
                Some(event) = self.backend.events.recv() => self.state.handle_wm(event),
                Some(action) = self.ui_actions.recv() => self.state.handle_ui(&action),
                else => return,
            }
        }
    }
}

fn start_application_runtime(settings: config::Settings) -> Result<ui::PanelInit, String> {
    let (ui_actions, actions) = mpsc::unbounded_channel();

    let availability = features::availability::FeatureAvailability::default();
    let watch_fullscreen =
        settings.reveal.on_hover.enabled && !settings.reveal.on_hover.on_fullscreen;
    let backend = backend::Backend::start(watch_fullscreen, availability.clone())
        .map_err(|error| error.to_string())?;
    let window_manager = WindowManagerState::default();
    let battery = backend::battery::Backend::start(availability.publisher(FeatureId::Battery));
    let alerts = AlertService::start();
    let battery_alerts = runtime::Task::spawn(
        BatteryAlerts::new(battery.state.subscribe(), alerts.publisher().register()).run(),
    );

    let services = features::FeatureServices {
        availability,
        audio: features::AudioService::default(),
        battery: battery.state.clone(),
        alerts: alerts.publisher(),
        window_manager,
        wm_commands: backend.wm_commands.clone(),
        monitor_selection: MonitorSelection::default(),
    };

    runtime::spawn(
        ApplicationController {
            state: ApplicationState::new(
                services.window_manager.clone(),
                backend.wm_commands.clone(),
                settings.reveal,
                settings.monitor.clone(),
                backend.initial_fullscreen_outputs.clone(),
            ),
            backend,
            ui_actions: actions,
            _battery: battery,
            _battery_alerts: battery_alerts,
            _alerts: alerts,
        }
        .run(),
    );

    Ok(ui::PanelInit {
        scale: settings.scale,
        monitor: settings.monitor,
        actions: ui_actions,
        features: features::resolve(settings.features),
        services,
        hover_enabled: settings.reveal.on_hover.enabled,
    })
}

fn launch_graphical_interface(init: ui::PanelInit) {
    let app = RelmApp::new("com.example.FernTopbar");

    relm4_icons::initialize_icons(
        ui::icon_names::GRESOURCE_BYTES,
        ui::icon_names::RESOURCE_PREFIX,
    );

    app.with_broker(&ui::BROKER)
        .visible_on_activate(false)
        .run::<ui::Panel>(init);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_overview_sends_a_command_without_dismissing_the_panel() {
        let (commands, mut receiver) = mpsc::unbounded_channel();
        let mut state = ApplicationState::new(
            WindowManagerState::default(),
            Some(commands),
            config::Reveal {
                on_niri_overview: true,
                on_hover: config::Hover {
                    enabled: true,
                    on_fullscreen: true,
                },
            },
            None,
            HashSet::new(),
        );

        state.handle_wm(backend::Event::OverviewOpened(None));
        state.hovered = true;
        state.handle_ui(&ui::Action::CloseOverview);

        assert!(matches!(
            receiver.try_recv(),
            Ok(backend::Command::CloseOverview)
        ));
        assert!(receiver.try_recv().is_err());
        assert!(state.overview);
        assert!(state.overview_visible());
        assert!(!state.overview_dismissed);
        assert!(state.hovered);
    }

    #[test]
    fn fullscreen_hover_policy_uses_trigger_output() {
        let reveal = config::Reveal {
            on_niri_overview: true,
            on_hover: config::Hover {
                enabled: true,
                on_fullscreen: false,
            },
        };

        let state = ApplicationState::new(
            WindowManagerState::default(),
            None,
            reveal,
            None,
            HashSet::from(["HDMI-A-1".to_owned()]),
        );

        assert!(!state.hover_allowed(Some("HDMI-A-1")));
        assert!(state.hover_allowed(Some("DP-1")));
    }

    #[test]
    fn configured_monitor_overrides_event_output() {
        let state = ApplicationState::new(
            WindowManagerState::default(),
            None,
            config::Reveal {
                on_niri_overview: true,
                on_hover: config::Hover {
                    enabled: true,
                    on_fullscreen: true,
                },
            },
            Some("DP-1".into()),
            HashSet::new(),
        );

        assert_eq!(
            state.target_output(Some("HDMI-A-1")).as_deref(),
            Some("DP-1")
        );
        assert_eq!(state.target_output(None).as_deref(), Some("DP-1"));
        assert_eq!(state.panel_output.as_deref(), Some("DP-1"));
    }

    #[test]
    fn panel_output_follows_allowed_reveals_without_changing_wm_data() {
        let window_manager = WindowManagerState::default();
        let workspaces = window_manager.subscribe_workspaces();
        let mut state = ApplicationState::new(
            window_manager,
            None,
            config::Reveal {
                on_niri_overview: true,
                on_hover: config::Hover {
                    enabled: true,
                    on_fullscreen: false,
                },
            },
            None,
            HashSet::from(["HDMI-A-1".into()]),
        );

        state.handle_ui(&ui::Action::TriggerEntered(Some("DP-1".into())));
        assert_eq!(state.panel_output.as_deref(), Some("DP-1"));

        state.handle_ui(&ui::Action::TriggerEntered(Some("HDMI-A-1".into())));
        assert_eq!(state.panel_output.as_deref(), Some("DP-1"));

        state.handle_wm(backend::Event::OverviewOpened(Some("HDMI-A-1".into())));
        assert_eq!(state.panel_output.as_deref(), Some("HDMI-A-1"));

        state.handle_wm(backend::Event::OverviewOpened(None));
        assert_eq!(state.panel_output.as_deref(), Some("HDMI-A-1"));
        assert!(!workspaces.has_changed().unwrap());
    }

    #[test]
    fn dismissal_overrides_overview_until_the_next_explicit_reveal() {
        let mut state = ApplicationState::new(
            WindowManagerState::default(),
            None,
            config::Reveal {
                on_niri_overview: true,
                on_hover: config::Hover {
                    enabled: true,
                    on_fullscreen: true,
                },
            },
            None,
            HashSet::new(),
        );

        state.handle_wm(backend::Event::OverviewOpened(None));
        state.hovered = true;

        let popup = PopupId::for_test();

        state.popups.insert(popup);
        state.handle_ui(&ui::Action::Dismissed);

        assert!(!state.hovered);
        assert!(state.popups.is_empty());
        assert!(state.overview);
        assert!(!state.overview_visible());

        state.handle_ui(&ui::Action::PopupClosed(popup));

        assert!(!state.overview_visible());

        state.handle_ui(&ui::Action::TriggerEntered(None));

        assert!(state.overview_visible());

        state.handle_ui(&ui::Action::Dismissed);
        state.handle_wm(backend::Event::OverviewClosed);
        state.handle_wm(backend::Event::OverviewOpened(None));

        assert!(state.overview_visible());
    }
}
