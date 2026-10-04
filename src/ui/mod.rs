//! Panel shell and composition of feature-independent [`core`] building blocks.

pub(crate) mod content;
pub(crate) mod core;
pub(crate) mod icon_names;
pub mod monitor;
mod trigger;

use crate::features::{EnabledFeatures, FeatureServices};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use monitor::{MonitorSelection, available_monitor_names, monitor_for_output};
use relm4::gtk::prelude::*;
use relm4::prelude::*;
use relm4::{ComponentController, MessageBroker, gtk};
use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Instant;

use content::PanelContent;
use core::{PopoverScope, PopupId, UiScale};
use trigger::PanelTrigger;

const BAR_HEIGHT: i32 = 34;
const HORIZONTAL_PADDING: i32 = 4;
const VERTICAL_PADDING: i32 = 4;
const SHADOW_SPACE: i32 = 16;
const PANEL_HEIGHT: i32 = BAR_HEIGHT + VERTICAL_PADDING + SHADOW_SPACE;
const ANIMATION_SECONDS: f64 = 0.24;

/// Application-to-panel message channel; it does not store popover registrations.
pub static BROKER: MessageBroker<Input> = MessageBroker::new();

/// Panel lifecycle events and compositor requests forwarded to the application.
#[derive(Debug)]
pub enum Action {
    TriggerEntered(Option<String>),
    PanelEntered,
    PanelLeft,
    Dismissed,
    CloseOverview,
    PopupOpened(PopupId),
    PopupClosed(PopupId),
}

/// Commands for panel visibility and child lifecycle handling.
#[derive(Debug)]
pub enum Input {
    Show(Option<String>),
    KeepOpen,
    Hide,
    Dismiss,
    Escape,
    FocusChanged(bool),
    OverviewChanged(bool),
    MonitorsChanged,
    Action(Action),
}

/// Panel monitor policy, enabled features, shared services and action channel.
pub struct PanelInit {
    pub scale: UiScale,
    pub monitor: Option<String>,
    pub actions: tokio::sync::mpsc::UnboundedSender<Action>,
    pub features: EnabledFeatures,
    pub services: FeatureServices,
    pub hover_enabled: bool,
}

/// Layer-shell panel owning feature composition, visibility and a popover scope.
///
/// Feature data and backend commands stay in child components. The panel passes
/// its [`PopoverScope`] scope through the tree and handles outside-click dismissal.
pub struct Panel {
    monitor: Option<String>,
    actions: tokio::sync::mpsc::UnboundedSender<Action>,
    content: Controller<PanelContent>,
    monitor_selection: MonitorSelection,
    // Each controller owns a mapped edge window and its hover signals. Keeping
    // only GTK children would let Relm4 shut down these independent components.
    _triggers: Vec<Controller<PanelTrigger>>,
    animation: Animation,
    popups: HashSet<PopupId>,
    popovers: PopoverScope,
    revealed: bool,
    focused: bool,
    overview: bool,
    monitor_subscription: Option<(gtk::gio::ListModel, gtk::glib::SignalHandlerId)>,
}

#[relm4::component(pub)]
impl Component for Panel {
    type Init = PanelInit;
    type Input = Input;
    type Output = ();
    type CommandOutput = ();

    view! {
        #[root]
        panel = gtk::ApplicationWindow {
            set_title: Some("fern-topbar"),
            set_decorated: false,
            set_default_height: PANEL_HEIGHT,
            add_css_class: "topbar-panel",

            add_controller = gtk::EventControllerMotion {
                connect_enter[sender] => move |_, _, _| {
                    sender.input(Input::Action(Action::PanelEntered));
                },
                connect_leave[sender] => move |_| {
                    sender.input(Input::Action(Action::PanelLeft));
                },
            },
            add_controller = gtk::GestureClick {
                set_button: 0,
                set_propagation_phase: gtk::PropagationPhase::Capture,
                connect_pressed[popovers, content_widget, sender] => move |controller, _, x, y| {
                    let clicked = controller
                        .widget()
                        .and_then(|panel| panel.pick(x, y, gtk::PickFlags::DEFAULT));

                    if !is_panel_target(clicked.as_ref(), &content_widget, &popovers) {
                        controller.set_state(gtk::EventSequenceState::Claimed);
                        sender.input(Input::Dismiss);
                    } else if clicked
                        .as_ref()
                        .is_none_or(|widget| !popovers.contains(widget))
                    {
                        popovers.close();
                    }
                },
            },
            add_controller = gtk::EventControllerKey {
                set_propagation_phase: gtk::PropagationPhase::Capture,
                connect_key_pressed[popovers, sender] => move |_, key, _, _| {
                    if key == gtk::gdk::Key::Escape {
                        if !popovers.close() {
                            sender.input(Input::Escape);
                        }

                        gtk::glib::Propagation::Stop
                    } else {
                        gtk::glib::Propagation::Proceed
                    }
                },
            },
            connect_is_active_notify[sender] => move |panel| {
                sender.input(Input::FocusChanged(panel.is_active()));
            },
            connect_unmap => move |panel| {
                GtkWindowExt::set_focus(panel, None::<&gtk::Widget>);
            },

            #[name = "canvas"]
            gtk::Fixed {
                set_size_request: (-1, PANEL_HEIGHT),
                set_overflow: gtk::Overflow::Hidden,
            },
        }
    }

