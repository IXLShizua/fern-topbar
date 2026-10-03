use super::{menu, state::Center, toasts};
use crate::backend::notifications::{Event, server::Controls};
use crate::ui::core::PopoverScope;
use relm4::gtk;
use relm4::gtk::prelude::*;
use relm4::prelude::*;
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

pub struct Host {
    center: Center,
    controls: Controls,
    menu: Controller<menu::Menu>,
    toasts: Controller<toasts::Toasts>,
    // GLib one-shot timers cannot be reused; ignore expirations superseded by
    // hover/history/menu changes and layout replies from an older display snapshot.
    timer_generation: Rc<Cell<u64>>,
    display_generation: u64,
}

pub struct HostInit {
    pub popovers: PopoverScope,
    pub controls: Controls,
}

#[derive(Debug)]
pub enum Input {
    ServerEvent(Event),
    Monitor(gtk::gdk::Monitor),
    Menu(menu::Output),
    Toast(toasts::Output),
    Expire(u64),
}

#[relm4::component(pub)]
impl Component for Host {
    type Init = HostInit;
    type Input = Input;
    type Output = ();
    type CommandOutput = ();

    view! {
        #[root]
        gtk::Box {
            set_halign: gtk::Align::Start,
            set_valign: gtk::Align::Center,
            #[local_ref]
            menu_widget -> gtk::MenuButton {},
        }
    }

    fn init(
        init: Self::Init,
        _root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let HostInit { popovers, controls } = init;

        let scale = popovers.scale();
        let menu = menu::Menu::builder()
            .launch(popovers)
            .forward(sender.input_sender(), Input::Menu);
        let toasts = toasts::Toasts::builder()
            .launch(scale)
            .forward(sender.input_sender(), Input::Toast);

        let menu_widget = menu.widget().widget().clone();
        let model = Self {
            center: Center::default(),
            controls,
            menu,
            toasts,
            timer_generation: Rc::new(Cell::new(0)),
            display_generation: 0,
        };

        let widgets = view_output!();

        ComponentParts { model, widgets }
    }

    fn update(&mut self, message: Self::Input, sender: ComponentSender<Self>, _root: &Self::Root) {
        match message {
            Input::ServerEvent(event) => self.handle_server_event(event, &sender),
            Input::Monitor(monitor) => self.toasts.emit(toasts::Input::Monitor(monitor)),
            Input::Menu(action) => self.handle_menu_action(action, &sender),
            Input::Toast(action) => self.handle_toast_action(action, &sender),
            Input::Expire(generation) => self.handle_expiration(generation, &sender),
        }
    }
}

impl Host {
    fn handle_server_event(&mut self, event: Event, sender: &ComponentSender<Self>) {
        self.center.apply(event);
        self.sync(sender);
    }

    fn handle_menu_action(&mut self, action: menu::Output, sender: &ComponentSender<Self>) {
        match action {
            menu::Output::Activate(id) => self.activate(id, sender),
            menu::Output::Dismiss(id) => self.dismiss(id, sender),
            menu::Output::Clear => self.clear(sender),
            menu::Output::History => {
                self.center.expand(true);
                self.sync(sender);
            }
            menu::Output::Popup(open) => {
                self.center.set_popup_open(open);
                self.toasts.emit(toasts::Input::Suppressed(open));
                self.schedule_expiry(sender);
            }
        }
    }

    fn handle_toast_action(&mut self, action: toasts::Output, sender: &ComponentSender<Self>) {
        match action {
            toasts::Output::Activate(id) => self.activate(id, sender),
            toasts::Output::Dismiss(id) => self.dismiss(id, sender),
            toasts::Output::HidePreviews(generation, ids) => {
                if generation == self.display_generation {
                    self.center.hide_previews(&ids);
                    self.sync(sender);
                }
            }
            toasts::Output::ToggleHistory => {
                self.center.expand(!self.center.display().expanded);
                self.sync(sender);
            }
            toasts::Output::Clear => self.clear(sender),
            toasts::Output::Hover(hovered) => {
                self.center.set_hovered(hovered);
                self.schedule_expiry(sender);
            }
        }
    }

