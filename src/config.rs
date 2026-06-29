use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const CONFIG_FILENAME: &str = "updater.json";
pub const UPDATER_EXE_NAME: &str = "Claude Desktop Updater.exe";
pub const LEGACY_UPDATER_EXE_NAME: &str = "claude-launcher.exe";
pub const APP_DATA_DIR_NAME: &str = "ClaudeDesktopUpdater";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InstallMode {
    Portable,
    User,
    System,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum UpdatePolicy {
    Always,
    #[default]
    Daily,
    Weekly,
    Never,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Fetcher {
    #[default]
    #[serde(rename = "official_msix", alias = "winget", alias = "official")]
    Winget,
    LocalMsix,
    ExtractDiagnostic,
}

impl Fetcher {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "winget" | "official" | "official_msix" => Some(Self::Winget),
            "local" | "local_msix" | "msix" => Some(Self::LocalMsix),
            "extract" | "extract_diagnostic" | "diagnostic" => Some(Self::ExtractDiagnostic),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AppLanguage {
    #[default]
    ZhCn,
    EnUs,
}

impl AppLanguage {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "zh" | "zh-cn" | "cn" | "chinese" => Some(Self::ZhCn),
            "en" | "en-us" | "english" => Some(Self::EnUs),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub install_mode: InstallMode,
    pub current_package_version: String,
    pub current_app_version: Option<String>,
    pub known_latest: Option<String>,
    #[serde(default)]
    pub update_policy: UpdatePolicy,
    #[serde(default)]
    pub last_check_unix: Option<u64>,
    #[serde(default)]
    pub skipped_version: Option<String>,
    #[serde(default)]
    pub fetcher: Fetcher,
    #[serde(default = "default_arch")]
    pub arch: String,
    #[serde(default = "default_true")]
    pub post_update_register: bool,
    #[serde(default)]
    pub keep_downloads: bool,
    #[serde(default = "default_true")]
    pub register_uninstall: bool,
    #[serde(default)]
    pub create_shortcut: bool,
    #[serde(default)]
    pub language: AppLanguage,
}

fn default_true() -> bool {
    true
}

fn default_arch() -> String {
    "x64".into()
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&raw)?)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let raw = serde_json::to_string_pretty(self)?;
        std::fs::write(path, raw)?;
        Ok(())
    }

    pub fn save_install(&self, install_root: &Path) -> anyhow::Result<()> {
        self.save(&install_root.join(CONFIG_FILENAME))?;
        let _ = clear_state_file_if_ours(install_root);
        Ok(())
    }
}

pub fn clear_state_file_if_ours(
    install_root: &Path,
) -> std::io::Result<Option<std::path::PathBuf>> {
    let Some(state_path) = state_file_path() else {
        return Ok(None);
    };
    let Ok(raw) = std::fs::read_to_string(&state_path) else {
        return Ok(None);
    };
    let Ok(state) = serde_json::from_str::<StateFile>(&raw) else {
        return Ok(None);
    };
    if !paths_equal(&state.install_root, install_root) {
        return Ok(None);
    }
    std::fs::remove_file(&state_path)?;
    Ok(Some(state_path))
}

#[derive(Debug, Serialize, Deserialize)]
struct StateFile {
    install_root: PathBuf,
    config: Config,
}

fn state_file_path() -> Option<PathBuf> {
    let base = std::env::var("LOCALAPPDATA").ok()?;
    Some(
        PathBuf::from(base)
            .join(APP_DATA_DIR_NAME)
            .join("state.json"),
    )
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    if let (Ok(ca), Ok(cb)) = (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        return ca == cb;
    }
    let norm = |p: &Path| {
        p.to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    };
    norm(a) == norm(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_claude_updater_config_shape() {
        let cfg = Config {
            install_mode: InstallMode::User,
            current_package_version: "1.15962.1.0".into(),
            current_app_version: Some("42.4.0".into()),
            known_latest: Some("1.15962.1".into()),
            update_policy: UpdatePolicy::Daily,
            last_check_unix: Some(123),
            skipped_version: None,
            fetcher: Fetcher::Winget,
            arch: "x64".into(),
            post_update_register: true,
            keep_downloads: false,
            register_uninstall: true,
            create_shortcut: true,
            language: AppLanguage::ZhCn,
        };

        let raw = serde_json::to_string(&cfg).expect("json");

        assert!(raw.contains("current_package_version"));
        assert!(raw.contains("current_app_version"));
        assert!(raw.contains("post_update_register"));
        assert!(raw.contains("keep_downloads"));
        assert!(raw.contains("language"));
        assert!(raw.contains("official_msix"));
        assert!(!raw.contains("current_version"));
        assert!(!raw.contains("use_current_junction"));
    }

    #[test]
    fn parses_fetcher_aliases() {
        assert_eq!(Fetcher::parse("official"), Some(Fetcher::Winget));
        assert_eq!(Fetcher::parse("official_msix"), Some(Fetcher::Winget));
        assert_eq!(Fetcher::parse("local_msix"), Some(Fetcher::LocalMsix));
        assert_eq!(
            Fetcher::parse("extract_diagnostic"),
            Some(Fetcher::ExtractDiagnostic)
        );
    }

    #[test]
    fn parses_language_aliases() {
        assert_eq!(AppLanguage::parse("zh-CN"), Some(AppLanguage::ZhCn));
        assert_eq!(AppLanguage::parse("english"), Some(AppLanguage::EnUs));
    }
}
