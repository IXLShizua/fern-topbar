use super::super::backend::{Item, MenuEntry};
use super::TrayInput;
use super::icon::TrayImageExt;
use super::menu_row::{Action as MenuAction, MenuRow, RowData};
use crate::ui::core::{
    Button, MenuPopover, PanelIconButton, PopoverScope, PopoverStyle, PopupRegistration,
};
use relm4::factory::{DynamicIndex, FactoryComponent, FactorySender, FactoryVecDeque, FactoryView};
use relm4::gtk::prelude::*;
use relm4::prelude::*;

const TRAY_ICON_SIZE: i32 = 16;
const OVERLAY_ICON_SIZE: i32 = 10;

pub struct TrayItem {
    _popup: Option<PopupRegistration>,
    popovers: PopoverScope,
    item: Item,
    history: MenuHistory,
    menu_title: String,
    menu_rows: FactoryVecDeque<MenuRow>,
    popup_visible: bool,
}

#[derive(Debug)]
pub enum TrayItemInput {
    Primary,
    Secondary,
    OpenMenu,
    Scroll(i32, bool),
    OpenPage(i32),
    Back,
    Menu(MenuAction),
    Popup,
    Popdown,
    Closed,
    RefreshIcon,
}

#[relm4::factory(pub)]
impl FactoryComponent for TrayItem {
    type Init = (Item, PopoverScope);
    type Input = TrayItemInput;
    type Output = TrayInput;
    type CommandOutput = ();
    type ParentWidget = gtk::Box;

