//! The shelf: a floating panel pinned to a screen edge that holds dropped
//! files until they are dragged back out.

use crate::actions::{self, ImageFormat, Resize};
use crate::clipboard::{self, ClipContent};
use crate::preview::{self, Snippet};
use crate::settings::Settings;
use crate::store::{self, ShelfItem};
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;
use uuid::Uuid;

#[cfg(debug_assertions)]
pub mod selftest;

const ROW_SPACING: i32 = 2;
const HEADER_HEIGHT: i32 = 34;
const SEARCH_HEIGHT: i32 = 44;
const SECTION_HEIGHT: i32 = 26;
const EMPTY_HEIGHT: i32 = 128;
const EDGE_MARGIN: i32 = 16;
/// Room around the shelf inside its window for the drop shadow.
const SHADOW_PAD: i32 = 8;
const REMOVED_STACK_CAP: usize = 10;
/// Stored files up to this size are hashed so re-copying them is detected.
const MAX_HASHED_BYTES: u64 = 32 * 1024 * 1024;

const CSS: &str = r#"
window.shoal, window.shoal-edge { background: transparent; }
.shelf {
    margin: 8px;
    border-radius: 14px;
    background-color: alpha(@theme_bg_color, 0.95);
    border: 1px solid alpha(@theme_fg_color, 0.12);
    box-shadow: 0 2px 8px alpha(black, 0.35);
}
.shelf.drop-hover { border: 2px solid @theme_selected_bg_color; }
.shelf .title { font-size: 9pt; font-weight: 600; opacity: 0.6; }
.shelf .section { font-size: 7.5pt; font-weight: 600; opacity: 0.5; margin: 6px 0 2px 10px; }
.shelf .row { border-radius: 10px; }
.shelf .row:hover { background-color: alpha(@theme_fg_color, 0.07); }
.shelf .row.selected { background-color: alpha(@theme_selected_bg_color, 0.30); }
.shelf .row.cursor { box-shadow: inset 0 0 0 1px alpha(@theme_selected_bg_color, 0.9); }
.shelf .item-name { font-size: 9pt; font-weight: 500; }
.shelf .item-sub { font-size: 7.5pt; opacity: 0.6; }
.shelf .pin-mark { opacity: 0.55; }
.shelf .hint { font-size: 8pt; opacity: 0.45; }
.shelf .dim-icon { opacity: 0.45; }
.shelf .header-button { min-width: 22px; min-height: 22px; padding: 2px; opacity: 0.7; }
.shelf searchbar > revealer > box { padding: 4px 8px; border: none; background: none; box-shadow: none; }
/* Breeze gives scrollbar sliders negative margins, which GTK rejects while
   the list resizes. */
.shelf scrollbar slider { margin: 0; min-width: 6px; min-height: 6px; }
"#;

pub fn content_hash<T: Hash + ?Sized>(value: &T) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn debug_enabled() -> bool {
    std::env::var_os("SHOAL_DEBUG").is_some()
}

pub struct Shelf {
    app: gtk::Application,
    window: gtk::Window,
    edge_windows: RefCell<Vec<gtk::Window>>,
    root: gtk::Box,
    list: gtk::Box,
    stack: gtk::Stack,
    empty_hint: gtk::Label,
    title: gtk::Label,
    spinner: gtk::Spinner,
    clear_button: gtk::Button,
    drag_all: gtk::Image,
    search_bar: gtk::SearchBar,
    search: gtk::SearchEntry,
    menu: gio::MenuModel,
    settings: Rc<RefCell<Settings>>,
    items: RefCell<Vec<ShelfItem>>,
    rows: RefCell<HashMap<Uuid, gtk::Widget>>,
    selected: RefCell<Vec<Uuid>>,
    /// Where shift-click and shift-arrow ranges start.
    anchor: Cell<Option<Uuid>>,
    /// The row arrow keys move and Enter opens.
    cursor: Cell<Option<Uuid>>,
    /// Set by a plain click on an already-selected row: selecting just that
    /// row waits for the release, so pressing on a selection can drag it all.
    pending_single: Cell<Option<Uuid>>,
    /// Groups of items removed together, oldest first. Backs "Bring Back
    /// Last Removed Files".
    removed_stack: RefCell<Vec<Vec<ShelfItem>>>,
    /// Hash of the most recent clipboard content seen or written by Shoal,
    /// so the same copy reported twice is not handled twice.
    last_capture: Cell<Option<u64>>,
    /// Content hashes of files in Shoal's storage, so re-copying something
    /// already on the shelf moves it to the top instead of duplicating it.
    stored_hashes: RefCell<HashMap<String, u64>>,
    external_drag: Cell<bool>,
    /// A drag is hovering the shelf or an edge strip.
    drag_hover: Cell<bool>,
    internal_drag: Cell<bool>,
    pending_drops: Cell<u32>,
    jobs: Cell<u32>,
    hiding: Cell<bool>,
    fade: RefCell<Option<glib::SourceId>>,
    on_changed: RefCell<Option<Box<dyn Fn()>>>,
}

impl Shelf {
    pub fn new(
        app: &gtk::Application,
        settings: Rc<RefCell<Settings>>,
        menu: gio::MenuModel,
    ) -> Rc<Self> {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(CSS);
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }

        let window = gtk::Window::builder()
            .application(app)
            .title("Shoal")
            .decorated(false)
            .resizable(false)
            .css_classes(["shoal"])
            .build();
        window.init_layer_shell();
        window.set_namespace(Some("shoal"));
        window.set_layer(Layer::Overlay);
        // Like a non-activating panel: focus only when clicked.
        window.set_keyboard_mode(KeyboardMode::OnDemand);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("shelf");
        root.set_overflow(gtk::Overflow::Hidden);
        window.set_child(Some(&root));

        let title = gtk::Label::builder()
            .label("Shoal")
            .xalign(0.0)
            .hexpand(true)
            .build();
        title.add_css_class("title");

        let spinner = gtk::Spinner::builder().visible(false).build();
        spinner.set_tooltip_text(Some("Working…"));

        let drag_all = gtk::Image::from_icon_name("document-multiple-symbolic");
        drag_all.set_tooltip_text(Some("Drag all items out at once"));
        drag_all.add_css_class("header-button");

        let clear_button = gtk::Button::from_icon_name("user-trash-symbolic");
        clear_button.set_tooltip_text(Some("Clear items (pinned items stay)"));
        clear_button.add_css_class("flat");
        clear_button.add_css_class("header-button");

        let gear = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text("Shoal menu")
            .menu_model(&menu)
            .build();
        gear.add_css_class("flat");
        gear.add_css_class("header-button");

        let header = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(2)
            .height_request(HEADER_HEIGHT)
            .margin_start(14)
            .margin_end(10)
            .build();
        header.append(&title);
        header.append(&spinner);
        header.append(&drag_all);
        header.append(&clear_button);
        header.append(&gear);
        root.append(&header);

        let search = gtk::SearchEntry::builder()
            .placeholder_text("Filter")
            .hexpand(true)
            .build();
        let search_bar = gtk::SearchBar::builder()
            .child(&search)
            .show_close_button(false)
            .build();
        search_bar.connect_entry(&search);
        // Typing anywhere on the shelf starts filtering.
        search_bar.set_key_capture_widget(Some(&window));
        root.append(&search_bar);

