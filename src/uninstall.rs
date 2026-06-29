use crate::cleanup::{self, CleanupReport};
use crate::config::{Config, InstallMode, CONFIG_FILENAME};
use crate::{elevate, registry, safety, shortcut};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub enum UninstallMsg {
    Phase { phase: String, detail: String },
    Progress(Option<f32>),
    Done { log_path: String },
    Error(String),
}

pub struct UninstallContext {
    pub root: PathBuf,
    pub cfg: Config,
}

pub fn load_context() -> Result<UninstallContext> {
    let exe = std::env::current_exe().context("current_exe")?;
    let root = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("exe has no parent directory"))?
        .to_path_buf();
    let cfg = Config::load(&root.join(CONFIG_FILENAME))
        .context("loading updater.json next to launcher")?;
    Ok(UninstallContext { root, cfg })
}

pub fn need_elevation(ctx: &UninstallContext) -> bool {
    matches!(ctx.cfg.install_mode, InstallMode::System) && !elevate::is_elevated()
}

pub fn run_worker(ctx: UninstallContext, on_msg: impl Fn(UninstallMsg)) {
    let root = &ctx.root;

    on_msg(UninstallMsg::Phase {
        phase: "Validating install".into(),
        detail: root.display().to_string(),
    });
    on_msg(UninstallMsg::Progress(None));
    if let Err(e) = safety::validate_uninstall_root(root) {
        on_msg(UninstallMsg::Error(format!(
            "Refused to uninstall: {e}\n\nNo files have been modified."
        )));
        return;
    }

    on_msg(UninstallMsg::Phase {
        phase: "Removing Start Menu shortcut".into(),
        detail: "".into(),
    });
    if let Some(link) = shortcut::link_path(ctx.cfg.install_mode).ok().flatten() {
        let _ = shortcut::remove(&link);
    }

    on_msg(UninstallMsg::Phase {
        phase: "Removing registry entries".into(),
        detail: "".into(),
    });
    let _ = registry::remove(ctx.cfg.install_mode);

    on_msg(UninstallMsg::Phase {
        phase: "Deleting updater files".into(),
        detail: "".into(),
    });
    let mut report = CleanupReport::new();
    whitelist_delete(root, &mut report);

    on_msg(UninstallMsg::Phase {
        phase: "Finalizing".into(),
        detail: "".into(),
    });
    report.self_delete = cleanup::delete_self_exe();

    match cleanup::retry_delete_dir_only(root) {
        Ok(()) => report.deleted.push(root.clone()),
        Err(e) => report
            .skipped
            .push((root.clone(), format!("root rmdir: {e}"))),
    }

    let log_path = write_report(root, &report)
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    on_msg(UninstallMsg::Done { log_path });
}

fn whitelist_delete(root: &Path, report: &mut CleanupReport) {
    cleanup::retry_delete_dir_all(&root.join("downloads"), report);
    cleanup::retry_delete_dir_all(&root.join("versions"), report);
    cleanup::retry_delete_dir_all(&root.join("diagnostic_extract"), report);

    let cfg = root.join(CONFIG_FILENAME);
    match cleanup::retry_delete_file(&cfg) {
        Ok(()) => report.deleted.push(cfg),
        Err(e) => report.skipped.push((cfg, format!("{e}"))),
    }

    match crate::config::clear_state_file_if_ours(root) {
        Ok(Some(p)) => report.deleted.push(p),
        Ok(None) => {}
        Err(e) => report
            .skipped
            .push((std::path::PathBuf::from("state.json"), format!("{e}"))),
    }
}

fn write_report(root: &Path, report: &CleanupReport) -> Result<PathBuf> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let log_path = std::env::temp_dir().join(format!("claude-desktop-updater-uninstall-{ts}.log"));
    std::fs::write(&log_path, report.to_log_string(root))
        .with_context(|| format!("writing {}", log_path.display()))?;
    Ok(log_path)
}
