//! System-wide hotkey through the XDG GlobalShortcuts portal. Wayland does
//! not let apps grab keys themselves; the portal asks the desktop to bind
//! one for us (KDE shows a one-time confirmation) and signals each press.

use gtk::gio::{self, prelude::*};
use gtk::glib::{self, variant::ObjectPath};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SHORTCUTS: &str = "org.freedesktop.portal.GlobalShortcuts";

pub const TOGGLE_SHELF: &str = "toggle-shelf";
const TOGGLE_DESCRIPTION: &str = "Show or hide Shoal";
const TOGGLE_TRIGGER: &str = "F5";

type Options = HashMap<String, glib::Variant>;

pub struct Hotkey {
    bus: gio::DBusConnection,
    session: RefCell<Option<ObjectPath>>,
    tokens: Cell<u32>,
    _activated: RefCell<Option<gio::SignalSubscription>>,
}

impl Hotkey {
    /// Binds the toggle shortcut and calls `on_press` with the shortcut id
    /// each time it is pressed. Failures are logged; the app keeps working
    /// through `shoal --toggle`.
    pub fn start(app_id: &str, on_press: impl Fn(&str) + 'static) -> Option<Rc<Self>> {
        let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)
            .map_err(|e| eprintln!("Shoal: no session bus for the global shortcut: {e}"))
            .ok()?;
        let hotkey = Rc::new(Self {
            bus,
            session: RefCell::default(),
            tokens: Cell::new(0),
            _activated: RefCell::default(),
        });
        let app_id = app_id.to_string();
        let this = hotkey.clone();
        glib::spawn_future_local(async move {
            if let Err(err) = this.setup(&app_id, on_press).await {
                eprintln!(
                    "Shoal: global shortcut unavailable ({err}); bind `shoal --toggle` in System Settings instead"
                );
            }
        });
        Some(hotkey)
    }

    fn token(&self) -> String {
        self.tokens.set(self.tokens.get() + 1);
        format!("shoal{}_{}", std::process::id(), self.tokens.get())
    }

    /// Calls a portal method that answers through a Request object's
    /// Response signal, and waits for that response.
    async fn request(
        &self,
        method: &str,
        params: impl FnOnce(Options) -> glib::Variant,
    ) -> Result<Options, String> {
        let token = self.token();
        let sender = self
            .bus
            .unique_name()
            .ok_or("no unique bus name")?
            .trim_start_matches(':')
            .replace('.', "_");
        let path = format!("{PORTAL_PATH}/request/{sender}/{token}");
        let (tx, rx) = async_channel::bounded::<glib::Variant>(1);
        // Subscribe before calling so the response cannot slip past.
        let _subscription = self.bus.subscribe_to_signal(
            Some(PORTAL),
            Some("org.freedesktop.portal.Request"),
            Some("Response"),
            Some(&path),
            None,
            gio::DBusSignalFlags::NONE,
            move |signal| {
                let _ = tx.try_send(signal.parameters.clone());
            },
        );
        let mut options = Options::new();
        options.insert("handle_token".into(), token.to_variant());
        self.bus
            .call_future(
                Some(PORTAL),
                PORTAL_PATH,
                SHORTCUTS,
                method,
                Some(&params(options)),
                Some(glib::VariantTy::new("(o)").unwrap()),
                gio::DBusCallFlags::NONE,
                -1,
            )
            .await
            .map_err(|e| format!("{method}: {e}"))?;
        let reply = rx.recv().await.map_err(|e| e.to_string())?;
        let (code, results) = reply
            .get::<(u32, Options)>()
            .ok_or_else(|| format!("{method}: unexpected reply {reply}"))?;
        match code {
            0 => Ok(results),
            1 => Err(format!("{method} was cancelled")),
            _ => Err(format!("{method} failed")),
        }
    }

    async fn setup(
        self: &Rc<Self>,
        app_id: &str,
        on_press: impl Fn(&str) + 'static,
    ) -> Result<(), String> {
        // Host (non-sandboxed) apps tell the portal who they are, so the
        // shortcut is saved under Shoal's name. Already registered is fine.
        let _ = self
            .bus
            .call_future(
                Some(PORTAL),
                PORTAL_PATH,
                "org.freedesktop.host.portal.Registry",
                "Register",
                Some(&(app_id, Options::new()).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                -1,
            )
            .await;

        let results = self
            .request("CreateSession", |mut options| {
                options.insert("session_handle_token".into(), self.token().to_variant());
                (options,).to_variant()
            })
            .await?;
        let session = results
            .get("session_handle")
            .and_then(|v| {
                v.get::<String>()
                    .or_else(|| v.get::<ObjectPath>().map(|p| p.as_str().to_string()))
            })
            .and_then(|s| ObjectPath::try_from(s).ok())
            .ok_or("CreateSession returned no session")?;
        self.session.replace(Some(session.clone()));

        let this = self.clone();
        let subscription = self.bus.subscribe_to_signal(
            Some(PORTAL),
            Some(SHORTCUTS),
            Some("Activated"),
            Some(PORTAL_PATH),
            None,
            gio::DBusSignalFlags::NONE,
            move |signal| {
                let Some((session, id, _, _)) = signal
                    .parameters
                    .get::<(ObjectPath, String, u64, Options)>()
                else {
                    return;
                };
                if this.session.borrow().as_ref() == Some(&session) {
                    on_press(&id);
                }
            },
        );
        self._activated.replace(Some(subscription));

        let listed = self
            .request("ListShortcuts", |options| {
                (session.clone(), options).to_variant()
            })
            .await?;
        let bound = listed
            .get("shortcuts")
            .and_then(|v| v.get::<Vec<(String, Options)>>())
            .unwrap_or_default();
        // Registered with a key: done. Registered without one (an earlier
        // request never finished): ask again.
        let has_key = bound.iter().any(|(id, props)| {
            id == TOGGLE_SHELF
                && props
                    .get("trigger_description")
                    .and_then(|v| v.get::<String>())
                    .is_some_and(|t| !t.is_empty())
        });
        if has_key {
            return Ok(());
        }

        let mut props = Options::new();
        props.insert("description".into(), TOGGLE_DESCRIPTION.to_variant());
        props.insert("preferred_trigger".into(), TOGGLE_TRIGGER.to_variant());
        let shortcuts = vec![(TOGGLE_SHELF.to_string(), props)];
        self.request("BindShortcuts", |options| {
            (session.clone(), shortcuts, String::new(), options).to_variant()
        })
        .await?;
        eprintln!("Shoal: bound global shortcut {TOGGLE_TRIGGER}");
        Ok(())
    }

    /// Opens the desktop's settings for Shoal's shortcut so the key can be
    /// changed.
    pub fn configure(&self) {
        let Some(session) = self.session.borrow().clone() else {
            eprintln!("Shoal: global shortcut not available; nothing to configure");
            return;
        };
        self.bus.call(
            Some(PORTAL),
            PORTAL_PATH,
            SHORTCUTS,
            "ConfigureShortcuts",
            Some(&(session, String::new(), Options::new()).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            -1,
            gio::Cancellable::NONE,
            |result| {
                if let Err(err) = result {
                    eprintln!("Shoal: could not open shortcut settings: {err}");
                }
            },
        );
    }
}
