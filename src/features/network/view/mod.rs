mod rows;
mod status;
mod templates;

pub use status::View;

use super::backend::{Backend, Event, Password, WifiNetwork};
use crate::ui::{
    core::{
        Button, Disclosure, MenuButtonStyle, MenuPopover, PanelMenuButton, PopoverScope,
        PopoverStyle, PopupRegistration, motion,
    },
    icon_names,
};
use relm4::{gtk::prelude::*, prelude::*};
use rows::Rows;
use templates::{InlinePrompt, TransportBlock};

#[derive(Clone, Debug)]
pub enum Input {
    BackendEvent(Event),
    SetWifi(bool),
    SetWired(bool),
    Popup(bool),
    Scan,
    Disconnect(String),
    Select(WifiNetwork),
    EditPassword(WifiNetwork),
    Forget(String),
    ConfirmForget,
    Connect,
    Cancel,
    Password(Password),
}

impl Input {
    fn requires_idle(&self) -> bool {
        !matches!(
            self,
            Self::BackendEvent(_) | Self::Popup(_) | Self::Cancel | Self::Password(_)
        )
    }
}

pub struct NetworkInit {
    pub popovers: PopoverScope,
    pub status: View,
    pub backend: Backend,
    pub percentage: bool,
}

pub struct Network {
    _popup: Option<PopupRegistration>,
    status: View,
    backend: Backend,
    rows: Rows,
    selected: Option<WifiNetwork>,
    forget: Option<String>,
    password: Password,
    percentage: bool,
}

#[relm4::component(pub)]
impl Component for Network {
    type Init = NetworkInit;
    type Input = Input;
    type Output = ();
    type CommandOutput = ();