    /// Validates the monitor, mounts features and configures the layer-shell root.
    ///
    /// Creates one popover scope and optional hover triggers before placing the
    /// initially hidden content in the animation canvas.
    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let PanelInit {
            scale,
            monitor,
            actions,
            features,
            services,
            hover_enabled,
        } = init;

        ensure_layer_shell_support();
        relm4::set_global_css(
            &scale.stylesheet(include_str!(concat!(env!("OUT_DIR"), "/styles.css"))),
        );

        let selected_monitor = monitor.as_deref().and_then(monitor_for_output);

        if let Some(output) = monitor.as_deref()
            && selected_monitor.is_none()
        {
            tracing::error!(
                monitor = %output,
                available = %available_monitor_names().join(", "),
                "configured monitor was not found"
            );

            relm4::main_application().quit();
        }

        let monitor_selection = services.monitor_selection.clone();

        if let Some(selected) = selected_monitor.clone() {
            monitor_selection.set(selected);
        }

        let popovers = PopoverScope::new(scale, sender.input_sender().clone());
        let content = PanelContent::builder()
            .launch(content::PanelContentInit {
                features,
                services,
                popovers: popovers.clone(),
            })
            .detach();

        let triggers = build_hover_triggers(hover_enabled, monitor.as_deref(), &actions);
        let display = gtk::gdk::Display::default().expect("GTK display is available");
        let monitor_list = display.monitors();
        let monitor_subscription = monitor_list.connect_items_changed({
            let sender = sender.clone();

            move |_, _, _, _| sender.input(Input::MonitorsChanged)
        });

        let model = Self {
            monitor,
            actions,
            content,
            monitor_selection,
            _triggers: triggers,
            animation: Animation::default(),
            popups: HashSet::new(),
            popovers,
            revealed: false,
            focused: false,
            overview: false,
            monitor_subscription: Some((monitor_list, monitor_subscription)),
        };

        let popovers = model.popovers.clone();
        let content_widget = model.content.widget().clone().upcast::<gtk::Widget>();
        let widgets = view_output!();
        widgets.canvas.put(
            model.content.widget(),
            f64::from(HORIZONTAL_PADDING),
            -f64::from(BAR_HEIGHT),
        );

        configure_panel_window(&root);
        scale.configure_window(&root);

        if let Some(selected) = selected_monitor.as_ref() {
            root.set_monitor(Some(selected));
        }

        connect_canvas_width_sync(
            &widgets.canvas,
            model.content.widget(),
            &root,
            model.animation.position.clone(),
            model.animation.interactive.clone(),
        );

        ComponentParts { model, widgets }
    }

    /// Dispatches visibility commands and lifecycle actions without feature routing.
    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        message: Self::Input,
        _sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        match message {
            Input::Show(output) => self.show(output, &widgets.canvas, root),
            Input::KeepOpen if self.revealed => {
                self.animation
                    .animate(&widgets.canvas, self.content.widget(), root, true)
            }
            Input::KeepOpen => {}
            Input::Hide => self.hide(&widgets.canvas, root),
            Input::Dismiss => {
                self.hide(&widgets.canvas, root);

                let _ = self.actions.send(Action::Dismissed);
            }
            Input::Escape => {
                if self.popovers.close() {
                    return;
                }

                if self.overview {
                    let _ = self.actions.send(Action::CloseOverview);
                } else {
                    self.hide(&widgets.canvas, root);

                    let _ = self.actions.send(Action::Dismissed);
                }
            }
            Input::FocusChanged(focused) => {
                let lost_focus = self.focused && !focused;

                self.focused = focused;

                if lost_focus {
                    self.popovers.close();
                    self.clear_focus(root);

                    if self.revealed && !self.overview {
                        self.hide(&widgets.canvas, root);

                        let _ = self.actions.send(Action::Dismissed);
                    }
                }
            }
            Input::OverviewChanged(overview) => {
                let closed = self.overview && !overview;

                self.overview = overview;

                if closed && self.revealed && !self.focused {
                    self.hide(&widgets.canvas, root);

                    let _ = self.actions.send(Action::Dismissed);
                }
            }
            Input::MonitorsChanged => {
                self.hide(&widgets.canvas, root);

                let _ = self.actions.send(Action::Dismissed);
            }
            Input::Action(action) => self.handle_action(action, &widgets.canvas, root),
        }
    }
}