        let list = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(ROW_SPACING)
            .margin_start(8)
            .margin_end(8)
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&list)
            .build();

        let empty = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .build();
        let empty_icon = gtk::Image::from_icon_name("folder-download-symbolic");
        empty_icon.set_pixel_size(24);
        empty_icon.add_css_class("dim-icon");
        let empty_hint = gtk::Label::new(Some("Drop files here"));
        empty_hint.add_css_class("hint");
        empty.append(&empty_icon);
        empty.append(&empty_hint);

        let stack = gtk::Stack::builder().vexpand(true).margin_bottom(8).build();
        stack.add_named(&scroll, Some("list"));
        stack.add_named(&empty, Some("empty"));
        root.append(&stack);

        let shelf = Rc::new(Self {
            app: app.clone(),
            window,
            edge_windows: RefCell::default(),
            root,
            list,
            stack,
            empty_hint,
            title,
            spinner,
            clear_button,
            drag_all,
            search_bar,
            search,
            menu,
            settings,
            items: RefCell::default(),
            rows: RefCell::default(),
            selected: RefCell::default(),
            anchor: Cell::new(None),
            cursor: Cell::new(None),
            pending_single: Cell::new(None),
            removed_stack: RefCell::default(),
            last_capture: Cell::new(None),
            stored_hashes: RefCell::default(),
            external_drag: Cell::new(false),
            drag_hover: Cell::new(false),
            internal_drag: Cell::new(false),
            pending_drops: Cell::new(0),
            jobs: Cell::new(0),
            hiding: Cell::new(false),
            fade: RefCell::default(),
            on_changed: RefCell::default(),
        });

        shelf.clear_button.connect_clicked({
            let shelf = shelf.clone();
            move |_| shelf.clear_all()
        });
        shelf.search.connect_search_changed({
            let shelf = shelf.clone();
            move |_| shelf.refresh()
        });
        shelf.search.connect_activate({
            let shelf = shelf.clone();
            move |_| shelf.open_targets(None)
        });
        shelf.search_bar.connect_search_mode_enabled_notify({
            let shelf = shelf.clone();
            move |bar| {
                if !bar.is_search_mode() && !shelf.search.text().is_empty() {
                    shelf.search.set_text("");
                }
                shelf.update_frame();
            }
        });
        shelf.window.connect_is_active_notify(|w| {
            if debug_enabled() {
                eprintln!("Shoal debug: shelf keyboard focus = {}", w.is_active());
            }
        });
        shelf.attach_drop_target(&shelf.root.clone().upcast(), None);
        shelf.attach_drag_all();
        shelf.attach_background_gestures();
        shelf.attach_keyboard();
        shelf.rebuild_edge_windows();
        if let Some(display) = gdk::Display::default() {
            let weak = Rc::downgrade(&shelf);
            display.monitors().connect_items_changed(move |_, _, _, _| {
                if let Some(shelf) = weak.upgrade() {
                    shelf.rebuild_edge_windows();
                }
            });
        }
        shelf
    }

    /// Called after anything the menus display changes.
    pub fn set_on_changed(&self, f: impl Fn() + 'static) {
        self.on_changed.replace(Some(Box::new(f)));
    }

    fn notify(&self) {
        if let Some(f) = self.on_changed.borrow().as_ref() {
            f();
        }
    }

    fn unpinned_count(&self) -> usize {
        self.items.borrow().iter().filter(|i| !i.pinned).count()
    }

    pub fn has_unpinned(&self) -> bool {
        self.unpinned_count() > 0
    }

    pub fn can_bring_back(&self) -> bool {
        !self.removed_stack.borrow().is_empty()
    }

    // MARK: Lifecycle

    pub fn restore(self: &Rc<Self>) {
        let items = store::load();
        store::sweep_orphans(&items);
        self.items.replace(items);
        self.seed_hashes();
        self.apply_layout_settings();
        if self.has_unpinned() {
            self.show();
        }
    }

    /// Hashes files already in Shoal's storage so copying them again after
    /// a relaunch is recognized.
    fn seed_hashes(self: &Rc<Self>) {
        let stored: Vec<String> = self
            .items
            .borrow()
            .iter()
            .filter(|i| i.is_stored())
            .map(|i| i.path.clone())
            .collect();
        let shelf = self.clone();
        glib::spawn_future_local(async move {
            let hashes = gio::spawn_blocking(move || {
                stored
                    .into_iter()
                    .filter(|p| {
                        std::fs::metadata(p)
                            .is_ok_and(|m| m.is_file() && m.len() <= MAX_HASHED_BYTES)
                    })
                    .filter_map(|p| {
                        std::fs::read(&p)
                            .ok()
                            .map(|data| (p, content_hash(data.as_slice())))
                    })
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap_or_default();
            shelf.stored_hashes.borrow_mut().extend(hashes);
        });
    }

    // MARK: Items

    pub fn add(self: &Rc<Self>, paths: Vec<PathBuf>, show: bool) {
        if paths.is_empty() {
            return;
        }
        let count = paths.len();
        {
            let mut items = self.items.borrow_mut();
            for path in paths {
                let path = path.to_string_lossy().into_owned();
                // Re-adding keeps the item's pin.
                let pinned = items.iter().any(|i| i.path == path && i.pinned);
                items.retain(|i| i.path != path);
                let mut item = ShelfItem::new(path);
                item.pinned = pinned;
                items.insert(0, item);
            }
        }
        self.refresh();
        if show {
            self.show();
        }
        eprintln!(
            "Shoal: added {count} item(s), shelf now holds {}",
            self.items.borrow().len()
        );
    }

    /// Removes items as one undo group.
    pub fn remove(self: &Rc<Self>, ids: &[Uuid]) {
        let removed: Vec<ShelfItem> = {
            let mut items = self.items.borrow_mut();
            let (gone, kept) = items.drain(..).partition(|i| ids.contains(&i.id));
            *items = kept;
            gone
        };
        if removed.is_empty() {
            return;
        }
        self.selected.borrow_mut().retain(|id| !ids.contains(id));
        self.push_removed(removed);
        self.refresh();
        self.hide_if_idle_soon();
    }

    /// Clears everything except pinned items.
    pub fn clear_all(self: &Rc<Self>) {
        let ids: Vec<Uuid> = self
            .items
            .borrow()
            .iter()
            .filter(|i| !i.pinned)
            .map(|i| i.id)
            .collect();
        self.remove(&ids);
        if !self.external_drag.get() {
            self.hide();
        }
    }

    pub fn bring_back_last_removed(self: &Rc<Self>) {
        // Skip past groups whose files no longer exist instead of burning
        // the undo on them, so one stale group cannot swallow the gesture.
        loop {
            let Some(group) = self.removed_stack.borrow_mut().pop() else {
                break;
            };
            let existing: Vec<ShelfItem> = group.into_iter().filter(ShelfItem::exists).collect();
            if existing.is_empty() {
                continue;
            }
            {
                let mut items = self.items.borrow_mut();
                for item in existing.iter().rev() {
                    items.retain(|i| i.path != item.path);
                    items.insert(0, item.clone());
                }
            }
            self.refresh();
            self.show();
            eprintln!("Shoal: brought back {} item(s)", existing.len());
            return;
        }
        beep();
        self.notify();
    }

    fn push_removed(&self, removed: Vec<ShelfItem>) {
        if removed.is_empty() {
            return;
        }
        let mut stack = self.removed_stack.borrow_mut();
        stack.push(removed);
        if stack.len() > REMOVED_STACK_CAP {
            let evicted = stack.remove(0);
            let items = self.items.borrow();
            for item in evicted {
                if !items.iter().any(|i| i.path == item.path) {
                    self.stored_hashes.borrow_mut().remove(&item.path);
                    schedule_storage_cleanup(&item);
                }
            }
        }
    }

    fn set_pinned(self: &Rc<Self>, ids: &[Uuid], pinned: bool) {
        for item in self.items.borrow_mut().iter_mut() {
            if ids.contains(&item.id) {
                item.pinned = pinned;
            }
        }
        self.refresh();
    }

    fn move_to_top(self: &Rc<Self>, id: Uuid) {
        {
            let mut items = self.items.borrow_mut();
            let Some(index) = items.iter().position(|i| i.id == id) else {
                return;
            };
            let item = items.remove(index);
            items.insert(0, item);
        }
        self.refresh();
    }

    /// The shelf item holding stored content with this hash, if any.
    fn item_with_hash(&self, hash: u64) -> Option<Uuid> {
        let hashes = self.stored_hashes.borrow();
        self.items
            .borrow()
            .iter()
            .find(|i| hashes.get(&i.path) == Some(&hash))
            .map(|i| i.id)
    }

    // MARK: Clipboard

    /// Puts copied or dropped content on the shelf. Content already on the
    /// shelf moves to the top instead of being duplicated. With `auto`, a
    /// repeat of the last clipboard content is ignored: apps and Plasma's
    /// clipboard manager often announce one copy more than once.
    pub fn capture(self: &Rc<Self>, content: ClipContent, auto: bool) {
        let show = !auto;
        let hash = match &content {
            ClipContent::Files(paths) => content_hash(paths),
            ClipContent::Image(data, _) => content_hash(data.as_slice()),
            ClipContent::Text(text) => content_hash(text.as_bytes()),
            ClipContent::Sensitive | ClipContent::Empty => {
                if !auto {
                    beep();
                }
                return;
            }
        };
        if auto && self.last_capture.get() == Some(hash) {
            return;
        }
        self.last_capture.set(Some(hash));

        if let ClipContent::Files(paths) = content {
            self.add(paths, show);
            return;
        }
        if let Some(id) = self.item_with_hash(hash) {
            self.move_to_top(id);
            if show {
                self.show();
            }
            return;
        }
        let path = match content {
            ClipContent::Image(data, ext) => store::save_data(&data, ext, "Clipboard"),
            ClipContent::Text(text) => store::save_snippet(&text),
            _ => None,
        };
        match path {
            Some(path) => {
                self.stored_hashes
                    .borrow_mut()
                    .insert(path.to_string_lossy().into_owned(), hash);
                self.add(vec![path], show);
            }
            None => beep(),
        }
    }

    /// Remembers content Shoal itself put on the clipboard so the watcher
    /// does not shelve it again.
    fn note_clipboard_write(&self, hash: u64) {
        self.last_capture.set(Some(hash));
    }

    // MARK: Drag in

    /// `monitor` is set for edge strips: the shelf opens on that screen.
    fn attach_drop_target(self: &Rc<Self>, widget: &gtk::Widget, monitor: Option<gdk::Monitor>) {
        let is_edge = monitor.is_some();
        let target = gtk::DropTargetAsync::new(None, gdk::DragAction::COPY);

        let shelf = self.clone();
        target.connect_accept(move |_, drop| {
            // Our own rows being dragged out must not land back on the shelf.
            if drop.drag().is_some() {
                return false;
            }
            let formats = drop.formats();
            let text_ok = !is_edge || shelf.settings.borrow().include_text_drags;
            offers_files(&formats) || offers_image(&formats) || (text_ok && offers_text(&formats))
        });

        let shelf = self.clone();
        target.connect_drag_enter(move |_, _, _, _| {
            shelf.drag_hover.set(true);
            if let Some(monitor) = &monitor {
                shelf.external_drag.set(true);
                if !shelf.is_visible() {
                    shelf.window.set_monitor(Some(monitor));
                }
                shelf.show();
            } else {
                shelf.root.add_css_class("drop-hover");
            }
            gdk::DragAction::COPY
        });

        let shelf = self.clone();
        target.connect_drag_leave(move |_, _| {
            shelf.drag_hover.set(false);
            shelf.root.remove_css_class("drop-hover");
            shelf.external_drag_left();
        });

        let shelf = self.clone();
        target.connect_drop(move |_, drop, _, _| {
            shelf.drag_hover.set(false);
            shelf.external_drag.set(false);
            shelf.root.remove_css_class("drop-hover");
            shelf.pending_drops.set(shelf.pending_drops.get() + 1);
            let shelf = shelf.clone();
            let drop = drop.clone();
            glib::spawn_future_local(async move {
                let ok = shelf.receive_drop(&drop).await;
                drop.finish(if ok {
                    gdk::DragAction::COPY
                } else {
                    gdk::DragAction::empty()
                });
                shelf.pending_drops.set(shelf.pending_drops.get() - 1);
                shelf.hide_if_idle_soon();
            });
            true
        });

        widget.add_controller(target);
    }

    /// Wayland gives no signal when a drag elsewhere ends, so once the drag
    /// leaves both the edge strip and the shelf, give it a moment to come
    /// back before treating it as over.
    fn external_drag_left(self: &Rc<Self>) {
        let shelf = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(1500), move || {
            if !shelf.drag_hover.get() && shelf.external_drag.get() {
                shelf.external_drag.set(false);
                shelf.hide_if_idle();
            }
        });
    }

    async fn receive_drop(self: &Rc<Self>, drop: &gdk::Drop) -> bool {
        let formats = drop.formats();

        // 1. Real files (file managers, most apps)
        if offers_files(&formats)
            && let Ok(value) = drop
                .read_value_future(gdk::FileList::static_type(), glib::Priority::DEFAULT)
                .await
            && let Ok(list) = value.get::<gdk::FileList>()
        {
            let paths: Vec<PathBuf> = list.files().iter().filter_map(|f| f.path()).collect();
            if !paths.is_empty() {
                self.add(paths, true);
                return true;
            }
        }

        // 2. Raw image data (images dragged from browsers)
        if offers_image(&formats)
            && let Ok(value) = drop
                .read_value_future(gdk::Texture::static_type(), glib::Priority::DEFAULT)
                .await
            && let Ok(texture) = value.get::<gdk::Texture>()
        {
            let png = texture.save_to_png_bytes();
            self.capture(ClipContent::Image(png.to_vec(), "png"), false);
            return true;
        }

        // 3. Plain text, including web links, becomes a snippet
        if (offers_text(&formats) || offers_files(&formats))
            && let Ok(value) = drop
                .read_value_future(glib::Type::STRING, glib::Priority::DEFAULT)
                .await
            && let Ok(text) = value.get::<String>()
            && !text.trim().is_empty()
        {
            self.capture(ClipContent::Text(text), false);
            return true;
        }
        false
    }

    // MARK: Drag out

    fn drag_out_actions(&self) -> gdk::DragAction {
        if self.settings.borrow().remove_original_on_drag_out {
            gdk::DragAction::MOVE | gdk::DragAction::COPY
        } else {
            gdk::DragAction::COPY
        }
    }

    /// What a drag of these items carries. A single snippet also carries its
    /// text (or link), so dropping it into a text field inserts the text.
    fn drag_content(&self, ids: &[Uuid]) -> Option<gdk::ContentProvider> {
        let items: Vec<ShelfItem> = self.items_for(ids);
        if items.is_empty() {
            return None;
        }
        let files: Vec<gio::File> = items
            .iter()
            .map(|i| gio::File::for_path(i.path()))
            .collect();
        let file_list =
            gdk::ContentProvider::for_value(&gdk::FileList::from_array(&files).to_value());
        if let [item] = items.as_slice()
            && item.is_snippet()
        {
            if let Some(Snippet::Link(url)) = preview::snippet(item.path()) {
                return Some(gdk::ContentProvider::new_union(&[
                    gdk::ContentProvider::for_bytes(
                        "text/uri-list",
                        &glib::Bytes::from_owned(format!("{url}\r\n").into_bytes()),
                    ),
                    gdk::ContentProvider::for_value(&url.to_value()),
                ]));
            }
            if let Some(text) = preview::read_snippet_text(item.path()) {
                return Some(gdk::ContentProvider::new_union(&[
                    file_list,
                    gdk::ContentProvider::for_value(&text.to_value()),
                ]));
            }
        }
        Some(file_list)
    }

    /// Wires a drag source carrying the items `ids()` returns; `on_success`
    /// gets those ids once the drop lands somewhere.
    fn attach_drag_source(
        self: &Rc<Self>,
        widget: &gtk::Widget,
        ids: impl Fn() -> Vec<Uuid> + 'static,
        on_success: impl Fn(&Rc<Self>, &[Uuid]) + 'static,
    ) {
        let source = gtk::DragSource::new();
        let cancelled = Rc::new(Cell::new(false));
        let dragged: Rc<RefCell<Vec<Uuid>>> = Rc::default();

        let shelf = self.clone();
        let dragged_c = dragged.clone();
        source.connect_prepare(move |source, _, _| {
            let ids = ids();
            let content = shelf.drag_content(&ids)?;
            dragged_c.replace(ids);
            source.set_actions(shelf.drag_out_actions());
            Some(content)
        });

        let shelf = self.clone();
        let icon_widget = widget.clone();
        let flag = cancelled.clone();
        source.connect_drag_begin(move |source, _| {
            flag.set(false);
            shelf.pending_single.set(None);
            let paintable = gtk::WidgetPaintable::new(Some(&icon_widget));
            source.set_icon(Some(&paintable), 24, 24);
            shelf.internal_drag.set(true);
        });

        let flag = cancelled.clone();
        source.connect_drag_cancel(move |_, _, _| {
            flag.set(true);
            false
        });

        let shelf = self.clone();
        let on_success = Rc::new(on_success);
        source.connect_drag_end(move |_, _, _| {
            let succeeded = !cancelled.get();
            let ids = dragged.take();
            let shelf = shelf.clone();
            let on_success = on_success.clone();
            // Rebuilding rows inside the drag source's own signal would
            // destroy it mid-emission.
            glib::idle_add_local_once(move || {
                shelf.internal_drag.set(false);
                if succeeded {
                    on_success(&shelf, &ids);
                }
                shelf.hide_if_idle_soon();
            });
        });

        widget.add_controller(source);
    }

    /// Dragged-out items leave the shelf, except pinned ones. In move mode
    /// the originals go to the Trash (pinned items are never trashed);
    /// targets that performed a move already removed them.
    fn drag_out_succeeded(self: &Rc<Self>, ids: &[Uuid]) {
        let leaving: Vec<ShelfItem> = self
            .items_for(ids)
            .into_iter()
            .filter(|i| !i.pinned)
            .collect();
        if self.settings.borrow().remove_original_on_drag_out {
            for item in &leaving {
                trash_original_soon(item.path().to_path_buf());
            }
        }
        let leaving: Vec<Uuid> = leaving.iter().map(|i| i.id).collect();
        self.remove(&leaving);
    }

    fn attach_drag_all(self: &Rc<Self>) {
        let shelf = self.clone();
        let ids = move || shelf.visible_items().iter().map(|i| i.id).collect();
        self.attach_drag_source(&self.drag_all.clone().upcast(), ids, |shelf, ids| {
            shelf.drag_out_succeeded(ids)
        });
    }

    // MARK: Selection

    fn items_for(&self, ids: &[Uuid]) -> Vec<ShelfItem> {
        self.visible_items()
            .into_iter()
            .chain(self.items.borrow().iter().cloned())
            .filter(|i| ids.contains(&i.id))
            .fold(Vec::new(), |mut acc, i| {
                if !acc.iter().any(|a: &ShelfItem| a.id == i.id) {
                    acc.push(i);
                }
                acc
            })
    }

    /// The selection in display order, or the cursor row if nothing is
    /// selected.
    fn targets(&self) -> Vec<Uuid> {
        let selected = self.selected.borrow();
        let ordered: Vec<Uuid> = self
            .visible_items()
            .iter()
            .map(|i| i.id)
            .filter(|id| selected.contains(id))
            .collect();
        if !ordered.is_empty() {
            return ordered;
        }
        self.cursor.get().into_iter().collect()
    }

    fn select_only(&self, id: Uuid) {
        self.selected.replace(vec![id]);
        self.anchor.set(Some(id));
        self.cursor.set(Some(id));
        self.update_selection_css();
    }

    fn toggle_selected(&self, id: Uuid) {
        {
            let mut selected = self.selected.borrow_mut();
            if let Some(i) = selected.iter().position(|s| *s == id) {
                selected.remove(i);
            } else {
                selected.push(id);
            }
        }
        self.anchor.set(Some(id));
        self.cursor.set(Some(id));
        self.update_selection_css();
    }

    fn select_range_to(&self, id: Uuid) {
        let order: Vec<Uuid> = self.visible_items().iter().map(|i| i.id).collect();
        let anchor = self.anchor.get().unwrap_or(id);
        let (Some(a), Some(b)) = (
            order.iter().position(|x| *x == anchor),
            order.iter().position(|x| *x == id),
        ) else {
            self.select_only(id);
            return;
        };
        let (lo, hi) = (a.min(b), a.max(b));
        self.selected.replace(order[lo..=hi].to_vec());
        self.cursor.set(Some(id));
        self.update_selection_css();
    }

    fn select_all(&self) {
        let all: Vec<Uuid> = self.visible_items().iter().map(|i| i.id).collect();
        self.anchor.set(all.first().copied());
        self.selected.replace(all);
        self.update_selection_css();
    }

    fn clear_selection(&self) {
        self.selected.borrow_mut().clear();
        self.update_selection_css();
    }

    fn update_selection_css(&self) {
        let selected = self.selected.borrow();
        let cursor = self.cursor.get();
        for (id, row) in self.rows.borrow().iter() {
            if selected.contains(id) {
                row.add_css_class("selected");
            } else {
                row.remove_css_class("selected");
            }
            if cursor == Some(*id) && self.window.is_active() {
                row.add_css_class("cursor");
            } else {
                row.remove_css_class("cursor");
            }
        }
    }

    /// Moves the keyboard cursor by `delta` rows (clamped), extending the
    /// selection with shift.
    fn move_cursor(&self, delta: isize, extend: bool) {
        let order: Vec<Uuid> = self.visible_items().iter().map(|i| i.id).collect();
        if order.is_empty() {
            return;
        }
        let current = self
            .cursor
            .get()
            .and_then(|c| order.iter().position(|x| *x == c));
        let next = match current {
            None if delta < 0 => order.len() - 1,
            None => 0,
            Some(i) => (i as isize + delta).clamp(0, order.len() as isize - 1) as usize,
        };
        let id = order[next];
        if extend {
            if self.anchor.get().is_none() {
                self.anchor.set(current.map(|i| order[i]).or(Some(id)));
            }
            self.select_range_to(id);
        } else {
            self.select_only(id);
        }
        if let Some(row) = self.rows.borrow().get(&id) {
            scroll_into_view(row);
        }
    }

    // MARK: Item actions

    fn open_targets(self: &Rc<Self>, ids: Option<Vec<Uuid>>) {
        let ids = ids.unwrap_or_else(|| self.targets());
        for item in self.items_for(&ids) {
            match item
                .is_snippet()
                .then(|| preview::snippet(item.path()))
                .flatten()
            {
                Some(Snippet::Link(url)) => open_uri(&url),
                _ => open_path(item.path()),
            }
        }
    }

    fn open_with(&self, ids: &[Uuid]) {
        let Some(item) = self.items_for(ids).into_iter().next() else {
            return;
        };
        let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(item.path())));
        launcher.set_always_ask(true);
        launcher.launch(Some(&self.window), gio::Cancellable::NONE, |result| {
            if let Err(err) = result
                && !err.matches(gtk::DialogError::Dismissed)
                && !err.matches(gtk::DialogError::Cancelled)
            {
                eprintln!("Shoal: Open With failed: {err}");
            }
        });
    }

    /// One snippet copies its text, so it pastes into any text field;
    /// anything else copies as files.
    fn copy_targets(&self, ids: &[Uuid]) {
        let items = self.items_for(ids);
        if let [item] = items.as_slice()
            && item.is_snippet()
            && let Some(text) = preview::read_snippet_text(item.path())
        {
            let text = match preview::snippet(item.path()) {
                Some(Snippet::Link(url)) => url,
                _ => text,
            };
            self.note_clipboard_write(content_hash(text.as_bytes()));
            clipboard::copy_text(&text);
            return;
        }
        if items.is_empty() {
            return;
        }
        let paths: Vec<PathBuf> = items.iter().map(|i| i.path().to_path_buf()).collect();
        self.note_clipboard_write(content_hash(&paths));
        clipboard::copy_files(&paths.iter().map(PathBuf::as_path).collect::<Vec<_>>());
    }

    fn copy_paths(&self, ids: &[Uuid]) {
        let text = self
            .items_for(ids)
            .iter()
            .map(|i| i.path.clone())
            .collect::<Vec<_>>()
            .join("\n");
        if text.is_empty() {
            return;
        }
        self.note_clipboard_write(content_hash(text.as_bytes()));
        clipboard::copy_text(&text);
    }

    /// Runs a quick action off the main thread and puts what it produced on
    /// the shelf. Failures are reported in a desktop notification.
    fn run_job<F>(self: &Rc<Self>, what: &'static str, job: F)
    where
        F: FnOnce() -> Vec<Result<PathBuf, String>> + Send + 'static,
    {
        self.jobs.set(self.jobs.get() + 1);
        self.spinner.set_visible(true);
        self.spinner.start();
        let shelf = self.clone();
        glib::spawn_future_local(async move {
            let results = gio::spawn_blocking(job)
                .await
                .unwrap_or_else(|_| vec![Err(format!("{what} stopped unexpectedly"))]);
            let mut made = Vec::new();
            let mut errors = Vec::new();
            for result in results {
                match result {
                    Ok(path) => made.push(path),
                    Err(err) => errors.push(err),
                }
            }
            // Oldest first, so the first result ends up on top.
            made.reverse();
            shelf.add(made, true);
            if !errors.is_empty() {
                for err in &errors {
                    eprintln!("Shoal: {what}: {err}");
                }
                beep();
                let note = gio::Notification::new(&format!("Shoal could not finish: {what}"));
                let mut body = errors
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n");
                if errors.len() > 3 {
                    body.push_str(&format!("\n…and {} more", errors.len() - 3));
                }
                note.set_body(Some(&body));
                shelf.app.send_notification(Some("job-error"), &note);
            }
            shelf.jobs.set(shelf.jobs.get() - 1);
            if shelf.jobs.get() == 0 {
                shelf.spinner.stop();
                shelf.spinner.set_visible(false);
            }
            shelf.hide_if_idle_soon();
        });
    }

    fn compress(self: &Rc<Self>, ids: &[Uuid]) {
        let paths: Vec<PathBuf> = self
            .items_for(ids)
            .iter()
            .map(|i| i.path().to_path_buf())
            .collect();
        if paths.is_empty() {
            return;
        }
        let name = actions::archive_name(&paths).unwrap_or_else(|| store::stamped_name("Archive"));
        self.run_job("Compress", move || {
            let result = store::reserve_path(&name, "zip")
                .and_then(|dest| actions::compress(&paths, &dest).map(|_| dest));
            vec![result]
        });
    }

    fn convert(self: &Rc<Self>, ids: &[Uuid], format: ImageFormat) {
        let paths: Vec<PathBuf> = self.image_paths(ids);
        self.run_job("Convert", move || {
            paths
                .iter()
                .filter(|p| {
                    let ext = p
                        .extension()
                        .map(|e| e.to_string_lossy().to_ascii_lowercase());
                    let same = match format {
                        ImageFormat::Jpeg => matches!(ext.as_deref(), Some("jpg" | "jpeg")),
                        _ => ext.as_deref() == Some(format.ext()),
                    };
                    !same
                })
                .map(|p| {
                    let dest = store::reserve_path(&actions::converted_name(p), format.ext())?;
                    actions::convert(p, format, &dest).map(|_| dest)
                })
                .collect()
        });
    }

    fn resize(self: &Rc<Self>, ids: &[Uuid], resize: Resize) {
        let paths: Vec<PathBuf> = self.image_paths(ids);
        self.run_job("Resize", move || {
            paths
                .iter()
                .map(|p| {
                    let (name, ext) = actions::resized_name(p, resize);
                    let dest = store::reserve_path(&name, ext)?;
                    actions::resize(p, resize, &dest).inspect_err(|_| {
                        let _ = std::fs::remove_file(&dest);
                    })?;
                    Ok(dest)
                })
                .collect()
        });
    }

    fn image_paths(&self, ids: &[Uuid]) -> Vec<PathBuf> {
        self.items_for(ids)
            .iter()
            .map(|i| i.path().to_path_buf())
            .filter(|p| actions::is_image(p))
            .collect()
    }

    // MARK: Menus

    /// Right-clicking the shelf background opens the app menu, so it stays
    /// reachable even when the tray icon is hidden. A left click on the
    /// background clears the selection.
    fn attach_background_gestures(self: &Rc<Self>) {
        let popover = gtk::PopoverMenu::from_model(Some(&self.menu));
        popover.set_parent(&self.root);
        popover.set_has_arrow(false);
        let shelf = self.clone();
        let click = gtk::GestureClick::builder().button(0).build();
        click.connect_pressed(move |gesture, _, x, y| {
            let on_row = shelf
                .root
                .pick(x, y, gtk::PickFlags::DEFAULT)
                .is_some_and(|w| row_ancestor(&w).is_some());
            if on_row {
                return;
            }
            if gesture.current_button() == gdk::BUTTON_SECONDARY {
                popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
                popover.popup();
            } else {
                shelf.clear_selection();
            }
        });
        self.root.add_controller(click);
    }

    /// Opens the item menu for the current targets at (x, y) in root
    /// coordinates.
    fn popup_item_menu(self: &Rc<Self>, x: f64, y: f64) {
        let ids = self.targets();
        let items = self.items_for(&ids);
        if items.is_empty() {
            return;
        }
        let group = gio::SimpleActionGroup::new();
        type MenuAction = Box<dyn Fn(Option<&glib::Variant>)>;
        let add = |name: &str, param: Option<&glib::VariantTy>, f: MenuAction| {
            let action = gio::SimpleAction::new(name, param);
            action.connect_activate(move |_, p| f(p));
            group.add_action(&action);
        };
        let s = self.clone();
        let i = ids.clone();
        add(
            "open",
            None,
            Box::new(move |_| s.open_targets(Some(i.clone()))),
        );
        let s = self.clone();
        let i = ids.clone();
        add("open-with", None, Box::new(move |_| s.open_with(&i)));
        let paths: Vec<PathBuf> = items.iter().map(|i| i.path().to_path_buf()).collect();
        add("reveal", None, Box::new(move |_| reveal_paths(&paths)));
        let s = self.clone();
        let i = ids.clone();
        add("copy", None, Box::new(move |_| s.copy_targets(&i)));
        let s = self.clone();
        let i = ids.clone();
        add("copy-path", None, Box::new(move |_| s.copy_paths(&i)));
        let all_pinned = items.iter().all(|i| i.pinned);
        let s = self.clone();
        let i = ids.clone();
        add(
            "pin",
            None,
            Box::new(move |_| s.set_pinned(&i, !all_pinned)),
        );
        let s = self.clone();
        let i = ids.clone();
        add("compress", None, Box::new(move |_| s.compress(&i)));
        let s = self.clone();
        let i = ids.clone();
        add(
            "convert",
            Some(glib::VariantTy::STRING),
            Box::new(move |p| {
                if let Some(f) = p.and_then(|p| p.str()).and_then(ImageFormat::from_id) {
                    s.convert(&i, f);
                }
            }),
        );
        let s = self.clone();
        let i = ids.clone();
        add(
            "resize",
            Some(glib::VariantTy::STRING),
            Box::new(move |p| {
                if let Some(r) = p.and_then(|p| p.str()).and_then(Resize::from_id) {
                    s.resize(&i, r);
                }
            }),
        );
        let s = self.clone();
        let i = ids.clone();
        add("remove", None, Box::new(move |_| s.remove(&i)));
        self.root.insert_action_group("item", Some(&group));

        let single = items.len() == 1;
        let menu = gio::Menu::new();
        let open = gio::Menu::new();
        open.append(Some("Open"), Some("item.open"));
        if single && !items[0].is_snippet() {
            open.append(Some("Open With…"), Some("item.open-with"));
        }
        open.append(Some("Show in File Manager"), Some("item.reveal"));
        menu.append_section(None, &open);

        let copy = gio::Menu::new();
        copy.append(Some("Copy"), Some("item.copy"));
        copy.append(
            Some(if single { "Copy Path" } else { "Copy Paths" }),
            Some("item.copy-path"),
        );
        copy.append(
            Some(if all_pinned { "Unpin" } else { "Pin to Shoal" }),
            Some("item.pin"),
        );
        menu.append_section(None, &copy);

        let quick = gio::Menu::new();
        quick.append(Some("Compress to ZIP"), Some("item.compress"));
        if items.iter().all(|i| actions::is_image(i.path())) {
            let convert = gio::Menu::new();
            for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::Webp] {
                let item = gio::MenuItem::new(Some(format.title()), None);
                item.set_action_and_target_value(
                    Some("item.convert"),
                    Some(&format.ext().to_variant()),
                );
                convert.append_item(&item);
            }
            quick.append_submenu(Some("Convert Image To"), &convert);
            let resize = gio::Menu::new();
            for r in Resize::ALL {
                let item = gio::MenuItem::new(Some(&r.title()), None);
                item.set_action_and_target_value(Some("item.resize"), Some(&r.id().to_variant()));
                resize.append_item(&item);
            }
            quick.append_submenu(Some("Resize Image"), &resize);
        }
        menu.append_section(None, &quick);

        let remove = gio::Menu::new();
        remove.append(Some("Remove from Shoal"), Some("item.remove"));
        menu.append_section(None, &remove);

        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        popover.set_parent(&self.root);
        popover.set_has_arrow(false);
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.connect_closed(|popover| {
            let popover = popover.clone();
            // Unparent after the activated action has run.
            glib::idle_add_local_once(move || popover.unparent());
        });
        popover.popup();
    }

    // MARK: Keyboard

    fn attach_keyboard(self: &Rc<Self>) {
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let shelf = self.clone();
        keys.connect_key_pressed(move |_, key, _, state| {
            if shelf.handle_key(key, state) {
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        self.window.add_controller(keys);
    }

    fn search_focused(&self) -> bool {
        gtk::prelude::GtkWindowExt::focus(&self.window).is_some_and(|w| {
            w == self.search.clone().upcast::<gtk::Widget>() || w.is_ancestor(&self.search)
        })
    }

    fn handle_key(self: &Rc<Self>, key: gdk::Key, state: gdk::ModifierType) -> bool {
        let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
        let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
        let in_search = self.search_focused();
        match key {
            gdk::Key::Up | gdk::Key::KP_Up => self.move_cursor(-1, shift),
            gdk::Key::Down | gdk::Key::KP_Down => self.move_cursor(1, shift),
            gdk::Key::Page_Up => self.move_cursor(-5, shift),
            gdk::Key::Page_Down => self.move_cursor(5, shift),
            gdk::Key::Home if !in_search => self.move_cursor(isize::MIN / 2, shift),
            gdk::Key::End if !in_search => self.move_cursor(isize::MAX / 2, shift),
            gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::ISO_Enter => self.open_targets(None),
            gdk::Key::Escape => {
                if self.search_bar.is_search_mode() {
                    self.search.set_text("");
                    self.search_bar.set_search_mode(false);
                } else if !self.selected.borrow().is_empty() {
                    self.clear_selection();
                } else {
                    self.hide();
                }
            }
            gdk::Key::Delete | gdk::Key::KP_Delete | gdk::Key::BackSpace if !in_search => {
                let ids = self.targets();
                self.remove(&ids);
            }
            gdk::Key::Menu => self.popup_item_menu_at_cursor(),
            gdk::Key::F10 if shift => self.popup_item_menu_at_cursor(),
            _ if ctrl && !in_search => match key.to_lower() {
                gdk::Key::a => self.select_all(),
                gdk::Key::c => self.copy_targets(&self.targets()),
                gdk::Key::v => {
                    self.app.activate_action("add-clipboard", None);
                }
                gdk::Key::z => self.bring_back_last_removed(),
                gdk::Key::p => {
                    let ids = self.targets();
                    let all_pinned = self.items_for(&ids).iter().all(|i| i.pinned);
                    self.set_pinned(&ids, !all_pinned);
                }
                gdk::Key::f => {
                    self.search_bar.set_search_mode(true);
                    self.search.grab_focus();
                }
                _ => return false,
            },
            _ if ctrl && key.to_lower() == gdk::Key::f => {
                self.search.grab_focus();
            }
            _ => return false,
        }
        true
    }

    fn popup_item_menu_at_cursor(self: &Rc<Self>) {
        let Some(id) = self
            .cursor
            .get()
            .or_else(|| self.selected.borrow().first().copied())
        else {
            return;
        };
        if !self.selected.borrow().contains(&id) {
            self.select_only(id);
        }
        let Some(row) = self.rows.borrow().get(&id).cloned() else {
            return;
        };
        let point = row
            .compute_point(
                &self.root,
                &gtk::graphene::Point::new(24.0, row.height() as f32 / 2.0),
            )
            .unwrap_or_else(|| gtk::graphene::Point::new(24.0, 24.0));
        self.popup_item_menu(f64::from(point.x()), f64::from(point.y()));
    }

    // MARK: Rows

    fn build_row(self: &Rc<Self>, item: &ShelfItem) -> gtk::Widget {
        let size = self.settings.borrow().size;
        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .height_request(size.row_height())
            .css_classes(["row"])
            .build();

        let snippet = item
            .is_snippet()
            .then(|| preview::snippet(item.path()))
            .flatten();
        let thumb = match &snippet {
            Some(Snippet::Color { rgba, .. }) => preview::color_swatch(*rgba, size.icon_size()),
            Some(Snippet::Link(_)) => {
                let image = gtk::Image::from_icon_name("internet-web-browser-symbolic");
                image.set_pixel_size(size.icon_size() * 3 / 4);
                image.set_size_request(size.icon_size(), size.icon_size());
                image.upcast()
            }
            _ => preview::thumbnail(item.path(), size.icon_size()),
        };
        thumb.set_margin_start(8);
        row.append(&thumb);

        let (name, subtitle, tooltip) = describe(item, snippet.as_ref());
        let name = gtk::Label::builder()
            .label(name)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .css_classes(["item-name"])
            .build();
        let sub = gtk::Label::builder()
            .label(subtitle)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .css_classes(["item-sub"])
            .build();
        row.set_tooltip_text(Some(&tooltip));
        let text = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .valign(gtk::Align::Center)
            .hexpand(true)
            .build();
        text.append(&name);
        text.append(&sub);
        row.append(&text);

        if item.pinned {
            let pin = gtk::Image::from_icon_name("pin-symbolic");
            pin.add_css_class("pin-mark");
            pin.set_tooltip_text(Some("Pinned: stays after dragging out"));
            row.append(&pin);
        }

        let remove = gtk::Button::from_icon_name("window-close-symbolic");
        remove.add_css_class("flat");
        remove.add_css_class("circular");
        remove.set_valign(gtk::Align::Center);
        remove.set_margin_end(6);
        remove.set_tooltip_text(Some("Remove from Shoal"));
        // Hidden but laid out, so names do not reflow on hover.
        remove.set_opacity(0.0);
        remove.set_can_target(false);
        row.append(&remove);

        let id = item.id;
        let shelf = self.clone();
        remove.connect_clicked(move |_| shelf.remove(&[id]));

        let hover = gtk::EventControllerMotion::new();
        hover.connect_enter(glib::clone!(
            #[weak]
            remove,
            move |_, _, _| {
                remove.set_opacity(1.0);
                remove.set_can_target(true);
            }
        ));
        hover.connect_leave(glib::clone!(
            #[weak]
            remove,
            move |_| {
                remove.set_opacity(0.0);
                remove.set_can_target(false);
            }
        ));
        row.add_controller(hover);

        let shelf = self.clone();
        let click = gtk::GestureClick::builder().button(0).build();
        click.connect_pressed(move |gesture, n_press, x, y| {
            let state = gesture.current_event_state();
            let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
            let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
            let selected = shelf.selected.borrow().contains(&id);
            if gesture.current_button() == gdk::BUTTON_SECONDARY {
                if !selected {
                    shelf.select_only(id);
                }
                let Some(row) = gesture.widget() else { return };
                let point = row
                    .compute_point(&shelf.root, &gtk::graphene::Point::new(x as f32, y as f32))
                    .unwrap_or_else(|| gtk::graphene::Point::new(x as f32, y as f32));
                shelf.popup_item_menu(f64::from(point.x()), f64::from(point.y()));
                return;
            }
            if n_press == 2 {
                shelf.open_targets(None);
            } else if ctrl {
                shelf.toggle_selected(id);
            } else if shift {
                shelf.select_range_to(id);
            } else if selected {
                shelf.cursor.set(Some(id));
                shelf.pending_single.set(Some(id));
                shelf.update_selection_css();
            } else {
                shelf.select_only(id);
            }
        });
        let shelf = self.clone();
        click.connect_released(move |_, _, _, _| {
            if shelf.pending_single.take() == Some(id) {
                shelf.select_only(id);
            }
        });
        row.add_controller(click);

        let shelf = self.clone();
        self.attach_drag_source(
            &row.clone().upcast(),
            move || {
                if shelf.selected.borrow().contains(&id) {
                    shelf.targets()
                } else {
                    vec![id]
                }
            },
            |shelf, ids| shelf.drag_out_succeeded(ids),
        );
        row.upcast()
    }

    // MARK: Show / hide

    pub fn is_visible(&self) -> bool {
        self.window.is_visible() && !self.hiding.get()
    }

    pub fn show(self: &Rc<Self>) {
        if self.is_visible() {
            return;
        }
        if !self.window.is_visible() {
            self.position_at_edge();
            self.window.set_opacity(0.0);
            self.window.set_visible(true);
        }
        self.hiding.set(false);
        self.fade_to(1.0, 150, None);
        self.notify();
    }

    /// Shows the shelf on the screen in use, with keyboard focus, for the
    /// hotkey, tray and command line.
    pub fn show_focused(self: &Rc<Self>) {
        if !self.window.is_visible() {
            // No fixed monitor: the compositor picks the active screen.
            self.window.set_monitor(None);
        }
        // Exclusive takes focus now; switching back to on-demand right after
        // keeps that focus while letting clicks elsewhere take it away.
        self.window.set_keyboard_mode(KeyboardMode::Exclusive);
        self.show();
        let shelf = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(300), move || {
            shelf.window.set_keyboard_mode(KeyboardMode::OnDemand);
        });
        if self.cursor.get().is_none() {
            self.cursor.set(self.visible_items().first().map(|i| i.id));
        }
        self.update_selection_css();
    }

    pub fn hide(self: &Rc<Self>) {
        if !self.is_visible() {
            return;
        }
        self.hiding.set(true);
        let shelf = self.clone();
        self.fade_to(
            0.0,
            180,
            Some(Box::new(move || {
                shelf.hiding.set(false);
                shelf.window.set_visible(false);
                shelf.window.set_opacity(1.0);
                shelf.window.set_keyboard_mode(KeyboardMode::OnDemand);
                shelf.search.set_text("");
                shelf.search_bar.set_search_mode(false);
                shelf.notify();
            })),
        );
        self.notify();
    }

    pub fn toggle_focused(self: &Rc<Self>) {
        if self.is_visible() {
            self.hide()
        } else {
            self.show_focused()
        }
    }

    fn fade_to(self: &Rc<Self>, target: f64, ms: i64, done: Option<Box<dyn FnOnce()>>) {
        if let Some(id) = self.fade.take() {
            id.remove();
        }
        let start = self.window.opacity();
        let t0 = glib::monotonic_time();
        let shelf = self.clone();
        let mut done = done;
        let id = glib::timeout_add_local(Duration::from_millis(16), move || {
            let progress = ((glib::monotonic_time() - t0) as f64 / (ms * 1000) as f64).min(1.0);
            shelf
                .window
                .set_opacity(start + (target - start) * progress);
            if progress < 1.0 {
                return glib::ControlFlow::Continue;
            }
            // Returning Break removes this source; just forget its id.
            let _ = shelf.fade.take();
            if let Some(done) = done.take() {
                done();
            }
            glib::ControlFlow::Break
        });
        self.fade.replace(Some(id));
    }

    fn hide_if_idle_soon(self: &Rc<Self>) {
        let shelf = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(350), move || shelf.hide_if_idle());
    }

    fn hide_if_idle(self: &Rc<Self>) {
        let busy = self.external_drag.get()
            || self.internal_drag.get()
            || self.pending_drops.get() > 0
            || self.jobs.get() > 0
            || self.window.is_active();
        if !self.has_unpinned() && !busy {
            self.hide();
        }
    }

    // MARK: Layout

    /// Re-applies position and size settings, called when the user changes
    /// them from the menu.
    pub fn apply_layout_settings(self: &Rc<Self>) {
        self.refresh();
        self.position_at_edge();
        self.rebuild_edge_windows();
    }

    /// One invisible 1px drop strip per screen, on the shelf's side.
    fn rebuild_edge_windows(self: &Rc<Self>) {
        for window in self.edge_windows.take() {
            window.destroy();
        }
        let settings = self.settings.borrow().clone();
        if !settings.edge_trigger {
            return;
        }
        let Some(display) = gdk::Display::default() else {
            return;
        };
        let monitors = display.monitors();
        let mut windows = Vec::new();
        for i in 0..monitors.n_items() {
            let Some(monitor) = monitors.item(i).and_downcast::<gdk::Monitor>() else {
                continue;
            };
            let edge = gtk::Window::builder()
                .application(&self.app)
                .title("Shoal edge")
                .decorated(false)
                .css_classes(["shoal-edge"])
                .build();
            edge.init_layer_shell();
            edge.set_namespace(Some("shoal-edge"));
            edge.set_layer(Layer::Overlay);
            edge.set_keyboard_mode(KeyboardMode::None);
            edge.set_monitor(Some(&monitor));
            edge.set_anchor(Edge::Top, true);
            edge.set_anchor(Edge::Bottom, true);
            edge.set_anchor(Edge::Left, settings.position.is_left());
            edge.set_anchor(Edge::Right, !settings.position.is_left());
            edge.set_default_size(1, 1);
            let strip = gtk::Box::builder().width_request(1).vexpand(true).build();
            edge.set_child(Some(&strip));
            self.attach_drop_target(&strip.upcast(), Some(monitor));
            edge.set_visible(true);
            windows.push(edge);
        }
        self.edge_windows.replace(windows);
    }

    fn position_at_edge(&self) {
        let pos = self.settings.borrow().position;
        let left = pos.is_left();
        self.window.set_anchor(Edge::Left, left);
        self.window.set_anchor(Edge::Right, !left);
        // Anchoring neither top nor bottom centers the shelf vertically.
        self.window.set_anchor(Edge::Top, pos.is_top());
        self.window.set_anchor(Edge::Bottom, pos.is_bottom());
        for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
            self.window.set_margin(edge, EDGE_MARGIN - SHADOW_PAD);
        }
    }

    fn monitor_height(&self) -> i32 {
        let Some(display) = gdk::Display::default() else {
            return 800;
        };
        self.window
            .monitor()
            .or_else(|| {
                self.window
                    .surface()
                    .and_then(|s| display.monitor_at_surface(&s))
            })
            .or_else(|| display.monitors().item(0).and_downcast::<gdk::Monitor>())
            .map(|m| m.geometry().height())
            .unwrap_or(800)
    }

    fn desired_height(&self) -> i32 {
        let visible = self.visible_items();
        let search = if self.search_bar.is_search_mode() {
            SEARCH_HEIGHT
        } else {
            0
        };
        if visible.is_empty() {
            return EMPTY_HEIGHT + search;
        }
        let count = visible.len() as i32;
        let row_height = self.settings.borrow().size.row_height();
        let pinned_section = if visible.iter().any(|i| i.pinned) {
            SECTION_HEIGHT
        } else {
            0
        };
        let list = count * row_height + (count - 1).max(0) * ROW_SPACING + 16 + pinned_section;
        (HEADER_HEIGHT + search + list).min(self.monitor_height() * 7 / 10)
    }

    fn update_frame(&self) {
        let width = self.settings.borrow().size.width();
        let height = self.desired_height();
        self.root.set_size_request(width, height);
        self.window
            .set_default_size(width + 2 * SHADOW_PAD, height + 2 * SHADOW_PAD);
    }

    /// Items in display order (unpinned first, then pinned), filtered by
    /// the search text.
    fn visible_items(&self) -> Vec<ShelfItem> {
        let query = self.search.text().to_lowercase();
        let items = self.items.borrow();
        let matches = |item: &ShelfItem| {
            if query.is_empty() {
                return true;
            }
            if item.display_name().to_lowercase().contains(&query) {
                return true;
            }
            item.is_snippet()
                && preview::read_snippet_text(item.path())
                    .is_some_and(|t| t.to_lowercase().contains(&query))
        };
        let unpinned = items.iter().filter(|i| !i.pinned);
        let pinned = items.iter().filter(|i| i.pinned);
        unpinned
            .chain(pinned)
            .filter(|i| matches(i))
            .cloned()
            .collect()
    }

    fn refresh(self: &Rc<Self>) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let visible = self.visible_items();
        let mut rows = HashMap::new();
        let mut pinned_header_shown = false;
        for item in &visible {
            if item.pinned && !pinned_header_shown {
                pinned_header_shown = true;
                let section = gtk::Label::builder()
                    .label("PINNED")
                    .xalign(0.0)
                    .css_classes(["section"])
                    .build();
                self.list.append(&section);
            }
            let row = self.build_row(item);
            self.list.append(&row);
            rows.insert(item.id, row);
        }
        self.rows.replace(rows);

        // Drop selection and cursor entries for items no longer shown.
        let shown: Vec<Uuid> = visible.iter().map(|i| i.id).collect();
        self.selected.borrow_mut().retain(|id| shown.contains(id));
        if self.cursor.get().is_some_and(|c| !shown.contains(&c)) {
            self.cursor.set(None);
        }
        self.update_selection_css();

        let items = self.items.borrow().clone();
        let filtering = !self.search.text().is_empty();
        self.empty_hint
            .set_label(if filtering && !items.is_empty() {
                "No matches"
            } else {
                "Drop files here"
            });
        self.stack
            .set_visible_child_name(if visible.is_empty() { "empty" } else { "list" });
        self.clear_button
            .set_visible(items.iter().any(|i| !i.pinned));
        self.title.set_label(&if items.is_empty() {
            "Shoal".into()
        } else {
            format!("Shoal · {}", items.len())
        });
        self.drag_all.set_visible(visible.len() >= 2);
        self.update_frame();
        store::save(&items);
        self.notify();
    }
}