    view! {
        #[root]
        #[template]
        PanelMenuButton(MenuButtonStyle::Labeled) {
            #[watch]
            set_visible: model.status.visible,
            #[watch]
            set_tooltip_text: Some(&model.status.tooltip),
            connect_active_notify[sender] => move |button| {
                sender.input(Input::Popup(button.is_active()));
            },

            #[wrap(Some)]
            set_child = &gtk::Box {
                set_spacing: 5,

                gtk::Image {
                    #[watch]
                    set_icon_name: Some(model.status.icon),
                },

                gtk::Label {
                    #[watch]
                    set_label: &model.status.summary,
                },

                #[name = "percentage"]
                gtk::Label {
                    add_css_class: "topbar-percentage",
                    #[watch]
                    set_visible: model.percentage && model.status.signal.is_some(),
                    #[watch]
                    set_label: &model.status.signal.map_or(String::new(), |signal| format!("{signal}%")),
                },
            },

            #[wrap(Some)]
            #[template]
            set_popover = &MenuPopover(PopoverStyle::Menu) {
                gtk::Box {
                    add_css_class: "topbar-network-menu",
                    set_orientation: gtk::Orientation::Vertical,
                    set_spacing: 12,

                    gtk::Box {
                        add_css_class: "topbar-network-header",
                        set_spacing: 8,

                        gtk::Label {
                            add_css_class: "topbar-menu-title",
                            set_label: "Network",
                            set_hexpand: true,
                            set_xalign: 0.0,
                        },

                        gtk::Spinner {
                            set_tooltip_text: Some("Updating network settings"),
                            #[watch]
                            set_spinning: model.status.busy,
                            #[watch]
                            set_opacity: if model.status.busy { 1.0 } else { 0.0 },
                        },
                    },

                    #[name = "scroll"]
                    gtk::ScrolledWindow {
                        set_policy: (gtk::PolicyType::Never, gtk::PolicyType::Automatic),
                        set_propagate_natural_height: true,
                        set_min_content_width: 360,
                        set_max_content_height: 480,

                        gtk::Box {
                            set_orientation: gtk::Orientation::Vertical,
                            set_spacing: 12,

                            #[name = "wifi_block"]
                            #[template]
                            TransportBlock(("Wi-Fi", icon_names::NETWORK_WIFI)) {
                                #[template_child]
                                subtitle {
                                    #[watch]
                                    set_label: model.status.wifi_subtitle(),
                                },

                                #[template_child]
                                toggle {
                                    #[watch]
                                    set_active: model.status.wifi_enabled,
                                    #[watch]
                                    set_sensitive: model.status.wifi_sensitive(),
                                    connect_state_set[sender] => move |_, enabled| {
                                        sender.input(Input::SetWifi(enabled));

                                        gtk::glib::Propagation::Stop
                                    },
                                },

                                #[template_child]
                                content {
                                    gtk::Box {
                                        add_css_class: "topbar-network-section-header",
                                        set_spacing: 8,
                                        #[watch]
                                        set_visible: model.status.wifi_enabled,

                                        gtk::Label {
                                            add_css_class: "topbar-network-section-title",
                                            set_label: "Nearby networks",
                                            set_hexpand: true,
                                            set_xalign: 0.0,
                                        },

                                        #[template]
                                        Button {
                                            add_css_class: "topbar-network-action",
                                            add_css_class: "topbar-network-icon-action",
                                            set_icon_name: "view-refresh-symbolic",
                                            set_tooltip_text: Some("Scan nearby Wi-Fi networks"),
                                            #[watch]
                                            set_sensitive: model.status.wifi_sensitive()
                                                && model.status.wifi_enabled,
                                            connect_clicked => Input::Scan,
                                        },
                                    },

                                    #[name = "password_prompt"]
                                    #[template]
                                    InlinePrompt {
                                        #[watch]
                                        set_visible: model.selected.is_some(),

                                        #[template_child]
                                        title {
                                            #[watch]
                                            set_label: &model.password_title(),
                                        },

                                        #[name = "password_entry"]
                                        gtk::PasswordEntry {
                                            set_show_peek_icon: true,
                                            connect_activate => Input::Connect,
                                            #[watch]
                                            set_sensitive: !model.status.busy,
                                            connect_changed[sender] => move |entry| {
                                                let password = Password(entry.text().to_string());

                                                sender.input(Input::Password(password));
                                            },
                                        },

                                        gtk::Box {
                                            set_spacing: 8,
                                            set_halign: gtk::Align::End,

                                            #[template]
                                            Button {
                                                add_css_class: "topbar-network-action",
                                                set_label: "Cancel",
                                                connect_clicked => Input::Cancel,
                                            },

                                            #[template]
                                            Button {
                                                add_css_class: "topbar-network-action",
                                                set_label: "Connect",
                                                add_css_class: "primary",
                                                #[watch]
                                                set_sensitive: !model.status.busy
                                                    && !model.password.0.is_empty(),
                                                connect_clicked => Input::Connect,
                                            },
                                        },
                                    },

                                    #[name = "forget_prompt"]
                                    #[template]
                                    InlinePrompt {
                                        #[watch]
                                        set_visible: model.forget.is_some(),

                                        #[template_child]
                                        title {
                                            #[watch]
                                            set_label: &model.forget_title(),
                                        },

                                        gtk::Box {
                                            set_spacing: 8,
                                            set_halign: gtk::Align::End,

                                            #[template]
                                            Button {
                                                add_css_class: "topbar-network-action",
                                                set_label: "Cancel",
                                                connect_clicked => Input::Cancel,
                                            },

                                            #[template]
                                            Button {
                                                add_css_class: "topbar-network-action",
                                                set_label: "Forget",
                                                add_css_class: "destructive",
                                                #[watch]
                                                set_sensitive: !model.status.busy,
                                                connect_clicked => Input::ConfirmForget,
                                            },
                                        },
                                    },

                                    gtk::Label {
                                        add_css_class: "topbar-network-empty",
                                        #[watch]
                                        set_label: model.status.wifi_empty_message(),
                                        set_wrap: true,
                                        set_max_width_chars: 36,
                                        #[watch]
                                        set_visible: !model.status.wifi_enabled
                                            || model.status.networks.is_empty(),
                                    },

                                    gtk::ScrolledWindow {
                                        set_policy: (
                                            gtk::PolicyType::Never,
                                            gtk::PolicyType::Automatic,
                                        ),
                                        set_propagate_natural_height: true,
                                        set_max_content_height: 240,
                                        #[watch]
                                        set_visible: model.status.wifi_enabled
                                            && !model.status.networks.is_empty(),

                                        #[local_ref]
                                        networks -> gtk::Box {
                                            set_orientation: gtk::Orientation::Vertical,
                                            set_spacing: 8,
                                        },
                                    },

                                    #[name = "saved_networks"]
                                    #[template]
                                    Disclosure("Saved networks") {
                                        #[watch]
                                        set_visible: !model.status.profiles.is_empty(),

                                        #[template_child]
                                        title {
                                            #[watch]
                                            set_label: &format!(
                                                "Saved networks · {}",
                                                model.status.profiles.len(),
                                            ),
                                        },

                                        #[template_child]
                                        content {
                                            #[local_ref]
                                            profiles -> gtk::Box {
                                                set_orientation: gtk::Orientation::Vertical,
                                                set_spacing: 8,
                                            },
                                        },
                                    },

                                    #[name = "wifi_details"]
                                    #[template]
                                    Disclosure("Connection details") {
                                        #[watch]
                                        set_visible: !model.rows.wifi_devices.is_empty(),

                                        #[template_child]
                                        content {
                                            #[local_ref]
                                            wifi_devices -> gtk::Box {
                                                set_orientation: gtk::Orientation::Vertical,
                                                set_spacing: 8,
                                            },
                                        },
                                    },
                                },
                            },

                            #[name = "ethernet_block"]
                            #[template]
                            TransportBlock(("Ethernet", icon_names::NETWORK_WIRED)) {
                                #[template_child]
                                subtitle {
                                    #[watch]
                                    set_label: model.status.wired_subtitle(),
                                },

                                #[template_child]
                                toggle {
                                    #[watch]
                                    set_active: model.status.wired_enabled,
                                    #[watch]
                                    set_sensitive: !model.status.busy
                                        && model.status.wired_available,
                                    connect_state_set[sender] => move |_, enabled| {
                                        sender.input(Input::SetWired(enabled));

                                        gtk::glib::Propagation::Stop
                                    },
                                },

                                #[template_child]
                                content {
                                    #[name = "ethernet_details"]
                                    #[template]
                                    Disclosure("Connection details") {
                                        #[watch]
                                        set_visible: !model.rows.wired_devices.is_empty(),

                                        #[template_child]
                                        content {
                                            #[local_ref]
                                            wired_devices -> gtk::Box {
                                                set_orientation: gtk::Orientation::Vertical,
                                                set_spacing: 8,
                                            },
                                        },
                                    },
                                },
                            },

                            #[name = "error"]
                            gtk::Label {
                                add_css_class: "topbar-network-error",
                                set_wrap: true,
                                set_wrap_mode: gtk::pango::WrapMode::WordChar,
                                set_max_width_chars: 42,
                                set_xalign: 0.0,
                                #[watch]
                                set_visible: model.status.error.is_some(),
                                #[watch]
                                set_label: model.status.error.as_deref().unwrap_or_default(),
                            },
                        },
                    },
                },
            },
        }
    }
    fn init(
        init: NetworkInit,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let mut model = Self {
            _popup: None,
            status: init.status,
            backend: init.backend,
            rows: Rows::new(&sender),
            selected: None,
            forget: None,
            password: Password(String::new()),
            percentage: init.percentage,
        };

        model.rows.update(&model.status);

        let wifi_devices = model.rows.wifi_devices.widget();
        let wired_devices = model.rows.wired_devices.widget();
        let networks = model.rows.networks.widget();
        let profiles = model.rows.profiles.widget();
        let widgets = view_output!();

        model._popup = Some(init.popovers.register_button(root.widget()));
        motion::animate_appearance(widgets.password_prompt.widget());
        motion::animate_appearance(widgets.forget_prompt.widget());
        motion::animate_appearance(&widgets.error);

        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        input: Self::Input,
        sender: ComponentSender<Self>,
        _root: &Self::Root,
    ) {
        if self.status.busy && input.requires_idle() {
            return;
        }

        let focus_password = matches!(&input, Input::Select(_) | Input::EditPassword(_));

        match input {
            Input::BackendEvent(event) => {
                self.apply_backend_event(event);

                if !self.has_forget_profile() {
                    self.forget = None;
                }
            }
            Input::SetWifi(enabled) => {
                self.status.wifi_enabled = enabled;
                self.start_request(self.backend.set_wifi(enabled));
            }
            Input::SetWired(enabled) => {
                self.status.wired_enabled = enabled;
                self.start_request(self.backend.set_wired(enabled));
            }
            Input::Scan => self.start_request(self.backend.scan()),
            Input::Disconnect(path) => self.start_request(self.backend.disconnect(&path)),
            Input::Select(network) => {
                if network.security.needs_password() && network.profile.is_none() {
                    self.show_password(network, &widgets.password_entry);
                } else {
                    self.start_request(self.backend.connect(network, None));
                }
            }
            Input::EditPassword(network) => self.show_password(network, &widgets.password_entry),
            Input::Password(password) => self.password = password,
            Input::Connect => {
                if !self.password.0.is_empty()
                    && let Some(network) = self.selected.take()
                {
                    let password = std::mem::replace(&mut self.password, Password(String::new()));

                    widgets.password_entry.set_text("");
                    self.start_request(self.backend.connect(network, Some(password)));
                }
            }
            Input::Forget(path) => {
                self.clear_prompt(&widgets.password_entry);
                self.forget = Some(path);
            }
            Input::ConfirmForget => {
                if let Some(path) = self.forget.take() {
                    self.start_request(self.backend.forget(&path));
                }
            }
            Input::Popup(open) => {
                if !open {
                    self.clear_prompt(&widgets.password_entry);
                }
            }
            Input::Cancel => self.clear_prompt(&widgets.password_entry),
        }

        self.rows.update(&self.status);
        self.update_view(widgets, sender);

        if focus_password && self.selected.is_some() {
            widgets.password_entry.grab_focus();
        }
    }
}

