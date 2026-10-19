//! Shelf items and the on-disk storage behind them.

use gtk::glib;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShelfItem {
    pub id: Uuid,
    pub path: String,
    /// Pinned items stay on the shelf after being dragged out, for things
    /// reused over and over: signatures, templates, logos.
    #[serde(default)]
    pub pinned: bool,
}

impl ShelfItem {
    pub fn new(path: String) -> Self {
        Self {
            id: Uuid::new_v4(),
            path,
            pinned: false,
        }
    }

    pub fn path(&self) -> &Path {
        Path::new(&self.path)
    }

    pub fn exists(&self) -> bool {
        self.path().exists()
    }

    pub fn display_name(&self) -> String {
        self.path()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.clone())
    }

    /// True for items living in Shoal's own storage: text snippets,
    /// clipboard images, dropped images and quick-action results.
    pub fn is_stored(&self) -> bool {
        self.path
            .starts_with(&format!("{}/", files_dir().display()))
    }

    /// A text snippet Shoal created from copied or dropped text.
    pub fn is_snippet(&self) -> bool {
        self.is_stored() && self.path().extension().is_some_and(|e| e == "txt")
    }

    /// The parent folder's name, shown under real files.
    pub fn folder_name(&self) -> String {
        self.path()
            .parent()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".into())
    }

    /// Kind and size, shown under items in Shoal's own storage, which
    /// would otherwise all show the meaningless folder "Files".
    pub fn kind_and_size(&self) -> String {
        let kind = self
            .path()
            .extension()
            .map(|e| e.to_string_lossy().to_uppercase())
            .filter(|e| !e.is_empty())
            .unwrap_or_else(|| "File".into());
        match fs::metadata(&self.path) {
            Ok(meta) if meta.is_file() => format!("{kind} · {}", format_bytes(meta.len())),
            _ => kind,
        }
    }
}

pub fn base_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = glib::user_data_dir().join("shoal");
        let _ = fs::create_dir_all(&dir);
        dir
    })
}

/// Holds files Shoal creates itself: text snippets, clipboard images,
/// dropped images and quick-action results.
pub fn files_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = base_dir().join("Files");
        let _ = fs::create_dir_all(&dir);
        dir
    })
}

fn items_file() -> PathBuf {
    base_dir().join("items.json")
}

pub fn save(items: &[ShelfItem]) {
    let file = items_file();
    let tmp = file.with_extension("json.tmp");
    let result = serde_json::to_vec(items)
        .map_err(std::io::Error::other)
        .and_then(|data| fs::write(&tmp, data))
        .and_then(|_| fs::rename(&tmp, &file));
    if let Err(err) = result {
        eprintln!("Shoal: could not save items: {err}");
    }
}

pub fn load() -> Vec<ShelfItem> {
    let Ok(data) = fs::read(items_file()) else {
        return Vec::new();
    };
    let items: Vec<ShelfItem> = serde_json::from_slice(&data).unwrap_or_default();
    items.into_iter().filter(ShelfItem::exists).collect()
}

pub fn stamped_name(prefix: &str) -> String {
    let stamp = glib::DateTime::now_local()
        .and_then(|now| now.format("%Y-%m-%d at %H.%M.%S"))
        .map(|s| s.to_string())
        .unwrap_or_default();
    format!("{prefix} {stamp}")
}

/// A path in the Files dir named `name.ext`, numbered if taken.
pub fn unique_path(name: &str, ext: &str) -> PathBuf {
    unique_path_in(files_dir(), name, ext)
}

/// Like `unique_path`, but creates the (empty) file so jobs running at the
/// same time can never pick the same name. Safe to call off the main thread.
pub fn reserve_path(name: &str, ext: &str) -> Result<PathBuf, String> {
    for _ in 0..100 {
        let path = unique_path(name, ext);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("could not create {}: {e}", path.display())),
        }
    }
    Err(format!("no free file name for {name}.{ext}"))
}

fn unique_path_in(dir: &Path, name: &str, ext: &str) -> PathBuf {
    let file = |n: &str| {
        if ext.is_empty() {
            n.to_string()
        } else {
            format!("{n}.{ext}")
        }
    };
    let mut path = dir.join(file(name));
    let mut n = 2;
    while path.exists() {
        path = dir.join(file(&format!("{name} {n}")));
        n += 1;
    }
    path
}

