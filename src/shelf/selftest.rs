//! `shoal --self-test` (debug builds only): drives the real shelf inside the
//! running app through selection, keyboard, pinning, drag-out bookkeeping,
//! undo, quick actions, clipboard capture and search. Run it with throwaway
//! XDG_DATA_HOME / XDG_CONFIG_HOME, as scripts/check.sh does.

use super::*;
use std::time::Instant;

struct Report {
    failures: Vec<String>,
    passed: usize,
}

impl Report {
    fn check(&mut self, name: &str, ok: bool) {
        if ok {
            self.passed += 1;
            println!("  ok    {name}");
        } else {
            self.failures.push(name.to_string());
            println!("  FAIL  {name}");
        }
    }
}

fn names(shelf: &Shelf) -> Vec<String> {
    shelf
        .visible_items()
        .iter()
        .map(|i| i.display_name())
        .collect()
}

fn id_of(shelf: &Shelf, name: &str) -> Uuid {
    shelf
        .items
        .borrow()
        .iter()
        .find(|i| i.display_name() == name)
        .map(|i| i.id)
        .unwrap_or_else(Uuid::nil)
}

async fn settle(ms: u64) {
    glib::timeout_future(Duration::from_millis(ms)).await;
}

async fn wait_for_jobs(shelf: &Shelf) {
    let start = Instant::now();
    while shelf.jobs.get() > 0 && start.elapsed() < Duration::from_secs(20) {
        settle(50).await;
    }
    settle(100).await;
}