impl Panel {
    /// Selects the configured or requested monitor, presents and reveals the panel.
    fn show(
        &mut self,
        output: Option<String>,
        canvas: &gtk::Fixed,
        panel: &gtk::ApplicationWindow,
    ) {
        if let Some(output) = self.monitor.as_ref().or(output.as_ref())
            && let Some(monitor) = monitor_for_output(output)
        {
            self.move_to_monitor(&monitor, canvas, panel);
            self.monitor_selection.set(monitor);
        }

        if !self.revealed {
            self.clear_focus(panel);
        }

        self.revealed = true;

        let needs_keyboard = panel.keyboard_mode() == KeyboardMode::None;

        panel.set_visible(true);
        self.animation
            .animate(canvas, self.content.widget(), panel, true);

        if needs_keyboard {
            arm_keyboard_after_map(panel, self.animation.interactive.clone());
        }
    }

    /// Closes menus and releases keyboard input before sliding the panel away.
    fn hide(&mut self, canvas: &gtk::Fixed, panel: &gtk::ApplicationWindow) {
        self.revealed = false;
        self.popovers.close();
        self.popups.clear();
        panel.set_keyboard_mode(KeyboardMode::None);
        self.clear_focus(panel);
        self.animation
            .animate(canvas, self.content.widget(), panel, false);
    }

    /// Resets the content to its hidden position before changing to another monitor.
    ///
    /// Leaves the canvas unchanged if the panel is already on that monitor.
    fn move_to_monitor(
        &self,
        monitor: &gtk::gdk::Monitor,
        canvas: &gtk::Fixed,
        panel: &gtk::ApplicationWindow,
    ) {
        if panel.monitor().as_ref() == Some(monitor) {
            return;
        }

        self.animation.reset();
        canvas.move_(
            self.content.widget(),
            f64::from(HORIZONTAL_PADDING),
            -f64::from(BAR_HEIGHT),
        );

        panel.set_monitor(Some(monitor));
    }

    /// Updates open-menu tracking, then forwards the action.
    ///
    /// Opening a menu finishes panel reveal without grabbing keyboard or pointer
    /// input. On-demand focus lets outside clicks reach their original target.
    fn handle_action(
        &mut self,
        action: Action,
        canvas: &gtk::Fixed,
        panel: &gtk::ApplicationWindow,
    ) {
        match action {
            Action::PopupOpened(popup) => {
                if !self.revealed {
                    self.popovers.close();
                    return;
                }

                self.animation
                    .finish_show(canvas, self.content.widget(), panel);

                self.popups.insert(popup);
            }
            Action::PopupClosed(popup) => {
                self.popups.remove(&popup);

                if !self.popovers.is_open() {
                    self.clear_focus(panel);
                }
            }
            _ => {}
        }

        let _ = self.actions.send(action);
    }

    /// Removes the focused GTK child without changing panel visibility.
    fn clear_focus(&self, panel: &gtk::ApplicationWindow) {
        GtkWindowExt::set_focus(panel, None::<&gtk::Widget>);
    }
}

/// Enables click/hover focus only after the initial non-interactive buffer was
/// painted. Niri focuses newly mapped OnDemand surfaces even without a click.
/// Waiting for the next frame avoids stealing focus from overview or launchers.
fn arm_keyboard_after_map(panel: &gtk::ApplicationWindow, interactive: Rc<Cell<bool>>) {
    let painted = Cell::new(false);

    panel.add_tick_callback(move |panel, _| {
        if !interactive.get() {
            return gtk::glib::ControlFlow::Break;
        }

        if !painted.replace(true) {
            return gtk::glib::ControlFlow::Continue;
        }

        if !panel.is_active() {
            GtkWindowExt::set_focus(panel, None::<&gtk::Widget>);
        }

        panel.set_keyboard_mode(KeyboardMode::OnDemand);

        gtk::glib::ControlFlow::Break
    });
}

impl Drop for Panel {
    /// Detaches monitor observation and closes the panel's remaining menu.
    fn drop(&mut self) {
        if let Some((monitors, handler)) = self.monitor_subscription.take() {
            monitors.disconnect(handler);
        }

        self.popovers.close();
    }
}

/// Interruptible panel-slide state shared with GTK frame-clock callbacks.
struct Animation {
    position: Rc<Cell<f64>>,
    generation: Rc<Cell<u64>>,
    interactive: Rc<Cell<bool>>,
}

