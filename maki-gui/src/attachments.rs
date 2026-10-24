//! Files attached to a prompt, from the clipboard, a file picker or a drop.
//! Images are kept as GPUI images to draw and reach the agent as base64;
//! any other file reaches it as a path for its tools to read.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use color_eyre::Result;
use color_eyre::eyre::eyre;
use gpui_kit::{Image, ImageFormat};
use maki_providers::{ImageMediaType, ImageSource};

const FILES_HEADING: &str = "Attached files:";

#[derive(Clone)]
pub struct Attachment {
    pub media: ImageMediaType,
    pub image: Arc<Image>,
}

impl Attachment {
    /// `None` for a format no provider takes, such as BMP or SVG.
    pub fn from_image(image: &Image) -> Option<Self> {
        Some(Self {
            media: media_of(image.format)?,
            image: Arc::new(image.clone()),
        })
    }

    pub fn from_path(path: &Path) -> Result<Self> {
        let media = path
            .extension()
            .and_then(|e| media_for_extension(&e.to_string_lossy()))
            .ok_or_else(|| eyre!("{} is not a PNG, JPEG, GIF or WebP image", path.display()))?;
        let bytes = fs::read(path).map_err(|e| eyre!("read {}: {e}", path.display()))?;
        Ok(Self {
            media,
            image: Arc::new(Image::from_bytes(format_of(media), bytes)),
        })
    }

    /// An image the agent already holds, from a resumed session.
    pub fn from_source(source: &ImageSource) -> Option<Self> {
        let bytes = STANDARD.decode(source.data.as_bytes()).ok()?;
        Some(Self {
            media: source.media_type,
            image: Arc::new(Image::from_bytes(format_of(source.media_type), bytes)),
        })
    }

    pub fn source(&self) -> ImageSource {
        ImageSource::new(self.media, STANDARD.encode(&self.image.bytes).into())
    }
}

/// `text` with the paths of `files` listed under it.
pub fn with_files(text: &str, files: &[PathBuf]) -> String {
    if files.is_empty() {
        return text.to_owned();
    }
    let list: String = files
        .iter()
        .map(|path| format!("\n- {}", path.display()))
        .collect();
    match text.is_empty() {
        true => format!("{FILES_HEADING}{list}"),
        false => format!("{text}\n\n{FILES_HEADING}{list}"),
    }
}

pub fn is_image(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| media_for_extension(&e.to_string_lossy()).is_some())
}

fn media_for_extension(extension: &str) -> Option<ImageMediaType> {
    match extension.to_ascii_lowercase().as_str() {
        "png" => Some(ImageMediaType::Png),
        "jpg" | "jpeg" => Some(ImageMediaType::Jpeg),
        "gif" => Some(ImageMediaType::Gif),
        "webp" => Some(ImageMediaType::Webp),
        _ => None,
    }
}

fn media_of(format: ImageFormat) -> Option<ImageMediaType> {
    match format {
        ImageFormat::Png => Some(ImageMediaType::Png),
        ImageFormat::Jpeg => Some(ImageMediaType::Jpeg),
        ImageFormat::Gif => Some(ImageMediaType::Gif),
        ImageFormat::Webp => Some(ImageMediaType::Webp),
        _ => None,
    }
}

fn format_of(media: ImageMediaType) -> ImageFormat {
    match media {
        ImageMediaType::Png => ImageFormat::Png,
        ImageMediaType::Jpeg => ImageFormat::Jpeg,
        ImageMediaType::Gif => ImageFormat::Gif,
        ImageMediaType::Webp => ImageFormat::Webp,
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const PIXELS: &[u8] = b"\x89PNG not really";

    #[test_case("shot.PNG", true ; "upper_case_png")]
    #[test_case("photo.jpeg", true ; "jpeg")]
    #[test_case("notes.txt", false ; "text")]
    #[test_case("no_extension", false ; "none")]
    fn recognises_image_files(name: &str, expected: bool) {
        assert_eq!(is_image(&PathBuf::from(name)), expected);
    }

    #[test_case("read these", &[], "read these" ; "no_files")]
    #[test_case("", &["a.pdf"], "Attached files:\n- a.pdf" ; "files_only")]
    #[test_case("look", &["a.pdf", "b.zip"], "look\n\nAttached files:\n- a.pdf\n- b.zip" ; "text_and_files")]
    fn files_are_listed_under_the_prompt(text: &str, files: &[&str], expected: &str) {
        let files: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
        assert_eq!(with_files(text, &files), expected);
    }

    #[test]
    fn survives_the_trip_through_base64() {
        let attachment = Attachment {
            media: ImageMediaType::Png,
            image: Arc::new(Image::from_bytes(ImageFormat::Png, PIXELS.to_vec())),
        };
        let back = Attachment::from_source(&attachment.source()).unwrap();
        assert_eq!(
            (back.media, back.image.bytes.as_slice()),
            (ImageMediaType::Png, PIXELS)
        );
    }

    #[test]
    fn refuses_formats_providers_do_not_take() {
        assert!(Attachment::from_image(&Image::from_bytes(ImageFormat::Bmp, Vec::new())).is_none());
    }
}