    view! {
        #[root]
        #[template]
        button = PanelIconButton {
            add_css_class: "topbar-tray-item",
            #[watch]
            set_tooltip_text: Some(&self.item.tooltip),
            connect_clicked => TrayItemInput::Primary,
            connect_scale_factor_notify => TrayItemInput::RefreshIcon,

            add_controller = gtk::GestureClick {
                set_button: 2,
                connect_pressed[sender] => move |gesture, _, _, _| {
                    sender.input(TrayItemInput::Secondary);
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                },
            },
            add_controller = gtk::GestureClick {
                set_button: 3,
                connect_pressed[sender] => move |gesture, _, _, _| {
                    sender.input(TrayItemInput::OpenMenu);
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                },
            },
            add_controller = gtk::EventControllerScroll::new(
                gtk::EventControllerScrollFlags::BOTH_AXES
                    | gtk::EventControllerScrollFlags::DISCRETE
            ) {
                connect_scroll[sender] => move |_, dx, dy| {
                    let (delta, horizontal) = if dx != 0.0 { (dx, true) } else { (dy, false) };

                    sender.input(TrayItemInput::Scroll(delta.signum() as i32, horizontal));

                    gtk::glib::Propagation::Stop
                },
            },

            gtk::Overlay {
                #[wrap(Some)]
                set_child = &gtk::Image {
                    set_pixel_size: TRAY_ICON_SIZE,
                    #[watch]
                    set_tray_icon: (
                        &self.item.icon,
                        self.item.icon_theme_path.as_deref(),
                        TRAY_ICON_SIZE,
                    ),
                },
                add_overlay = &gtk::Image {
                    set_pixel_size: OVERLAY_ICON_SIZE,
                    set_halign: gtk::Align::End,
                    set_valign: gtk::Align::End,
                    #[watch]
                    set_visible: self.item.overlay.is_some(),
                    #[watch]
                    set_tray_icon: (
                        self.item.overlay.as_ref().unwrap_or(&self.item.icon),
                        self.item.icon_theme_path.as_deref(),
                        OVERLAY_ICON_SIZE,
                    ),
                },
            },
        },

        #[name = "popover"]
        #[template]
        MenuPopover(PopoverStyle::Menu) {
            add_css_class: "topbar-tray-popover",
            set_parent: button.widget(),
            connect_closed => TrayItemInput::Closed,

            gtk::ScrolledWindow {
                set_policy: (gtk::PolicyType::Never, gtk::PolicyType::Automatic),
                set_max_content_height: 420,
                set_max_content_width: 300,
                set_propagate_natural_height: true,
                set_propagate_natural_width: true,

                gtk::Box {
                    add_css_class: "topbar-tray-menu",
                    set_orientation: gtk::Orientation::Vertical,

                    gtk::Box {
                        add_css_class: "topbar-tray-menu-header",
                        set_orientation: gtk::Orientation::Horizontal,
                        set_spacing: 6,

                        #[template]
                        Button {
                            add_css_class: "topbar-tray-back",
                            set_icon_name: "go-previous-symbolic",
                            #[watch]
                            set_visible: self.history.current() != 0,
                            connect_clicked => TrayItemInput::Back,
                        },
                        gtk::Label {
                            add_css_class: "topbar-tray-menu-title",
                            set_halign: gtk::Align::Start,
                            set_hexpand: true,
                            #[watch]
                            set_label: &self.menu_title,
                        },
                    },

                    gtk::Label {
                        add_css_class: "topbar-tray-empty",
                        set_label: "No actions",
                        #[watch]
                        set_visible: self.menu_rows.is_empty(),
                    },

                    #[local_ref]
                    menu_rows -> gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                    },
                },
            },
        }
    }

    fn init_model(
        (item, popovers): Self::Init,
        _index: &DynamicIndex,
        sender: FactorySender<Self>,
    ) -> Self {
        let menu_rows = FactoryVecDeque::builder()
            .launch_default()
            .forward(sender.input_sender(), TrayItemInput::Menu);

        let mut model = Self {
            _popup: None,
            popovers,
            menu_title: item.title.clone(),
            item,
            history: MenuHistory::new(),
            menu_rows,
            popup_visible: false,
        };

        model.sync_menu(true, None);

        model
    }

    fn init_widgets(
        &mut self,
        _index: &DynamicIndex,
        root: Self::Root,
        _returned_widget: &<Self::ParentWidget as FactoryView>::ReturnedWidget,
        sender: FactorySender<Self>,
    ) -> Self::Widgets {
        let menu_rows = self.menu_rows.widget();
        let widgets = view_output!();

        self._popup = Some(
            self.popovers
                .register(widgets.popover.widget(), root.widget()),
        );

        let button = root.widget().downgrade();

        widgets
            .popover
            .widget()
            .connect_visible_notify(move |popover| {
                if let Some(button) = button.upgrade() {
                    if popover.is_visible() {
                        button.set_state_flags(gtk::StateFlags::CHECKED, false);
                    } else {
                        button.unset_state_flags(gtk::StateFlags::CHECKED);
                    }
                }
            });

        widgets
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        message: Self::Input,
        sender: FactorySender<Self>,
    ) {
        let update_view = match message {
            TrayItemInput::Primary => {
                let message = if self.item.item_is_menu {
                    TrayInput::Open(self.item.id.clone(), 0)
                } else {
                    TrayInput::Activate(self.item.id.clone())
                };

                let _ = sender.output(message);

                false
            }
            TrayItemInput::Secondary => {
                let _ = sender.output(TrayInput::Secondary(self.item.id.clone()));

                false
            }
            TrayItemInput::OpenMenu => {
                if self.popup_visible {
                    widgets.popover.widget().popdown();
                } else {
                    let _ = sender.output(TrayInput::Open(self.item.id.clone(), 0));
                }

                false
            }
            TrayItemInput::Scroll(delta, horizontal) => {
                let _ = sender.output(TrayInput::Scroll(self.item.id.clone(), delta, horizontal));

                false
            }
            TrayItemInput::OpenPage(page) => {
                let previous = self.history.current();

                self.history.open(page);

                let changed = self.history.current() != previous;

                if changed {
                    self.sync_menu(true, None);
                }

                changed
            }
            TrayItemInput::Back => {
                let previous = self.history.current();

                self.history.back();

                let changed = self.history.current() != previous;

                if changed {
                    self.sync_menu(true, None);

                    let _ = sender.output(TrayInput::PageChanged(
                        self.item.id.clone(),
                        self.history.current(),
                    ));
                }

                changed
            }
            TrayItemInput::Menu(MenuAction::Open(page)) => {
                let _ = sender.output(TrayInput::Open(self.item.id.clone(), page));

                false
            }
            TrayItemInput::Menu(MenuAction::Click(entry)) => {
                let _ = sender.output(TrayInput::Click(self.item.id.clone(), entry));

                false
            }
            TrayItemInput::Popup => {
                if !self.popup_visible {
                    self.popup_visible = true;
                    widgets.popover.widget().popup();
                }

                false
            }
            TrayItemInput::Popdown => {
                widgets.popover.widget().popdown();

                false
            }
            TrayItemInput::Closed => {
                self.popup_visible = false;

                false
            }
            TrayItemInput::RefreshIcon => true,
        };

        if update_view {
            self.update_view(widgets, sender);
        }
    }

    fn shutdown(&mut self, widgets: &mut Self::Widgets, _output: relm4::Sender<Self::Output>) {
        if widgets.popover.widget().parent().is_some() {
            widgets.popover.widget().unparent();
        }
    }
}