    fn handle_expiration(&mut self, generation: u64, sender: &ComponentSender<Self>) {
        if self.timer_generation.get() == generation {
            self.center.expire();
            self.sync(sender);
        }
    }

    fn dismiss(&mut self, id: u32, sender: &ComponentSender<Self>) {
        self.center.dismiss(id);
        self.controls.close(id, 2);
        self.sync(sender);
    }

    fn activate(&mut self, id: u32, sender: &ComponentSender<Self>) {
        let Some(notification) = self.center.history().iter().find(|item| item.id == id) else {
            return;
        };

        if !notification.default_action {
            return;
        }

        let close_after = !notification.resident;

        self.controls.invoke_default(id, close_after);

        if close_after {
            self.center.dismiss(id);
            self.sync(sender);
        }
    }

    fn clear(&mut self, sender: &ComponentSender<Self>) {
        for item in self.center.history() {
            self.controls.close(item.id, 2);
        }

        self.center.clear();
        self.sync(sender);
    }

    fn sync(&mut self, sender: &ComponentSender<Self>) {
        self.display_generation = self.display_generation.wrapping_add(1);
        self.menu
            .emit(menu::Input::Items(self.center.history().to_vec()));
        self.toasts.emit(toasts::Input::Display(
            self.display_generation,
            self.center.display(),
        ));

        self.schedule_expiry(sender);
    }

    fn schedule_expiry(&self, sender: &ComponentSender<Self>) {
        let generation = self.timer_generation.get().wrapping_add(1);

        self.timer_generation.set(generation);

        if let Some(wait) = self.center.next_wait() {
            let input = sender.input_sender().clone();

            gtk::glib::timeout_add_local_once(wait.max(Duration::from_millis(1)), move || {
                let _ = input.send(Input::Expire(generation));
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::dbus::tests::Bus;
    use crate::backend::notifications::{Notification, Urgency, server::Backend};

    #[gtk::test]
    fn notifications_popup_hides_and_restores_desktop_previews() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _bus = runtime.block_on(Bus::new());
        let backend = Backend::start(Default::default());

        relm4::main_application()
            .register(None::<&gtk::gio::Cancellable>)
            .unwrap();

        let host = Host::builder()
            .launch(HostInit {
                popovers: PopoverScope::default(),
                controls: backend.controls(),
            })
            .detach();

        let panel = gtk::ApplicationWindow::builder()
            .application(&relm4::main_application())
            .child(host.widget())
            .build();
        panel.present();

        let notify = |id| {
            host.emit(Input::ServerEvent(Event::Added(
                Notification {
                    id,
                    app: "Test".into(),
                    icon: String::new(),
                    desktop_entry: None,
                    summary: format!("Notification {id}"),
                    body: String::new(),
                    default_action: false,
                    resident: false,
                    urgency: Urgency::Normal,
                    request_attention: false,
                },
                1_000,
            )));
        };

        let settle = |wait| {
            let main_loop = gtk::glib::MainLoop::new(None, false);
            let quit = main_loop.clone();

            gtk::glib::timeout_add_local_once(wait, move || quit.quit());
            main_loop.run();
        };

        notify(1);
        settle(Duration::from_millis(100));

        let toasts = host.model().toasts.widget().clone();
        let button = host.model().menu.widget().widget().clone();

        assert!(toasts.get_visible());

        button.popup();
        settle(Duration::from_millis(100));
        assert!(button.is_active());
        assert!(!toasts.get_visible());
        assert_eq!(host.model().center.next_wait(), None);

        notify(2);
        settle(Duration::from_millis(1_100));
        assert!(!toasts.get_visible());
        assert_eq!(host.model().center.display().items.len(), 2);

        button.popdown();
        settle(Duration::from_millis(100));
        assert!(!button.is_active());
        assert!(toasts.get_visible());
        assert_eq!(host.model().center.display().items.len(), 2);
        assert!(host.model().center.next_wait().is_some());

        // Opening desktop history through the menu also releases suppression.
        button.popup();
        settle(Duration::from_millis(100));
        host.model().menu.emit(menu::Input::History);
        settle(Duration::from_millis(100));
        assert!(!button.is_active());
        assert!(toasts.get_visible());
        assert!(host.model().center.display().expanded);

        toasts.destroy();
        panel.destroy();
    }
}
