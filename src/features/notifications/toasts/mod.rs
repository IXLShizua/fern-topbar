use super::state::Display;
use crate::backend::notifications::Notification;
#[cfg(test)]
use crate::backend::notifications::Urgency;
use crate::ui::core::{ActionButton, Button, UiScale};
use card::ToastCard;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use relm4::factory::FactoryVecDeque;
use relm4::gtk;
use relm4::gtk::prelude::*;
use relm4::prelude::*;
use std::time::Duration;

mod card;

const CARD_TRANSITION_MS: u32 = 220;
const TOAST_TOP_MARGIN: i32 = 58;
const TOAST_WIDTH: i32 = 380;
const FALLBACK_HEIGHT_LIMIT: i32 = 500;

pub struct Toasts {
    scale: UiScale,
    cards: FactoryVecDeque<ToastCard>,
    desired_order: Vec<u32>,
    pending_reveals: Vec<u32>,
    expanded: bool,
    items_empty: bool,
    history_label: String,
    history_visible: bool,
    history_leaving: bool,
    window_visible: bool,
    // Keep the display and card state while the panel's notifications menu is open.
    suppressed: bool,
    hide_generation: u64,
    display_generation: u64,
    card_forwarder: gtk::glib::JoinHandle<()>,
}

impl Drop for Toasts {
    fn drop(&mut self) {
        self.card_forwarder.abort();
    }
}

#[derive(Debug)]
pub enum Input {
    Display(u64, Display),
    Suppressed(bool),
    Monitor(gtk::gdk::Monitor),
    Activate(u32),
    Dismiss(u32),
    ToggleHistory,
    Clear,
    Hover(bool),
    RemoveCard(u32),
    HideHistory(u64),
    HideWindow(u64),
    LayoutChanged,
    CardLayoutReady(u32),
}

#[derive(Debug)]
pub enum Output {
    Activate(u32),
    Dismiss(u32),
    ToggleHistory,
    Clear,
    Hover(bool),
    HidePreviews(u64, Vec<u32>),
}

#[relm4::component(pub)]
impl Component for Toasts {
    type Init = UiScale;
    type Input = Input;
    type Output = Output;
    type CommandOutput = ();

    view! {
        #[root]
        window = gtk::ApplicationWindow {
            set_decorated: false,
            set_resizable: false,
            add_css_class: "topbar-toast-window",
            #[watch]
            set_visible: model.window_visible && !model.suppressed,

            add_controller = gtk::EventControllerMotion {
                connect_enter[sender] => move |_, _, _| sender.input(Input::Hover(true)),
                connect_leave[sender] => move |_| sender.input(Input::Hover(false)),
            },

            #[name = "container"]
            gtk::Box {
                add_css_class: "topbar-toast-container",
                set_orientation: gtk::Orientation::Vertical,
                set_width_request: TOAST_WIDTH,

                gtk::Box {
                    add_css_class: "topbar-toast-toolbar",
                    set_orientation: gtk::Orientation::Horizontal,
                    set_spacing: 8,
                    #[watch]
                    set_visible: model.expanded || model.history_visible,

                    #[name = "history_header"]
                    gtk::Box {
                        add_css_class: "topbar-toast-header",
                        set_orientation: gtk::Orientation::Horizontal,
                        set_spacing: 8,
                        set_hexpand: true,
                        #[watch]
                        set_visible: model.expanded,

                        gtk::Label {
                            add_css_class: "topbar-toast-heading",
                            set_label: "Notification history",
                            set_halign: gtk::Align::Start,
                            set_hexpand: true,
                        },
                        #[template]
                        Button {
                            add_css_class: "topbar-toast-control",
                            set_label: "Clear all",
                            #[watch]
                            set_sensitive: !model.items_empty,
                            connect_clicked => Input::Clear,
                        },
                    },

                    #[name = "history_toggle"]
                    #[template]
                    ActionButton {
                        add_css_class: "topbar-toast-expand",
                        set_halign: gtk::Align::End,
                        #[watch]
                        set_hexpand: !model.expanded,
                        #[watch]
                        set_class_active: ("leaving", model.history_leaving),
                        #[watch]
                        set_visible: model.history_visible,
                        #[watch]
                        set_sensitive: !model.history_leaving,
                        #[watch]
                        set_label: &model.history_label,
                        connect_clicked => Input::ToggleHistory,
                    },
                },

                #[name = "scroll"]
                gtk::ScrolledWindow {
                    add_css_class: "topbar-toast-scroll",
                    set_policy: (gtk::PolicyType::Never, gtk::PolicyType::Automatic),
                    set_propagate_natural_height: true,
                    set_propagate_natural_width: false,
                    set_valign: gtk::Align::Start,

                    #[local_ref]
                    list -> gtk::Box {
                        add_css_class: "topbar-toast-list",
                        set_orientation: gtk::Orientation::Vertical,
                        set_spacing: 7,
                    },
                },
            },
        }
    }

