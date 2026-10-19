//! Quick actions on shelf items: compress to ZIP, convert and resize
//! images. Each writes a new file and never modifies the originals. These
//! block, so callers run them off the main thread.

use image::{DynamicImage, ImageDecoder, ImageReader, imageops::FilterType};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use zip::write::SimpleFileOptions;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageFormat {
    Png,
    Jpeg,
    Webp,
}

impl ImageFormat {
    pub fn ext(self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpg",
            ImageFormat::Webp => "webp",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            ImageFormat::Png => "PNG",
            ImageFormat::Jpeg => "JPEG",
            ImageFormat::Webp => "WebP",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        [Self::Png, Self::Jpeg, Self::Webp]
            .into_iter()
            .find(|f| f.ext() == id)
    }

    /// The format a resized copy is saved in: the original's own format when
    /// it is one we write, PNG otherwise.
    fn for_path(path: &Path) -> Self {
        match extension(path).as_str() {
            "jpg" | "jpeg" => ImageFormat::Jpeg,
            "webp" => ImageFormat::Webp,
            _ => ImageFormat::Png,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resize {
    Half,
    /// Fit within a square of this many pixels.
    LongestSide(u32),
}

impl Resize {
    pub const ALL: [Resize; 4] = [
        Resize::Half,
        Resize::LongestSide(1920),
        Resize::LongestSide(1280),
        Resize::LongestSide(800),
    ];

    pub fn id(self) -> String {
        match self {
            Resize::Half => "half".into(),
            Resize::LongestSide(px) => px.to_string(),
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "half" => Some(Resize::Half),
            px => px.parse().ok().map(Resize::LongestSide),
        }
    }

    pub fn title(self) -> String {
        match self {
            Resize::Half => "Half Size (50%)".into(),
            Resize::LongestSide(px) => format!("Fit in {px} px"),
        }
    }

    fn suffix(self) -> String {
        match self {
            Resize::Half => "50%".into(),
            Resize::LongestSide(px) => format!("{px}px"),
        }
    }
}

fn extension(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .or_else(|| path.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Untitled".into())
}

/// Image files the convert and resize actions can read.
pub fn is_image(path: &Path) -> bool {
    path.is_file()
        && matches!(
            extension(path).as_str(),
            "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "tif" | "tiff"
        )
}

/// Decodes an image, applying its EXIF orientation so phone photos come out
/// upright.
fn load(path: &Path) -> Result<DynamicImage, String> {
    let fail = |e: &dyn std::fmt::Display| format!("could not read {}: {e}", path.display());
    let mut decoder = ImageReader::open(path)
        .map_err(|e| fail(&e))?
        .with_guessed_format()
        .map_err(|e| fail(&e))?
        .into_decoder()
        .map_err(|e| fail(&e))?;
    let orientation = decoder.orientation().map_err(|e| fail(&e))?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(|e| fail(&e))?;
    image.apply_orientation(orientation);
    Ok(image)
}

fn save(image: &DynamicImage, format: ImageFormat, dest: &Path) -> Result<(), String> {
    let fail = |e: &dyn std::fmt::Display| format!("could not write {}: {e}", dest.display());
    let out = BufWriter::new(File::create(dest).map_err(|e| fail(&e))?);
    let result = match format {
        ImageFormat::Png => image.write_with_encoder(image::codecs::png::PngEncoder::new(out)),
        // JPEG has no alpha channel, so flatten first.
        ImageFormat::Jpeg => DynamicImage::ImageRgb8(image.to_rgb8())
            .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(out, 90)),
        ImageFormat::Webp => DynamicImage::ImageRgba8(image.to_rgba8())
            .write_with_encoder(image::codecs::webp::WebPEncoder::new_lossless(out)),
    };
    result.map_err(|e| {
        let _ = fs::remove_file(dest);
        fail(&e)
    })
}

/// File name for a converted copy: `photo.png`.
pub fn converted_name(path: &Path) -> String {
    stem(path)
}

/// File name for a resized copy: `photo 1920px`.
pub fn resized_name(path: &Path, resize: Resize) -> (String, &'static str) {
    (
        format!("{} {}", stem(path), resize.suffix()),
        ImageFormat::for_path(path).ext(),
    )
}

pub fn convert(path: &Path, format: ImageFormat, dest: &Path) -> Result<(), String> {
    save(&load(path)?, format, dest)
}

pub fn resize(path: &Path, resize: Resize, dest: &Path) -> Result<(), String> {
    let image = load(path)?;
    let (w, h) = (image.width(), image.height());
    let resized = match resize {
        Resize::Half => image.resize_exact((w / 2).max(1), (h / 2).max(1), FilterType::Lanczos3),
        Resize::LongestSide(px) => {
            if w.max(h) <= px {
                return Err(format!(
                    "{} is already {w}×{h}, within {px} px",
                    path.display()
                ));
            }
            image.resize(px, px, FilterType::Lanczos3)
        }
    };
    save(&resized, ImageFormat::for_path(path), dest)
}

/// Archive name: the item's own name for one item, otherwise `None` so the
/// caller can pick a timestamped name.
pub fn archive_name(paths: &[PathBuf]) -> Option<String> {
    match paths {
        [one] => Some(stem(one)),
        _ => None,
    }
}

/// Zips files and folders (recursively) into `dest`. Entries are named by
/// their file names; clashing names get numbered.
pub fn compress(paths: &[PathBuf], dest: &Path) -> Result<(), String> {
    let result = (|| -> io::Result<()> {
        let mut zip = zip::ZipWriter::new(BufWriter::new(File::create(dest)?));
        let mut used = HashSet::new();
        for path in paths {
            let base = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "item".into());
            let mut name = base.clone();
            let mut n = 2;
            while !used.insert(name.clone()) {
                name = match base.rsplit_once('.') {
                    Some((s, e)) if !s.is_empty() => format!("{s} {n}.{e}"),
                    _ => format!("{base} {n}"),
                };
                n += 1;
            }
            add_to_zip(&mut zip, path, &name)?;
        }
        zip.finish()?;
        Ok(())
    })();
    result.map_err(|e| {
        let _ = fs::remove_file(dest);
        format!("could not create {}: {e}", dest.display())
    })
}

fn add_to_zip<W: io::Write + io::Seek>(
    zip: &mut zip::ZipWriter<W>,
    path: &Path,
    name: &str,
) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // symlink_metadata so a symlinked folder cannot send us around a loop;
    // symlinked files are followed and stored as regular files.
    let meta = fs::symlink_metadata(path)?;
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(fs::metadata(path).map_or(0o644, |m| m.permissions().mode()))
        .large_file(true);
    if meta.is_dir() {
        zip.add_directory(format!("{name}/"), options)?;
        let mut entries: Vec<_> = fs::read_dir(path)?.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let child = format!("{name}/{}", entry.file_name().to_string_lossy());
            add_to_zip(zip, &entry.path(), &child)?;
        }
    } else if meta.is_file() || (meta.is_symlink() && path.is_file()) {
        zip.start_file(name, options)?;
        io::copy(&mut File::open(path)?, zip)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::RgbaImage;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("shoal-actions-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_png(dir: &Path, w: u32, h: u32) -> PathBuf {
        let path = dir.join("sample.png");
        RgbaImage::from_pixel(w, h, image::Rgba([255, 136, 0, 128]))
            .save(&path)
            .unwrap();
        path
    }

    #[test]
    fn converts_between_formats() {
        let dir = temp_dir();
        let src = sample_png(&dir, 40, 30);
        for format in [ImageFormat::Jpeg, ImageFormat::Webp, ImageFormat::Png] {
            let dest = dir.join(format!("out.{}", format.ext()));
            convert(&src, format, &dest).unwrap();
            let back = image::open(&dest).unwrap();
            assert_eq!((back.width(), back.height()), (40, 30));
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resizes_keeping_aspect() {
        let dir = temp_dir();
        let src = sample_png(&dir, 400, 200);
        let dest = dir.join("half.png");
        resize(&src, Resize::Half, &dest).unwrap();
        assert_eq!(image::image_dimensions(&dest).unwrap(), (200, 100));
        resize(&src, Resize::LongestSide(100), &dest).unwrap();
        assert_eq!(image::image_dimensions(&dest).unwrap(), (100, 50));
        assert!(resize(&src, Resize::LongestSide(800), &dest).is_err());
        assert_eq!(
            resized_name(&src, Resize::LongestSide(800)),
            ("sample 800px".into(), "png")
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unreadable_image_is_an_error_not_a_panic() {
        let dir = temp_dir();
        let bogus = dir.join("bogus.png");
        fs::write(&bogus, b"not an image").unwrap();
        assert!(convert(&bogus, ImageFormat::Jpeg, &dir.join("o.jpg")).is_err());
        assert!(!dir.join("o.jpg").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn zips_files_and_folders() {
        let dir = temp_dir();
        let a = dir.join("a");
        let b = dir.join("b");
        fs::create_dir_all(a.join("nested")).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("nested/deep.txt"), "deep").unwrap();
        fs::write(a.join("same.txt"), "one").unwrap();
        fs::write(b.join("same.txt"), "two").unwrap();
        let dest = dir.join("out.zip");
        compress(&[a.clone(), a.join("same.txt"), b.join("same.txt")], &dest).unwrap();
        let mut archive = zip::ZipArchive::new(File::open(&dest).unwrap()).unwrap();
        let mut names: Vec<String> = archive
            .file_names()
            .map(|n| n.unwrap().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "a/",
                "a/nested/",
                "a/nested/deep.txt",
                "a/same.txt",
                "same 2.txt",
                "same.txt"
            ]
        );
        let mut deep = String::new();
        io::Read::read_to_string(
            &mut archive.by_name("a/nested/deep.txt").unwrap(),
            &mut deep,
        )
        .unwrap();
        assert_eq!(deep, "deep");
        assert_eq!(archive_name(&[a]), Some("a".into()));
        fs::remove_dir_all(&dir).unwrap();
    }
}
