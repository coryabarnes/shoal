//! Shoal: a drag-and-drop shelf for Linux. Drag a file, park it on the
//! shelf, drop it where it belongs. Based on Ledge for macOS.

mod actions;
mod clipboard;
mod hotkey;
mod preview;
mod settings;
mod shelf;
mod store;
mod trash;
mod tray;

use gtk::prelude::*;
use gtk::{gio, glib};
use hotkey::Hotkey;
use ksni::blocking::TrayMethods;
use settings::{Settings, ShelfPosition, ShelfSize};
use shelf::Shelf;
use std::cell::{OnceCell, RefCell};
use std::path::PathBuf;
use std::process::Child;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tray::{Cmd, ShoalTray, TrayState};

const APP_ID: &str = "io.github.coryabarnes.Shoal";

fn app_id() -> &'static str {
    // Match the desktop file exported by snapd, so Wayland groups the window
    // under Shoal's launcher and keeps it separate from a native installation.
    if std::env::var_os("SNAP").is_some() {
        "shoal_io.github.coryabarnes.Shoal"
    } else {
        APP_ID
    }
}

/// `--self-test` runs a private instance: no tray icon, clipboard watcher or
/// hotkey, and no clash with a Shoal already running.
fn is_self_test() -> bool {
    std::env::args().any(|a| a == "--self-test")
}

const USAGE: &str = "Usage: shoal [--toggle | --show | --hide | --quit]

  (no option)  start Shoal, or show the shelf if it is already running
  --toggle     show or hide the shelf (Shoal also binds F5 for this)
  --show       show the shelf
  --hide       hide the shelf
  --quit       quit the running instance
";

struct App {
    shelf: Rc<Shelf>,
    settings: Rc<RefCell<Settings>>,
    tray: RefCell<Option<ksni::blocking::Handle<ShoalTray>>>,
    clipboard_enabled: Arc<AtomicBool>,
    clipboard_child: RefCell<Option<Child>>,
    hotkey: RefCell<Option<Rc<Hotkey>>>,
}

impl App {
    fn tray_state(&self) -> TrayState {
        let s = self.settings.borrow();
        TrayState {
            shelf_visible: self.shelf.is_visible(),
            can_bring_back: self.shelf.can_bring_back(),
            has_unpinned: self.shelf.has_unpinned(),
            watch_clipboard: s.watch_clipboard,
            move_out: s.remove_original_on_drag_out,
            edge_trigger: s.edge_trigger,
            text_drags: s.include_text_drags,
            start_at_login: autostart_file().exists(),
            position: s.position,
            size: s.size,
        }
    }
}

fn autostart_file() -> PathBuf {
    glib::user_config_dir()
        .join("autostart")
        .join("shoal.desktop")
}

fn set_start_at_login(enabled: bool) {
    let path = autostart_file();
    let result = if enabled {
        let exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "shoal".into());
        let entry = format!(
            "[Desktop Entry]\nType=Application\nName=Shoal\nComment=Drag-and-drop shelf\nExec=\"{exe}\"\nIcon={}\nX-GNOME-Autostart-enabled=true\n",
            tray::ICON
        );
        path.parent()
            .map(std::fs::create_dir_all)
            .transpose()
            .and_then(|_| std::fs::write(&path, entry))
    } else {
        std::fs::remove_file(&path).or_else(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(e)
            }
        })
    };
    if let Err(err) = result {
        eprintln!("Shoal: could not change login item: {err}");
    }
}