impl Default for Animation {
    /// Starts at the hidden position with no active animation generation.
    fn default() -> Self {
        Self {
            position: Rc::new(Cell::new(-f64::from(BAR_HEIGHT))),
            generation: Rc::new(Cell::new(0)),
            interactive: Rc::new(Cell::new(false)),
        }
    }
}

impl Animation {
    /// Resets the stored position; the caller is responsible for moving the canvas.
    fn reset(&self) {
        self.position.set(-f64::from(BAR_HEIGHT));
    }

    /// Slides panel content to its shown or hidden position over 240 milliseconds.
    ///
    /// A new generation supersedes previous callbacks. Hiding unmaps the panel
    /// only after the slide finishes; this is shell-specific, not a core fade.
    fn animate(
        &self,
        canvas: &gtk::Fixed,
        content: &gtk::CenterBox,
        panel: &gtk::ApplicationWindow,
        show: bool,
    ) {
        self.interactive.set(show);

        let generation = self.generation.get().wrapping_add(1);

        self.generation.set(generation);

        let start = self.position.get();

        sync_input_region(panel, canvas, start, show);

        let end = if show {
            f64::from(VERTICAL_PADDING)
        } else {
            -f64::from(BAR_HEIGHT)
        };

        if (start - end).abs() < f64::EPSILON {
            if !show {
                panel.set_visible(false);
            }

            return;
        }

        let position = self.position.clone();
        let current_generation = self.generation.clone();
        let content = content.clone();
        let panel = panel.clone();
        let started = Instant::now();

        canvas.add_tick_callback(move |canvas, _| {
            if current_generation.get() != generation {
                return gtk::glib::ControlFlow::Break;
            }

            sync_width(canvas, &content);

            let progress = (started.elapsed().as_secs_f64() / ANIMATION_SECONDS).min(1.0);
            let eased = 1.0 - (1.0 - progress).powi(3);
            let y = start + (end - start) * eased;

            position.set(y);
            canvas.move_(&content, f64::from(HORIZONTAL_PADDING), y.round());
            sync_input_region(&panel, canvas, y.round(), show);

            if progress >= 1.0 {
                position.set(end);
                canvas.move_(&content, f64::from(HORIZONTAL_PADDING), end);

                if !show {
                    panel.set_visible(false);
                }

                gtk::glib::ControlFlow::Break
            } else {
                gtk::glib::ControlFlow::Continue
            }
        });
    }

    /// Cancels pending slide callbacks and places visible content at its final position.
    fn finish_show(
        &self,
        canvas: &gtk::Fixed,
        content: &gtk::CenterBox,
        panel: &gtk::ApplicationWindow,
    ) {
        self.generation.set(self.generation.get().wrapping_add(1));

        let position = f64::from(VERTICAL_PADDING);

        self.position.set(position);
        self.interactive.set(true);
        sync_width(canvas, content);
        canvas.move_(content, f64::from(HORIZONTAL_PADDING), position);
        panel.set_visible(true);
        sync_input_region(panel, canvas, position, true);
    }
}

/// Logs an error and quits the application if layer-shell is unavailable.
fn ensure_layer_shell_support() {
    if !gtk4_layer_shell::is_supported() {
        tracing::error!("fern-topbar requires a Wayland compositor with layer-shell support");
        relm4::main_application().quit();
    }
}

/// Mounts edge triggers on matching monitors, or none when hover is disabled.
fn build_hover_triggers(
    enabled: bool,
    output: Option<&str>,
    actions: &tokio::sync::mpsc::UnboundedSender<Action>,
) -> Vec<Controller<PanelTrigger>> {
    if !enabled {
        return Vec::new();
    }

    let display = gtk::gdk::Display::default().expect("GTK display is available");
    let monitors = display.monitors();

    (0..monitors.n_items())
        .filter_map(|index| monitors.item(index).and_downcast::<gtk::gdk::Monitor>())
        .filter(|monitor| {
            output.is_none_or(|output| monitor.connector().as_deref() == Some(output))
        })
        .map(|monitor| {
            PanelTrigger::builder()
                .launch(trigger::PanelTriggerInit {
                    monitor,
                    actions: actions.clone(),
                })
                .detach()
        })
        .collect()
}

/// Excludes transparent shell padding while accepting panel and active-menu trees.
fn is_panel_target(
    widget: Option<&gtk::Widget>,
    content: &gtk::Widget,
    popovers: &PopoverScope,
) -> bool {
    widget.is_some_and(|widget| {
        widget == content || widget.is_ancestor(content) || popovers.contains(widget)
    })
}

