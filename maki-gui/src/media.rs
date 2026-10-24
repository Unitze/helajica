//! Pictures and videos a reply embeds with markdown image syntax,
//! `![caption](path)`, split out of its text so the window can draw them:
//! an image in place, a video as a card that opens the system player.

use std::path::{Path, PathBuf};

const OPEN: &str = "![";
const TARGET_OPEN: &str = "](";
const TARGET_CLOSE: char = ')';
const FILE_SCHEME: &str = "file://";
const IMAGE_EXTENSIONS: [&str; 6] = ["png", "jpg", "jpeg", "gif", "webp", "bmp"];
const VIDEO_EXTENSIONS: [&str; 6] = ["mp4", "webm", "mov", "mkv", "avi", "m4v"];

#[derive(Debug, PartialEq)]
pub enum Segment<'a> {
    Text(&'a str),
    Image(PathBuf),
    Video(PathBuf),
}

/// `text` cut around every embed of a local image or video file. Web links
/// and other files stay in the text, for markdown to draw as it would.
pub fn split(text: &str) -> Vec<Segment<'_>> {
    let mut segments = Vec::new();
    let mut rest = text;
    let mut from = 0;
    while let Some(open) = rest[from..].find(OPEN).map(|i| from + i) {
        let Some((target, end)) = embed_target(&rest[open..]) else {
            from = open + OPEN.len();
            continue;
        };
        let Some(media) = classify(target) else {
            from = open + end;
            continue;
        };
        if open > 0 {
            segments.push(Segment::Text(&rest[..open]));
        }
        segments.push(media);
        rest = &rest[open + end..];
        from = 0;
    }
    if !rest.is_empty() {
        segments.push(Segment::Text(rest));
    }
    segments
}

/// The target of the embed `text` starts with, and the length of the embed.
fn embed_target(text: &str) -> Option<(&str, usize)> {
    let caption_end = text.find(TARGET_OPEN)?;
    if text[..caption_end].contains('\n') {
        return None;
    }
    let target_start = caption_end + TARGET_OPEN.len();
    let target_len = text[target_start..].find(TARGET_CLOSE)?;
    let target = &text[target_start..target_start + target_len];
    let target = target.trim().trim_start_matches('<').trim_end_matches('>');
    Some((target, target_start + target_len + 1))
}

fn classify(target: &str) -> Option<Segment<'static>> {
    let path = target
        .strip_prefix(FILE_SCHEME)
        .map(|rest| {
            rest.strip_prefix('/')
                .filter(|r| has_drive(r))
                .unwrap_or(rest)
        })
        .unwrap_or(target);
    if path.contains("://") {
        return None;
    }
    let path = PathBuf::from(path);
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    if IMAGE_EXTENSIONS.contains(&extension.as_str()) {
        Some(Segment::Image(path))
    } else if VIDEO_EXTENSIONS.contains(&extension.as_str()) {
        Some(Segment::Video(path))
    } else {
        None
    }
}

/// `C:/…` after `file:///`, where the slash belongs to the scheme.
fn has_drive(path: &str) -> bool {
    path.as_bytes().get(1) == Some(&b':')
}

/// A relative embed is read from the project folder.
pub fn resolve(path: &Path, project: Option<&Path>) -> PathBuf {
    match (path.is_relative(), project) {
        (true, Some(project)) => project.join(path),
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const PLOT: &str = "/tmp/plot.png";
    const CLIP: &str = "out/clip.mp4";

    #[test]
    fn plain_text_is_one_segment() {
        assert_eq!(split("no media here"), vec![Segment::Text("no media here")]);
    }

    #[test]
    fn an_image_splits_the_text_around_it() {
        assert_eq!(
            split("Here:\n![plot](/tmp/plot.png)\nDone."),
            vec![
                Segment::Text("Here:\n"),
                Segment::Image(PathBuf::from(PLOT)),
                Segment::Text("\nDone."),
            ]
        );
    }

    #[test_case("![clip](out/clip.mp4)", Segment::Video(PathBuf::from(CLIP)) ; "relative_video")]
    #[test_case("![clip](<out/clip.mp4>)", Segment::Video(PathBuf::from(CLIP)) ; "angle_brackets")]
    #[test_case("![p](file:///tmp/plot.png)", Segment::Image(PathBuf::from(PLOT)) ; "file_url")]
    #[test_case("![p](file:///C:/shots/a.PNG)", Segment::Image(PathBuf::from("C:/shots/a.PNG")) ; "windows_file_url")]
    fn reads_the_embed(text: &str, expected: Segment<'static>) {
        assert_eq!(split(text), vec![expected]);
    }

    #[test_case("![logo](https://example.com/logo.png)" ; "web_image")]
    #[test_case("![notes](notes.txt)" ; "not_media")]
    #[test_case("![broken](plot.png" ; "unclosed")]
    #[test_case("![a\nb](plot.png)" ; "caption_across_lines")]
    fn leaves_other_text_alone(text: &str) {
        assert_eq!(split(text), vec![Segment::Text(text)]);
    }

    #[test_case(Some("/proj"), "out/a.png", "/proj/out/a.png" ; "relative_joins_project")]
    #[test_case(None, "out/a.png", "out/a.png" ; "no_project")]
    fn resolves_against_the_project(project: Option<&str>, path: &str, expected: &str) {
        assert_eq!(
            resolve(Path::new(path), project.map(Path::new)),
            PathBuf::from(expected)
        );
    }
}