    fn init(
        scale: UiScale,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // Factory forwarding requires Send inputs, but Input::Monitor owns a GTK object.
        // This bridge only routes child UI events and must stay on the GTK thread.
        let (card_sender, card_receiver) = relm4::channel::<card::Output>();
        let cards = FactoryVecDeque::builder()
            .launch_default()
            .forward(&card_sender, |output| output);
        let input = sender.input_sender().clone();
        let card_forwarder = relm4::spawn_local(async move {
            while let Some(output) = card_receiver.recv().await {
                if input.send(card_output(output)).is_err() {
                    break;
                }
            }
        });

        let model = Self {
            scale,
            cards,
            desired_order: Vec::new(),
            pending_reveals: Vec::new(),
            expanded: false,
            items_empty: true,
            history_label: String::new(),
            history_visible: false,
            history_leaving: false,
            window_visible: false,
            suppressed: false,
            hide_generation: 0,
            display_generation: 0,
            card_forwarder,
        };

        let list = model.cards.widget();
        let widgets = view_output!();
        // GTK retains the size group through its widgets; keeping a Rust field
        // is unnecessary. Hidden history chrome must still set the shared height.
        let toolbar_height = gtk::SizeGroup::new(gtk::SizeGroupMode::Vertical);
        toolbar_height.add_widget(&widgets.history_header);
        toolbar_height.add_widget(widgets.history_toggle.widget());

        configure_toast_window(&root, scale);
        scale.configure_window(&root);

        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        message: Self::Input,
        sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        match message {
            Input::Suppressed(suppressed) => self.suppressed = suppressed,
            Input::Display(generation, display) => {
                self.display_generation = generation;
                self.render(&display, &sender);
            }
            Input::Monitor(monitor) => {
                root.set_monitor(Some(&monitor));
            }
            Input::Activate(id) => {
                let _ = sender.output(Output::Activate(id));
            }
            Input::Dismiss(id) => {
                let _ = sender.output(Output::Dismiss(id));
            }
            Input::ToggleHistory => {
                let _ = sender.output(Output::ToggleHistory);
            }
            Input::Clear => {
                let _ = sender.output(Output::Clear);
            }
            Input::Hover(hovered) => {
                let _ = sender.output(Output::Hover(hovered));
            }
            Input::RemoveCard(id) => {
                self.remove_card(id);
            }
            Input::HideHistory(generation) => {
                if generation == self.hide_generation && self.history_leaving {
                    self.history_visible = false;
                    self.history_leaving = false;
                }
            }
            Input::HideWindow(generation) => {
                if generation == self.hide_generation && self.items_empty {
                    self.window_visible = false;

                    let _ = sender.output(Output::Hover(false));
                }
            }
            Input::CardLayoutReady(id) => self.pending_reveals.push(id),
            Input::LayoutChanged => {}
        }

        self.update_view(widgets, sender.clone());

        let max_height = self.scale.units(toast_height_limit(root)).max(1);

        if !self.expanded {
            let hidden = trim_previews(
                &mut self.cards,
                &mut self.desired_order,
                &widgets.container,
                &widgets.scroll,
                max_height,
            );

            if !hidden.is_empty() {
                let _ = sender.output(Output::HidePreviews(self.display_generation, hidden));
            }

            self.order_cards();
        }

        resize_to_content(
            root,
            &widgets.container,
            &widgets.scroll,
            max_height,
            self.expanded.then_some(self.cards.widget()),
        );

        // Truncation reveals Show more and changes card height after first layout.
        // Fit the entire batch before any card fades in, or history briefly paints
        // an extra bottom card using the earlier, shorter measurements.
        if !self.pending_reveals.is_empty()
            && self
                .cards
                .iter()
                .all(|card| card.layout_ready() || card.leaving())
        {
            for id in self.pending_reveals.drain(..) {
                if let Some(index) = self.cards.iter().position(|card| card.id() == id) {
                    self.cards.send(index, card::Input::Reveal);
                }
            }
        }
    }
}

fn card_output(output: card::Output) -> Input {
    match output {
        card::Output::Activate(id) => Input::Activate(id),
        card::Output::Dismiss(id) => Input::Dismiss(id),
        card::Output::Remove(id) => Input::RemoveCard(id),
        card::Output::LayoutChanged => Input::LayoutChanged,
        card::Output::LayoutReady(id) => Input::CardLayoutReady(id),
    }
}

fn configure_toast_window(root: &gtk::ApplicationWindow, scale: UiScale) {
    root.set_application(Some(&relm4::main_application()));
    root.init_layer_shell();
    root.set_layer(Layer::Overlay);
    root.set_anchor(Edge::Top, true);
    root.set_anchor(Edge::Right, true);
    root.set_margin(Edge::Top, scale.pixels(TOAST_TOP_MARGIN));
    root.set_margin(Edge::Right, scale.pixels(12));
    root.set_exclusive_zone(0);
    root.set_keyboard_mode(KeyboardMode::None);
    root.set_namespace(Some("fern-topbar-notifications"));
}

impl Toasts {
    fn render(&mut self, display: &Display, sender: &ComponentSender<Self>) {
        self.hide_generation = self.hide_generation.wrapping_add(1);

        let generation = self.hide_generation;

        self.expanded = display.expanded;
        self.items_empty = display.items.is_empty();
        self.desired_order = display.items.iter().rev().map(|item| item.id).collect();

        self.update_history_control(display, generation, sender);
        self.sync_cards(display);

        if self.items_empty {
            let input = sender.input_sender().clone();

            gtk::glib::timeout_add_local_once(
                Duration::from_millis(u64::from(CARD_TRANSITION_MS)),
                move || {
                    let _ = input.send(Input::HideWindow(generation));
                },
            );
        } else {
            self.window_visible = true;
        }
    }