/// Configures the overlay panel across the top edge without reserving screen space.
fn configure_panel_window(panel: &gtk::ApplicationWindow) {
    panel.set_application(Some(&relm4::main_application()));
    panel.init_layer_shell();
    panel.set_layer(Layer::Overlay);
    panel.set_anchor(Edge::Top, true);
    panel.set_anchor(Edge::Left, true);
    panel.set_anchor(Edge::Right, true);
    panel.set_exclusive_zone(0);
    panel.set_keyboard_mode(KeyboardMode::None);
    panel.set_namespace(Some("fern-topbar"));
}

/// Keeps content width synchronized when the fixed animation canvas is allocated.
fn connect_canvas_width_sync(
    canvas: &gtk::Fixed,
    content: &gtk::CenterBox,
    panel: &gtk::ApplicationWindow,
    position: Rc<Cell<f64>>,
    interactive: Rc<Cell<bool>>,
) {
    let content = content.clone();
    let panel = panel.downgrade();

    canvas.connect_notify_local(Some("width"), move |canvas, _| {
        sync_width(canvas, &content);

        if let Some(panel) = panel.upgrade() {
            sync_input_region(&panel, canvas, position.get(), interactive.get());
        }
    });
}

/// Makes the shadow and transparent shell margins pass pointer input through.
fn sync_input_region(
    panel: &gtk::ApplicationWindow,
    canvas: &gtk::Fixed,
    position: f64,
    interactive: bool,
) {
    let Some(surface) = panel.surface() else {
        return;
    };

    if !interactive {
        surface.set_input_region(Some(&gtk::cairo::Region::create()));
        return;
    }

    let top = (position.round() as i32).max(0);
    let bottom = (position.round() as i32 + BAR_HEIGHT).clamp(0, PANEL_HEIGHT);
    let scale = UiScale::for_widget(canvas);
    let region = gtk::cairo::Region::create_rectangle(&gtk::cairo::RectangleInt::new(
        scale.pixels(HORIZONTAL_PADDING),
        scale.pixels(top),
        scale.pixels((canvas.width() - 2 * HORIZONTAL_PADDING).max(0)),
        scale.pixels((bottom - top).max(0)),
    ));

    surface.set_input_region(Some(&region));
}

