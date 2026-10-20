//! Screen-edge placement for X11 sessions (XFCE and others) that have no
//! Wayland layer-shell protocol. The shelf's window is made
//! override-redirect — the same trick panels and docks use outside of
//! layer-shell — so the window manager never decorates, reparents or
//! repositions it, and Shoal places it itself.

use gdk4_x11::X11Surface;
use gtk::gdk;
use gtk::prelude::*;
use std::cell::OnceCell;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ChangeWindowAttributesAux, ConfigureWindowAux, ConnectionExt, InputFocus, StackMode,
};
use x11rb::rust_connection::RustConnection;

thread_local! {
    static CONN: OnceCell<Option<RustConnection>> = const { OnceCell::new() };
}

fn with_conn<T>(f: impl FnOnce(&RustConnection) -> T) -> Option<T> {
    CONN.with(|cell| {
        cell.get_or_init(|| {
            RustConnection::connect(None)
                .map(|(conn, _)| conn)
                .map_err(|e| eprintln!("Shoal: no X11 connection for window placement: {e}"))
                .ok()
        })
        .as_ref()
        .map(f)
    })
}

pub fn is_x11() -> bool {
    gdk::Display::default()
        .map(|d| d.downcast::<gdk4_x11::X11Display>().is_ok())
        .unwrap_or(false)
}

fn xid(window: &gtk::Window) -> Option<u32> {
    let surface = window.surface()?;
    Some(surface.downcast::<X11Surface>().ok()?.xid() as u32)
}

/// Creates the window's X11 surface (without mapping it) and marks it
/// override-redirect. Must be called once, before the window is first
/// shown: changing the attribute after the window manager has already
/// taken the (non-override-redirect) window under management is too late.
pub fn prepare(window: &gtk::Window) {
    gtk::prelude::WidgetExt::realize(window);
    let Some(id) = xid(window) else { return };
    with_conn(|conn| {
        let _ = conn
            .change_window_attributes(id, &ChangeWindowAttributesAux::new().override_redirect(1));
        let _ = conn.flush();
    });
}

/// Moves, resizes and raises the window. Since it's override-redirect, no
/// window manager will do this for us.
pub fn place(window: &gtk::Window, x: i32, y: i32, width: i32, height: i32) {
    let Some(id) = xid(window) else { return };
    with_conn(|conn| {
        let _ = conn.configure_window(
            id,
            &ConfigureWindowAux::new()
                .x(x)
                .y(y)
                .width(width as u32)
                .height(height as u32)
                .stack_mode(StackMode::ABOVE),
        );
        let _ = conn.flush();
    });
}

/// Takes keyboard focus immediately, approximating layer-shell's
/// `KeyboardMode::Exclusive` for the moment the shelf is opened by hotkey
/// or tray.
pub fn focus(window: &gtk::Window) {
    let Some(id) = xid(window) else { return };
    with_conn(|conn| {
        let _ = conn.set_input_focus(InputFocus::PARENT, id, x11rb::CURRENT_TIME);
        let _ = conn.flush();
    });
}

/// The monitor to place the shelf on: the one under the pointer, falling
/// back to the first monitor. GDK4 has no portable "global pointer
/// position" query (Wayland does not allow it), but X11 does, via
/// `QueryPointer` on the root window.
pub fn pointer_monitor(display: &gdk::Display) -> Option<gdk::Monitor> {
    let (x, y) = with_conn(|conn| -> Option<(i32, i32)> {
        let screen = &conn.setup().roots[0];
        let reply = conn.query_pointer(screen.root).ok()?.reply().ok()?;
        Some((reply.root_x as i32, reply.root_y as i32))
    })
    .flatten()?;
    let monitors = display.monitors();
    for i in 0..monitors.n_items() {
        let monitor = monitors.item(i).and_downcast::<gdk::Monitor>()?;
        let geom = monitor.geometry();
        if x >= geom.x()
            && x < geom.x() + geom.width()
            && y >= geom.y()
            && y < geom.y() + geom.height()
        {
            return Some(monitor);
        }
    }
    monitors.item(0).and_downcast::<gdk::Monitor>()
}
