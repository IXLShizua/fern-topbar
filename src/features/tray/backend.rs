use crate::backend::dbus::{Service, probe};
use crate::features::availability::{Availability, AvailabilityPublisher, ProbeError};
use crate::runtime::{self, Task};
use std::collections::{HashMap, HashSet};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

#[derive(Clone, Debug, PartialEq)]
pub struct Icon {
    pub name: String,
    pub pixmaps: Vec<Pixmap>,
}

impl Icon {
    pub fn best_pixmap(&self, target_size: i32) -> Option<&Pixmap> {
        let target_size = target_size.max(1);

        self.pixmaps
            .iter()
            .filter(|pixmap| pixmap.width.min(pixmap.height) >= target_size)
            .min_by_key(|pixmap| pixmap.width.min(pixmap.height))
            .or_else(|| {
                self.pixmaps
                    .iter()
                    .max_by_key(|pixmap| pixmap.width.min(pixmap.height))
            })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pixmap {
    pub width: i32,
    pub height: i32,
    pub argb: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MenuEntry {
    pub id: i32,
    pub label: String,
    pub icon: String,
    pub icon_data: Option<Vec<u8>>,
    pub shortcut: Option<String>,
    pub enabled: bool,
    pub separator: bool,
    pub toggle: Option<(bool, bool)>,
    pub children: Vec<MenuEntry>,
    pub submenu: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub id: String,
    pub title: String,
    pub tooltip: String,
    pub icon: Icon,
    pub overlay: Option<Icon>,
    pub icon_theme_path: Option<String>,
    pub menu: Vec<MenuEntry>,
    pub menu_path: Option<String>,
    pub item_is_menu: bool,
}

#[derive(Debug)]
pub struct TrayUpdate {
    pub items: Vec<Item>,
    pub changed_items: HashSet<String>,
    pub changed_menus: HashSet<String>,
    pub changed_titles: HashSet<String>,
    pub changed_menu_rows: HashMap<String, HashSet<i32>>,
}

#[derive(Debug)]
pub enum Command {
    Activate(String),
    SecondaryActivate(String),
    ContextMenu(String),
    OpenMenu(String, i32),
    ClickMenu(String, i32),
    Scroll(String, i32, bool),
}

fn changed_menu_rows(previous: &[MenuEntry], current: &[MenuEntry]) -> HashSet<i32> {
    let previous_by_id = previous
        .iter()
        .map(|entry| (entry.id, entry))
        .collect::<HashMap<_, _>>();

    let mut changed = HashSet::new();

    for entry in current {
        if let Some(previous) = previous_by_id.get(&entry.id) {
            if !same_menu_row(previous, entry) {
                changed.insert(entry.id);
            }

            changed.extend(changed_menu_rows(&previous.children, &entry.children));
        }
    }

    changed
}

fn same_menu_row(a: &MenuEntry, b: &MenuEntry) -> bool {
    a.id == b.id
        && a.label == b.label
        && a.icon == b.icon
        && a.icon_data == b.icon_data
        && a.shortcut == b.shortcut
        && a.enabled == b.enabled
        && a.separator == b.separator
        && a.toggle == b.toggle
        && (a.submenu || !a.children.is_empty()) == (b.submenu || !b.children.is_empty())
}

#[derive(Clone)]
pub struct Controls {
    commands: UnboundedSender<Command>,
}

impl Controls {
    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }
}

pub struct Backend {
    controls: Controls,
    items: Option<UnboundedReceiver<TrayUpdate>>,
    _task: Task,
}

impl Backend {
    pub fn start(availability: AvailabilityPublisher) -> Self {
        let (command_sender, mut commands) = mpsc::unbounded_channel();
        let (item_events, items) = mpsc::unbounded_channel();
        let task = Task::spawn(async move {
            let mut service = Service::exported(dbus::TrayBus::host_name(), availability.clone());

            loop {
                let connection = tokio::select! {
                    connection = service.connect() => connection,
                    _ = item_events.closed() => return,
                    command = commands.recv() => {
                        if command.is_none() {
                            return;
                        }

                        continue;
                    }
                };
                let result = service
                    .run(async {
                        let mut driver =
                            probe(TrayDriver::connect(connection, item_events.clone())).await?;
                        availability.set(Availability::Available);

                        let result = driver.run(&mut commands).await;
                        driver.bus.close().await;

                        result
                    })
                    .await;

                if result.is_ok() {
                    return;
                }

                let _ = item_events.send(TrayUpdate {
                    items: Vec::new(),
                    changed_items: HashSet::new(),
                    changed_menus: HashSet::new(),
                    changed_titles: HashSet::new(),
                    changed_menu_rows: HashMap::new(),
                });
            }
        });

        Self {
            controls: Controls {
                commands: command_sender,
            },
            items: Some(items),
            _task: task,
        }
    }

    pub fn controls(&self) -> Controls {
        self.controls.clone()
    }

    pub fn take_items(&mut self) -> UnboundedReceiver<TrayUpdate> {
        self.items.take().expect("tray items taken once")
    }
}

#[derive(Debug)]
enum DriverEvent {
    Disconnected,
    ItemRegistered(String),
    ItemUnregistered(String),
    ItemChanged(String),
    ItemRead(String, Option<Box<dbus::ObservedItem>>),
    MenuChanged(String),
    MenuRead(String, dbus::MenuPath, Vec<MenuEntry>),
    NameOwnerChanged { name: String, has_owner: bool },
}

struct TrayDriver {
    bus: dbus::TrayBus,
    item_events: UnboundedSender<TrayUpdate>,
    event_sender: UnboundedSender<DriverEvent>,
    backend_events: UnboundedReceiver<DriverEvent>,
    // Mutable D-Bus state and the last emitted snapshot serve different purposes:
    // compare against the emitted one to update only changed UI rows/icons.
    last_items: Option<Vec<Item>>,
    last_published_items: Option<Vec<Item>>,
    last_targets: Vec<dbus::ListenerTarget>,
    // Coalesce signals during an in-flight read without losing a later change.
    // The *_again sets schedule one fresh read after the current one completes.
    item_reads: HashSet<String>,
    item_read_again: HashSet<String>,
    menu_reads: HashSet<String>,
    menu_read_again: HashSet<String>,
    listeners: dbus::SignalListeners,
}

impl TrayDriver {
    async fn connect(
        connection: zbus::Connection,
        item_events: UnboundedSender<TrayUpdate>,
    ) -> zbus::Result<Self> {
        let (event_sender, backend_events) = mpsc::unbounded_channel();
        let bus = dbus::TrayBus::connect(connection, &event_sender).await?;
        let listeners = dbus::SignalListeners::start(&bus, &event_sender);

        Ok(Self {
            bus,
            item_events,
            event_sender,
            backend_events,
            last_items: None,
            last_published_items: None,
            last_targets: Vec::new(),
            item_reads: HashSet::new(),
            item_read_again: HashSet::new(),
            menu_reads: HashSet::new(),
            menu_read_again: HashSet::new(),
            listeners,
        })
    }

    async fn run(&mut self, commands: &mut UnboundedReceiver<Command>) -> Result<(), Availability> {
        self.reconcile().await;

        loop {
            tokio::select! {
                _ = self.item_events.closed() => return Ok(()),
                command = commands.recv() => {
                    let Some(command) = command else {
                        return Ok(());
                    };

                    self.bus.execute(command).await;
                }
                event = self.backend_events.recv() => {
                    let Some(event) = event else {
                        return Err(Availability::Failed(ProbeError::Connect));
                    };

                    if !self.handle_event(event).await {
                        return Err(Availability::Failed(ProbeError::Connect));
                    }
                }
            }
        }
    }

    async fn handle_event(&mut self, event: DriverEvent) -> bool {
        match event {
            DriverEvent::Disconnected => false,
            DriverEvent::ItemRegistered(id) => self.register_item(id),
            DriverEvent::ItemUnregistered(id) => self.unregister_item(&id),
            DriverEvent::ItemChanged(id) => self.start_item_read(id),
            DriverEvent::ItemRead(id, item) => self.apply_item_read(id, item.map(|item| *item)),
            DriverEvent::MenuChanged(id) => self.start_menu_read(id),
            DriverEvent::MenuRead(id, path, menu) => self.apply_menu_read(id, path, menu),
            DriverEvent::NameOwnerChanged { name, has_owner } => {
                if self.bus.handle_name_owner_changed(&name, has_owner).await {
                    self.reconcile().await
                } else {
                    true
                }
            }
        }
    }

    fn start_item_read(&mut self, id: String) -> bool {
        if !self.last_targets.iter().any(|target| target.item_id == id) {
            return true;
        }

        if !self.item_reads.insert(id.clone()) {
            self.item_read_again.insert(id.clone());
            return true;
        }

        let bus = self.bus.clone();
        let events = self.event_sender.clone();
        let request_id = id.clone();
        let cached_item = self
            .last_items
            .as_ref()
            .and_then(|items| items.iter().find(|item| item.id == id))
            .cloned();

        runtime::spawn(async move {
            let observed = bus.read_item(&request_id, cached_item).await;
            let _ = events.send(DriverEvent::ItemRead(request_id, observed.map(Box::new)));
        });

        true
    }

    fn register_item(&mut self, id: String) -> bool {
        if self.last_targets.iter().any(|target| target.item_id == id) {
            return true;
        }

        self.last_targets.push(dbus::ListenerTarget {
            item_id: id.clone(),
            menu_path: None,
        });

        self.listeners
            .replace_items(&self.bus, self.last_targets.clone(), &self.event_sender);

        self.start_item_read(id)
    }

    fn unregister_item(&mut self, id: &str) -> bool {
        let registered = self.last_targets.iter().any(|target| target.item_id == id);

        if !registered {
            return true;
        }

        self.last_targets.retain(|target| target.item_id != id);

        if let Some(items) = self.last_items.as_mut() {
            items.retain(|item| item.id != id);
        }

        self.item_read_again.remove(id);
        self.menu_read_again.remove(id);
        self.publish_items();
        self.listeners
            .replace_items(&self.bus, self.last_targets.clone(), &self.event_sender);

        true
    }

    fn apply_item_read(&mut self, id: String, observed: Option<dbus::ObservedItem>) -> bool {
        self.item_reads.remove(&id);

        let Some(target_position) = self
            .last_targets
            .iter()
            .position(|target| target.item_id == id)
        else {
            self.item_read_again.remove(&id);
            return true;
        };

        let items = self.last_items.get_or_insert_with(Vec::new);
        let old_position = items.iter().position(|item| item.id == id);

        items.retain(|item| item.id != id);

        let target = match observed {
            Some(observed) => {
                let position = old_position.unwrap_or(items.len()).min(items.len());

                items.insert(position, observed.item);

                observed.target
            }
            None => dbus::ListenerTarget {
                item_id: id.to_owned(),
                menu_path: None,
            },
        };

        self.last_targets.retain(|existing| existing.item_id != id);
        self.last_targets
            .insert(target_position.min(self.last_targets.len()), target);
        self.publish_items();
        self.listeners
            .replace_items(&self.bus, self.last_targets.clone(), &self.event_sender);

        if self.item_read_again.remove(&id) {
            let _ = self.event_sender.send(DriverEvent::ItemChanged(id));
        }

        true
    }

    fn start_menu_read(&mut self, id: String) -> bool {
        if !self.menu_reads.insert(id.clone()) {
            self.menu_read_again.insert(id.clone());
            return true;
        }

        let Some(path) = self
            .last_targets
            .iter()
            .find(|target| target.item_id == id)
            .and_then(|target| target.menu_path.as_ref())
            .cloned()
        else {
            self.menu_reads.remove(&id);
            return true;
        };

        let bus = self.bus.clone();
        let events = self.event_sender.clone();
        let request_id = id.clone();
        let request_path = path.clone();

        runtime::spawn(async move {
            let menu = bus.read_menu(&request_id, &path).await;
            let _ = events.send(DriverEvent::MenuRead(request_id, request_path, menu));
        });

        true
    }

    fn apply_menu_read(&mut self, id: String, path: dbus::MenuPath, menu: Vec<MenuEntry>) -> bool {
        self.menu_reads.remove(&id);

        let still_current = self
            .last_targets
            .iter()
            .any(|target| target.item_id == id && target.menu_path.as_ref() == Some(&path));

        if still_current
            && let Some(item) = self
                .last_items
                .as_mut()
                .and_then(|items| items.iter_mut().find(|item| item.id == id))
            && item.menu != menu
        {
            item.menu = menu;
            self.publish_items();
        }

        if self.menu_read_again.remove(&id) {
            let _ = self.event_sender.send(DriverEvent::MenuChanged(id));
        }

        true
    }

    fn publish_items(&mut self) {
        let Some(items) = self.last_items.as_ref() else {
            return;
        };
        let mut update = TrayUpdate {
            items: items.clone(),
            changed_items: HashSet::new(),
            changed_menus: HashSet::new(),
            changed_titles: HashSet::new(),
            changed_menu_rows: HashMap::new(),
        };

        let previous = self.last_published_items.as_ref();

        for item in &update.items {
            match previous.and_then(|items| items.iter().find(|old| old.id == item.id)) {
                Some(old) if old == item => {}
                Some(old) => {
                    update.changed_items.insert(item.id.clone());

                    if old.menu != item.menu {
                        update.changed_menus.insert(item.id.clone());
                        update
                            .changed_menu_rows
                            .insert(item.id.clone(), changed_menu_rows(&old.menu, &item.menu));
                    }

                    if old.title != item.title {
                        update.changed_titles.insert(item.id.clone());
                    }
                }
                None => {
                    update.changed_items.insert(item.id.clone());
                    update.changed_menus.insert(item.id.clone());
                    update.changed_titles.insert(item.id.clone());
                }
            }
        }

        if let Some(previous) = previous {
            for item in previous {
                if !update.items.iter().any(|current| current.id == item.id) {
                    update.changed_items.insert(item.id.clone());
                    update.changed_menus.insert(item.id.clone());
                    update.changed_titles.insert(item.id.clone());
                }
            }
        }

        if previous.is_some_and(|previous| previous == &update.items) {
            return;
        }

        self.last_published_items = Some(update.items.clone());

        if self.item_events.send(update).is_err() {
            tracing::warn!("tray update receiver dropped");
        }
    }

    async fn reconcile(&mut self) -> bool {
        self.bus.reconcile_watcher().await;

        let dbus::TraySnapshot { items, targets } = self.bus.read_snapshot().await;

        self.last_items = Some(items);
        self.last_targets = targets;
        self.publish_items();

        self.listeners
            .replace_items(&self.bus, self.last_targets.clone(), &self.event_sender);

        true
    }
}

mod dbus {
    use super::{Command, DriverEvent, Icon, Item, MenuEntry, Pixmap};
    use crate::runtime::Task;
    use futures_util::{StreamExt, future::join_all, stream::select_all};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc::UnboundedSender;
    use zbus::Connection;
    use zbus::message::Header;
    use zbus::zvariant::{OwnedObjectPath, OwnedValue};

    const WATCHER: &str = "org.kde.StatusNotifierWatcher";
    const WATCHER_PATH: &str = "/StatusNotifierWatcher";

    type RawMenuProperties = HashMap<String, OwnedValue>;
    type MenuLayout = (u32, (i32, RawMenuProperties, Vec<OwnedValue>));

    #[derive(Clone)]
    pub struct TrayBus {
        connection: Connection,
        owned_items: RegisteredItems,
        host_name: String,
        registered_with: Arc<Mutex<Option<String>>>,
    }

    impl TrayBus {
        pub fn host_name() -> String {
            format!("org.freedesktop.StatusNotifierHost-{}", std::process::id())
        }

        pub async fn connect(
            connection: Connection,
            events: &UnboundedSender<DriverEvent>,
        ) -> zbus::Result<Self> {
            let owned_items = RegisteredItems::default();
            let host_name = Self::host_name();
            let bus = Self {
                connection,
                owned_items,
                host_name,
                registered_with: Arc::new(Mutex::new(None)),
            };

            bus.register_local_watcher(events).await?;
            bus.register_host().await?;

            Ok(bus)
        }

        pub async fn close(&self) {
            let _ = self.connection.release_name(self.host_name.as_str()).await;
            let _ = self.connection.release_name(WATCHER).await;
            let _ = self
                .connection
                .object_server()
                .remove::<LocalWatcher, _>(WATCHER_PATH)
                .await;
        }

        pub async fn reconcile_watcher(&self) {
            let mut owner = self.watcher_owner().await;

            if owner.is_none() && self.claim_watcher_name().await {
                owner = self.watcher_owner().await;
            }

            let changed = self.registered_with.lock().unwrap().as_ref() != owner.as_ref();

            if !changed {
                return;
            }

            if owner.is_some() {
                self.register_with_watcher().await;
            }

            *self.registered_with.lock().unwrap() = owner;
        }

        pub async fn handle_name_owner_changed(&self, name: &str, has_owner: bool) -> bool {
            if !has_owner {
                let removed = self.owned_items.remove_owner(name);

                self.emit_unregistered(removed).await;
            }

            name == WATCHER
        }

        pub async fn read_snapshot(&self) -> TraySnapshot {
            let ids = self.registered_items().await;
            let mut items = Vec::with_capacity(ids.len());
            let mut targets = Vec::with_capacity(ids.len());

            let observations = join_all(
                ids.iter()
                    .map(|id| async move { self.read_item(id, None).await }),
            )
            .await;

            for (id, observed) in ids.into_iter().zip(observations) {
                match observed {
                    Some(observed) => {
                        items.push(observed.item);
                        targets.push(observed.target);
                    }
                    None => targets.push(ListenerTarget {
                        item_id: id,
                        menu_path: None,
                    }),
                }
            }

            TraySnapshot { items, targets }
        }

        pub async fn execute(&self, command: Command) {
            match command {
                Command::Activate(id) => self.call_item_action(&id, ItemAction::Activate).await,
                Command::SecondaryActivate(id) => {
                    self.call_item_action(&id, ItemAction::SecondaryActivate)
                        .await
                }
                Command::ContextMenu(id) => {
                    self.call_item_action(&id, ItemAction::ContextMenu).await
                }
                Command::OpenMenu(id, parent) => self.open_menu(&id, parent).await,
                Command::ClickMenu(id, entry) => self.click_menu_entry(&id, entry).await,
                Command::Scroll(id, delta, horizontal) => {
                    self.scroll_item(&id, delta, horizontal).await
                }
            }
        }

        pub async fn watch_owner_changes(self, events: UnboundedSender<DriverEvent>) {
            let Ok(proxy) = zbus::fdo::DBusProxy::new(&self.connection).await else {
                let _ = events.send(DriverEvent::Disconnected);

                return;
            };

            let Ok(mut signals) = proxy.receive_name_owner_changed().await else {
                let _ = events.send(DriverEvent::Disconnected);

                return;
            };

            while let Some(signal) = signals.next().await {
                let Ok(args) = signal.args() else {
                    continue;
                };
                let event = DriverEvent::NameOwnerChanged {
                    name: args.name().to_string(),
                    has_owner: args.new_owner().is_some(),
                };

                if events.send(event).is_err() {
                    break;
                }
            }

            let _ = events.send(DriverEvent::Disconnected);
        }

        pub async fn watch_watcher(self, events: UnboundedSender<DriverEvent>) {
            let Ok(proxy) = StatusNotifierWatcherProxy::new(&self.connection).await else {
                return;
            };
            let (Ok(registered), Ok(unregistered)) = (
                proxy.receive_status_notifier_item_registered().await,
                proxy.receive_status_notifier_item_unregistered().await,
            ) else {
                return;
            };

            let registered = registered
                .filter_map(|signal| async move {
                    signal
                        .args()
                        .ok()
                        .map(|args| DriverEvent::ItemRegistered(args.id().to_string()))
                })
                .boxed();

            let unregistered = unregistered
                .filter_map(|signal| async move {
                    signal
                        .args()
                        .ok()
                        .map(|args| DriverEvent::ItemUnregistered(args.id().to_string()))
                })
                .boxed();

            forward_event_stream(select_all(vec![registered, unregistered]), events).await;
        }

        pub async fn watch_item(self, id: String, events: UnboundedSender<DriverEvent>) {
            let Ok(proxy) = self.item_proxy(&id).await else {
                return;
            };
            let (
                Ok(new_title),
                Ok(new_icon),
                Ok(new_attention),
                Ok(new_overlay),
                Ok(new_tooltip),
                Ok(new_status),
            ) = (
                proxy.receive_new_title().await,
                proxy.receive_new_icon().await,
                proxy.receive_new_attention_icon().await,
                proxy.receive_new_overlay_icon().await,
                proxy.receive_new_tool_tip().await,
                proxy.receive_new_status().await,
            )
            else {
                return;
            };

            let signals = select_all(vec![
                new_title.map(|_| ()).boxed(),
                new_icon.map(|_| ()).boxed(),
                new_attention.map(|_| ()).boxed(),
                new_overlay.map(|_| ()).boxed(),
                new_tooltip.map(|_| ()).boxed(),
                new_status.map(|_| ()).boxed(),
            ]);

            let _ = events.send(DriverEvent::ItemChanged(id.clone()));
            let event_id = id.clone();

            forward_signal_stream(signals, events, move || {
                DriverEvent::ItemChanged(event_id.clone())
            })
            .await;
        }

        async fn watch_menu(
            self,
            id: String,
            path: MenuPath,
            events: UnboundedSender<DriverEvent>,
        ) {
            let Ok(proxy) = self.menu_proxy(&id, &path).await else {
                return;
            };

            if let Ok(layout_updated) = proxy.receive_layout_updated().await {
                let signals = layout_updated.map(|_| ());
                let _ = events.send(DriverEvent::MenuChanged(id.clone()));
                let event_id = id.clone();

                forward_signal_stream(signals, events, move || {
                    DriverEvent::MenuChanged(event_id.clone())
                })
                .await;
            }
        }

        async fn register_local_watcher(
            &self,
            events: &UnboundedSender<DriverEvent>,
        ) -> zbus::Result<()> {
            let bus = zbus::fdo::DBusProxy::new(&self.connection).await?;
            let owner = bus.name_has_owner(WATCHER.try_into().unwrap()).await?;

            self.connection
                .object_server()
                .at(
                    WATCHER_PATH,
                    LocalWatcher {
                        items: self.owned_items.clone(),
                        wake: events.clone(),
                    },
                )
                .await?;

            if !owner {
                match self.connection.request_name(WATCHER).await {
                    Ok(()) | Err(zbus::Error::NameTaken) => {}
                    Err(error) => return Err(error),
                }
            }

            Ok(())
        }

        async fn register_host(&self) -> zbus::Result<()> {
            self.connection.request_name(self.host_name.as_str()).await
        }

        async fn watcher_owner(&self) -> Option<String> {
            zbus::fdo::DBusProxy::new(&self.connection)
                .await
                .ok()?
                .get_name_owner(WATCHER.try_into().ok()?)
                .await
                .ok()
                .map(|owner| owner.to_string())
        }

        async fn claim_watcher_name(&self) -> bool {
            self.connection.request_name(WATCHER).await.is_ok()
        }

        async fn register_with_watcher(&self) {
            if let Ok(proxy) = StatusNotifierWatcherProxy::new(&self.connection).await {
                let _ = proxy.register_status_notifier_host(&self.host_name).await;
            }
        }

        async fn emit_unregistered(&self, ids: impl IntoIterator<Item = String>) {
            let Ok(emitter) =
                zbus::object_server::SignalEmitter::new(&self.connection, WATCHER_PATH)
            else {
                return;
            };

            for id in ids {
                let _ = LocalWatcher::status_notifier_item_unregistered(&emitter, &id).await;
            }
        }

        async fn registered_items(&self) -> Vec<String> {
            let Ok(proxy) = StatusNotifierWatcherProxy::new(&self.connection).await else {
                return Vec::new();
            };

            proxy
                .registered_status_notifier_items()
                .await
                .unwrap_or_default()
        }

        pub async fn read_item(&self, id: &str, cached_item: Option<Item>) -> Option<ObservedItem> {
            let proxy = self.item_proxy(id).await.ok()?;
            let status = ItemStatus::from(proxy.status().await.unwrap_or_default());

            if status == ItemStatus::Passive {
                return None;
            }

            let title = proxy.title().await.unwrap_or_default();
            let (icons, icon_theme_path, tooltip, menu_path, item_is_menu) = tokio::join!(
                self.read_item_icons(&proxy, status),
                proxy.icon_theme_path(),
                self.read_item_tooltip(&proxy, &title),
                proxy.menu(),
                proxy.item_is_menu(),
            );

            let (icon, overlay) = icons;
            let icon_theme_path = icon_theme_path.ok().filter(|path| !path.is_empty());
            let menu_path = menu_path
                .ok()
                .and_then(|path| MenuPath::try_from(path).ok());
            let menu = match &menu_path {
                Some(path)
                    if cached_item.as_ref().is_some_and(|cached| {
                        cached.menu_path.as_deref() == Some(path.as_ref())
                    }) =>
                {
                    cached_item.as_ref().unwrap().menu.clone()
                }
                Some(path) => self.read_menu(id, path).await,
                None => Vec::new(),
            };

            let item_is_menu = item_is_menu.unwrap_or(false);

            Some(ObservedItem {
                target: ListenerTarget {
                    item_id: id.to_owned(),
                    menu_path: menu_path.clone(),
                },
                item: Item {
                    id: id.to_owned(),
                    title,
                    tooltip,
                    icon,
                    overlay,
                    icon_theme_path,
                    menu,
                    menu_path: menu_path.map(MenuPath::into_string),
                    item_is_menu,
                },
            })
        }

        pub async fn read_menu(&self, id: &str, path: &MenuPath) -> Vec<MenuEntry> {
            let Ok(proxy) = self.menu_proxy(id, path).await else {
                return Vec::new();
            };
            let Ok((_, (_, _, children))) = proxy.get_layout(0, -1, Vec::new()).await else {
                return Vec::new();
            };

            children
                .into_iter()
                .filter_map(|child| RawMenuEntry::try_from(child).ok()?.into_entry())
                .collect::<Vec<_>>()
        }

        async fn read_item_icons(
            &self,
            proxy: &StatusNotifierItemProxy<'_>,
            status: ItemStatus,
        ) -> (Icon, Option<Icon>) {
            let visible = if status == ItemStatus::NeedsAttention {
                self.read_icon(proxy, ItemIconKind::Attention)
            } else {
                self.read_icon(proxy, ItemIconKind::Regular)
            };

            let overlay = self.read_icon(proxy, ItemIconKind::Overlay);
            let (mut visible, overlay) = tokio::join!(visible, overlay);

            if visible.is_empty() {
                visible = self.read_icon(proxy, ItemIconKind::Regular).await;
            }

            let overlay = (!overlay.is_empty()).then_some(overlay);

            (visible, overlay)
        }

        async fn read_icon(&self, proxy: &StatusNotifierItemProxy<'_>, kind: ItemIconKind) -> Icon {
            let (name, pixmaps) = match kind {
                ItemIconKind::Regular => tokio::join!(proxy.icon_name(), proxy.icon_pixmap()),
                ItemIconKind::Attention => {
                    tokio::join!(proxy.attention_icon_name(), proxy.attention_icon_pixmap())
                }
                ItemIconKind::Overlay => {
                    tokio::join!(proxy.overlay_icon_name(), proxy.overlay_icon_pixmap())
                }
            };

            Icon::from_dbus(name.unwrap_or_default(), pixmaps)
        }

        async fn read_item_tooltip(
            &self,
            proxy: &StatusNotifierItemProxy<'_>,
            fallback: &str,
        ) -> String {
            proxy
                .tool_tip()
                .await
                .map(
                    |(_, _, title, body)| match (title.is_empty(), body.is_empty()) {
                        (true, false) => body,
                        (false, false) => format!("{title}\n{body}"),
                        _ => title,
                    },
                )
                .unwrap_or_else(|_| fallback.to_owned())
        }

        async fn call_item_action(&self, id: &str, action: ItemAction) {
            let Ok(proxy) = self.item_proxy(id).await else {
                return;
            };

            let result = match action {
                ItemAction::Activate => proxy.activate(0, 0).await,
                ItemAction::SecondaryActivate => proxy.secondary_activate(0, 0).await,
                ItemAction::ContextMenu => proxy.context_menu(0, 0).await,
            };

            let _ = result;
        }

        async fn open_menu(&self, id: &str, parent: i32) {
            if let Some(proxy) = self.item_menu_proxy(id).await {
                let _ = proxy.about_to_show(parent).await;
            }
        }

        async fn click_menu_entry(&self, id: &str, entry: i32) {
            if let Some(proxy) = self.item_menu_proxy(id).await {
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|time| time.as_millis() as u32)
                    .unwrap_or(0);

                let _ = proxy
                    .event(entry, "clicked", OwnedValue::from(0i32), timestamp)
                    .await;
            }
        }

        async fn scroll_item(&self, id: &str, delta: i32, horizontal: bool) {
            let Ok(proxy) = self.item_proxy(id).await else {
                return;
            };
            let orientation = if horizontal { "horizontal" } else { "vertical" };
            let _ = proxy.scroll(delta, orientation).await;
        }

        async fn item_proxy(&self, id: &str) -> zbus::Result<StatusNotifierItemProxy<'static>> {
            let address = ItemAddress::from(id);

            StatusNotifierItemProxy::builder(&self.connection)
                .destination(address.service.to_owned())?
                .path(address.path.to_owned())?
                .build()
                .await
        }

        async fn menu_proxy(
            &self,
            id: &str,
            path: &MenuPath,
        ) -> zbus::Result<DBusMenuProxy<'static>> {
            let address = ItemAddress::from(id);

            DBusMenuProxy::builder(&self.connection)
                .destination(address.service.to_owned())?
                .path(path.as_ref().to_owned())?
                .build()
                .await
        }