/// Name, subtitle and tooltip for a row.
fn describe(item: &ShelfItem, snippet: Option<&Snippet>) -> (String, String, String) {
    let stem = || {
        item.path()
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| item.display_name())
    };
    match snippet {
        Some(Snippet::Link(url)) => {
            let short = url
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .trim_start_matches("www.")
                .to_string();
            (short, preview::subtitle(snippet.unwrap()), url.clone())
        }
        Some(s @ Snippet::Color { literal, .. }) => {
            (literal.clone(), preview::subtitle(s), literal.clone())
        }
        Some(s @ Snippet::Text { preview: text, .. }) => {
            (stem(), preview::subtitle(s), text.clone())
        }
        None => {
            let mut subtitle = if item.is_stored() {
                item.kind_and_size()
            } else {
                item.folder_name()
            };
            if actions::is_image(item.path())
                && let Some((w, h)) = preview::image_size(item.path())
            {
                subtitle.push_str(&format!(" · {w}×{h}"));
            }
            (item.display_name(), subtitle, item.path.clone())
        }
    }
}

fn row_ancestor(widget: &gtk::Widget) -> Option<gtk::Widget> {
    let mut current = Some(widget.clone());
    while let Some(w) = current {
        if w.has_css_class("row") {
            return Some(w);
        }
        current = w.parent();
    }
    None
}