fn build_menu() -> gio::Menu {
    let menu = gio::Menu::new();

    let clip = gio::Menu::new();
    clip.append(
        Some("Bring Back Last Removed Files"),
        Some("app.bring-back"),
    );
    clip.append(
        Some("Add Clipboard Contents to Shoal"),
        Some("app.add-clipboard"),
    );
    clip.append(
        Some("Automatically Add Clipboard"),
        Some("app.watch-clipboard"),
    );
    menu.append_section(None, &clip);

    let layout = gio::Menu::new();
    let positions = gio::Menu::new();
    for pos in ShelfPosition::ALL {
        let item = gio::MenuItem::new(Some(pos.title()), None);
        item.set_action_and_target_value(Some("app.position"), Some(&pos.id().to_variant()));
        positions.append_item(&item);
    }
    layout.append_submenu(Some("Window Position"), &positions);
    let sizes = gio::Menu::new();
    for size in ShelfSize::ALL {
        let item = gio::MenuItem::new(Some(size.title()), None);
        item.set_action_and_target_value(Some("app.size"), Some(&size.id().to_variant()));
        sizes.append_item(&item);
    }
    layout.append_submenu(Some("Window Size"), &sizes);
    layout.append(Some("Hide Shoal"), Some("app.toggle-shelf"));
    menu.append_section(None, &layout);

    let prefs = gio::Menu::new();
    prefs.append(Some("Remove Original After Drag-Out"), Some("app.move-out"));
    prefs.append(
        Some("Show Shelf When Dragging to Screen Edge"),
        Some("app.edge-trigger"),
    );
    prefs.append(Some("Show Shelf for Text Drags"), Some("app.text-drags"));
    prefs.append(Some("Start at Login"), Some("app.start-at-login"));
    prefs.append(
        Some("Change Global Shortcut…"),
        Some("app.configure-shortcut"),
    );
    menu.append_section(None, &prefs);

    let end = gio::Menu::new();
    end.append(Some("Clear Shoal (Keeps Pinned)"), Some("app.clear"));
    end.append(Some("About Shoal"), Some("app.about"));
    end.append(Some("Quit Shoal"), Some("app.quit"));
    menu.append_section(None, &end);

    menu
}

fn show_about(app: &gtk::Application) {
    let about = gtk::AboutDialog::builder()
        .application(app)
        .program_name("Shoal")
        .version(env!("CARGO_PKG_VERSION"))
        .comments("A free, open source drag-and-drop shelf for Linux.\nPark files on the shelf, drop them where they belong.")
        .authors(["Cory Barnes https://github.com/coryabarnes"])
        .copyright("© 2026 Cory Barnes")
        .website("https://github.com/coryabarnes/shoal")
        .website_label("Shoal on GitHub")
        .license_type(gtk::License::MitX11)
        .logo_icon_name(tray::ICON)
        .build();
    about.add_credit_section(
        "Based on Ledge for macOS",
        &["Matt Marcotte https://github.com/marcotteink/ledge"],
    );
    about.present();
}