    fn update_history_control(
        &mut self,
        display: &Display,
        generation: u64,
        sender: &ComponentSender<Self>,
    ) {
        let show = !display.items.is_empty() && (display.hidden > 0 || display.expanded);

        if show {
            self.history_visible = true;
            self.history_leaving = false;
            self.history_label = if display.expanded {
                "Close history".into()
            } else {
                format!(
                    "View history · {} notifications",
                    display.hidden + display.items.len()
                )
            };
        } else if self.history_visible && !self.history_leaving {
            self.history_leaving = true;

            let input = sender.input_sender().clone();

            gtk::glib::timeout_add_local_once(
                Duration::from_millis(u64::from(CARD_TRANSITION_MS)),
                move || {
                    let _ = input.send(Input::HideHistory(generation));
                },
            );
        }
    }

    fn sync_cards(&mut self, display: &Display) {
        for index in 0..self.cards.len() {
            let card = &self.cards[index];

            if !display.items.iter().any(|item| item.id == card.id()) && !card.leaving() {
                self.cards.send(index, card::Input::Leave);
            }
        }

        for item in display.items.iter().rev() {
            if let Some(index) = self.cards.iter().position(|card| card.id() == item.id) {
                if !self.cards[index].matches(item) || self.cards[index].leaving() {
                    self.cards.send(index, card::Input::Replace(item.clone()));
                }
            } else {
                self.cards.guard().push_back(item.clone());
            }
        }

        self.order_cards();
    }

    fn remove_card(&mut self, id: u32) {
        let index = self
            .cards
            .iter()
            .position(|card| card.id() == id && card.leaving());

        if let Some(index) = index {
            self.cards.guard().remove(index);
            self.order_cards();
        }
    }

    fn order_cards(&mut self) {
        if self.cards.iter().any(ToastCard::leaving) {
            return;
        }

        let mut cards = self.cards.guard();

        for (target, id) in self.desired_order.iter().copied().enumerate() {
            if let Some(current) = (target..cards.len()).find(|&index| cards[index].id() == id) {
                cards.move_to(current, target);
            }
        }
    }
}

fn toast_height_limit(root: &gtk::ApplicationWindow) -> i32 {
    let monitor = root.monitor().or_else(|| {
        gtk::prelude::WidgetExt::display(root)
            .monitors()
            .item(0)
            .and_downcast::<gtk::gdk::Monitor>()
    });

    monitor.map_or(FALLBACK_HEIGHT_LIMIT, |monitor| {
        (monitor.geometry().height() / 2).max(1)
    })
}

fn natural_size(container: &gtk::Box, scroll: &gtk::ScrolledWindow) -> (i32, i32) {
    scroll.set_max_content_height(i32::MAX);

    let (_, height, _, _) = container.measure(gtk::Orientation::Vertical, TOAST_WIDTH);

    (TOAST_WIDTH, height)
}

fn trim_previews(
    cards: &mut FactoryVecDeque<ToastCard>,
    order: &mut Vec<u32>,
    container: &gtk::Box,
    scroll: &gtk::ScrolledWindow,
    max_height: i32,
) -> Vec<u32> {
    if natural_size(container, scroll).1 <= max_height {
        return Vec::new();
    }

    {
        let mut cards = cards.guard();

        for index in (0..cards.len()).rev() {
            if !order.contains(&cards[index].id()) {
                cards.remove(index);
            }
        }
    }

    let mut hidden = Vec::new();

    while order.len() > 1 && natural_size(container, scroll).1 > max_height {
        // Keep critical previews ahead of ordinary ones, but fit complete cards
        // for every urgency. Overflow stays available in notification history.
        let position = order
            .iter()
            .rposition(|id| {
                cards
                    .iter()
                    .any(|card| card.id() == *id && !card.is_critical())
            })
            .unwrap_or(order.len() - 1);

        let oldest = order.remove(position);
        let index = cards.iter().position(|card| card.id() == oldest);

        if let Some(index) = index {
            cards.guard().remove(index);
            hidden.push(oldest);
        }
    }

    hidden
}

fn resize_to_content(
    window: &impl IsA<gtk::Window>,
    container: &gtk::Box,
    scroll: &gtk::ScrolledWindow,
    max_height: i32,
    history_list: Option<&gtk::Box>,
) {
    let (width, _) = natural_size(container, scroll);
    let mut limit = max_height.max(1);

    loop {
        scroll.set_max_content_height(limit);

        let (_, height, _, _) = container.measure(gtk::Orientation::Vertical, width);

        if height <= max_height || limit == 1 {
            break;
        }

        limit = (limit - (height - max_height)).max(1);
    }

    if let Some(list) = history_list {
        let content_width = width - horizontal_insets(container) - horizontal_insets(scroll);
        let (_, content_height, _, _) = list.measure(gtk::Orientation::Vertical, content_width);
        let scrollbar_width = if scroll.is_overlay_scrolling() || content_height <= limit {
            0
        } else {
            scroll
                .vscrollbar()
                .measure(gtk::Orientation::Horizontal, -1)
                .1
        };

        let height = history_content_height(list, content_width - scrollbar_width, limit);

        scroll.set_max_content_height(height);
    }

    let (_, height, _, _) = container.measure(gtk::Orientation::Vertical, width);
    let scale = UiScale::for_widget(container);

    window.set_default_size(
        scale.pixels(width),
        scale.pixels(height.clamp(1, max_height.max(1))),
    );
}

