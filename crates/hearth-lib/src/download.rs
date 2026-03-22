use std::path::Path;

use anyhow::Context;
use tracing::info;

/// Asset pattern for the current platform.
pub fn platform_asset_pattern(base: &str) -> String {
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "amd64"
    };
    format!("{base}-darwin-{arch}")
}

/// Construct the GitHub Releases API URL for the latest release.
pub fn latest_release_url(repo: &str) -> String {
    format!("https://api.github.com/repos/{repo}/releases/latest")
}

/// Download a binary from a GitHub Release.
///
/// Hits the releases API, finds the asset matching `asset_pattern`,
/// downloads it, and extracts to `dest`. Supports `.tar.gz` and `.zip` archives.
pub async fn download_github_release(
    repo: &str,
    asset_pattern: &str,
    dest: &Path,
) -> anyhow::Result<()> {
    let url = latest_release_url(repo);

    let client = reqwest::Client::builder()
        .user_agent("hearth")
        .build()?;

    // Fetch latest release metadata
    let release: serde_json::Value = client
        .get(&url)
        .send()
        .await?
        .error_for_status()
        .context("failed to fetch latest release")?
        .json()
        .await?;

    // Find matching asset
    let assets = release["assets"]
        .as_array()
        .context("no assets in release")?;

    let asset = assets
        .iter()
        .find(|a| {
            a["name"]
                .as_str()
                .is_some_and(|n| n.contains(asset_pattern))
        })
        .context(format!("no asset matching '{asset_pattern}' in release"))?;

    let download_url = asset["browser_download_url"]
        .as_str()
        .context("asset missing download URL")?;

    let tag = release["tag_name"].as_str().unwrap_or("unknown");
    info!(repo, tag, asset = asset_pattern, "downloading release asset");

    // Download the asset
    let bytes = client
        .get(download_url)
        .send()
        .await?
        .error_for_status()
        .context("failed to download asset")?
        .bytes()
        .await?;

    // Ensure destination directory exists
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Extract based on file extension
    let asset_name = asset["name"].as_str().unwrap_or("");
    if asset_name.ends_with(".tar.gz") || asset_name.ends_with(".tgz") {
        let decoder = flate2::read::GzDecoder::new(&bytes[..]);
        let mut archive = tar::Archive::new(decoder);
        let parent = dest.parent().context("dest has no parent")?;
        archive.unpack(parent)?;
        info!(path = %dest.display(), "extracted release archive");
    } else if asset_name.ends_with(".zip") {
        let tmp = dest.with_extension("zip");
        std::fs::write(&tmp, &bytes)?;
        let file = std::fs::File::open(&tmp)?;
        let mut archive = zip::ZipArchive::new(file)?;
        let parent = dest.parent().context("dest has no parent")?;
        archive.extract(parent)?;
        std::fs::remove_file(&tmp)?;
        info!(path = %dest.display(), "extracted zip archive");
    } else {
        std::fs::write(dest, &bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))?;
        }
        info!(path = %dest.display(), "wrote binary");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_asset_pattern_contains_darwin() {
        let pattern = platform_asset_pattern("mailpit");
        assert!(pattern.contains("darwin"));
        assert!(pattern.starts_with("mailpit-darwin-"));
    }

    #[test]
    fn latest_release_url_format() {
        let url = latest_release_url("axllent/mailpit");
        assert_eq!(
            url,
            "https://api.github.com/repos/axllent/mailpit/releases/latest"
        );
    }
}
