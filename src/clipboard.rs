//! Clipboard access through wl-clipboard (Wayland) or xclip (X11, e.g.
//! XFCE). Both read and set the clipboard without needing keyboard focus,
//! which a GTK client never has while the user is copying in another app.
//! xclip has no watch mode, so the X11 path polls instead of subscribing.

use crate::x11_backend;
use gtk::gio::{self, prelude::*};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[derive(Debug, Hash, PartialEq)]
pub enum ClipContent {
    Files(Vec<PathBuf>),
    Image(Vec<u8>, &'static str),
    Text(String),
    /// Marked secret by a password manager; never captured.
    Sensitive,
    Empty,
}

/// KDE's equivalent of macOS org.nspasteboard.ConcealedType.
const SENSITIVE_TYPE: &str = "x-kde-passwordManagerHint";

/// Plain-text types in order of preference. Asking wl-paste for plain
/// "text" can return text/html from browsers, which saved raw markup.
const PLAIN_TEXT_TYPES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "STRING",
    "TEXT",
];

const IMAGE_TYPES: [(&str, &str); 3] = [
    ("image/png", "png"),
    ("image/jpeg", "jpg"),
    ("application/pdf", "pdf"),
];

fn wl_paste(args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("wl-paste")
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

fn xclip_paste(args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("xclip")
        .args(["-selection", "clipboard"])
        .args(args)
        .args(["-o"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

/// Lists the MIME types currently on the clipboard.
fn list_types() -> Option<Vec<String>> {
    let raw = if x11_backend::is_x11() {
        xclip_paste(&["-t", "TARGETS"])
    } else {
        wl_paste(&["--list-types"])
    }?;
    Some(
        String::from_utf8_lossy(&raw)
            .lines()
            .map(str::to_owned)
            .collect(),
    )
}

/// Reads one MIME type's bytes off the clipboard.
fn paste_type(mime: &str) -> Option<Vec<u8>> {
    if x11_backend::is_x11() {
        xclip_paste(&["-t", mime])
    } else {
        wl_paste(&["--type", mime])
    }
}

/// Like `paste_type`, but for text: strips a single trailing newline, the
/// way `wl-paste --no-newline` does (xclip has no equivalent flag).
fn paste_text_type(mime: &str) -> Option<Vec<u8>> {
    let data = if x11_backend::is_x11() {
        xclip_paste(&["-t", mime])
    } else {
        wl_paste(&["--no-newline", "--type", mime])
    }?;
    if x11_backend::is_x11() {
        let mut data = data;
        if data.last() == Some(&b'\n') {
            data.pop();
            if data.last() == Some(&b'\r') {
                data.pop();
            }
        }
        Some(data)
    } else {
        Some(data)
    }
}

fn parse_uri_list(data: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(data)
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("file://"))
        .filter_map(|uri| gio::File::for_uri(uri).path())
        .collect()
}

/// Rough HTML-to-text for apps that offer only text/html: drops tags,
/// scripts and styles, turns block ends into newlines, decodes common
/// entities.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::new();
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('>') else {
            rest = "";
            break;
        };
        let tag = rest[start + 1..start + end].trim().to_ascii_lowercase();
        rest = &rest[start + end + 1..];
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        if !tag.starts_with('/') && (name == "script" || name == "style") {
            let close = format!("</{name}");
            match rest.to_ascii_lowercase().find(&close) {
                Some(i) => rest = &rest[i..],
                None => rest = "",
            }
            continue;
        }
        if matches!(
            name.as_str(),
            "br" | "p" | "div" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
        ) && (name == "br" || tag.starts_with('/'))
        {
            out.push('\n');
        }
    }
    out.push_str(rest);
    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&");
    decoded.trim().to_string()
}

fn is_single_url(text: &str) -> bool {
    let t = text.trim();
    (t.starts_with("http://") || t.starts_with("https://")) && !t.contains(char::is_whitespace)
}

fn read_text(types: &[String]) -> Option<String> {
    let has = |mime: &str| types.iter().any(|t| t == mime);
    let plain = PLAIN_TEXT_TYPES
        .iter()
        .find(|t| has(t))
        .and_then(|t| paste_text_type(t))
        .and_then(|d| String::from_utf8(d).ok())
        .filter(|t| !t.trim().is_empty());
    plain.or_else(|| {
        has("text/html")
            .then(|| paste_text_type("text/html"))
            .flatten()
            .map(|d| html_to_text(&String::from_utf8_lossy(&d)))
            .filter(|t| !t.is_empty())
    })
}

/// Reads the clipboard: files first, then text, then images. Blocks on the
/// backend's clipboard CLI, so call it off the main thread.
pub fn read() -> ClipContent {
    let Some(types) = list_types() else {
        return ClipContent::Empty;
    };
    let has = |mime: &str| types.iter().any(|t| t == mime);

    if has(SENSITIVE_TYPE) {
        return ClipContent::Sensitive;
    }
    if has("text/uri-list") {
        let paths = paste_text_type("text/uri-list")
            .map(|d| parse_uri_list(&d))
            .unwrap_or_default();
        if !paths.is_empty() {
            return ClipContent::Files(paths);
        }
    }
    let image_type = IMAGE_TYPES.iter().find(|(mime, _)| has(mime));
    let text = read_text(&types);
    // Office suites put a rendered picture next to copied text; the text is
    // what was meant. Browsers' "Copy Image" can add the image's address as
    // text; there the image is what was meant.
    if let Some(text) = text
        .as_ref()
        .filter(|t| image_type.is_none() || !is_single_url(t))
    {
        return ClipContent::Text(text.clone());
    }
    if let Some((mime, ext)) = image_type
        && let Some(data) = paste_type(mime).filter(|d| !d.is_empty())
    {
        return ClipContent::Image(data, ext);
    }
    text.map_or(ClipContent::Empty, ClipContent::Text)
}

/// Starts watching the clipboard for changes; while `enabled` is set, each
/// change is read and sent to `tx`. Returns the watcher child process, if
/// any, so it can be killed on quit (the X11 path polls in a thread
/// instead, since xclip has no watch mode, and returns `None`).
pub fn watch(enabled: Arc<AtomicBool>, tx: async_channel::Sender<ClipContent>) -> Option<Child> {
    if x11_backend::is_x11() {
        watch_x11(enabled, tx);
        return None;
    }
    let mut child = Command::new("wl-paste")
        .args(["--watch", "echo", "changed"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| {
            eprintln!("Shoal: clipboard watching unavailable (is wl-clipboard installed?): {err}")
        })
        .ok()?;
    let stdout = child.stdout.take()?;
    std::thread::spawn(move || {
        // The first line reports the clipboard as it was at launch, which
        // is not a new copy.
        for line in BufReader::new(stdout).lines().skip(1) {
            if line.is_err() {
                break;
            }
            if enabled.load(Ordering::Relaxed) && tx.send_blocking(read()).is_err() {
                break;
            }
        }
    });
    Some(child)
}

fn content_hash(content: &ClipContent) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

/// Polls the clipboard every 500ms for a change. xclip has no watch mode
/// (nothing analogous to wl-paste's data-control protocol access), so this
/// is the X11 fallback; it costs a couple of short-lived `xclip` processes
/// per tick while idle.
fn watch_x11(enabled: Arc<AtomicBool>, tx: async_channel::Sender<ClipContent>) {
    std::thread::spawn(move || {
        // The clipboard as it is at launch is not a new copy.
        let mut last = content_hash(&read());
        loop {
            std::thread::sleep(Duration::from_millis(500));
            if !enabled.load(Ordering::Relaxed) {
                continue;
            }
            let content = read();
            let hash = content_hash(&content);
            if hash == last {
                continue;
            }
            last = hash;
            if tx.send_blocking(content).is_err() {
                break;
            }
        }
    });
}

fn set_clipboard(mime: &str, data: &[u8]) {
    let (program, args): (&str, &[&str]) = if x11_backend::is_x11() {
        ("xclip", &["-selection", "clipboard", "-t"])
    } else {
        ("wl-copy", &["--type"])
    };
    let child = Command::new(program)
        .args(args)
        .arg(mime)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match child {
        Ok(mut child) => {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(data);
            }
            // Both wl-copy and xclip fork to serve the selection; reap the
            // parent so it doesn't linger as a zombie.
            std::thread::spawn(move || child.wait());
        }
        Err(err) => eprintln!("Shoal: could not run {program}: {err}"),
    }
}

pub fn copy_text(text: &str) {
    set_clipboard("text/plain;charset=utf-8", text.as_bytes());
}

pub fn copy_files(paths: &[&Path]) {
    let list: String = paths
        .iter()
        .map(|p| format!("{}\r\n", gio::File::for_path(p).uri()))
        .collect();
    set_clipboard("text/uri-list", list.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_becomes_plain_text() {
        assert_eq!(html_to_text("<b>bold</b> &amp; <i>it</i>"), "bold & it");
        assert_eq!(html_to_text("<p>one</p><p>two</p>"), "one\ntwo");
        assert_eq!(html_to_text("a<br>b<BR/>c"), "a\nb\nc");
        assert_eq!(
            html_to_text("<style>p{color:red}</style><script>x()</script>hi"),
            "hi"
        );
        assert_eq!(html_to_text("1 &lt; 2 &gt; 0"), "1 < 2 > 0");
        assert_eq!(html_to_text("unterminated <b"), "unterminated");
    }

    #[test]
    fn uri_lists_keep_only_local_files() {
        let list = b"# comment\r\nfile:///tmp/a%20b.txt\r\nhttps://example.com/x\r\n";
        assert_eq!(parse_uri_list(list), [PathBuf::from("/tmp/a b.txt")]);
    }

    #[test]
    fn single_urls() {
        assert!(is_single_url(" https://example.com/a.png "));
        assert!(!is_single_url("see https://example.com"));
    }
}