fn add_actions(app: &gtk::Application, state: &Rc<App>, sync: &Rc<dyn Fn()>) {
    let simple = |name: &str, f: Box<dyn Fn()>| {
        let action = gio::SimpleAction::new(name, None);
        action.connect_activate(move |_, _| f());
        app.add_action(&action);
    };
    let shelf = state.shelf.clone();
    simple(
        "bring-back",
        Box::new(move || shelf.bring_back_last_removed()),
    );
    let shelf = state.shelf.clone();
    simple("toggle-shelf", Box::new(move || shelf.toggle_focused()));
    let shelf = state.shelf.clone();
    simple("clear", Box::new(move || shelf.clear_all()));
    let app_c = app.clone();
    simple("about", Box::new(move || show_about(&app_c)));
    let app_c = app.clone();
    simple("quit", Box::new(move || app_c.quit()));
    let st = state.clone();
    simple(
        "configure-shortcut",
        Box::new(move || match st.hotkey.borrow().as_ref() {
            Some(hotkey) => hotkey.configure(),
            None => eprintln!(
                "Shoal: global shortcut unavailable; bind `shoal --toggle` in System Settings"
            ),
        }),
    );
    let shelf = state.shelf.clone();
    simple(
        "add-clipboard",
        Box::new(move || {
            let shelf = shelf.clone();
            glib::spawn_future_local(async move {
                if let Ok(content) = gio::spawn_blocking(clipboard::read).await {
                    shelf.capture(content, false);
                }
            });
        }),
    );

    // Boolean settings: activating toggles them, and menus show checkmarks.
    let toggle = |name: &str, initial: bool, apply: Box<dyn Fn(bool)>| {
        let action = gio::SimpleAction::new_stateful(name, None, &initial.to_variant());
        let sync = sync.clone();
        action.connect_change_state(move |action, value| {
            let Some(on) = value.and_then(|v| v.get::<bool>()) else {
                return;
            };
            action.set_state(&on.to_variant());
            apply(on);
            sync();
        });
        app.add_action(&action);
    };
    let s = state.settings.borrow().clone();
    let st = state.clone();
    toggle(
        "watch-clipboard",
        s.watch_clipboard,
        Box::new(move |on| {
            st.settings.borrow_mut().watch_clipboard = on;
            st.settings.borrow().save();
            st.clipboard_enabled.store(on, Ordering::Relaxed);
        }),
    );
    let st = state.clone();
    toggle(
        "move-out",
        s.remove_original_on_drag_out,
        Box::new(move |on| {
            st.settings.borrow_mut().remove_original_on_drag_out = on;
            st.settings.borrow().save();
        }),
    );
    let st = state.clone();
    toggle(
        "edge-trigger",
        s.edge_trigger,
        Box::new(move |on| {
            st.settings.borrow_mut().edge_trigger = on;
            st.settings.borrow().save();
            st.shelf.apply_layout_settings();
        }),
    );
    let st = state.clone();
    toggle(
        "text-drags",
        s.include_text_drags,
        Box::new(move |on| {
            st.settings.borrow_mut().include_text_drags = on;
            st.settings.borrow().save();
        }),
    );
    toggle(
        "start-at-login",
        autostart_file().exists(),
        Box::new(set_start_at_login),
    );

    // Radio settings carry the chosen id as their state.
    let radio = |name: &str, initial: &str, apply: Box<dyn Fn(&str) -> bool>| {
        let action = gio::SimpleAction::new_stateful(
            name,
            Some(glib::VariantTy::STRING),
            &initial.to_variant(),
        );
        let sync = sync.clone();
        action.connect_change_state(move |action, value| {
            let Some(id) = value.and_then(|v| v.get::<String>()) else {
                return;
            };
            if apply(&id) {
                action.set_state(&id.to_variant());
                sync();
            }
        });
        app.add_action(&action);
    };
    let st = state.clone();
    radio(
        "position",
        s.position.id(),
        Box::new(move |id| {
            let Some(pos) = ShelfPosition::from_id(id) else {
                return false;
            };
            st.settings.borrow_mut().position = pos;
            st.settings.borrow().save();
            st.shelf.apply_layout_settings();
            true
        }),
    );
    let st = state.clone();
    radio(
        "size",
        s.size.id(),
        Box::new(move |id| {
            let Some(size) = ShelfSize::from_id(id) else {
                return false;
            };
            st.settings.borrow_mut().size = size;
            st.settings.borrow().save();
            st.shelf.apply_layout_settings();
            true
        }),
    );
}

fn startup(app: &gtk::Application) -> Rc<App> {
    gtk::Window::set_default_icon_name(tray::ICON);
    let settings = Rc::new(RefCell::new(Settings::load()));
    let menu = build_menu();
    let shelf = Shelf::new(app, settings.clone(), menu.upcast());
    let watch_clipboard = settings.borrow().watch_clipboard;
    let state = Rc::new(App {
        shelf,
        clipboard_enabled: Arc::new(AtomicBool::new(watch_clipboard)),
        settings,
        tray: RefCell::default(),
        clipboard_child: RefCell::default(),
        hotkey: RefCell::default(),
    });

    // Keeps the tray menu and action availability in step with the shelf.
    let sync: Rc<dyn Fn()> = {
        let weak = Rc::downgrade(&state);
        let app = app.clone();
        Rc::new(move || {
            let Some(state) = weak.upgrade() else { return };
            let enable = |name: &str, on: bool| {
                if let Some(action) = app.lookup_action(name).and_downcast::<gio::SimpleAction>() {
                    action.set_enabled(on);
                }
            };
            enable("bring-back", state.shelf.can_bring_back());
            enable("clear", state.shelf.has_unpinned());
            let tray_state = state.tray_state();
            if let Some(handle) = state.tray.borrow().as_ref() {
                handle.update(|tray| tray.state = tray_state);
            }
        })
    };
    add_actions(app, &state, &sync);
    let sync_c = sync.clone();
    state.shelf.set_on_changed(move || sync_c());

    let self_test = is_self_test();
    let (tx, rx) = async_channel::unbounded::<Cmd>();
    if !self_test {
        match ShoalTray::new(state.tray_state(), tx).spawn() {
            Ok(handle) => {
                state.tray.replace(Some(handle));
            }
            Err(err) => eprintln!("Shoal: no system tray available ({err}); use `shoal --toggle`"),
        }
    }
    let app_c = app.clone();
    glib::spawn_future_local(async move {
        while let Ok(cmd) = rx.recv().await {
            match cmd {
                Cmd::Action(name) => app_c.activate_action(name, None),
                Cmd::Position(pos) => app_c.change_action_state("position", &pos.id().to_variant()),
                Cmd::Size(size) => app_c.change_action_state("size", &size.id().to_variant()),
            }
        }
    });

    let (clip_tx, clip_rx) = async_channel::unbounded();
    if !self_test {
        state
            .clipboard_child
            .replace(clipboard::watch(state.clipboard_enabled.clone(), clip_tx));
    }
    let shelf = state.shelf.clone();
    glib::spawn_future_local(async move {
        while let Ok(content) = clip_rx.recv().await {
            shelf.capture(content, true);
        }
    });

    let shelf = state.shelf.clone();
    state.hotkey.replace(
        (!self_test)
            .then(|| {
                Hotkey::start(app_id(), move |id| {
                    if id == hotkey::TOGGLE_SHELF {
                        shelf.toggle_focused();
                    }
                })
            })
            .flatten(),
    );

    state.shelf.restore();
    sync();
    eprintln!("Shoal is running");
    state
}