/// Measures the complete cards before layout, including CSS insets and box spacing.
fn history_content_height(list: &gtk::Box, width: i32, limit: i32) -> i32 {
    let (min_width, _, _, _) = list.measure(gtk::Orientation::Horizontal, -1);
    let width = width.max(min_width).max(1);
    let (_, natural_height, _, _) = list.measure(gtk::Orientation::Vertical, width);

    if natural_height <= limit {
        return natural_height.max(1);
    }

    let card_width = (width - horizontal_insets(list)).max(1);
    let heights: Vec<_> = std::iter::successors(list.first_child(), |child| child.next_sibling())
        .filter(|child| child.get_visible())
        .map(|child| child.measure(gtk::Orientation::Vertical, card_width).1)
        .collect();

    let spacing = list.spacing();
    let total_spacing = spacing * heights.len().saturating_sub(1) as i32;
    let insets = (natural_height - heights.iter().sum::<i32>() - total_spacing).max(0);
    let mut height = insets;
    let mut fitted_height = None;

    for (index, card_height) in heights.into_iter().enumerate() {
        if index > 0 {
            height += spacing;
        }

        height += card_height;

        if height > limit {
            break;
        }

        fitted_height = Some(height);
    }

    // A card taller than the entire viewport must remain scrollable.
    fitted_height.unwrap_or(limit).max(1)
}

// Native measurements include CSS, but wrapping text needs the inner horizontal
// width. GTK4 exposes those insets only through the deprecated StyleContext API;
// replacing them with fixed padding would break theme and scrollbar-aware fitting.
#[allow(
    deprecated,
    reason = "GTK4 has no replacement for reading computed CSS insets"
)]
fn horizontal_insets(widget: &impl IsA<gtk::Widget>) -> i32 {
    let style = widget.style_context();
    let padding = style.padding();
    let border = style.border();
    let margin = style.margin();

    i32::from(
        padding.left()
            + padding.right()
            + border.left()
            + border.right()
            + margin.left()
            + margin.right(),
    ) + widget.margin_start()
        + widget.margin_end()
}