impl Network {
    fn apply_backend_event(&mut self, event: Event) {
        match event {
            Event::Updated(snapshot) => {
                let busy = self.status.busy || self.backend.is_busy();
                let error = self.status.error.take();

                self.status = View::from_status(snapshot, error, true);
                self.status.busy = busy;
            }
            Event::Error(message) => self.status.error = Some(message),
            Event::Busy(busy) => {
                self.status.busy = busy || self.backend.is_busy();

                if busy {
                    self.status.error = None;
                }
            }
        }
    }

    fn start_request(&mut self, accepted: bool) {
        self.status.busy = self.backend.is_busy();

        if accepted {
            self.status.error = None;
        }
    }

    fn show_password(&mut self, network: WifiNetwork, entry: &gtk::PasswordEntry) {
        self.clear_prompt(entry);
        self.selected = Some(network);
    }

    fn clear_prompt(&mut self, entry: &gtk::PasswordEntry) {
        self.selected = None;
        self.forget = None;
        self.password.0.clear();
        entry.set_text("");
    }

    fn has_forget_profile(&self) -> bool {
        self.forget.as_ref().is_some_and(|path| {
            self.status
                .profiles
                .iter()
                .any(|profile| &profile.path == path)
        })
    }

    fn password_title(&self) -> String {
        self.selected
            .as_ref()
            .map(|network| format!("Password for {}", network.name))
            .unwrap_or_default()
    }

