//! System tray icon (StatusNotifierItem), the Linux stand-in for the macOS
//! menu bar item. Menu clicks are forwarded to the GTK main loop as commands.

use crate::settings::{ShelfPosition, ShelfSize};
use ksni::menu::{CheckmarkItem, RadioGroup, RadioItem, StandardItem, SubMenu};
use ksni::{MenuItem, ToolTip};

pub const ICON: &str = "shoal";

pub enum Cmd {
    Action(&'static str),
    Position(ShelfPosition),
    Size(ShelfSize),
}

#[derive(Clone, Default)]
pub struct TrayState {
    pub shelf_visible: bool,
    pub can_bring_back: bool,
    pub has_unpinned: bool,
    pub watch_clipboard: bool,
    pub move_out: bool,
    pub edge_trigger: bool,
    pub text_drags: bool,
    pub start_at_login: bool,
    pub position: ShelfPosition,
    pub size: ShelfSize,
}

pub struct ShoalTray {
    pub state: TrayState,
    tx: async_channel::Sender<Cmd>,
}

impl ShoalTray {
    pub fn new(state: TrayState, tx: async_channel::Sender<Cmd>) -> Self {
        Self { state, tx }
    }

    fn send(&self, cmd: Cmd) {
        let _ = self.tx.try_send(cmd);
    }
}

fn action(label: &str, enabled: bool, name: &'static str) -> MenuItem<ShoalTray> {
    StandardItem {
        label: label.into(),
        enabled,
        activate: Box::new(move |t: &mut ShoalTray| t.send(Cmd::Action(name))),
        ..Default::default()
    }
    .into()
}

fn toggle(label: &str, checked: bool, name: &'static str) -> MenuItem<ShoalTray> {
    CheckmarkItem {
        label: label.into(),
        checked,
        activate: Box::new(move |t: &mut ShoalTray| t.send(Cmd::Action(name))),
        ..Default::default()
    }
    .into()
}

impl ksni::Tray for ShoalTray {
    fn id(&self) -> String {
        "shoal".into()
    }

    fn title(&self) -> String {
        "Shoal".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        // Send pixels directly: a sandbox's icon theme may not be visible to
        // the host tray. Cache the small sizes commonly used by desktop panels.
        static ICONS: std::sync::OnceLock<Vec<ksni::Icon>> = std::sync::OnceLock::new();
        ICONS
            .get_or_init(|| {
                let source = image::load_from_memory(include_bytes!("../assets/shoal-512.png"))
                    .expect("valid bundled Shoal icon");
                [16, 22, 32, 64]
                    .into_iter()
                    .map(|size| {
                        let mut rgba = source
                            .resize_exact(size, size, image::imageops::FilterType::Lanczos3)
                            .into_rgba8()
                            .into_raw();
                        for pixel in rgba.as_chunks_mut::<4>().0 {
                            pixel.rotate_right(1); // RGBA to network-order ARGB.
                        }
                        ksni::Icon {
                            width: size as i32,
                            height: size as i32,
                            data: rgba,
                        }
                    })
                    .collect()
            })
            .clone()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: "Shoal".into(),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(Cmd::Action("toggle-shelf"));
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let s = &self.state;
        let positions = SubMenu {
            label: "Window Position".into(),
            submenu: vec![
                RadioGroup {
                    selected: ShelfPosition::ALL
                        .iter()
                        .position(|p| *p == s.position)
                        .unwrap_or(0),
                    select: Box::new(|t: &mut Self, i| {
                        t.send(Cmd::Position(ShelfPosition::ALL[i]))
                    }),
                    options: ShelfPosition::ALL
                        .iter()
                        .map(|p| RadioItem {
                            label: p.title().into(),
                            ..Default::default()
                        })
                        .collect(),
                }
                .into(),
            ],
            ..Default::default()
        };
        let sizes = SubMenu {
            label: "Window Size".into(),
            submenu: vec![
                RadioGroup {
                    selected: ShelfSize::ALL
                        .iter()
                        .position(|z| *z == s.size)
                        .unwrap_or(0),
                    select: Box::new(|t: &mut Self, i| t.send(Cmd::Size(ShelfSize::ALL[i]))),
                    options: ShelfSize::ALL
                        .iter()
                        .map(|z| RadioItem {
                            label: z.title().into(),
                            ..Default::default()
                        })
                        .collect(),
                }
                .into(),
            ],
            ..Default::default()
        };
        vec![
            action(
                "Bring Back Last Removed Files",
                s.can_bring_back,
                "bring-back",
            ),
            action("Add Clipboard Contents to Shoal", true, "add-clipboard"),
            toggle(
                "Automatically Add Clipboard",
                s.watch_clipboard,
                "watch-clipboard",
            ),
            MenuItem::Separator,
            positions.into(),
            sizes.into(),
            action(
                if s.shelf_visible {
                    "Hide Shoal"
                } else {
                    "Show Shoal"
                },
                true,
                "toggle-shelf",
            ),
            MenuItem::Separator,
            toggle("Remove Original After Drag-Out", s.move_out, "move-out"),
            toggle(
                "Show Shelf When Dragging to Screen Edge",
                s.edge_trigger,
                "edge-trigger",
            ),
            toggle("Show Shelf for Text Drags", s.text_drags, "text-drags"),
            toggle("Start at Login", s.start_at_login, "start-at-login"),
            action("Change Global Shortcut…", true, "configure-shortcut"),
            MenuItem::Separator,
            action("Clear Shoal (Keeps Pinned)", s.has_unpinned, "clear"),
            action("About Shoal", true, "about"),
            MenuItem::Separator,
            action("Quit Shoal", true, "quit"),
        ]
    }
}