fn scroll_into_view(row: &gtk::Widget) {
    let Some(scroll) = row
        .ancestor(gtk::ScrolledWindow::static_type())
        .and_downcast::<gtk::ScrolledWindow>()
    else {
        return;
    };
    let Some(point) = row.compute_point(&scroll, &gtk::graphene::Point::new(0.0, 0.0)) else {
        return;
    };
    let adj = scroll.vadjustment();
    let top = f64::from(point.y()) + adj.value();
    let bottom = top + f64::from(row.height());
    if top < adj.value() {
        adj.set_value(top);
    } else if bottom > adj.value() + adj.page_size() {
        adj.set_value(bottom - adj.page_size());
    }
}

fn offers_files(formats: &gdk::ContentFormats) -> bool {
    formats.contains_type(gdk::FileList::static_type())
        || formats.contain_mime_type("text/uri-list")
}

fn offers_image(formats: &gdk::ContentFormats) -> bool {
    formats.contains_type(gdk::Texture::static_type())
        || formats.mime_types().iter().any(|m| m.starts_with("image/"))
}

fn offers_text(formats: &gdk::ContentFormats) -> bool {
    formats.contains_type(glib::Type::STRING)
        || formats.mime_types().iter().any(|m| {
            m.starts_with("text/plain") || matches!(m.as_str(), "UTF8_STRING" | "STRING" | "TEXT")
        })
}

