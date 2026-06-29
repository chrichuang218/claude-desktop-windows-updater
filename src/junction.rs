//! Codex-style install-root layout for the Appx-managed Claude install.
//!
//! Claude itself remains installed and registered by Windows. These junctions
//! only give the updater root the familiar `versions/<version>/current` shape.

use crate::claude::ClaudePackageStatus;
use anyhow::{Context, Result};
use std::path::Path;

pub fn ensure_versions_layout(root: &Path, status: &ClaudePackageStatus) -> Result<()> {
    std::fs::create_dir_all(root.join("downloads"))
        .with_context(|| format!("creating {}", root.join("downloads").display()))?;
    let versions = root.join("versions");
    std::fs::create_dir_all(&versions)
        .with_context(|| format!("creating {}", versions.display()))?;

    let version_link = versions.join(&status.version);
    set_junction(&version_link, Path::new(&status.install_location))?;

    let current_link = versions.join("current");
    set_junction(&current_link, &version_link)?;

    Ok(())
}

pub fn remove(link: &Path) -> Result<()> {
    if !link.exists() && !is_reparse_point(link) {
        return Ok(());
    }
    std::fs::remove_dir(link).with_context(|| format!("removing junction at {}", link.display()))
}

fn set_junction(link: &Path, target: &Path) -> Result<()> {
    if !target.is_dir() {
        anyhow::bail!("junction target {} does not exist", target.display());
    }

    if link.exists() || is_reparse_point(link) {
        remove(link)?;
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        let mut command = std::process::Command::new("cmd");
        let status = command
            .args([
                "/c",
                "mklink",
                "/J",
                &link.to_string_lossy(),
                &target.to_string_lossy(),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .with_context(|| {
                format!(
                    "creating junction {} -> {}",
                    link.display(),
                    target.display()
                )
            })?;
        if !status.success() {
            anyhow::bail!("mklink /J failed with exit code {:?}", status.code());
        }
        Ok(())
    }

    #[cfg(not(windows))]
    {
        let _ = (link, target);
        anyhow::bail!("junctions only supported on Windows");
    }
}

fn is_reparse_point(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| metadata_is_reparse_point(&m))
        .unwrap_or(false)
}

#[cfg(windows)]
fn metadata_is_reparse_point(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse_point(meta: &std::fs::Metadata) -> bool {
    meta.file_type().is_symlink()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn creates_codex_style_versions_layout() {
        let base = std::env::temp_dir().join(format!(
            "claude-desktop-updater-junction-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let root = base.join("install-root");
        let appx = base.join("appx-target");
        std::fs::create_dir_all(&appx).expect("appx target");
        std::fs::write(appx.join("Claude.exe"), b"fake").expect("target marker");

        let status = ClaudePackageStatus {
            package_full_name: "Claude_1.2.3.4_x64__pzs8sxrjxfjjc".into(),
            package_family_name: "Claude_pzs8sxrjxfjjc".into(),
            version: "1.2.3.4".into(),
            architecture: "X64".into(),
            install_location: appx.to_string_lossy().into_owned(),
            signature_kind: Some("Store".into()),
        };

        ensure_versions_layout(&root, &status).expect("versions layout");

        assert!(root.join("downloads").is_dir());
        assert!(root
            .join("versions")
            .join("1.2.3.4")
            .join("Claude.exe")
            .is_file());
        assert!(root
            .join("versions")
            .join("current")
            .join("Claude.exe")
            .is_file());

        let _ = remove(&root.join("versions").join("current"));
        let _ = remove(&root.join("versions").join("1.2.3.4"));
        let _ = std::fs::remove_dir_all(base);
    }
}