fn same_notification(a: &Notification, b: &Notification) -> bool {
    a.app == b.app
        && a.icon == b.icon
        && a.desktop_entry == b.desktop_entry
        && a.summary == b.summary
        && a.body == b.body
        && a.default_action == b.default_action
        && a.resident == b.resident
        && a.urgency == b.urgency
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notification(id: u32, body: String) -> Notification {
        Notification {
            id,
            app: String::new(),
            icon: String::new(),
            desktop_entry: None,
            summary: "Wide text".into(),
            body,
            default_action: true,
            resident: false,
            urgency: Urgency::Normal,
            request_attention: false,
        }
    }

    #[gtk::test]
    fn critical_previews_fit_whole_cards_like_normal_previews() {
        relm4::set_global_css(include_str!(concat!(env!("OUT_DIR"), "/styles.css")));

        let window = gtk::Window::new();
        window.set_decorated(false);
        window.set_resizable(false);

        let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
        container.add_css_class("topbar-toast-container");
        container.set_width_request(TOAST_WIDTH);

        let scroll = gtk::ScrolledWindow::new();
        scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroll.set_propagate_natural_height(true);

        let mut cards = card_factory();
        cards.widget().add_css_class("topbar-toast-list");
        scroll.set_child(Some(cards.widget()));
        container.append(&scroll);
        window.set_child(Some(&container));

        for id in [2, 1] {
            let mut item = notification(id, "Critical battery warning".into());
            item.urgency = Urgency::Critical;
            cards.guard().push_back(item);
        }

        let (_, two_cards_height) = natural_size(&container, &scroll);
        let second_height = cards
            .widget()
            .last_child()
            .unwrap()
            .measure(gtk::Orientation::Vertical, TOAST_WIDTH)
            .1;

        cards
            .guard()
            .push_back(notification(3, "Ordinary notification".into()));

        let mut order = vec![2, 1, 3];
        let max_height = two_cards_height - second_height / 2;
        let hidden = trim_previews(&mut cards, &mut order, &container, &scroll, max_height);

        assert_eq!(hidden, [3, 1]);
        assert_eq!(order, [2]);
        assert!(cards.iter().all(ToastCard::is_critical));
        assert!(
            cards
                .widget()
                .first_child()
                .unwrap()
                .first_child()
                .unwrap()
                .has_css_class("critical")
        );
        resize_to_content(&window, &container, &scroll, max_height, None);

        let critical_size = window.default_size();

        window.present();

        let main_loop = gtk::glib::MainLoop::new(None, false);

        for _ in 0..3 {
            run_frame(&main_loop);
        }

        let card = cards.widget().first_child().unwrap();
        let bounds = card.compute_bounds(&scroll).unwrap();

        assert!(bounds.height() > 0.0);
        assert!(f64::from(bounds.y() + bounds.height()) <= scroll.vadjustment().page_size() + 1.0);
        assert!(window.height() <= max_height);

        cards.send(
            0,
            card::Input::Replace(notification(2, "Critical battery warning".into())),
        );
        run_frame(&main_loop);
        resize_to_content(&window, &container, &scroll, max_height, None);
        assert_eq!(window.default_size(), critical_size);
        assert_eq!(cards.len(), 1);
        window.destroy();
    }

    fn card_factory() -> FactoryVecDeque<ToastCard> {
        let cards: FactoryVecDeque<ToastCard> =
            FactoryVecDeque::builder().launch_default().detach();
        cards.widget().set_orientation(gtk::Orientation::Vertical);
        cards.widget().set_spacing(7);

        cards
    }

    #[gtk::test]
    fn switching_to_history_does_not_flash_extra_cards_or_move_the_stack() {
        relm4::set_global_css(include_str!(concat!(env!("OUT_DIR"), "/styles.css")));
        relm4::main_application()
            .register(None::<&gtk::gio::Cancellable>)
            .unwrap();

        let settings = gtk::Settings::default().unwrap();
        let animations = settings.is_gtk_enable_animations();

        for (factor, animated) in [
            (0.75, true),
            (1.0, true),
            (1.25, true),
            (1.5, true),
            (2.0, true),
            (1.0, false),
        ] {
            settings.set_gtk_enable_animations(animated);

            let toasts = Toasts::builder()
                .launch(UiScale::new(factor).unwrap())
                .detach();
            let items: Vec<_> = (1..=10)
                .map(|id| {
                    notification(
                        id,
                        "Long body which needs Show more and wraps across several lines. "
                            .repeat(20),
                    )
                })
                .collect();

            toasts.emit(Input::Display(
                1,
                Display {
                    items: items[8..].to_vec(),
                    hidden: 8,
                    expanded: false,
                },
            ));

            let main_loop = gtk::glib::MainLoop::new(None, false);

            for _ in 0..4 {
                run_frame(&main_loop);
            }

            let window = toasts.widget();
            let scroll = toasts.widgets().scroll.clone();
            let list = toasts.model().cards.widget().clone();
            let first = list.first_child().unwrap();
            let before = first.compute_bounds(window).unwrap().y();
            let preview_items = items[8..].to_vec();
            let frames = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let frame_clock = window.frame_clock().unwrap();
            let frame_handler = frame_clock.connect_after_paint({
                let frames = frames.clone();
                let scroll = scroll.clone();
                let list = list.clone();
                let first = first.clone();
                let window = window.clone();

                move |_| {
                    let page = scroll.vadjustment().page_size();
                    let mut partial = Vec::new();
                    let visible: Vec<_> =
                        std::iter::successors(list.first_child(), |card| card.next_sibling())
                            .enumerate()
                            .filter_map(|(index, card)| {
                                let bounds = card.compute_bounds(&scroll)?;
                                let painted = card
                                    .first_child()
                                    .is_some_and(|content| content.opacity() > 0.0)
                                    && bounds.height() > 0.0
                                    && f64::from(bounds.y()) < page
                                    && bounds.y() + bounds.height() > 0.0;

                                if painted && f64::from(bounds.y() + bounds.height()) > page + 1.0 {
                                    partial.push(index);
                                }

                                painted.then_some(index)
                            })
                            .collect();

                    let first_y = first.compute_bounds(&window).unwrap().y();

                    frames.borrow_mut().push((visible, partial, first_y));
                }
            });

            toasts.emit(Input::Display(
                2,
                Display {
                    items,
                    hidden: 0,
                    expanded: true,
                },
            ));

            for _ in 0..6 {
                run_frame(&main_loop);
            }

            frame_clock.disconnect(frame_handler);

            let frames = frames.borrow();
            let final_visible = &frames.last().unwrap().0;

            assert!(!final_visible.is_empty());
            assert!(
                frames.iter().all(|frame| frame.1.is_empty()),
                "partial cards painted at scale {factor}: {frames:?}"
            );
            assert!(
                frames.iter().all(|frame| frame.2 == before),
                "stack moved during switching at scale {factor}: {frames:?}"
            );
            assert!(
                frames
                    .iter()
                    .all(|frame| frame.0.iter().all(|card| final_visible.contains(card))),
                "extra cards flashed at scale {factor}: {frames:?}"
            );

            let after = first.compute_bounds(window).unwrap().y();

            assert_eq!(before, after, "stack moved at scale {factor}");
            drop(frames);

            let widgets = toasts.widgets();

            assert_eq!(
                widgets
                    .history_header
                    .compute_bounds(&widgets.container)
                    .unwrap()
                    .height(),
                widgets
                    .history_toggle
                    .widget()
                    .compute_bounds(&widgets.container)
                    .unwrap()
                    .height()
            );
            drop(widgets);

            toasts.emit(Input::Display(
                3,
                Display {
                    items: preview_items,
                    hidden: 8,
                    expanded: false,
                },
            ));

            for _ in 0..4 {
                run_frame(&main_loop);
            }

            assert_eq!(
                first.compute_bounds(window).unwrap().y(),
                before,
                "stack moved on closing at scale {factor}"
            );
            window.destroy();
        }

        settings.set_gtk_enable_animations(animations);
    }

    #[gtk::test]
    fn notifications_keep_their_width_when_expanded_collapsed_or_replaced() {
        let provider = gtk::CssProvider::new();
        provider.load_from_data(include_str!(concat!(env!("OUT_DIR"), "/styles.css")));

        let display = gtk::gdk::Display::default().unwrap();

        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );

        let window = gtk::Window::new();
        window.set_decorated(false);
        window.set_resizable(false);

        let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
        container.add_css_class("topbar-toast-container");
        container.set_width_request(TOAST_WIDTH);

        let scroll = gtk::ScrolledWindow::new();
        scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroll.set_propagate_natural_height(true);
        scroll.set_propagate_natural_width(false);

        let mut cards = card_factory();
        cards.widget().add_css_class("topbar-toast-list");
        scroll.set_child(Some(cards.widget()));
        container.append(&scroll);
        window.set_child(Some(&container));
        cards
            .guard()
            .push_back(notification(1, "Long notification text ".repeat(100)));

        resize_to_content(&window, &container, &scroll, FALLBACK_HEIGHT_LIMIT, None);
        window.present();

        let main_loop = gtk::glib::MainLoop::new(None, false);

        for _ in 0..3 {
            run_frame(&main_loop);
        }

        let revealer = cards
            .widget()
            .first_child()
            .unwrap()
            .downcast::<gtk::Revealer>()
            .unwrap();

        let card = revealer.child().unwrap().downcast::<gtk::Box>().unwrap();
        let hint = card
            .first_child()
            .unwrap()
            .last_child()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap();

        let card_width = card.width();
        let collapsed_height = window.default_size().1;

        assert_eq!(window.width(), TOAST_WIDTH);
        assert!(card_width > 0);
        assert!(hint.get_visible());
        assert_eq!(hint.label().as_deref(), Some("Show more"));

        hint.emit_clicked();
        run_frame(&main_loop);
        resize_to_content(&window, &container, &scroll, FALLBACK_HEIGHT_LIMIT, None);
        run_frame(&main_loop);

        assert!(cards[0].expanded());
        assert_eq!(hint.label().as_deref(), Some("Show less"));
        assert!(window.default_size().1 > collapsed_height);
        assert_eq!(window.default_size().0, TOAST_WIDTH);
        assert_eq!(window.width(), TOAST_WIDTH);
        assert_eq!(card.width(), card_width);

        hint.emit_clicked();
        run_frame(&main_loop);
        resize_to_content(&window, &container, &scroll, FALLBACK_HEIGHT_LIMIT, None);
        run_frame(&main_loop);

        assert!(!cards[0].expanded());
        assert_eq!(window.width(), TOAST_WIDTH);
        assert_eq!(card.width(), card_width);

        let mut short = notification(1, String::new());
        short.summary = "Hi".into();
        cards.send(0, card::Input::Replace(short));
        run_frame(&main_loop);
        resize_to_content(&window, &container, &scroll, FALLBACK_HEIGHT_LIMIT, None);
        run_frame(&main_loop);

        assert_eq!(window.width(), TOAST_WIDTH);
        assert_eq!(card.width(), card_width);
        assert!(window.default_size().1 <= collapsed_height);

        window.destroy();
        gtk::style_context_remove_provider_for_display(&display, &provider);
    }

    #[gtk::test]
    fn preview_window_fits_content_and_hides_oldest_overflow() {
        let window = gtk::Window::new();
        window.set_decorated(false);
        window.set_resizable(false);
        window.set_default_size(TOAST_WIDTH, 20);

        let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
        container.set_width_request(TOAST_WIDTH);

        let scroll = gtk::ScrolledWindow::new();
        scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroll.set_propagate_natural_height(true);
        scroll.set_propagate_natural_width(false);

        let mut cards = card_factory();

        scroll.set_child(Some(cards.widget()));

        let footer = gtk::Button::with_label("View history");
        footer.set_halign(gtk::Align::End);
        container.append(&footer);
        container.append(&scroll);
        window.set_child(Some(&container));
        cards
            .guard()
            .push_back(notification(3, "Newest notification".into()));

        let single_size = natural_size(&container, &scroll);
        let max_height = single_size.1 + 5;

        resize_to_content(&window, &container, &scroll, max_height, None);

        assert_eq!(window.default_size(), single_size);

        window.present();

        let main_loop = gtk::glib::MainLoop::new(None, false);

        run_frame(&main_loop);

        assert_eq!(natural_size(&container, &scroll), single_size);

        run_frame(&main_loop);
        run_frame(&main_loop);

        assert_eq!(natural_size(&container, &scroll), single_size);
        assert!(window.height() >= single_size.1);
        assert!(window.height() <= max_height);

        let initial_history_position = history_position(&footer, &window);

        cards
            .guard()
            .push_back(notification(2, "Older notification".into()));
        cards
            .guard()
            .push_back(notification(1, "Oldest notification".into()));

        let full_size = natural_size(&container, &scroll);

        assert!(full_size.1 > max_height);

        let mut order = vec![3, 2, 1];
        let hidden = trim_previews(&mut cards, &mut order, &container, &scroll, max_height);

        assert_eq!(hidden, [1, 2]);
        assert_eq!(order, [3]);
        assert_eq!(cards.len(), 1);

        resize_to_content(&window, &container, &scroll, max_height, None);

        assert_eq!(window.default_size(), single_size);

        cards.guard().push_back(notification(
            4,
            "Another notification with a wider line".into(),
        ));

        let grown_size = natural_size(&container, &scroll);

        resize_to_content(&window, &container, &scroll, grown_size.1, None);

        assert!(window.default_size().1 > single_size.1);

        run_frame(&main_loop);

        assert_eq!(natural_size(&container, &scroll), grown_size);
        assert_eq!(history_position(&footer, &window), initial_history_position);

        run_frame(&main_loop);
        run_frame(&main_loop);

        assert_eq!(natural_size(&container, &scroll), grown_size);
        assert_eq!(history_position(&footer, &window), initial_history_position);
        assert!(window.height() >= grown_size.1);

        cards.send(1, card::Input::Leave);
        run_frame(&main_loop);

        assert_eq!(history_position(&footer, &window), initial_history_position);

        run_frame(&main_loop);

        assert_eq!(history_position(&footer, &window), initial_history_position);

        cards.guard().remove(1);
        resize_to_content(&window, &container, &scroll, max_height, None);

        assert_eq!(window.default_size(), natural_size(&container, &scroll));

        run_frame(&main_loop);

        assert!(window.height() <= max_height);
        assert_eq!(history_position(&footer, &window), initial_history_position);

        cards
            .guard()
            .push_back(notification(5, "Very long notification ".repeat(100)));

        let mut order = vec![5, 3];

        trim_previews(&mut cards, &mut order, &container, &scroll, max_height);
        resize_to_content(&window, &container, &scroll, max_height, None);

        assert!(window.default_size().1 <= max_height);
        assert_eq!(order, [5]);

        run_frame(&main_loop);

        assert!(window.height() <= max_height);

        window.destroy();
    }

    #[gtk::test]
    fn history_height_ends_after_a_complete_card_and_keeps_the_rest_scrollable() {
        relm4::set_global_css(include_str!(concat!(env!("OUT_DIR"), "/styles.css")));

        for (factor, overlay_scrolling) in [
            (0.75, true),
            (1.0, true),
            (1.5, true),
            (2.0, true),
            (1.0, false),
        ] {
            let scale = UiScale::new(factor).unwrap();
            let window = gtk::Window::new();
            window.set_decorated(false);
            window.set_resizable(false);

            let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
            container.add_css_class("topbar-toast-container");
            container.set_width_request(TOAST_WIDTH);

            let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            header.add_css_class("topbar-toast-toolbar");
            header.append(&gtk::Label::new(Some("Notification history")));

            let close = ActionButton::init(());
            close.widget().set_label("Close history");
            header.append(close.widget());
            container.append(&header);

            let scroll = gtk::ScrolledWindow::new();
            scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
            scroll.set_propagate_natural_height(true);
            scroll.set_overlay_scrolling(overlay_scrolling);

            let mut cards = card_factory();
            cards.widget().add_css_class("topbar-toast-list");

            for id in 1..=6 {
                cards
                    .guard()
                    .push_back(notification(id, "Notification body\n".repeat(id as usize)));
            }

            scroll.set_child(Some(cards.widget()));
            container.append(&scroll);
            window.set_child(Some(&container));
            scale.configure_window(&window);
            resize_to_content(
                &window,
                &container,
                &scroll,
                FALLBACK_HEIGHT_LIMIT,
                Some(cards.widget()),
            );

            let opening_size = window.default_size();

            window.present();

            let main_loop = gtk::glib::MainLoop::new(None, false);
            let painted_frames = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let frame_clock = window.frame_clock().unwrap();
            let record_frames = |frames: &std::rc::Rc<std::cell::RefCell<Vec<Vec<u32>>>>| {
                let frames = frames.clone();
                let scroll = scroll.clone();
                let list = cards.widget().clone();

                frame_clock.connect_after_paint(move |_| {
                    let page = scroll.vadjustment().page_size();
                    let partial: Vec<_> =
                        std::iter::successors(list.first_child(), |card| card.next_sibling())
                            .enumerate()
                            .filter_map(|(index, card)| {
                                let bounds = card.compute_bounds(&scroll)?;

                                (bounds.height() > 0.0
                                    && f64::from(bounds.y()) < page
                                    && f64::from(bounds.y() + bounds.height()) > page + 1.0)
                                    .then_some(index as u32)
                            })
                            .collect();

                    frames.borrow_mut().push(partial);
                })
            };

            let frame_handler = record_frames(&painted_frames);

            for _ in 0..3 {
                run_frame(&main_loop);
                assert_eq!(window.default_size(), opening_size);

                let page = scroll.vadjustment().page_size();

                for card in
                    std::iter::successors(cards.widget().first_child(), |card| card.next_sibling())
                {
                    let bounds = card.compute_bounds(&scroll).unwrap();

                    if f64::from(bounds.y()) < page && bounds.height() > 0.0 {
                        assert!(
                            f64::from(bounds.y() + bounds.height()) <= page + 1.0,
                            "partial card on opening at scale {factor}: {bounds:?}, viewport {page}"
                        );
                    }
                }
            }

            frame_clock.disconnect(frame_handler);
            assert!(!painted_frames.borrow().is_empty());
            assert!(
                painted_frames.borrow().iter().all(Vec::is_empty),
                "partial cards during opening at scale {factor}: {:?}",
                painted_frames.borrow()
            );

            let third = cards
                .widget()
                .first_child()
                .unwrap()
                .next_sibling()
                .unwrap()
                .next_sibling()
                .unwrap();

            let bounds = third.compute_bounds(&scroll).unwrap();
            let list_limit = (bounds.y() + bounds.height() / 2.0) as i32;
            let chrome = scale.units(window.default_size().1) - scroll.max_content_height();
            let budget = list_limit + chrome;

            resize_to_content(&window, &container, &scroll, budget, None);
            run_frame(&main_loop);

            let before = third.compute_bounds(&scroll).unwrap();

            assert!(f64::from(before.y()) < scroll.vadjustment().page_size());
            assert!(f64::from(before.y() + before.height()) > scroll.vadjustment().page_size());

            painted_frames.borrow_mut().clear();

            let frame_handler = record_frames(&painted_frames);

            resize_to_content(&window, &container, &scroll, budget, Some(cards.widget()));

            let final_size = window.default_size();

            for _ in 0..3 {
                run_frame(&main_loop);
                assert_eq!(window.default_size(), final_size);
            }

            frame_clock.disconnect(frame_handler);
            assert!(!painted_frames.borrow().is_empty());
            assert!(
                painted_frames.borrow().iter().all(Vec::is_empty),
                "partial cards during resize at scale {factor}: {:?}",
                painted_frames.borrow()
            );

            let adjustment = scroll.vadjustment();
            let page = adjustment.page_size();
            let mut visible = 0;

            for card in
                std::iter::successors(cards.widget().first_child(), |card| card.next_sibling())
            {
                let bounds = card.compute_bounds(&scroll).unwrap();

                if f64::from(bounds.y()) < page && bounds.height() > 0.0 {
                    visible += 1;
                    assert!(
                        f64::from(bounds.y() + bounds.height()) <= page + 1.0,
                        "partial card at scale {factor}: {bounds:?}, viewport {page}"
                    );
                }
            }

            assert_eq!(visible, 2);
            assert_eq!(cards.len(), 6);
            assert!(window.height() <= scale.pixels(budget));
            assert_eq!(window.width(), scale.pixels(TOAST_WIDTH));

            adjustment.set_value(adjustment.upper() - page);
            run_frame(&main_loop);

            let last = cards
                .widget()
                .last_child()
                .unwrap()
                .compute_bounds(&scroll)
                .unwrap();

            assert!(last.y() >= 0.0);
            assert!(f64::from(last.y() + last.height()) <= page + 1.0);
            window.destroy();
        }
    }

    #[gtk::test]
    fn scaled_previews_fit_the_pixel_budget_and_keep_the_history_control_stable() {
        relm4::set_global_css(include_str!(concat!(env!("OUT_DIR"), "/styles.css")));

        for factor in [0.75, 1.25, 1.5, 2.0] {
            let scale = UiScale::new(factor).unwrap();
            let window = gtk::Window::new();
            window.set_decorated(false);
            window.set_resizable(false);

            let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
            container.set_width_request(TOAST_WIDTH);

            let scroll = gtk::ScrolledWindow::new();
            scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
            scroll.set_propagate_natural_height(true);
            scroll.set_propagate_natural_width(false);

            let mut cards = card_factory();

            scroll.set_child(Some(cards.widget()));

            let history = ActionButton::init(());
            history.widget().set_label("View history");
            history.widget().set_halign(gtk::Align::End);
            container.append(history.widget());
            container.append(&scroll);
            window.set_child(Some(&container));
            scale.configure_window(&window);
            cards
                .guard()
                .push_back(notification(3, "Newest notification".into()));

            let pixel_limit = 200;
            let content_limit = scale.units(pixel_limit);

            resize_to_content(&window, &container, &scroll, content_limit, None);
            window.present();

            let main_loop = gtk::glib::MainLoop::new(None, false);

            run_frame(&main_loop);

            assert!(window.height() <= pixel_limit);
            assert_eq!(window.width(), scale.pixels(TOAST_WIDTH));

            let history_position_before = history_position(history.widget(), &window);

            for id in [2, 1] {
                cards
                    .guard()
                    .push_back(notification(id, "Older notification\n".repeat(20)));
            }

            let mut order = vec![3, 2, 1];
            let hidden = trim_previews(&mut cards, &mut order, &container, &scroll, content_limit);

            assert!(!hidden.is_empty());
            assert_eq!(hidden[0], 1);

            resize_to_content(&window, &container, &scroll, content_limit, None);
            run_frame(&main_loop);

            assert!(window.height() <= pixel_limit);
            assert_eq!(window.width(), scale.pixels(TOAST_WIDTH));
            assert_eq!(
                history_position(history.widget(), &window),
                history_position_before
            );

            window.destroy();
        }
    }

    fn run_frame(main_loop: &gtk::glib::MainLoop) {
        let quit = main_loop.clone();

        gtk::glib::timeout_add_local_once(Duration::from_millis(100), move || quit.quit());
        main_loop.run();
    }

    fn history_position(button: &gtk::Button, window: &gtk::Window) -> (f32, f32) {
        let bounds = button.compute_bounds(window).unwrap();

        (
            bounds.y(),
            window.width() as f32 - bounds.x() - bounds.width(),
        )
    }
}