fn main() -> glib::ExitCode {
    let app = gtk::Application::builder()
        .application_id(app_id())
        .flags(if is_self_test() {
            gio::ApplicationFlags::HANDLES_COMMAND_LINE | gio::ApplicationFlags::NON_UNIQUE
        } else {
            gio::ApplicationFlags::HANDLES_COMMAND_LINE
        })
        .build();
    let state: Rc<OnceCell<Rc<App>>> = Rc::default();

    let st = state.clone();
    app.connect_startup(move |app| {
        // Lives in the tray with no windows open, like a menu bar app.
        std::mem::forget(app.hold());
        let _ = st.set(startup(app));
    });

    let st = state.clone();
    app.connect_command_line(move |app, cmd| {
        let args: Vec<String> = cmd
            .arguments()
            .iter()
            .skip(1)
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let Some(state) = st.get() else {
            return glib::ExitCode::FAILURE;
        };
        match args.first().map(String::as_str) {
            None if cmd.is_remote() => state.shelf.show_focused(),
            None => {}
            Some("--toggle") => state.shelf.toggle_focused(),
            Some("--show") => state.shelf.show_focused(),
            Some("--hide") => state.shelf.hide(),
            Some("--quit") => app.quit(),
            #[cfg(debug_assertions)]
            Some("--self-test") if !cmd.is_remote() => {
                // Never let the test touch a real shelf.
                let temp = std::env::temp_dir();
                let isolated = ["XDG_DATA_HOME", "XDG_CONFIG_HOME"].iter().all(|v| {
                    std::env::var_os(v).is_some_and(|d| PathBuf::from(d).starts_with(&temp))
                });
                if !isolated {
                    eprintln!(
                        "--self-test needs XDG_DATA_HOME and XDG_CONFIG_HOME under {}",
                        temp.display()
                    );
                    std::process::exit(2);
                }
                let shelf = state.shelf.clone();
                glib::spawn_future_local(async move {
                    let ok = shelf::selftest::run(shelf).await;
                    std::process::exit(if ok { 0 } else { 1 });
                });
            }
            Some("-h" | "--help") => {
                cmd.print_literal(USAGE);
                return glib::ExitCode::SUCCESS;
            }
            Some(other) => {
                cmd.printerr_literal(&format!("shoal: unknown option {other}\n\n{USAGE}"));
                return glib::ExitCode::FAILURE;
            }
        }
        glib::ExitCode::SUCCESS
    });

    let st = state.clone();
    app.connect_shutdown(move |_| {
        if let Some(state) = st.get() {
            if let Some(mut child) = state.clipboard_child.take() {
                let _ = child.kill();
            }
            if let Some(handle) = state.tray.take() {
                handle.shutdown().wait();
            }
        }
    });

    app.run()
}