/// Filename derived from the text itself, so snippets on the shelf are
/// recognizable at a glance instead of all reading "Snippet <timestamp>".
pub fn snippet_name(text: &str) -> String {
    let mut name = text
        .lines()
        .map(|l| l.trim_matches([' ', '\t']))
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string();
    for prefix in ["https://", "http://", "www."] {
        if name
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        {
            name = name[prefix.len()..].to_string();
        }
    }
    name = name
        .chars()
        .map(|c| {
            if "/:\\?%*|\"<>".contains(c) || c.is_control() {
                '-'
            } else {
                c
            }
        })
        .collect();
    if name.chars().count() > 40 {
        let head: String = name.chars().take(40).collect();
        name = format!("{}…", head.trim());
    }
    // A leading dot would hide the file in file managers
    name = name.trim_start_matches('.').trim().to_string();
    if name.is_empty() {
        stamped_name("Snippet")
    } else {
        name
    }
}

pub fn save_snippet(text: &str) -> Option<PathBuf> {
    let path = unique_path(&snippet_name(text), "txt");
    fs::write(&path, text).ok().map(|_| path)
}

pub fn save_data(data: &[u8], ext: &str, prefix: &str) -> Option<PathBuf> {
    let path = unique_path(&stamped_name(prefix), ext);
    fs::write(&path, data).ok().map(|_| path)
}

/// Removes anything in the Files dir that no restored item references.
pub fn sweep_orphans(items: &[ShelfItem]) {
    let Ok(entries) = fs::read_dir(files_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let p = path.to_string_lossy();
        let referenced = items
            .iter()
            .any(|i| i.path == p || i.path.starts_with(&format!("{p}/")));
        if !referenced {
            let _ = if path.is_dir() {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
        }
    }
}

/// Decimal units, matching how file managers report sizes.
pub fn format_bytes(n: u64) -> String {
    match n {
        1 => "1 byte".into(),
        0..1000 => format!("{n} bytes"),
        _ => {
            let units = ["KB", "MB", "GB", "TB"];
            let mut value = n as f64 / 1000.0;
            let mut i = 0;
            while value >= 1000.0 && i < units.len() - 1 {
                value /= 1000.0;
                i += 1;
            }
            if i == 0 {
                format!("{value:.0} KB")
            } else {
                format!("{value:.1} {}", units[i])
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippet_names() {
        assert_eq!(snippet_name("\n  hello world  \nsecond"), "hello world");
        assert_eq!(
            snippet_name("HTTPS://www.example.com/a?b"),
            "example.com-a-b"
        );
        assert_eq!(snippet_name("...hidden"), "hidden");
        let long = "a".repeat(50);
        assert_eq!(snippet_name(&long), format!("{}…", "a".repeat(40)));
        assert!(snippet_name("   \n\t").starts_with("Snippet "));
    }

    #[test]
    fn byte_formatting() {
        assert_eq!(format_bytes(1), "1 byte");
        assert_eq!(format_bytes(999), "999 bytes");
        assert_eq!(format_bytes(12_345), "12 KB");
        assert_eq!(format_bytes(1_234_567), "1.2 MB");
    }

    #[test]
    fn unique_paths_number_collisions() {
        let dir = std::env::temp_dir().join(format!("shoal-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(unique_path_in(&dir, "a", "txt"), dir.join("a.txt"));
        fs::write(dir.join("a.txt"), "").unwrap();
        assert_eq!(unique_path_in(&dir, "a", "txt"), dir.join("a 2.txt"));
        fs::write(dir.join("a 2.txt"), "").unwrap();
        assert_eq!(unique_path_in(&dir, "a", "txt"), dir.join("a 3.txt"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn items_without_pinned_field_still_load() {
        let old = r#"[{"id":"d109803a-1854-45f9-bef5-f1e280b6f4fa","path":"/tmp/x"}]"#;
        let items: Vec<ShelfItem> = serde_json::from_str(old).unwrap();
        assert!(!items[0].pinned);
    }
}