        async fn item_menu_proxy(&self, id: &str) -> Option<DBusMenuProxy<'static>> {
            let path = self
                .item_proxy(id)
                .await
                .ok()?
                .menu()
                .await
                .ok()
                .and_then(|path| MenuPath::try_from(path).ok())?;

            self.menu_proxy(id, &path).await.ok()
        }
    }

    pub struct TraySnapshot {
        pub items: Vec<Item>,
        pub targets: Vec<ListenerTarget>,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct ListenerTarget {
        pub item_id: String,
        pub menu_path: Option<MenuPath>,
    }

    #[derive(Debug)]
    pub struct ObservedItem {
        pub item: Item,
        pub target: ListenerTarget,
    }

    pub struct SignalListeners {
        // Task owns cancellation: removing a tray item or replacing its menu
        // drops the old subscriptions even if the corresponding stream is idle.
        _fixed: Vec<Task>,
        dynamic: HashMap<String, ItemListeners>,
    }

    struct ItemListeners {
        _item: Task,
        menu: Option<(MenuPath, Task)>,
    }

    impl SignalListeners {
        pub fn start(bus: &TrayBus, events: &UnboundedSender<DriverEvent>) -> Self {
            Self {
                _fixed: vec![
                    Task::spawn(bus.clone().watch_owner_changes(events.clone())),
                    Task::spawn(bus.clone().watch_watcher(events.clone())),
                ],
                dynamic: HashMap::new(),
            }
        }

        pub fn replace_items(
            &mut self,
            bus: &TrayBus,
            targets: Vec<ListenerTarget>,
            events: &UnboundedSender<DriverEvent>,
        ) {
            let desired = targets
                .iter()
                .map(|target| target.item_id.as_str())
                .collect::<std::collections::HashSet<_>>();

            self.dynamic.retain(|id, _| desired.contains(id.as_str()));

            for target in &targets {
                let listeners = self
                    .dynamic
                    .entry(target.item_id.clone())
                    .or_insert_with(|| ItemListeners {
                        _item: Task::spawn(
                            bus.clone()
                                .watch_item(target.item_id.clone(), events.clone()),
                        ),
                        menu: None,
                    });

                let current_path = listeners.menu.as_ref().map(|(path, _)| path);

                if current_path != target.menu_path.as_ref() {
                    listeners.menu = target.menu_path.clone().map(|path| {
                        let task = Task::spawn(bus.clone().watch_menu(
                            target.item_id.clone(),
                            path.clone(),
                            events.clone(),
                        ));

                        (path, task)
                    });
                }
            }
        }
    }

    #[derive(Clone, Default)]
    struct RegisteredItems(Arc<Mutex<Vec<RegisteredItem>>>);

    impl RegisteredItems {
        fn ids(&self) -> Vec<String> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .map(|item| item.id.clone())
                .collect()
        }

        fn register(&self, id: String, owner: String) -> bool {
            let mut entries = self.0.lock().unwrap();

            if entries.iter().any(|item| item.id == id) {
                return false;
            }

            entries.push(RegisteredItem { id, owner });

            true
        }

        fn remove_owner(&self, name: &str) -> Vec<String> {
            let mut entries = self.0.lock().unwrap();
            let removed = entries
                .iter()
                .filter(|item| item.belongs_to(name))
                .map(|item| item.id.clone())
                .collect();

            entries.retain(|item| !item.belongs_to(name));

            removed
        }
    }

    struct RegisteredItem {
        id: String,
        owner: String,
    }

    impl RegisteredItem {
        fn belongs_to(&self, owner: &str) -> bool {
            self.owner == owner || ItemAddress::from(self.id.as_str()).service == owner
        }
    }

    #[derive(Clone, Copy)]
    struct ItemAddress<'a> {
        service: &'a str,
        path: &'a str,
    }

    impl<'a> From<&'a str> for ItemAddress<'a> {
        fn from(id: &'a str) -> Self {
            match id.find('/') {
                Some(index) => Self {
                    service: &id[..index],
                    path: &id[index..],
                },
                None => Self {
                    service: id,
                    path: "/StatusNotifierItem",
                },
            }
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct MenuPath(String);

    impl MenuPath {
        fn into_string(self) -> String {
            self.0
        }
    }

    impl TryFrom<OwnedObjectPath> for MenuPath {
        type Error = ();

        fn try_from(path: OwnedObjectPath) -> Result<Self, Self::Error> {
            let path = path.to_string();

            if matches!(path.as_str(), "/" | "/NO_DBUSMENU") {
                Err(())
            } else {
                Ok(Self(path))
            }
        }
    }

    impl AsRef<str> for MenuPath {
        fn as_ref(&self) -> &str {
            &self.0
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ItemStatus {
        Passive,
        NeedsAttention,
        Other,
    }

    impl From<String> for ItemStatus {
        fn from(status: String) -> Self {
            match status.as_str() {
                "Passive" => Self::Passive,
                "NeedsAttention" => Self::NeedsAttention,
                _ => Self::Other,
            }
        }
    }

    #[derive(Clone, Copy)]
    enum ItemIconKind {
        Regular,
        Attention,
        Overlay,
    }

    #[derive(Clone, Copy)]
    enum ItemAction {
        Activate,
        SecondaryActivate,
        ContextMenu,
    }

    struct MenuProperties(RawMenuProperties);

    impl MenuProperties {
        fn value<T: TryFrom<OwnedValue>>(&self, key: &str) -> Option<T> {
            self.0.get(key)?.try_clone().ok()?.try_into().ok()
        }

        fn visible(&self) -> bool {
            self.value("visible").unwrap_or(true)
        }

        fn label(&self) -> String {
            MenuLabel::from(self.value::<String>("label").unwrap_or_default()).into_string()
        }

        fn icon_name(&self) -> String {
            self.value("icon-name").unwrap_or_default()
        }

        fn icon_data(&self) -> Option<Vec<u8>> {
            self.value("icon-data")
        }

        fn shortcut(&self) -> Option<String> {
            self.value::<Vec<Vec<String>>>("shortcut")
                .and_then(|shortcuts| shortcuts.into_iter().next())
                .map(|keys| keys.join("+"))
        }

        fn enabled(&self) -> bool {
            self.value("enabled").unwrap_or(true)
        }

        fn separator(&self) -> bool {
            self.value::<String>("type").as_deref() == Some("separator")
        }

        fn toggle(&self) -> Option<MenuToggle> {
            let toggle_type = self.value::<String>("toggle-type")?;

            (!toggle_type.is_empty()).then(|| MenuToggle {
                radio: toggle_type == "radio",
                checked: self.value::<i32>("toggle-state") == Some(1),
            })
        }

        fn submenu(&self) -> bool {
            self.value::<String>("children-display").as_deref() == Some("submenu")
        }
    }

    struct MenuToggle {
        radio: bool,
        checked: bool,
    }

    impl MenuToggle {
        fn into_pair(self) -> (bool, bool) {
            (self.radio, self.checked)
        }
    }

    struct MenuLabel(String);

    impl MenuLabel {
        fn into_string(self) -> String {
            self.0
        }
    }

    impl From<String> for MenuLabel {
        fn from(label: String) -> Self {
            let mut chars = label.chars();
            let mut normalized = String::with_capacity(label.len());

            while let Some(character) = chars.next() {
                if character != '_' {
                    normalized.push(character);
                    continue;
                }

                if chars.clone().next() == Some('_') {
                    chars.next();
                    normalized.push('_');
                }
            }

            Self(normalized)
        }
    }

    struct RawMenuEntry {
        id: i32,
        properties: MenuProperties,
        children: Vec<OwnedValue>,
    }

    impl RawMenuEntry {
        fn into_entry(self) -> Option<MenuEntry> {
            if !self.properties.visible() {
                return None;
            }

            Some(MenuEntry {
                id: self.id,
                label: self.properties.label(),
                icon: self.properties.icon_name(),
                icon_data: self.properties.icon_data(),
                shortcut: self.properties.shortcut(),
                enabled: self.properties.enabled(),
                separator: self.properties.separator(),
                toggle: self.properties.toggle().map(MenuToggle::into_pair),
                children: self
                    .children
                    .into_iter()
                    .filter_map(|child| RawMenuEntry::try_from(child).ok()?.into_entry())
                    .collect(),
                submenu: self.properties.submenu(),
            })
        }
    }

    impl TryFrom<OwnedValue> for RawMenuEntry {
        type Error = ();

        fn try_from(value: OwnedValue) -> Result<Self, Self::Error> {
            let (id, properties, children): (i32, RawMenuProperties, Vec<OwnedValue>) =
                value.try_into().map_err(|_| ())?;

            Ok(Self {
                id,
                properties: MenuProperties(properties),
                children,
            })
        }
    }

    impl Icon {
        fn from_dbus(name: String, pixmaps: Result<Vec<(i32, i32, Vec<u8>)>, zbus::Error>) -> Self {
            Self {
                name,
                pixmaps: pixmaps
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|(width, height, argb)| Pixmap::from_argb(width, height, argb))
                    .collect(),
            }
        }

        fn is_empty(&self) -> bool {
            self.name.is_empty() && self.pixmaps.is_empty()
        }
    }

    impl Pixmap {
        fn from_argb(width: i32, height: i32, argb: Vec<u8>) -> Option<Self> {
            if width <= 0 || height <= 0 {
                return None;
            }

            let expected_len = (width as usize)
                .saturating_mul(height as usize)
                .saturating_mul(4);

            (argb.len() == expected_len).then_some(Self {
                width,
                height,
                argb,
            })
        }
    }

    struct LocalWatcher {
        items: RegisteredItems,
        wake: UnboundedSender<DriverEvent>,
    }

    // Tray clients invoke this exported interface over D-Bus. Properties and
    // signal declarations remain part of introspection even without Rust callers.
    #[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
    impl LocalWatcher {
        #[zbus(property)]
        fn registered_status_notifier_items(&self) -> Vec<String> {
            self.items.ids()
        }

        #[zbus(property)]
        fn is_status_notifier_host_registered(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn protocol_version(&self) -> i32 {
            0
        }

        async fn register_status_notifier_item(
            &self,
            service: String,
            #[zbus(header)] header: Header<'_>,
            #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        ) -> zbus::fdo::Result<()> {
            let sender = header
                .sender()
                .ok_or_else(|| zbus::fdo::Error::Failed("Missing sender".into()))?;
            let id = if service.starts_with('/') {
                format!("{sender}{service}")
            } else {
                service
            };

            if !self.items.register(id.clone(), sender.to_string()) {
                return Ok(());
            }

            Self::status_notifier_item_registered(&emitter, &id).await?;

            let _ = self.wake.send(DriverEvent::ItemRegistered(id));

            Ok(())
        }

        async fn register_status_notifier_host(
            &self,
            _service: String,
            #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        ) -> zbus::fdo::Result<()> {
            Self::status_notifier_host_registered(&emitter).await?;

            Ok(())
        }

        #[zbus(signal)]
        async fn status_notifier_item_registered(
            emitter: &zbus::object_server::SignalEmitter<'_>,
            id: &str,
        ) -> zbus::Result<()>;

        #[zbus(signal)]
        async fn status_notifier_item_unregistered(
            emitter: &zbus::object_server::SignalEmitter<'_>,
            id: &str,
        ) -> zbus::Result<()>;

        #[zbus(signal)]
        async fn status_notifier_host_registered(
            emitter: &zbus::object_server::SignalEmitter<'_>,
        ) -> zbus::Result<()>;

        #[zbus(signal)]
        async fn status_notifier_host_unregistered(
            emitter: &zbus::object_server::SignalEmitter<'_>,
        ) -> zbus::Result<()>;
    }

    #[zbus::proxy(
        interface = "org.kde.StatusNotifierWatcher",
        default_service = "org.kde.StatusNotifierWatcher",
        default_path = "/StatusNotifierWatcher"
    )]
    trait StatusNotifierWatcher {
        fn register_status_notifier_host(&self, service: &str) -> zbus::Result<()>;

        fn register_status_notifier_item(&self, service: &str) -> zbus::Result<()>;

        #[zbus(property)]
        fn registered_status_notifier_items(&self) -> zbus::Result<Vec<String>>;

        #[zbus(signal)]
        fn status_notifier_item_registered(&self, id: &str) -> zbus::Result<()>;

        #[zbus(signal)]
        fn status_notifier_item_unregistered(&self, id: &str) -> zbus::Result<()>;
    }

    #[zbus::proxy(interface = "org.kde.StatusNotifierItem")]
    trait StatusNotifierItem {
        fn activate(&self, x: i32, y: i32) -> zbus::Result<()>;

        fn secondary_activate(&self, x: i32, y: i32) -> zbus::Result<()>;

        fn context_menu(&self, x: i32, y: i32) -> zbus::Result<()>;

        fn scroll(&self, delta: i32, orientation: &str) -> zbus::Result<()>;

        #[zbus(property)]
        fn status(&self) -> zbus::Result<String>;

        #[zbus(property)]
        fn title(&self) -> zbus::Result<String>;

        #[zbus(property)]
        fn icon_name(&self) -> zbus::Result<String>;

        #[zbus(property)]
        fn icon_pixmap(&self) -> zbus::Result<Vec<(i32, i32, Vec<u8>)>>;

        #[zbus(property)]
        fn attention_icon_name(&self) -> zbus::Result<String>;

        #[zbus(property)]
        fn attention_icon_pixmap(&self) -> zbus::Result<Vec<(i32, i32, Vec<u8>)>>;

        #[zbus(property)]
        fn overlay_icon_name(&self) -> zbus::Result<String>;

        #[zbus(property)]
        fn overlay_icon_pixmap(&self) -> zbus::Result<Vec<(i32, i32, Vec<u8>)>>;

        #[zbus(property)]
        fn icon_theme_path(&self) -> zbus::Result<String>;

        #[zbus(property)]
        fn tool_tip(&self) -> zbus::Result<(String, Vec<(i32, i32, Vec<u8>)>, String, String)>;

        #[zbus(property)]
        fn menu(&self) -> zbus::Result<OwnedObjectPath>;

        #[zbus(property)]
        fn item_is_menu(&self) -> zbus::Result<bool>;

        #[zbus(signal)]
        fn new_title(&self) -> zbus::Result<()>;

        #[zbus(signal)]
        fn new_icon(&self) -> zbus::Result<()>;

        #[zbus(signal)]
        fn new_attention_icon(&self) -> zbus::Result<()>;

        #[zbus(signal)]
        fn new_overlay_icon(&self) -> zbus::Result<()>;

        #[zbus(signal)]
        fn new_tool_tip(&self) -> zbus::Result<()>;

        #[zbus(signal)]
        fn new_status(&self, status: &str) -> zbus::Result<()>;
    }

    #[zbus::proxy(interface = "com.canonical.dbusmenu")]
    trait DBusMenu {
        fn get_layout(
            &self,
            parent: i32,
            depth: i32,
            properties: Vec<String>,
        ) -> zbus::Result<MenuLayout>;

        fn about_to_show(&self, id: i32) -> zbus::Result<bool>;

        fn event(&self, id: i32, event: &str, data: OwnedValue, timestamp: u32)
        -> zbus::Result<()>;

        #[zbus(signal)]
        fn layout_updated(&self, revision: u32, parent: i32) -> zbus::Result<()>;
    }

    async fn forward_event_stream<S>(mut event_stream: S, events: UnboundedSender<DriverEvent>)
    where
        S: futures_util::Stream<Item = DriverEvent> + Unpin,
    {
        while let Some(event) = event_stream.next().await {
            if events.send(event).is_err() {
                break;
            }
        }
    }

    async fn forward_signal_stream<S, F>(
        mut signals: S,
        events: UnboundedSender<DriverEvent>,
        make_event: F,
    ) where
        S: futures_util::Stream + Unpin,
        F: Fn() -> DriverEvent,
    {
        while signals.next().await.is_some() {
            if events.send(make_event()).is_err() {
                break;
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::super::Backend;
        use super::*;
        use std::sync::{Arc, Mutex};
        use tokio::sync::mpsc;
        use zbus::zvariant::{Str, Value};

        struct MockItem;

        #[zbus::interface(name = "org.kde.StatusNotifierItem")]
        impl MockItem {
            #[zbus(property)]
            fn status(&self) -> &str {
                "Active"
            }

            #[zbus(property)]
            fn title(&self) -> &str {
                "Test app"
            }

            #[zbus(property)]
            fn icon_name(&self) -> &str {
                "test-icon"
            }

            #[zbus(property)]
            fn menu(&self) -> OwnedObjectPath {
                OwnedObjectPath::try_from("/TestMenu").unwrap()
            }

            #[zbus(property)]
            fn item_is_menu(&self) -> bool {
                true
            }

            fn activate(&self, _x: i32, _y: i32) {}
        }

        struct MockMenu {
            clicks: UnboundedSender<i32>,
            label: Arc<Mutex<String>>,
        }

        #[zbus::interface(name = "com.canonical.dbusmenu")]
        impl MockMenu {
            fn get_layout(
                &self,
                _parent: i32,
                _depth: i32,
                _properties: Vec<String>,
            ) -> MenuLayout {
                let mut properties = HashMap::new();
                properties.insert(
                    "label".to_string(),
                    OwnedValue::from(Str::from(self.label.lock().unwrap().as_str())),
                );

                let entry =
                    OwnedValue::try_from(Value::new((1i32, properties, Vec::<OwnedValue>::new())))
                        .unwrap();

                (1, (0, HashMap::new(), vec![entry]))
            }

            fn about_to_show(&self, _id: i32) -> bool {
                false
            }

            fn event(&self, id: i32, _event: &str, _data: OwnedValue, _timestamp: u32) {
                let _ = self.clicks.send(id);
            }

            #[zbus(signal)]
            async fn layout_updated(
                emitter: &zbus::object_server::SignalEmitter<'_>,
                revision: u32,
                parent: i32,
            ) -> zbus::Result<()>;
        }

        fn test_pixmap(size: i32) -> Pixmap {
            Pixmap {
                width: size,
                height: size,
                argb: vec![0; size as usize * size as usize * 4],
            }
        }

        #[test]
        fn selects_pixmap_for_physical_render_size() {
            let icon = Icon {
                name: String::new(),
                pixmaps: vec![test_pixmap(16), test_pixmap(32), test_pixmap(64)],
            };

            assert_eq!(icon.best_pixmap(18).unwrap().width, 32);
            assert_eq!(icon.best_pixmap(36).unwrap().width, 64);
            assert_eq!(icon.best_pixmap(96).unwrap().width, 64);
        }

        #[test]
        fn rejects_malformed_pixmaps() {
            assert!(Pixmap::from_argb(0, 16, Vec::new()).is_none());
            assert!(Pixmap::from_argb(16, 16, vec![0; 10]).is_none());

            assert_eq!(
                Pixmap::from_argb(24, 24, vec![0; 24 * 24 * 4]),
                Some(test_pixmap(24)),
            );
        }

        #[test]
        fn decodes_recursive_dbusmenu_layout() {
            let mut child_props = HashMap::new();
            child_props.insert(
                "label".to_string(),
                OwnedValue::from(Str::from("_Settings")),
            );

            child_props.insert(
                "toggle-type".to_string(),
                OwnedValue::from(Str::from("checkmark")),
            );

            child_props.insert("toggle-state".to_string(), OwnedValue::from(1i32));

            let child =
                OwnedValue::try_from(Value::new((2i32, child_props, Vec::<OwnedValue>::new())))
                    .unwrap();

            let mut parent_props = HashMap::new();
            parent_props.insert("label".to_string(), OwnedValue::from(Str::from("More")));
            parent_props.insert(
                "children-display".to_string(),
                OwnedValue::from(Str::from("submenu")),
            );

            let parent =
                OwnedValue::try_from(Value::new((1i32, parent_props, vec![child]))).unwrap();
            let entry = RawMenuEntry::try_from(parent)
                .unwrap()
                .into_entry()
                .unwrap();

            assert_eq!(entry.label, "More");
            assert!(entry.submenu);
            assert_eq!(entry.children[0].label, "Settings");
            assert_eq!(entry.children[0].toggle, Some((false, true)));

            let invalid_path = OwnedObjectPath::try_from("/NO_DBUSMENU").unwrap();

            assert!(MenuPath::try_from(invalid_path).is_err());
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn hosts_items_and_dispatches_dbusmenu_clicks() {
            let bus = crate::backend::dbus::tests::Bus::new().await;
            let publisher = crate::features::availability::AvailabilityPublisher::default();
            let mut readiness = publisher.subscribe();
            let mut backend = Backend::start(publisher);

            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while !readiness.borrow_and_update().is_available() {
                    readiness.changed().await.unwrap();
                }
            })
            .await
            .unwrap();

            let commands = backend.controls();
            let mut receiver = backend.take_items();
            let (clicks, mut clicked) = mpsc::unbounded_channel();
            let label = Arc::new(Mutex::new("_Open".to_string()));
            let service = bus.connect().await;

            service
                .object_server()
                .at("/StatusNotifierItem", MockItem)
                .await
                .unwrap();

            service
                .object_server()
                .at(
                    "/TestMenu",
                    MockMenu {
                        clicks,
                        label: label.clone(),
                    },
                )
                .await
                .unwrap();

            StatusNotifierWatcherProxy::new(&service)
                .await
                .unwrap()
                .register_status_notifier_item("/StatusNotifierItem")
                .await
                .unwrap();

            let item_id = format!("{}/StatusNotifierItem", service.unique_name().unwrap());
            let item = loop {
                let snapshot =
                    tokio::time::timeout(std::time::Duration::from_secs(3), receiver.recv())
                        .await
                        .unwrap()
                        .unwrap();

                if let Some(item) = snapshot.items.into_iter().find(|item| item.id == item_id) {
                    break item;
                }
            };

            let item_id = item.id.clone();

            assert_eq!(item.title, "Test app");
            assert_eq!(item.icon.name, "test-icon");
            assert!(item.item_is_menu);
            assert_eq!(item.menu[0].label, "Open");

            commands.send(Command::OpenMenu(item_id.clone(), 0));
            commands.send(Command::ClickMenu(item.id, 1));

            assert_eq!(
                tokio::time::timeout(std::time::Duration::from_secs(3), clicked.recv())
                    .await
                    .unwrap(),
                Some(1),
            );

            *label.lock().unwrap() = "_Changed".to_string();

            let emitter = zbus::object_server::SignalEmitter::new(&service, "/TestMenu").unwrap();

            MockMenu::layout_updated(&emitter, 2, 0).await.unwrap();

            let mut changed = false;

            for _ in 0..5 {
                let Ok(Some(items)) =
                    tokio::time::timeout(std::time::Duration::from_secs(1), receiver.recv()).await
                else {
                    continue;
                };

                changed |= items
                    .items
                    .iter()
                    .find(|item| item.id == item_id)
                    .and_then(|item| item.menu.first())
                    .is_some_and(|entry| entry.label == "Changed");

                if changed {
                    break;
                }
            }

            assert!(changed, "menu should update after LayoutUpdated");

            service.close().await.unwrap();

            let mut removed = false;

            for _ in 0..5 {
                let Ok(Some(items)) =
                    tokio::time::timeout(std::time::Duration::from_secs(1), receiver.recv()).await
                else {
                    continue;
                };

                removed |= !items.items.iter().any(|item| item.id == item_id);

                if removed {
                    break;
                }
            }

            assert!(removed, "item should disappear when its bus owner exits");
        }
    }
}
