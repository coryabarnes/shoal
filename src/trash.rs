//! Keep original files recoverable in the desktop's normal Trash, including
//! when Shoal's data directory is redirected into a Snap sandbox.

use gtk::gio::{self, prelude::*};
use gtk::glib::{self, variant::Handle};
use std::path::Path;

pub async fn move_original(path: &Path) -> Result<(), String> {
    if std::env::var_os("SNAP").is_some() {
        via_portal(path).await
    } else {
        gio::File::for_path(path)
            .trash(gio::Cancellable::NONE)
            .map_err(|err| err.to_string())
    }
}

async fn via_portal(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;

    // Never follow a symlink and accidentally trash its target. Older portals
    // require regular files to be opened read/write. For directories, try a
    // read-only descriptor; unsupported portals leave the original in place.
    let is_directory = path.is_dir();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(!is_directory)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|err| err.to_string())?;
    let descriptors = gio::UnixFDList::new();
    let handle = descriptors.append(&file).map_err(|err| err.to_string())?;
    let bus = gio::bus_get_future(gio::BusType::Session)
        .await
        .map_err(|err| err.to_string())?;
    let (reply, _) = bus
        .call_with_unix_fd_list_future(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Trash",
            "TrashFile",
            Some(&(Handle(handle),).to_variant()),
            Some(glib::VariantTy::new("(u)").expect("valid TrashFile reply type")),
            gio::DBusCallFlags::NONE,
            10_000,
            Some(&descriptors),
        )
        .await
        .map_err(|err| err.to_string())?;
    if reply.get::<(u32,)>() == Some((1,)) {
        Ok(())
    } else {
        Err("the desktop could not move this item to Trash; the original was kept".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a desktop session with the Trash portal"]
    fn portal_uses_desktop_trash_and_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let name = format!("shoal-trash-check-{}", uuid::Uuid::new_v4());
        let root = std::path::PathBuf::from(std::env::var_os("HOME").expect("home"));
        let path = root.join(&name);
        let link = root.join(format!("{name}-link"));
        let trash = glib::user_data_dir().join("Trash");
        std::fs::write(&path, b"Shoal recoverable Trash integration check").unwrap();
        symlink(&path, &link).unwrap();
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                context.block_on(async {
                    assert!(via_portal(&link).await.is_err());
                    assert!(link.is_symlink());
                    assert!(path.exists(), "symlink target must stay in place");
                    via_portal(&path).await.expect("desktop Trash portal");
                });
            })
            .unwrap();
        assert!(!path.exists());
        let info = std::fs::read_dir(trash.join("info"))
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| entry.file_name().to_string_lossy().starts_with(&name))
            .expect("restore information in the desktop Trash")
            .path();
        let trashed = trash.join("files").join(info.file_stem().unwrap());
        assert_eq!(
            std::fs::read(&trashed).unwrap(),
            b"Shoal recoverable Trash integration check"
        );
        assert!(std::fs::read_to_string(&info).unwrap().contains(&name));
        // Remove only the uniquely named fixtures created by this test.
        std::fs::remove_file(link).unwrap();
        std::fs::remove_file(trashed).unwrap();
        std::fs::remove_file(info).unwrap();
    }
}
