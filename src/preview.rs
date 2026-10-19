//! What a shelf row shows for an item: thumbnails, file icons, and smart
//! previews for text snippets (color swatches, links, text excerpts).
//! Everything is computed locally; nothing is fetched from the network.

use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Snippets larger than this are treated as plain text without previews.
const MAX_SNIPPET_BYTES: u64 = 262_144;
const PREVIEW_LINES: usize = 12;
const PREVIEW_CHARS: usize = 600;

#[derive(Clone, Debug, PartialEq)]
pub enum Snippet {
    /// A CSS color literal such as `#ff8800` or `rgb(255 136 0)`.
    Color { literal: String, rgba: gdk::RGBA },
    /// A single web address.
    Link(String),
    Text {
        preview: String,
        words: usize,
        lines: usize,
    },
}

fn is_color_literal(s: &str) -> bool {
    if let Some(hex) = s.strip_prefix('#') {
        return matches!(hex.len(), 3 | 4 | 6 | 8) && hex.chars().all(|c| c.is_ascii_hexdigit());
    }
    let lower = s.to_ascii_lowercase();
    ["rgb(", "rgba(", "hsl(", "hsla("]
        .iter()
        .any(|p| lower.starts_with(p))
        && lower.ends_with(')')
}

fn is_link(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://"))
        && !s.chars().any(char::is_whitespace)
        && glib::Uri::is_valid(s, glib::UriFlags::NONE).is_ok()
}

/// GTK parses the comma form of rgb()/hsl(); rewrite the modern
/// space-separated form (`hsl(30 100% 50% / 0.5)`) into it.
fn comma_form(literal: &str) -> String {
    let Some((name, args)) = literal.split_once('(') else {
        return literal.to_string();
    };
    let args = args.trim_end_matches(')');
    if args.contains(',') {
        return literal.to_string();
    }
    let parts: Vec<&str> = args
        .split(|c: char| c.is_whitespace() || c == '/')
        .filter(|p| !p.is_empty())
        .collect();
    let mut name = name.trim().to_ascii_lowercase();
    if parts.len() == 4 && !name.ends_with('a') {
        name.push('a');
    }
    format!("{name}({})", parts.join(", "))
}

