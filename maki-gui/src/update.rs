//! Self-update from the GitHub releases of this app.
//!
//! Each release carries one raw binary per platform, named by
//! [`asset_name`]. Installing swaps the running executable in place: on
//! Windows the running file can be renamed but not overwritten, so it moves
//! aside to `.old` first and is cleaned up on the next launch.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as Process;

use color_eyre::Result;
use color_eyre::eyre::{bail, eyre};
use isahc::config::{Configurable as _, RedirectPolicy};
use isahc::{ReadResponseExt as _, Request, RequestExt as _};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

const LATEST_RELEASE: &str = "https://api.github.com/repos/Unitze/helajica/releases/latest";
const USER_AGENT: &str = concat!("helajica/", env!("CARGO_PKG_VERSION"));
const GITHUB_JSON: &str = "application/vnd.github+json";
const ASSET_PREFIX: &str = "helajica";
const SHA256_PREFIX: &str = "sha256:";
const STAGED_EXTENSION: &str = "new";
const REPLACED_EXTENSION: &str = "old";
/// Set to check for updates from a debug build, which otherwise never does
/// so that `cargo run` is not replaced by a release.
const FORCE_ENV: &str = "HELAJICA_UPDATE_CHECK";

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub version: Version,
    url: String,
    sha256: Option<String>,
}

#[derive(Deserialize)]
struct LatestRelease {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
}

pub fn enabled() -> bool {
    !cfg!(debug_assertions) || std::env::var_os(FORCE_ENV).is_some()
}

/// `helajica-windows-x86_64.exe`, `helajica-macos-aarch64`, ...
pub fn asset_name() -> String {
    format!(
        "{ASSET_PREFIX}-{}-{}{}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::env::consts::EXE_SUFFIX
    )
}

/// The latest release when it is newer than this build and ships a binary
/// for this platform. Blocks on the network.
pub fn check() -> Result<Option<Release>> {
    let mut response = Request::get(LATEST_RELEASE)
        .header("User-Agent", USER_AGENT)
        .header("Accept", GITHUB_JSON)
        .redirect_policy(RedirectPolicy::Follow)
        .body(())?
        .send()?;
    if !response.status().is_success() {
        bail!("GitHub answered {}", response.status());
    }
    let latest: LatestRelease = serde_json::from_str(&response.text()?)?;
    Ok(newer_release(
        latest,
        &Version::parse(CURRENT)?,
        &asset_name(),
    ))
}

fn newer_release(latest: LatestRelease, current: &Version, asset: &str) -> Option<Release> {
    let version = Version::parse(latest.tag_name.trim_start_matches('v')).ok()?;
    if version <= *current {
        return None;
    }
    let asset = latest.assets.into_iter().find(|a| a.name == asset)?;
    Some(Release {
        version,
        url: asset.browser_download_url,
        sha256: asset
            .digest
            .and_then(|d| d.strip_prefix(SHA256_PREFIX).map(str::to_owned)),
    })
}

/// Downloads `release` over the running executable and returns its path.
pub fn install(release: &Release) -> Result<PathBuf> {
    let mut response = Request::get(&release.url)
        .header("User-Agent", USER_AGENT)
        .redirect_policy(RedirectPolicy::Follow)
        .body(())?
        .send()?;
    if !response.status().is_success() {
        bail!("download answered {}", response.status());
    }
    let bytes = response.bytes()?;
    if let Some(expected) = &release.sha256 {
        let actual = hex(&Sha256::digest(&bytes));
        if !actual.eq_ignore_ascii_case(expected) {
            bail!("checksum mismatch: expected {expected}, got {actual}");
        }
    }
    let exe = std::env::current_exe()?;
    replace(&exe, &bytes)?;
    Ok(exe)
}

fn replace(exe: &Path, bytes: &[u8]) -> Result<()> {
    let staged = exe.with_extension(STAGED_EXTENSION);
    fs::write(&staged, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))?;
    }
    if cfg!(windows) {
        let old = exe.with_extension(REPLACED_EXTENSION);
        let _ = fs::remove_file(&old);
        fs::rename(exe, &old)?;
    }
    fs::rename(&staged, exe).map_err(|e| eyre!("install {}: {e}", exe.display()))
}

/// Starts the freshly installed executable; the caller quits after.
pub fn relaunch(exe: &Path) -> Result<()> {
    Process::new(exe)
        .args(std::env::args_os().skip(1))
        .spawn()?;
    Ok(())
}

/// Removes what the previous update moved aside. Best effort.
pub fn clean_up() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = fs::remove_file(exe.with_extension(REPLACED_EXTENSION));
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const ASSET: &str = "helajica-windows-x86_64.exe";
    const URL: &str = "https://example.com/helajica.exe";

    fn latest(tag: &str, asset: &str) -> LatestRelease {
        LatestRelease {
            tag_name: tag.into(),
            assets: vec![Asset {
                name: asset.into(),
                browser_download_url: URL.into(),
                digest: Some("sha256:abc".into()),
            }],
        }
    }

    #[test_case("v0.2.0", ASSET, Some("0.2.0") ; "newer")]
    #[test_case("v0.1.0", ASSET, None ; "same")]
    #[test_case("v0.0.9", ASSET, None ; "older")]
    #[test_case("v0.2.0", "helajica-linux-x86_64", None ; "no_asset_for_platform")]
    #[test_case("nightly", ASSET, None ; "not_semver")]
    fn offers_only_newer_releases(tag: &str, asset: &str, expected: Option<&str>) {
        let offered = newer_release(latest(tag, asset), &Version::new(0, 1, 0), ASSET);
        assert_eq!(offered.map(|r| r.version.to_string()).as_deref(), expected);
    }

    #[test]
    fn reads_the_digest() {
        let offered =
            newer_release(latest("v1.0.0", ASSET), &Version::new(0, 1, 0), ASSET).unwrap();
        assert_eq!(
            (offered.url.as_str(), offered.sha256.as_deref()),
            (URL, Some("abc"))
        );
    }

    #[test]
    fn replace_swaps_the_file_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("app.exe");
        fs::write(&exe, b"old").unwrap();
        replace(&exe, b"new").unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"new");
        assert!(!exe.with_extension(STAGED_EXTENSION).exists());
    }
}
