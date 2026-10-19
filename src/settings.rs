//! User preferences, stored as JSON under the XDG config dir.

use gtk::glib;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ShelfPosition {
    LeftTop,
    LeftMiddle,
    LeftBottom,
    RightTop,
    #[default]
    RightMiddle,
    RightBottom,
}

impl ShelfPosition {
    // Wayland does not let clients read the global pointer position, so the
    // macOS "At Mouse Pointer" option has no equivalent here.
    pub const ALL: [Self; 6] = [
        Self::LeftTop,
        Self::LeftMiddle,
        Self::LeftBottom,
        Self::RightTop,
        Self::RightMiddle,
        Self::RightBottom,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::LeftTop => "left-top",
            Self::LeftMiddle => "left-middle",
            Self::LeftBottom => "left-bottom",
            Self::RightTop => "right-top",
            Self::RightMiddle => "right-middle",
            Self::RightBottom => "right-bottom",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.id() == id)
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::LeftTop => "Left Edge, Top",
            Self::LeftMiddle => "Left Edge, Middle",
            Self::LeftBottom => "Left Edge, Bottom",
            Self::RightTop => "Right Edge, Top",
            Self::RightMiddle => "Right Edge, Middle",
            Self::RightBottom => "Right Edge, Bottom",
        }
    }

    pub fn is_left(self) -> bool {
        matches!(self, Self::LeftTop | Self::LeftMiddle | Self::LeftBottom)
    }

    pub fn is_top(self) -> bool {
        matches!(self, Self::LeftTop | Self::RightTop)
    }

    pub fn is_bottom(self) -> bool {
        matches!(self, Self::LeftBottom | Self::RightBottom)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ShelfSize {
    Small,
    #[default]
    Medium,
    Large,
}

impl ShelfSize {
    pub const ALL: [Self; 3] = [Self::Small, Self::Medium, Self::Large];

    pub fn id(self) -> &'static str {
        match self {
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.id() == id)
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Small => "Small",
            Self::Medium => "Medium",
            Self::Large => "Large",
        }
    }

    pub fn width(self) -> i32 {
        match self {
            Self::Small => 190,
            Self::Medium => 230,
            Self::Large => 284,
        }
    }

    pub fn row_height(self) -> i32 {
        match self {
            Self::Small => 46,
            Self::Medium => 56,
            Self::Large => 68,
        }
    }

    pub fn icon_size(self) -> i32 {
        match self {
            Self::Small => 26,
            Self::Medium => 32,
            Self::Large => 42,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub position: ShelfPosition,
    pub size: ShelfSize,
    pub watch_clipboard: bool,
    pub remove_original_on_drag_out: bool,
    pub include_text_drags: bool,
    /// Stands in for macOS drag-pasteboard polling: a 1px drop strip on the
    /// shelf's screen edge that opens the shelf when a drag reaches it.
    pub edge_trigger: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            position: ShelfPosition::default(),
            size: ShelfSize::default(),
            watch_clipboard: false,
            remove_original_on_drag_out: false,
            include_text_drags: true,
            edge_trigger: true,
        }
    }
}

impl Settings {
    fn path() -> PathBuf {
        glib::user_config_dir().join("shoal").join("settings.json")
    }

    pub fn load() -> Self {
        fs::read(Self::path())
            .ok()
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        match serde_json::to_vec_pretty(self) {
            Ok(data) => {
                if let Err(err) = fs::write(&path, data) {
                    eprintln!("Shoal: could not save settings: {err}");
                }
            }
            Err(err) => eprintln!("Shoal: could not encode settings: {err}"),
        }
    }
}