fn beep() {
    if let Some(display) = gdk::Display::default() {
        display.beep();
    }
}

fn open_uri(uri: &str) {
    if let Err(err) = gio::AppInfo::launch_default_for_uri(uri, gio::AppLaunchContext::NONE) {
        eprintln!("Shoal: could not open {uri}: {err}");
    }
}

fn open_path(path: &Path) {
    open_uri(&gio::File::for_path(path).uri());
}

/// Asks the file manager to show the items selected in their folder,
/// falling back to opening the first one's folder.
fn reveal_paths(paths: &[PathBuf]) {
    let uris: Vec<String> = paths
        .iter()
        .map(|p| gio::File::for_path(p).uri().to_string())
        .collect();
    let parent = paths
        .first()
        .and_then(|p| p.parent())
        .map(Path::to_path_buf);
    let fallback = move || {
        if let Some(parent) = &parent {
            open_path(parent);
        }
    };
    let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else {
        fallback();
        return;
    };
    bus.call(
        Some("org.freedesktop.FileManager1"),
        "/org/freedesktop/FileManager1",
        "org.freedesktop.FileManager1",
        "ShowItems",
        Some(&(uris, String::new()).to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        5000,
        gio::Cancellable::NONE,
        move |result| {
            if result.is_err() {
                fallback();
            }
        },
    );
}

/// Delayed briefly so a still-running copy at the destination finishes
/// first. The Trash is recoverable, unlike deleting outright.
fn trash_original_soon(path: PathBuf) {
    glib::timeout_add_local_once(Duration::from_millis(1500), move || {
        if !path.exists() {
            return;
        }
        glib::spawn_future_local(async move {
            match crate::trash::move_original(&path).await {
                Ok(()) => eprintln!(
                    "Shoal: moved original to Trash after drag-out: {}",
                    path.display()
                ),
                Err(err) => eprintln!("Shoal: could not trash original {}: {err}", path.display()),
            }
        });
    });
}

/// Files Shoal created in its own storage get deleted once their item can
/// no longer come back via undo. Delayed so a receiving app finishes
/// copying.
fn schedule_storage_cleanup(item: &ShelfItem) {
    if !item.is_stored() {
        return;
    }
    let path = item.path().to_path_buf();
    glib::timeout_add_local_once(Duration::from_secs(60), move || {
        let _ = std::fs::remove_file(&path);
        if let Some(parent) = path.parent()
            && parent != store::files_dir()
        {
            // Only succeeds when empty.
            let _ = std::fs::remove_dir(parent);
        }
    });
}