/// Requests the canvas width minus horizontal padding when it is positive and new.
fn sync_width(canvas: &gtk::Fixed, content: &gtk::CenterBox) {
    let width = canvas.width() - 2 * HORIZONTAL_PADDING;

    if width > 0 && content.width_request() != width {
        content.set_size_request(width, BAR_HEIGHT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gtk::test]
    fn dismissal_boundary_includes_controls_but_not_transparent_shell_padding() {
        let window = gtk::Window::new();
        let canvas = gtk::Fixed::new();
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let button = gtk::Button::new();
        let menu = gtk::Popover::new();
        let menu_child = gtk::Label::new(Some("Menu content"));
        let outside = gtk::Label::new(Some("Outside"));

        menu.set_child(Some(&menu_child));
        menu.set_parent(&button);
        content.append(&button);
        canvas.put(&content, 4.0, 4.0);
        window.set_child(Some(&canvas));

        let popovers = PopoverScope::default();
        let _popup = popovers.register(&menu, &button);

        window.present();
        menu.popup();

        for widget in [
            content.upcast_ref(),
            button.upcast_ref(),
            menu_child.upcast_ref(),
        ] {
            assert!(is_panel_target(
                Some(widget),
                content.upcast_ref(),
                &popovers
            ));
        }

        for widget in [
            canvas.upcast_ref(),
            window.upcast_ref(),
            outside.upcast_ref(),
        ] {
            assert!(!is_panel_target(
                Some(widget),
                content.upcast_ref(),
                &popovers
            ));
        }

        assert!(!is_panel_target(None, content.upcast_ref(), &popovers));

        popovers.close();
        menu.unparent();
        window.destroy();
    }
}

#[cfg(test)]
mod dismissal_tests {
    use super::core::{
        Button, Disclosure, MenuButtonStyle, MenuPopover, PanelIconButton, PanelMenuButton,
        PopoverStyle,
    };
    use super::*;
    use crate::backend::wm::state::WindowManagerState;
    use crate::features;
    use std::thread;
    use std::time::Duration;
    use wayland_client::{
        Connection, Dispatch, QueueHandle, delegate_noop,
        globals::{GlobalListContents, registry_queue_init},
        protocol::{wl_output, wl_pointer, wl_registry},
    };
    use wayland_protocols_wlr::virtual_pointer::v1::client::{
        zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
        zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    };

    #[derive(Default)]
    struct PointerState {
        output: Option<String>,
    }

    delegate_noop!(PointerState: ignore ZwlrVirtualPointerManagerV1);
    delegate_noop!(PointerState: ignore ZwlrVirtualPointerV1);

    impl Dispatch<wl_output::WlOutput, ()> for PointerState {
        fn event(
            state: &mut Self,
            _proxy: &wl_output::WlOutput,
            event: wl_output::Event,
            _data: &(),
            _connection: &Connection,
            _handle: &QueueHandle<Self>,
        ) {
            if let wl_output::Event::Name { name } = event {
                state.output = Some(name);
            }
        }
    }

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for PointerState {
        fn event(
            _state: &mut Self,
            _proxy: &wl_registry::WlRegistry,
            _event: wl_registry::Event,
            _data: &GlobalListContents,
            _connection: &Connection,
            _handle: &QueueHandle<Self>,
        ) {
        }
    }

    fn settle() {
        let context = gtk::glib::MainContext::default();

        for _ in 0..30 {
            while context.pending() {
                context.iteration(false);
            }

            thread::sleep(Duration::from_millis(10));
        }
    }

    fn click(pointer: &ZwlrVirtualPointerV1, mouse_button: u32) {
        pointer.frame();
        pointer.button(2, mouse_button, wl_pointer::ButtonState::Pressed);
        pointer.frame();
        pointer.button(3, mouse_button, wl_pointer::ButtonState::Released);
        pointer.frame();
    }

    fn move_pointer(
        pointer: &ZwlrVirtualPointerV1,
        time: u32,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    ) {
        // Niri's nested X11 output is vertically flipped, including absolute input.
        let y = if std::env::var_os("FERN_TOPBAR_TEST_FLIP_Y").is_some() {
            height.saturating_sub(y)
        } else {
            y
        };

        pointer.motion_absolute(time, x, y, width, height);
    }

    fn press_escape() {
        assert!(
            std::process::Command::new("wtype")
                .args(["-k", "Escape"])
                .status()
                .expect("the keyboard integration test requires wtype")
                .success()
        );

        settle();
    }

    fn set_overview(open: bool) {
        assert!(
            std::process::Command::new("niri")
                .args([
                    "msg",
                    "action",
                    if open {
                        "open-overview"
                    } else {
                        "close-overview"
                    },
                ])
                .status()
                .expect("the integration test requires an isolated niri session")
                .success()
        );

        settle();
    }

    #[gtk::test]
    #[ignore = "requires isolated niri, virtual-pointer and wtype; changes overview and input focus"]
    fn outside_click_reaches_its_target_and_dismisses_panel_and_menu() {
        let scale = std::env::var("FERN_TOPBAR_TEST_SCALE")
            .map(|value| UiScale::new(value.parse().unwrap()).unwrap())
            .unwrap_or_default();

        assert!(gtk4_layer_shell::is_supported());

        relm4::main_application()
            .register(None::<&gtk::gio::Cancellable>)
            .unwrap();

        let connection = Connection::connect_to_env().unwrap();
        let (globals, mut queue) = registry_queue_init::<PointerState>(&connection).unwrap();
        let handle = queue.handle();
        let manager: ZwlrVirtualPointerManagerV1 = globals.bind(&handle, 2..=2, ()).unwrap();
        let output: wl_output::WlOutput = globals.bind(&handle, 4..=4, ()).unwrap();
        let pointer = manager.create_virtual_pointer_with_output(None, Some(&output), &handle, ());
        let mut state = PointerState::default();

        queue.roundtrip(&mut state).unwrap();

        let monitor = monitor_for_output(state.output.as_deref().unwrap()).unwrap();
        let geometry = monitor.geometry();
        let features = EnabledFeatures::default();

        let (actions, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let panel = Panel::builder()
            .launch(PanelInit {
                scale,
                monitor: state.output.clone(),
                actions,
                features,
                services: FeatureServices {
                    availability: features::availability::FeatureAvailability::default(),
                    audio: features::AudioService::default(),
                    battery: Default::default(),
                    alerts: Default::default(),
                    window_manager: WindowManagerState::default(),
                    monitor_selection: MonitorSelection::default(),
                    wm_commands: None,
                },
                hover_enabled: false,
            })
            .detach();

        let button = PanelMenuButton::init(MenuButtonStyle::Labeled);
        button.widget().set_label("Test menu");

        let plain = Button::init(());
        plain.widget().set_label("Workspace");

        let icon = PanelIconButton::init(());
        icon.widget().set_label("Tray");

        let panel_entry = gtk::Entry::new();
        let panel_controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        panel_controls.append(&panel_entry);
        panel_controls.append(plain.widget());
        panel_controls.append(icon.widget());
        panel_controls.append(button.widget());

        let activations = std::rc::Rc::new(std::cell::Cell::new(0));

        for control in [plain.widget(), icon.widget()] {
            let activations = activations.clone();

            control.connect_clicked(move |_| activations.set(activations.get() + 1));
        }

        let menu = MenuPopover::init(PopoverStyle::Menu);
        let menu_content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let entry = gtk::Entry::new();
        let details = Disclosure::init("Connection details");

        menu_content.append(&entry);
        menu_content.append(details.widget());
        menu.widget().set_child(Some(&menu_content));
        button.widget().set_popover(Some(menu.widget()));
        panel
            .model()
            .content
            .widget()
            .set_center_widget(Some(&panel_controls));

        let _popup = panel.model().popovers.register_button(button.widget());

        let target = gtk::ApplicationWindow::new(&relm4::main_application());
        target.set_decorated(false);
        target.init_layer_shell();
        target.set_layer(Layer::Top);
        target.set_monitor(Some(&monitor));
        target.set_keyboard_mode(KeyboardMode::OnDemand);
        target.set_exclusive_zone(-1);

        for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            target.set_anchor(edge, true);
        }

        let received_clicks = Rc::new(Cell::new(0_u32));
        let clicks = gtk::GestureClick::new();
        clicks.set_button(0);
        clicks.connect_pressed({
            let received_clicks = received_clicks.clone();

            move |_, _, _, _| received_clicks.set(received_clicks.get() + 1)
        });

        target.add_controller(clicks);

        let received_escapes = Rc::new(Cell::new(0));
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed({
            let received_escapes = received_escapes.clone();

            move |_, key, _, _| {
                if key == gtk::gdk::Key::Escape {
                    received_escapes.set(received_escapes.get() + 1);

                    gtk::glib::Propagation::Stop
                } else {
                    gtk::glib::Propagation::Proceed
                }
            }
        });

        target.add_controller(keys);
        target.set_child(Some(&gtk::Button::with_label("Underlying click target")));
        target.present();
        settle();

        for (mouse_button, open_menu, outside_y, overview) in [
            (0x110, true, geometry.height() / 2, false),
            (0x111, true, geometry.height() / 2, false),
            (0x110, false, scale.pixels(PANEL_HEIGHT - 4), false),
            (0x110, true, geometry.height() / 2, true),
            (0x110, false, geometry.height() / 2, true),
        ] {
            set_overview(overview);
            move_pointer(
                &pointer,
                1,
                geometry.width() as u32 / 10,
                geometry.height() as u32 / 2,
                geometry.width() as u32,
                geometry.height() as u32,
            );

            click(&pointer, 0x110);
            queue.roundtrip(&mut state).unwrap();
            settle();
            panel.emit(Input::OverviewChanged(overview));
            panel.emit(Input::Show(None));
            settle();

            assert!(
                target.is_active(),
                "revealing the panel must not steal focus"
            );
            assert!(GtkWindowExt::focus(panel.widget()).is_none());

            let previous_escapes = received_escapes.get();

            press_escape();

            assert_eq!(received_escapes.get(), previous_escapes + 1);
            assert!(panel_entry.grab_focus());

            let focused = GtkWindowExt::focus(panel.widget()).unwrap();
            let previous_activations = activations.get();

            for control in [plain.widget(), icon.widget()] {
                let bounds = control.compute_bounds(panel.widget()).unwrap();

                move_pointer(
                    &pointer,
                    1,
                    (bounds.x() + bounds.width() / 2.0) as u32,
                    (bounds.y() + bounds.height() / 2.0) as u32,
                    geometry.width() as u32,
                    geometry.height() as u32,
                );

                click(&pointer, 0x110);
                queue.roundtrip(&mut state).unwrap();
                settle();

                assert!(!control.has_focus());
                assert_eq!(GtkWindowExt::focus(panel.widget()).as_ref(), Some(&focused));
            }

            assert_eq!(activations.get(), previous_activations + 2);

            let bounds = button.widget().compute_bounds(panel.widget()).unwrap();

            move_pointer(
                &pointer,
                1,
                (bounds.x() + bounds.width() / 2.0) as u32,
                (bounds.y() + bounds.height() / 2.0) as u32,
                geometry.width() as u32,
                geometry.height() as u32,
            );

            click(&pointer, 0x110);
            queue.roundtrip(&mut state).unwrap();
            settle();

            assert!(panel.model().revealed);
            assert!(panel.widget().is_active());
            assert_eq!(panel.widget().keyboard_mode(), KeyboardMode::OnDemand);

            let focused = GtkWindowExt::focus(panel.widget()).unwrap();

            assert!(focused != *button.widget() && !focused.is_ancestor(button.widget()));

            if open_menu {
                assert!(menu.widget().is_mapped());

                if let Ok(path) = std::env::var("FERN_TOPBAR_TEST_PREVIEW") {
                    let snapshot = gtk::Snapshot::new();
                    let paintable = gtk::WidgetPaintable::new(Some(menu.widget()));
                    paintable.snapshot(
                        &snapshot,
                        menu.widget().width().into(),
                        menu.widget().height().into(),
                    );

                    let node = snapshot.to_node().unwrap();

                    menu.widget()
                        .native()
                        .unwrap()
                        .renderer()
                        .unwrap()
                        .render_texture(&node, None)
                        .save_to_png(path)
                        .unwrap();
                }

                let popup = menu
                    .widget()
                    .surface()
                    .unwrap()
                    .downcast::<gtk::gdk::Popup>()
                    .unwrap();

                assert!(entry.grab_focus());

                let focused = GtkWindowExt::focus(panel.widget()).unwrap();
                let toggle_bounds = details
                    .toggle
                    .widget()
                    .compute_bounds(menu.widget())
                    .unwrap();

                let expanded = details.revealer.reveals_child();

                move_pointer(
                    &pointer,
                    1,
                    (popup.position_x() as f32 + toggle_bounds.x() + toggle_bounds.width() / 2.0)
                        as u32,
                    (popup.position_y() as f32 + toggle_bounds.y() + toggle_bounds.height() / 2.0)
                        as u32,
                    geometry.width() as u32,
                    geometry.height() as u32,
                );

                click(&pointer, 0x110);
                queue.roundtrip(&mut state).unwrap();
                settle();

                assert!(panel.model().revealed);
                assert!(menu.widget().get_visible());
                assert_eq!(details.revealer.reveals_child(), !expanded);
                assert!(!details.toggle.widget().has_focus());
                assert_eq!(GtkWindowExt::focus(panel.widget()).as_ref(), Some(&focused));
            } else {
                button.widget().popdown();
                settle();
            }

            while receiver.try_recv().is_ok() {}

            if overview {
                if open_menu {
                    press_escape();

                    assert!(!menu.widget().get_visible());
                    assert!(panel.model().revealed);
                    assert!(panel.model().overview);

                    while let Ok(action) = receiver.try_recv() {
                        assert!(!matches!(action, Action::CloseOverview | Action::Dismissed));
                    }
                }

                press_escape();

                assert!(matches!(receiver.try_recv(), Ok(Action::CloseOverview)));
                assert!(receiver.try_recv().is_err());
                assert!(panel.model().revealed);
                assert!(panel.model().overview);

                if open_menu {
                    button.widget().popup();
                    settle();
                }
                // A launcher opening over overview must receive Escape immediately,
                // while the pointer stays over the panel/menu.
                target.set_visible(false);
                settle();
                target.present();
                settle();

                assert!(target.is_active());
                assert!(!menu.widget().get_visible());
                assert!(GtkWindowExt::focus(panel.widget()).is_none());

                let previous_escapes = received_escapes.get();

                press_escape();

                assert_eq!(received_escapes.get(), previous_escapes + 1);
            }

            let previous_clicks = received_clicks.get();

            move_pointer(
                &pointer,
                1,
                geometry.width() as u32 / 10,
                outside_y as u32,
                geometry.width() as u32,
                geometry.height() as u32,
            );

            click(&pointer, mouse_button);
            queue.roundtrip(&mut state).unwrap();
            settle();

            assert_eq!(received_clicks.get(), previous_clicks + 1);

            if overview {
                assert!(!panel.widget().is_active());
                assert!(panel.model().revealed);
                assert!(!menu.widget().get_visible());
                assert!(panel.model().popups.is_empty());
                assert!(GtkWindowExt::focus(panel.widget()).is_none());
                assert!(
                    (0..receiver.len())
                        .all(|_| !matches!(receiver.try_recv(), Ok(Action::Dismissed)))
                );

                panel.emit(Input::OverviewChanged(false));
                set_overview(false);
                settle();
            }

            assert!(!panel.model().revealed);
            assert!(!menu.widget().get_visible());
            assert!(!panel.widget().get_visible());
            assert_eq!(panel.widget().keyboard_mode(), KeyboardMode::None);
            assert!(panel.model().popups.is_empty());
            assert!(GtkWindowExt::focus(panel.widget()).is_none());
            assert!(
                (0..receiver.len()).any(|_| matches!(receiver.try_recv(), Ok(Action::Dismissed)))
            );

            panel.emit(Input::KeepOpen);
            settle();

            assert!(!panel.widget().get_visible());
        }

        pointer.destroy();
        panel.widget().destroy();
        target.destroy();
    }
}