pub async fn run(shelf: Rc<Shelf>) -> bool {
    let mut r = Report {
        failures: Vec::new(),
        passed: 0,
    };
    let dir = std::env::temp_dir().join(format!("shoal-selftest-{}", Uuid::new_v4()));
    std::fs::create_dir_all(dir.join("folder")).unwrap();
    image::RgbaImage::from_pixel(64, 32, image::Rgba([10, 120, 200, 255]))
        .save(dir.join("photo.png"))
        .unwrap();
    std::fs::write(dir.join("notes.txt"), "hello").unwrap();
    std::fs::write(dir.join("folder/inner.txt"), "inner").unwrap();

    println!("shelf basics");
    shelf.add(
        vec![
            dir.join("folder"),
            dir.join("notes.txt"),
            dir.join("photo.png"),
        ],
        true,
    );
    settle(300).await;
    r.check("shows after adding", shelf.is_visible());
    r.check(
        "newest first",
        names(&shelf) == ["photo.png", "notes.txt", "folder"],
    );
    let photo = id_of(&shelf, "photo.png");
    let notes = id_of(&shelf, "notes.txt");
    let folder = id_of(&shelf, "folder");

    println!("selection and keyboard");
    shelf.select_only(photo);
    shelf.handle_key(gdk::Key::Down, gdk::ModifierType::SHIFT_MASK);
    r.check("shift+down extends", shelf.targets() == [photo, notes]);
    shelf.handle_key(gdk::Key::Down, gdk::ModifierType::empty());
    r.check("down moves single", shelf.targets() == [folder]);
    shelf.handle_key(gdk::Key::a, gdk::ModifierType::CONTROL_MASK);
    r.check("ctrl+a selects all", shelf.targets().len() == 3);
    shelf.handle_key(gdk::Key::Escape, gdk::ModifierType::empty());
    r.check(
        "escape clears selection",
        shelf.selected.borrow().is_empty(),
    );
    shelf.toggle_selected(photo);
    shelf.toggle_selected(folder);
    r.check(
        "ctrl-click style toggles keep display order",
        shelf.targets() == [photo, folder],
    );
    shelf.anchor.set(Some(photo));
    shelf.select_range_to(folder);
    r.check("shift-click range", shelf.targets().len() == 3);

    println!("pinning");
    shelf.select_only(notes);
    shelf.handle_key(gdk::Key::p, gdk::ModifierType::CONTROL_MASK);
    r.check(
        "ctrl+p pins",
        shelf
            .items
            .borrow()
            .iter()
            .any(|i| i.id == notes && i.pinned),
    );
    r.check(
        "pinned sorts last",
        names(&shelf).last().map(String::as_str) == Some("notes.txt"),
    );
    shelf.drag_out_succeeded(&[photo, notes]);
    settle(50).await;
    r.check(
        "drag-out removes unpinned, keeps pinned",
        names(&shelf) == ["folder", "notes.txt"],
    );
    r.check("original kept (copy mode)", dir.join("photo.png").exists());
    shelf.bring_back_last_removed();
    r.check(
        "undo brings photo back",
        names(&shelf).contains(&"photo.png".into()),
    );
    let photo = id_of(&shelf, "photo.png");

    println!("quick actions");
    shelf.compress(&[folder, photo]);
    wait_for_jobs(&shelf).await;
    let zip = shelf
        .items
        .borrow()
        .iter()
        .find(|i| i.path.ends_with(".zip"))
        .cloned();
    r.check(
        "compress made a zip on the shelf",
        zip.as_ref().is_some_and(|z| z.exists()),
    );
    if let Some(zip) = zip {
        let archive = zip::ZipArchive::new(std::fs::File::open(zip.path()).unwrap()).unwrap();
        let entries: Vec<String> = archive
            .file_names()
            .map(|n| n.unwrap().into_owned())
            .collect();
        r.check(
            "zip holds folder tree and photo",
            entries.contains(&"folder/inner.txt".into()) && entries.contains(&"photo.png".into()),
        );
    }
    shelf.convert(&[photo], ImageFormat::Jpeg);
    wait_for_jobs(&shelf).await;
    let jpg = id_of(&shelf, "photo.jpg");
    r.check("convert to JPEG", !jpg.is_nil());
    shelf.convert(&[jpg], ImageFormat::Jpeg);
    wait_for_jobs(&shelf).await;
    r.check(
        "converting to the same format makes nothing",
        names(&shelf)
            .iter()
            .filter(|n| n.starts_with("photo") && n.ends_with(".jpg"))
            .count()
            == 1,
    );
    shelf.resize(&[photo], Resize::Half);
    wait_for_jobs(&shelf).await;
    let half = shelf
        .items
        .borrow()
        .iter()
        .find(|i| i.display_name() == "photo 50%.png")
        .cloned();
    r.check(
        "resize half is 32×16",
        half.is_some_and(|h| image::image_dimensions(h.path()).ok() == Some((32, 16))),
    );
    let before = shelf.items.borrow().len();
    shelf.resize(&[photo], Resize::LongestSide(1920));
    wait_for_jobs(&shelf).await;
    r.check(
        "resize that would enlarge is refused, no leftover file",
        shelf.items.borrow().len() == before
            && !store::files_dir().join("photo 1920px.png").exists(),
    );

    println!("clipboard capture");
    let count = shelf.items.borrow().len();
    shelf.capture(ClipContent::Text("alpha".into()), true);
    shelf.capture(ClipContent::Text("alpha".into()), true);
    r.check(
        "repeat announcement of one copy ignored",
        shelf.items.borrow().len() == count + 1,
    );
    shelf.capture(ClipContent::Text("beta".into()), true);
    shelf.capture(ClipContent::Text("alpha".into()), true);
    r.check(
        "recopy moves to top, no duplicate",
        shelf.items.borrow().len() == count + 2
            && shelf.items.borrow()[0].display_name() == "alpha.txt",
    );
    shelf.capture(ClipContent::Text("#ff8800".into()), true);
    let color = shelf.items.borrow()[0].clone();
    let snippet = preview::snippet(color.path());
    r.check(
        "color snippet detected",
        matches!(snippet, Some(Snippet::Color { .. })),
    );
    r.check(
        "color row describes itself",
        describe(&color, snippet.as_ref()).1 == "Color · #ff8800",
    );
    shelf.capture(ClipContent::Text("https://example.com/page".into()), true);
    let link = shelf.items.borrow()[0].clone();
    let snippet = preview::snippet(link.path());
    r.check(
        "link row",
        describe(&link, snippet.as_ref()).0 == "example.com/page",
    );
    shelf.capture(ClipContent::Sensitive, true);
    r.check(
        "secret copies ignored",
        shelf.items.borrow()[0].id == link.id,
    );

    println!("search");
    shelf.search_bar.set_search_mode(true);
    shelf.search.set_text("alph");
    settle(400).await;
    r.check("filter by name", names(&shelf) == ["alpha.txt"]);
    shelf.search.set_text("8800");
    settle(400).await;
    r.check("filter by snippet text", names(&shelf).len() == 1);
    shelf.search.set_text("zzzz");
    settle(400).await;
    r.check(
        "no matches shows hint",
        shelf.empty_hint.label() == "No matches",
    );
    shelf.handle_key(gdk::Key::Escape, gdk::ModifierType::empty());
    settle(400).await;
    r.check(
        "escape ends search",
        !shelf.search_bar.is_search_mode() && shelf.search.text().is_empty(),
    );

    println!("delete and clear");
    let first = shelf.visible_items()[0].id;
    shelf.select_only(first);
    shelf.handle_key(gdk::Key::Delete, gdk::ModifierType::empty());
    r.check(
        "delete removes selection",
        !shelf.items.borrow().iter().any(|i| i.id == first),
    );
    shelf.clear_all();
    r.check("clear keeps only pinned", names(&shelf) == ["notes.txt"]);
    settle(600).await;
    r.check("hides when only pinned remain", !shelf.is_visible());
    shelf.handle_key(gdk::Key::z, gdk::ModifierType::CONTROL_MASK);
    r.check("ctrl+z undoes clear", shelf.items.borrow().len() > 1);

    let _ = std::fs::remove_dir_all(&dir);
    println!(
        "\nself-test: {} passed, {} failed",
        r.passed,
        r.failures.len()
    );
    r.failures.is_empty()
}