    fn forget_title(&self) -> String {
        self.forget
            .as_ref()
            .and_then(|path| {
                self.status
                    .profiles
                    .iter()
                    .find(|profile| &profile.path == path)
            })
            .map(|profile| format!("Forget {} and its saved credentials?", profile.name))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::super::backend::{Backend, Command, ConnectionKind, Event, Security, Snapshot};
    use super::super::backend::{DeviceInfo, DeviceState, WifiProfile};
    use super::*;

    fn settle() {
        let context = gtk::glib::MainContext::default();

        for _ in 0..1000 {
            if !context.pending() {
                break;
            }

            context.iteration(false);
        }
    }

    #[gtk::test]
    fn percentage_tracks_active_wifi_and_hides_for_other_connections() {
        for percentage in [false, true] {
            let (backend, _) = Backend::test_channel();
            let component = Network::builder()
                .launch(NetworkInit {
                    popovers: PopoverScope::default(),
                    status: View::default(),
                    backend,
                    percentage,
                })
                .detach();
            let mut status = Snapshot {
                wifi_enabled: true,
                wifi_available: true,
                wifi_hardware_enabled: true,
                connection_kind: Some(ConnectionKind::Wifi),
                networks: vec![WifiNetwork {
                    ssid: b"Test".to_vec(),
                    name: "Test".into(),
                    security: Security::Open,
                    strength: 73,
                    device: "/wifi".into(),
                    access_point: "/ap".into(),
                    active: true,
                    profile: None,
                }],
                ..Snapshot::default()
            };

            for strength in [73, 0, 22] {
                status.networks[0].strength = strength;
                component.emit(Input::BackendEvent(Event::Updated(status.clone())));
                settle();

                assert_eq!(
                    component.widgets().percentage.label(),
                    format!("{strength}%")
                );
                assert_eq!(component.widgets().percentage.get_visible(), percentage);
            }

            for kind in [
                Some(ConnectionKind::Ethernet),
                None,
                Some(ConnectionKind::Wifi),
            ] {
                status.connection_kind = kind;
                status.networks[0].active = kind != Some(ConnectionKind::Wifi);
                component.emit(Input::BackendEvent(Event::Updated(status.clone())));
                settle();

                assert!(!component.widgets().percentage.get_visible());
                assert_eq!(component.widgets().percentage.label(), "");
            }
        }
    }

    #[gtk::test]
    fn password_is_cleared_on_close_and_forgetting_requires_confirmation() {
        let (backend, mut commands) = Backend::test_channel();
        let profile = WifiProfile {
            path: "/profile".into(),
            name: "Test".into(),
            ssid: b"Test".to_vec(),
            security: Security::Psk,
        };

        let status = View::from_status(
            Snapshot {
                profiles: vec![profile],
                wifi_enabled: true,
                wifi_available: true,
                wifi_hardware_enabled: true,
                ..Snapshot::default()
            },
            None,
            true,
        );

        let component = Network::builder()
            .launch(NetworkInit {
                percentage: false,
                popovers: PopoverScope::default(),
                status,
                backend,
            })
            .detach();

        settle();
        component.emit(Input::Select(WifiNetwork {
            ssid: b"Test".to_vec(),
            name: "Test".into(),
            security: Security::Psk,
            strength: 50,
            device: "/device".into(),
            access_point: "/ap".into(),
            active: false,
            profile: None,
        }));

        settle();

        assert!(component.model().selected.is_some());

        component
            .widgets()
            .password_entry
            .set_text("secret-password");

        settle();

        assert_eq!(component.model().password.0, "secret-password");

        component.emit(Input::Popup(false));
        settle();

        assert!(component.model().selected.is_none());
        assert!(component.model().password.0.is_empty());
        assert!(component.widgets().password_entry.text().is_empty());
        assert!(commands.try_recv().is_err());

        component.emit(Input::Forget("/profile".into()));
        settle();

        assert!(commands.try_recv().is_err());

        component.emit(Input::ConfirmForget);
        settle();

        assert!(matches!(commands.try_recv(), Ok(Command::Forget(path)) if path == "/profile"));

        component.emit(Input::Scan);
        settle();

        assert!(commands.try_recv().is_err());
        assert!(component.model().status.busy);
    }

    #[gtk::test]
    fn backend_errors_survive_state_updates_until_the_next_operation() {
        let (backend, _commands) = Backend::test_channel();
        let component = Network::builder()
            .launch(NetworkInit {
                percentage: false,
                popovers: PopoverScope::default(),
                status: View::default(),
                backend,
            })
            .detach();

        component.emit(Input::BackendEvent(Event::Busy(true)));
        settle();

        assert!(component.model().status.busy);

        component.emit(Input::BackendEvent(Event::Busy(false)));
        component.emit(Input::BackendEvent(Event::Error(
            "Permission denied".into(),
        )));
        component.emit(Input::BackendEvent(Event::Updated(Snapshot::default())));
        settle();

        assert!(!component.model().status.busy);
        assert_eq!(
            component.model().status.error.as_deref(),
            Some("Permission denied"),
        );

        component.emit(Input::BackendEvent(Event::Busy(true)));
        settle();

        assert!(component.model().status.error.is_none());
    }

    #[gtk::test]
    fn transports_keep_their_own_devices_and_saved_networks_are_collapsed() {
        relm4::set_global_css(include_str!(concat!(env!("OUT_DIR"), "/styles.css")));
        relm4_icons::initialize_icons(icon_names::GRESOURCE_BYTES, icon_names::RESOURCE_PREFIX);

        let (backend, _commands) = Backend::test_channel();
        let network = WifiNetwork {
            ssid: b"Home".to_vec(),
            name: "Home".into(),
            security: Security::Psk,
            strength: 92,
            device: "/wifi".into(),
            access_point: "/ap".into(),
            active: true,
            profile: Some("/home".into()),
        };

        let mut nearby = network.clone();
        nearby.name = "A very long neighbouring network name".into();
        nearby.ssid = nearby.name.as_bytes().to_vec();
        nearby.strength = 68;
        nearby.active = false;

        let snapshot = Snapshot {
            wifi_enabled: true,
            wifi_available: true,
            wifi_hardware_enabled: true,
            wired_available: true,
            wired_enabled: true,
            wired_connected: true,
            networks: vec![network, nearby],
            profiles: vec![WifiProfile {
                path: "/home".into(),
                name: "Home".into(),
                ssid: b"Home".to_vec(),
                security: Security::Psk,
            }],
            devices: vec![
                DeviceInfo {
                    path: "/wifi".into(),
                    interface: "wlan0".into(),
                    wireless: true,
                    state: DeviceState::Connected,
                    addresses: vec!["192.168.1.20/24".into()],
                },
                DeviceInfo {
                    path: "/ethernet".into(),
                    interface: "enp4s0".into(),
                    wireless: false,
                    state: DeviceState::Connected,
                    addresses: vec![
                        "192.168.1.21/24".into(),
                        "2001:db8:1234:5678:90ab:cdef:1234:5678/64".into(),
                    ],
                },
            ],
            ..Snapshot::default()
        };

        let component = Network::builder()
            .launch(NetworkInit {
                percentage: false,
                popovers: PopoverScope::default(),
                status: View::from_status(snapshot, None, true),
                backend,
            })
            .detach();

        settle();

        let widgets = component.widgets();
        let model = component.model();

        assert_eq!(model.rows.wifi_devices.len(), 1);
        assert_eq!(model.rows.wired_devices.len(), 1);
        assert_eq!(
            widgets.wifi_details.content.first_child().as_ref(),
            Some(model.rows.wifi_devices.widget().upcast_ref::<gtk::Widget>()),
        );
        assert!(
            widgets
                .wifi_details
                .widget()
                .is_ancestor(widgets.wifi_block.widget())
        );
        assert_eq!(
            widgets.ethernet_details.content.first_child().as_ref(),
            Some(
                model
                    .rows
                    .wired_devices
                    .widget()
                    .upcast_ref::<gtk::Widget>()
            ),
        );
        assert!(
            widgets
                .ethernet_details
                .widget()
                .is_ancestor(widgets.ethernet_block.widget())
        );
        assert!(!widgets.saved_networks.toggle.widget().is_active());
        assert!(!widgets.wifi_details.toggle.widget().is_active());
        assert!(!widgets.ethernet_details.toggle.widget().is_active());

        drop(model);
        drop(widgets);

        let window = gtk::Window::new();
        window.set_child(Some(component.widget().widget()));
        window.present();
        component.widget().widget().popup();

        for _ in 0..20 {
            settle();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let popover = component.widget().widget().popover().unwrap();

        if let Ok(path) = std::env::var("NETWORK_UI_PREVIEW") {
            let snapshot = gtk::Snapshot::new();
            let paintable = gtk::WidgetPaintable::new(Some(&popover));
            paintable.snapshot(&snapshot, popover.width().into(), popover.height().into());

            let node = snapshot.to_node().unwrap();
            let renderer = popover.native().unwrap().renderer().unwrap();

            renderer
                .render_texture(&node, None)
                .save_to_png(path)
                .unwrap();
        }

        let content = popover.child().unwrap();
        let widgets = component.widgets();

        for block in [&widgets.wifi_block, &widgets.ethernet_block] {
            let root = block.widget();
            let header = root.first_child().unwrap().compute_bounds(root).unwrap();
            let body = block.content.compute_bounds(root).unwrap();
            let right = root.width() as f32 - header.x() - header.width();
            let bottom = root.height() as f32 - body.y() - body.height();

            assert!((header.x() - right).abs() <= 1.0);
            assert!((header.x() - header.y()).abs() <= 1.0);
            assert!((header.x() - bottom).abs() <= 1.0);
            assert_eq!(root.spacing(), 12);
        }

        for section in [
            &widgets.saved_networks,
            &widgets.wifi_details,
            &widgets.ethernet_details,
        ] {
            assert!(section.toggle.widget().width() < 220);
            assert!(section.toggle.widget().height() <= 32);
            assert_eq!(section.toggle.widget().halign(), gtk::Align::Start);

            let button = section.toggle.widget();
            let child = button.child().unwrap().compute_bounds(button).unwrap();
            let border = button.compute_bounds(button).unwrap();
            let left = child.x() - border.x();

            assert!(left >= 10.0, "Missing left padding: {left}");

            let right = border.x() + border.width() - child.x() - child.width();

            assert!(right >= 10.0, "Missing right padding: {right}");
        }

        drop(widgets);

        assert!(content.width() <= 370, "Content width: {}", content.width());
        assert!(
            content.height() <= 520,
            "Content height: {}",
            content.height()
        );

        component
            .widgets()
            .saved_networks
            .toggle
            .widget()
            .set_active(true);

        component
            .widgets()
            .wifi_details
            .toggle
            .widget()
            .set_active(true);

        component
            .widgets()
            .ethernet_details
            .toggle
            .widget()
            .set_active(true);

        for _ in 0..20 {
            settle();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(
            content.width() <= 370,
            "Expanded width: {}",
            content.width()
        );
        assert!(
            content.height() <= 520,
            "Expanded height: {}",
            content.height()
        );

        component.widget().widget().popdown();
        window.close();
    }

    #[gtk::test]
    fn missing_wifi_hardware_has_a_clear_disabled_state() {
        let (backend, _commands) = Backend::test_channel();
        let component = Network::builder()
            .launch(NetworkInit {
                percentage: false,
                popovers: PopoverScope::default(),
                status: View::from_status(Snapshot::default(), None, true),
                backend,
            })
            .detach();

        settle();

        assert!(!component.widgets().wifi_block.toggle.is_sensitive());
        assert_eq!(
            component.model().status.wifi_empty_message(),
            "No Wi-Fi adapter detected"
        );
        assert!(component.model().rows.wifi_devices.is_empty());
    }

    #[gtk::test]
    fn enter_on_an_empty_password_does_not_submit_a_connection() {
        let (backend, mut commands) = Backend::test_channel();
        let component = Network::builder()
            .launch(NetworkInit {
                percentage: false,
                popovers: PopoverScope::default(),
                status: View::default(),
                backend,
            })
            .detach();

        component.emit(Input::Select(WifiNetwork {
            ssid: b"Test".to_vec(),
            name: "Test".into(),
            security: Security::Psk,
            strength: 50,
            device: "/wifi".into(),
            access_point: "/ap".into(),
            active: false,
            profile: None,
        }));

        settle();
        component
            .widgets()
            .password_entry
            .emit_by_name::<()>("activate", &[]);

        settle();

        assert!(component.model().selected.is_some());
        assert!(commands.try_recv().is_err());

        component.widgets().password_entry.set_text("test-password");
        settle();
        component
            .widgets()
            .password_entry
            .emit_by_name::<()>("activate", &[]);

        settle();

        assert!(matches!(
            commands.try_recv(),
            Ok(Command::Connect(_, Some(_)))
        ));
    }
}