pub fn classify(text: &str) -> Snippet {
    let trimmed = text.trim();
    if is_color_literal(trimmed) {
        let parsed = gdk::RGBA::parse(trimmed).or_else(|_| gdk::RGBA::parse(comma_form(trimmed)));
        if let Ok(rgba) = parsed {
            return Snippet::Color {
                literal: trimmed.to_string(),
                rgba,
            };
        }
    }
    if is_link(trimmed) {
        return Snippet::Link(trimmed.to_string());
    }
    let mut preview: String = text
        .lines()
        .take(PREVIEW_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    if preview.chars().count() > PREVIEW_CHARS {
        preview = preview.chars().take(PREVIEW_CHARS).collect::<String>() + "…";
    } else if text.lines().count() > PREVIEW_LINES {
        preview.push_str("\n…");
    }
    Snippet::Text {
        preview,
        words: text.split_whitespace().count(),
        lines: text.lines().count().max(1),
    }
}

thread_local! {
    static SNIPPETS: RefCell<HashMap<PathBuf, Option<Snippet>>> = RefCell::new(HashMap::new());
    static THUMBNAILS: RefCell<HashMap<(PathBuf, i32), gdk::Texture>> = RefCell::new(HashMap::new());
}

pub fn read_snippet_text(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_SNIPPET_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// Classifies a snippet file. Snippet files never change after creation,
/// so the result is cached for the life of the process.
pub fn snippet(path: &Path) -> Option<Snippet> {
    if let Some(cached) = SNIPPETS.with_borrow(|c| c.get(path).cloned()) {
        return cached;
    }
    let result = read_snippet_text(path).map(|t| classify(&t));
    SNIPPETS.with_borrow_mut(|c| c.insert(path.to_path_buf(), result.clone()));
    result
}

pub fn subtitle(snippet: &Snippet) -> String {
    match snippet {
        Snippet::Color { literal, .. } => format!("Color · {literal}"),
        Snippet::Link(url) => {
            let host = glib::Uri::parse(url, glib::UriFlags::NONE)
                .ok()
                .and_then(|u| u.host())
                .map(|h| h.trim_start_matches("www.").to_string())
                .unwrap_or_default();
            format!("Link · {host}")
        }
        Snippet::Text { words, lines, .. } => {
            let w = if *words == 1 { "word" } else { "words" };
            if *lines > 1 {
                format!("Text · {words} {w}, {lines} lines")
            } else {
                format!("Text · {words} {w}")
            }
        }
    }
}

/// Pixel dimensions of an image file, read from its header only.
pub fn image_size(path: &Path) -> Option<(i32, i32)> {
    let (_, w, h) = gtk::gdk_pixbuf::Pixbuf::file_info(path)?;
    (w > 0 && h > 0).then_some((w, h))
}

/// A rounded square filled with the snippet's color.
pub fn color_swatch(rgba: gdk::RGBA, size: i32) -> gtk::Widget {
    let area = gtk::DrawingArea::builder()
        .content_width(size)
        .content_height(size)
        .valign(gtk::Align::Center)
        .build();
    area.set_draw_func(move |_, cr, w, h| {
        let (w, h) = (f64::from(w), f64::from(h));
        let inset = 2.0;
        let r = 6.0;
        let (x0, y0, x1, y1) = (inset, inset, w - inset, h - inset);
        cr.new_sub_path();
        cr.arc(x1 - r, y0 + r, r, -std::f64::consts::FRAC_PI_2, 0.0);
        cr.arc(x1 - r, y1 - r, r, 0.0, std::f64::consts::FRAC_PI_2);
        cr.arc(
            x0 + r,
            y1 - r,
            r,
            std::f64::consts::FRAC_PI_2,
            std::f64::consts::PI,
        );
        cr.arc(
            x0 + r,
            y0 + r,
            r,
            std::f64::consts::PI,
            3.0 * std::f64::consts::FRAC_PI_2,
        );
        cr.close_path();
        cr.set_source_rgba(
            f64::from(rgba.red()),
            f64::from(rgba.green()),
            f64::from(rgba.blue()),
            f64::from(rgba.alpha()),
        );
        let _ = cr.fill_preserve();
        // Hairline border so white and transparent swatches stay visible.
        cr.set_source_rgba(0.5, 0.5, 0.5, 0.45);
        cr.set_line_width(1.0);
        let _ = cr.stroke();
    });
    area.upcast()
}

/// The file's theme icon, swapped for a real preview when one is available:
/// the shared freedesktop thumbnail cache (filled by Dolphin), or the image
/// itself for image files.
pub fn thumbnail(path: &Path, size: i32) -> gtk::Widget {
    let image = gtk::Image::new();
    image.set_pixel_size(size);
    image.set_size_request(size, size);
    image.set_valign(gtk::Align::Center);

    let key = (path.to_path_buf(), size);
    if let Some(texture) = THUMBNAILS.with_borrow(|cache| cache.get(&key).cloned()) {
        image.set_paintable(Some(&texture));
        return image.upcast();
    }

    let file = gio::File::for_path(path);
    let info = file
        .query_info(
            "standard::icon,standard::content-type,thumbnail::path",
            gio::FileQueryInfoFlags::NONE,
            gio::Cancellable::NONE,
        )
        .ok();
    match info.as_ref().and_then(|i| i.icon()) {
        Some(icon) => image.set_from_gicon(&icon),
        None => image.set_icon_name(Some("text-x-generic")),
    }
    let source = info.as_ref().and_then(|info| {
        let cached = info
            .attribute_byte_string("thumbnail::path")
            .map(PathBuf::from);
        let is_image = info.content_type().is_some_and(|t| t.starts_with("image/"));
        cached.or_else(|| is_image.then(|| path.to_path_buf()))
    });
    let Some(source) = source else {
        return image.upcast();
    };

    let pixels = size * 2; // sharp on HiDPI
    glib::spawn_future_local(glib::clone!(
        #[weak]
        image,
        async move {
            let decoded = gio::spawn_blocking(move || {
                let pixbuf =
                    gtk::gdk_pixbuf::Pixbuf::from_file_at_scale(&source, pixels, pixels, true)
                        .ok()?;
                Some((
                    pixbuf.read_pixel_bytes(),
                    pixbuf.width(),
                    pixbuf.height(),
                    pixbuf.rowstride(),
                    pixbuf.has_alpha(),
                ))
            })
            .await
            .ok()
            .flatten();
            let Some((bytes, width, height, stride, alpha)) = decoded else {
                return;
            };
            let format = if alpha {
                gdk::MemoryFormat::R8g8b8a8
            } else {
                gdk::MemoryFormat::R8g8b8
            };
            let texture = gdk::MemoryTexture::new(width, height, format, &bytes, stride as usize)
                .upcast::<gdk::Texture>();
            THUMBNAILS.with_borrow_mut(|cache| cache.insert(key, texture.clone()));
            image.set_paintable(Some(&texture));
        }
    ));
    image.upcast()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors() {
        for c in [
            "#f80",
            "#ff8800",
            "#FF880080",
            "rgb(255, 136, 0)",
            "rgb(255 136 0)",
            "hsl(30 100% 50%)",
            "hsl(30 100% 50% / 0.5)",
        ] {
            assert!(
                matches!(classify(c), Snippet::Color { .. }),
                "{c} should be a color"
            );
        }
        for c in ["#ff888", "#zzzzzz", "red", "#ff8800 and more"] {
            assert!(
                !matches!(classify(c), Snippet::Color { .. }),
                "{c} should not be a color"
            );
        }
    }

    #[test]
    fn links() {
        assert_eq!(
            classify("  https://example.com/a?b=1 \n"),
            Snippet::Link("https://example.com/a?b=1".into())
        );
        assert!(!matches!(
            classify("https://example.com and text"),
            Snippet::Link(_)
        ));
        assert!(!matches!(classify("ftp://example.com"), Snippet::Link(_)));
        assert_eq!(
            subtitle(&classify("https://www.example.com/x")),
            "Link · example.com"
        );
    }

    #[test]
    fn text_previews() {
        let s = classify("one two\nthree");
        assert_eq!(subtitle(&s), "Text · 3 words, 2 lines");
        assert_eq!(subtitle(&classify("hello")), "Text · 1 word");
        let long: String = (0..20).map(|i| format!("line {i}\n")).collect();
        let Snippet::Text { preview, .. } = classify(&long) else {
            panic!("expected text")
        };
        assert_eq!(preview.lines().count(), PREVIEW_LINES + 1);
        assert!(preview.ends_with('…'));
    }
}