impl TrayItem {
    pub fn id(&self) -> &str {
        &self.item.id
    }

    pub fn has_menu(&self) -> bool {
        self.item.menu_path.is_some()
    }

    pub fn replace(
        &mut self,
        item: Item,
        menu_changed: bool,
        title_changed: bool,
        changed_rows: Option<&std::collections::HashSet<i32>>,
    ) {
        let root_title_changed = self.history.current() == 0 && title_changed;

        self.item = item;

        if menu_changed
            && MenuTree::new(&self.item.menu)
                .page(self.history.current())
                .is_none()
        {
            self.history.open(0);
        }

        if menu_changed || root_title_changed {
            self.sync_menu(false, changed_rows);
        }
    }

    fn sync_menu(
        &mut self,
        replace_existing: bool,
        changed_rows: Option<&std::collections::HashSet<i32>>,
    ) {
        let page_id = self.history.current();
        let page = MenuTree::new(&self.item.menu).page(page_id);
        let title = page
            .and_then(|page| page.title)
            .unwrap_or(&self.item.title)
            .to_owned();

        if self.menu_title != title {
            self.menu_title = title;
        }

        let entries = page.map_or(&[][..], |page| page.entries);
        let mut rows = self.menu_rows.guard();

        let entry_ids = entries
            .iter()
            .map(|entry| entry.id)
            .collect::<std::collections::HashSet<_>>();

        let mut index = 0;

        while index < rows.len() {
            if entry_ids.contains(&rows[index].id()) {
                index += 1;
            } else {
                rows.remove(index);
            }
        }

        for (target, entry) in entries.iter().enumerate() {
            if let Some(source) = (target..rows.len()).find(|&index| rows[index].id() == entry.id) {
                if source != target {
                    rows.move_to(source, target);
                }

                if replace_existing
                    || changed_rows.is_some_and(|changed| changed.contains(&entry.id))
                {
                    rows[target].replace(entry);
                }
            } else {
                rows.insert(target, RowData::from(entry));
            }
        }

        drop(rows);
    }
}

struct MenuHistory {
    pages: Vec<i32>,
}

impl MenuHistory {
    fn new() -> Self {
        Self { pages: vec![0] }
    }

    fn current(&self) -> i32 {
        self.pages.last().copied().unwrap_or(0)
    }

    fn open(&mut self, page: i32) {
        if page == 0 {
            self.pages.clear();
            self.pages.push(0);
        } else if self.pages.last().copied() != Some(page) {
            self.pages.push(page);
        }
    }

    fn back(&mut self) {
        if self.pages.len() > 1 {
            self.pages.pop();
        }
    }
}

struct MenuTree<'a> {
    entries: &'a [MenuEntry],
}

#[derive(Clone, Copy)]
struct MenuPage<'a> {
    title: Option<&'a str>,
    entries: &'a [MenuEntry],
}

impl<'a> MenuTree<'a> {
    fn new(entries: &'a [MenuEntry]) -> Self {
        Self { entries }
    }

    fn page(&self, id: i32) -> Option<MenuPage<'a>> {
        if id == 0 {
            return Some(MenuPage {
                title: None,
                entries: self.entries,
            });
        }

        Self::find_page(self.entries, id)
    }

    fn find_page(entries: &'a [MenuEntry], id: i32) -> Option<MenuPage<'a>> {
        for entry in entries {
            if entry.id == id {
                return Some(MenuPage {
                    title: Some(&entry.label),
                    entries: &entry.children,
                });
            }

            if let Some(page) = Self::find_page(&entry.children, id) {
                return Some(page);
            }
        }

        None
    }
}
