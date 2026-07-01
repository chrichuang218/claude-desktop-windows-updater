#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod claude;
mod cleanup;
mod config;
mod dark_window;
mod dialogs;
mod elevate;
mod junction;
mod path_dialog;
mod registry;
mod safety;
mod shortcut;
mod uninstall;

use anyhow::Context;
use config::{
    AppLanguage, Config, InstallMode, UpdatePolicy, APP_DATA_DIR_NAME, CONFIG_FILENAME,
    LEGACY_UPDATER_EXE_NAME, UPDATER_EXE_NAME,
};
use slint::ComponentHandle;
use std::cmp::Ordering;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

slint::include_modules!();

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--self-test") {
        return run_self_test();
    }
    if handle_cli(&args)? {
        return Ok(());
    }

    if installed_config_path()?.exists() {
        return run_installed_mode();
    }

    dark_window::install();
    let ui = AppWindow::new()?;
    prepare_window(&ui);
    wire_installer_ui(&ui, parse_auto_install(&args))?;
    show_when_ready(&ui);
    slint::run_event_loop()?;
    Ok(())
}

fn run_self_test() -> anyhow::Result<()> {
    claude::run_powershell("$PSVersionTable.PSVersion | Out-Null; exit 0")
        .context("self-test PowerShell launch")?;
    Ok(())
}

#[derive(Debug, Clone)]
struct InstalledUpdateContext {
    root: PathBuf,
    cfg: Config,
    status: Option<claude::ClaudePackageStatus>,
    latest: claude::OfficialMsixMetadata,
}

fn finish_ui_session(ui_weak: &slint::Weak<AppWindow>) {
    if let Some(ui) = ui_weak.upgrade() {
        let _ = ui.window().hide();
    }
    let _ = slint::quit_event_loop();

    // Some final screens are reached after elevation and background worker hops.
    // If Slint does not tear down promptly, make Close/Launch deterministic.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(250));
        std::process::exit(0);
    });
}

fn run_installed_mode() -> anyhow::Result<()> {
    let cfg_path = installed_config_path()?;
    let root = cfg_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("updater.json has no parent directory"))?
        .to_path_buf();
    let mut cfg = Config::load(&cfg_path)?;

    let current_status = match claude::query_package_status() {
        Ok(status) => {
            sync_installed_config(&root, &mut cfg, &status, None)?;
            Some(status)
        }
        Err(_) => None,
    };

    if current_status.is_some() && !update_check_due(&cfg) {
        if !matches!(cfg.update_policy, UpdatePolicy::Never) {
            if let (Some(status), Some(known_latest)) =
                (current_status.as_ref(), cfg.known_latest.clone())
            {
                if let Some(current_screen) = cached_update_screen(
                    status,
                    Some(&known_latest),
                    cfg.skipped_version.as_deref(),
                ) {
                    return show_update_prompt(
                        InstalledUpdateContext {
                            root,
                            cfg,
                            status: Some(status.clone()),
                            latest: claude::OfficialMsixMetadata {
                                version: known_latest,
                                msix_url: String::new(),
                            },
                        },
                        current_screen,
                    );
                }
            }
        }
        return claude::launch_registered_claude();
    }

    let latest = claude::query_official_msix_metadata()?;
    if let Some(status) = current_status.as_ref() {
        sync_installed_config(&root, &mut cfg, status, Some(latest.version.clone()))?;
    } else {
        cfg.known_latest = Some(latest.version.clone());
        cfg.last_check_unix = Some(now_unix());
        cfg.save_install(&root)?;
    }

    if current_status.is_some() && cfg.skipped_version.as_deref() == Some(latest.version.as_str()) {
        return claude::launch_registered_claude();
    }

    let current_screen =
        update_screen_for_status(current_status.as_ref(), &latest.version, None).unwrap_or(12);

    show_update_prompt(
        InstalledUpdateContext {
            root,
            cfg,
            status: current_status,
            latest,
        },
        current_screen,
    )
}

fn cached_update_screen(
    current: &claude::ClaudePackageStatus,
    known_latest: Option<&str>,
    skipped_version: Option<&str>,
) -> Option<i32> {
    let known_latest = known_latest?;
    if skipped_version == Some(known_latest) {
        return None;
    }

    update_screen_for_status(Some(current), known_latest, skipped_version)
}

fn update_screen_for_status(
    current: Option<&claude::ClaudePackageStatus>,
    latest_version: &str,
    skipped_version: Option<&str>,
) -> Option<i32> {
    if current.is_none() {
        return Some(14);
    }
    if skipped_version == Some(latest_version) {
        return None;
    }
    if current_install_satisfies_official_msix(current?, latest_version) {
        Some(13)
    } else {
        Some(12)
    }
}

fn show_update_prompt(ctx: InstalledUpdateContext, current_screen: i32) -> anyhow::Result<()> {
    dark_window::install();
    let ui = AppWindow::new()?;
    prepare_window(&ui);
    ui.set_language(language_to_int(ctx.cfg.language));
    ui.set_current_screen(current_screen);
    ui.set_update_current_version(
        ctx.status
            .as_ref()
            .map(|status| claude::display_version(&status.version))
            .unwrap_or_else(|| missing_appx_label(ctx.cfg.language).into())
            .into(),
    );
    ui.set_update_latest_version(claude::display_version(&ctx.latest.version).into());
    wire_update_ui(&ui, ctx)?;
    show_when_ready(&ui);
    slint::run_event_loop()?;
    Ok(())
}

fn handle_cli(args: &[String]) -> anyhow::Result<bool> {
    if args.iter().any(|a| a == "--status") {
        return run_status().map(|_| true);
    }
    if args.iter().any(|a| a == "--check") {
        return run_check().map(|_| true);
    }
    if args.iter().any(|a| a == "--repair-register") {
        return run_repair_register().map(|_| true);
    }
    reject_unsupported_legacy_cli(args)?;
    if args.iter().any(|a| a == "--update") {
        return run_update_cli().map(|_| true);
    }
    if args.iter().any(|a| a == "--auto-update") {
        return run_auto_update_ui().map(|_| true);
    }
    if args.iter().any(|a| a == "--launch") {
        return claude::launch_registered_claude().map(|_| true);
    }
    if args.iter().any(|a| a == "--uninstall") {
        return run_uninstall_ui().map(|_| true);
    }
    Ok(false)
}

fn reject_unsupported_legacy_cli(args: &[String]) -> anyhow::Result<()> {
    const REMOVED: &[&str] = &["--msix", "--extract-msix", "--source", "--msix-path"];
    if let Some(flag) = args.iter().find(|arg| REMOVED.contains(&arg.as_str())) {
        anyhow::bail!(
            "{flag} is no longer supported. Claude Desktop Updater now installs only Anthropic's official MSIX/Appx package; use --update."
        );
    }
    Ok(())
}

fn run_status() -> anyhow::Result<()> {
    let status = claude::query_package_status()?;

    println!("Claude package:");
    println!("  PackageFullName   : {}", status.package_full_name);
    println!("  PackageFamilyName : {}", status.package_family_name);
    println!("  Version           : {}", status.version);
    println!(
        "  App version       : {}",
        read_app_version(&status).unwrap_or_else(|| "unknown".into())
    );
    println!("  Architecture      : {}", status.architecture);
    println!("  InstallLocation   : {}", status.install_location);
    println!(
        "  SignatureKind     : {}",
        status.signature_kind.as_deref().unwrap_or("unknown")
    );
    let app_id_registered = claude::query_start_apps_registered()?;
    let protocol_registered = claude::query_protocol_registered()?;
    let manifest = claude::query_manifest_integrations(&status)?;
    println!("  Install type      : Appx");
    println!("  AppID registered  : {}", app_id_registered);
    println!("  Protocol registered: {}", protocol_registered);
    println!("  StartupTask declared: {}", manifest.startup_task);
    println!("  Service declared  : {}", manifest.service);
    println!(
        "  Firewall declared : {}",
        manifest.claude_firewall && manifest.cowork_firewall
    );
    println!("  Launch AppID      : {}", claude::CLAUDE_APP_ID);
    Ok(())
}

fn run_check() -> anyhow::Result<()> {
    let metadata = claude::query_official_msix_metadata()?;
    println!("Latest Claude MSIX:");
    println!("  Version       : {}", metadata.version);
    println!("  MSIX URL      : {}", metadata.msix_url);
    Ok(())
}

fn run_repair_register() -> anyhow::Result<()> {
    let before = claude::query_package_status()?;
    println!("Repairing Claude Appx registration...");
    claude::run_powershell(&claude::register_manifest_command(&before))?;
    verify_claude_registration()?;
    println!("Claude registration is healthy.");
    Ok(())
}

fn run_update_cli() -> anyhow::Result<()> {
    if !elevate::is_elevated() {
        elevate::respawn_elevated("--update")?;
        return Ok(());
    }

    let before = claude::query_package_status().ok();
    let latest = claude::query_official_msix_metadata().ok();
    let status = update_from_winget(false, true)?;
    if before
        .as_ref()
        .zip(latest.as_ref())
        .is_some_and(|(before, latest)| {
            claude::compare_versions(&latest.version, &before.version) != Ordering::Greater
        })
    {
        println!("Claude is current and registered.");
    } else {
        println!("Claude updated and registered.");
    }
    println!("  Package version: {}", status.version);
    Ok(())
}

fn run_auto_update_ui() -> anyhow::Result<()> {
    if !elevate::is_elevated() {
        elevate::respawn_elevated("--auto-update")?;
        return Ok(());
    }

    let cfg_path = installed_config_path()?;
    let root = cfg_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("updater.json has no parent directory"))?
        .to_path_buf();
    let cfg = Config::load(&cfg_path)?;
    let status = claude::query_package_status().ok();
    let latest = claude::query_official_msix_metadata()?;
    let ctx = InstalledUpdateContext {
        root,
        cfg: cfg.clone(),
        status,
        latest,
    };

    dark_window::install();
    let ui = AppWindow::new()?;
    prepare_window(&ui);
    ui.set_language(language_to_int(cfg.language));
    wire_update_ui(&ui, ctx.clone())?;
    ui.set_current_screen(4);
    let (phase, detail) = progress_text(
        cfg.language,
        "Resolving official MSIX",
        "Downloading official installer",
    );
    ui.set_progress_phase(phase.into());
    ui.set_progress_detail(detail.into());
    ui.set_progress_indeterminate(true);
    start_gui_update(ui.as_weak(), ctx);
    show_when_ready(&ui);
    slint::run_event_loop()?;
    Ok(())
}

fn update_from_winget(
    keep_downloads: bool,
    print: bool,
) -> anyhow::Result<claude::ClaudePackageStatus> {
    update_from_winget_with_progress(keep_downloads, print, |_phase, _detail, _fraction| {})
}

fn update_from_winget_with_progress(
    keep_downloads: bool,
    print: bool,
    progress: impl Fn(&str, &str, Option<f32>),
) -> anyhow::Result<claude::ClaudePackageStatus> {
    progress(
        "Resolving official MSIX",
        "Downloading official installer",
        None,
    );
    let metadata = claude::query_official_msix_metadata()?;

    if let Ok(current) = claude::query_package_status() {
        if current_install_satisfies_official_msix(&current, &metadata.version) {
            if print {
                println!(
                    "Claude is already current: package {} / latest {}",
                    current.version, metadata.version
                );
            }
            progress("Verifying registration", "Claude is already current", None);
            verify_claude_registration()?;
            return Ok(current);
        }
    }

    let download_dir = claude_download_dir();
    std::fs::create_dir_all(&download_dir)?;
    let installer = download_dir.join(format!("Claude-{}.msix", metadata.version));

    if print {
        println!("Downloading Claude {}...", metadata.version);
    }
    progress(
        "Downloading Claude",
        &format!("Preparing official installer {}", metadata.version),
        Some(0.0),
    );
    download_file(&metadata.msix_url, &installer, |done, total| {
        progress(
            "Downloading Claude",
            &format_download_detail(done, total),
            total.map(|t| (done as f32 / t as f32).clamp(0.0, 1.0)),
        );
    })?;
    if print {
        println!("Running installer: {}", installer.display());
    }
    progress(
        "Closing Claude",
        "Stopping running Claude processes before update",
        None,
    );
    claude::stop_running_claude()?;
    prepare_appx_for_official_msix_update(&metadata.version, &progress)?;
    progress(
        "Running Claude installer",
        "Installing official MSIX package",
        None,
    );
    claude::run_powershell(&claude::msix_install_command(&installer))?;
    claude::stop_running_claude()?;
    if !keep_downloads {
        let _ = std::fs::remove_file(&installer);
    }
    progress(
        "Verifying registration",
        "Checking Windows Appx registration",
        None,
    );
    let status = wait_for_installed_version(&metadata.version)?;
    verify_claude_registration()?;
    Ok(status)
}

fn prepare_appx_for_official_msix_update(
    latest_version: &str,
    progress: &impl Fn(&str, &str, Option<f32>),
) -> anyhow::Result<()> {
    let Ok(appx) = claude::query_package_status() else {
        return Ok(());
    };
    if !claude::package_is_developer_signed(&appx) {
        return Ok(());
    }
    if claude::compare_versions(&appx.version, latest_version) != Ordering::Less {
        return Ok(());
    }

    progress(
        "Preparing Appx registration",
        "Removing developer-registered Claude package",
        None,
    );
    claude::run_powershell(&claude::remove_package_command(&appx))?;
    for _ in 0..30 {
        if claude::query_package_status().is_err() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    anyhow::bail!(
        "Timed out removing developer-registered Claude package {}",
        appx.package_full_name
    )
}

fn current_install_satisfies_official_msix(
    current: &claude::ClaudePackageStatus,
    latest_version: &str,
) -> bool {
    claude::package_is_appx(current)
        && claude::compare_versions(latest_version, &current.version) != Ordering::Greater
}

fn wait_for_installed_version(latest_version: &str) -> anyhow::Result<claude::ClaudePackageStatus> {
    let mut last_status = None;
    for _ in 0..30 {
        if let Ok(status) = claude::query_package_status() {
            if ensure_installed_version(&status.version, latest_version).is_ok() {
                return Ok(status);
            }
            last_status = Some(status);
        }
        std::thread::sleep(Duration::from_secs(1));
    }

    let installed = last_status
        .as_ref()
        .map(|status| status.version.as_str())
        .unwrap_or("unknown");
    ensure_installed_version(installed, latest_version)?;
    unreachable!()
}

fn ensure_installed_version(installed_version: &str, latest_version: &str) -> anyhow::Result<()> {
    if claude::compare_versions(latest_version, installed_version) == Ordering::Greater {
        anyhow::bail!(
            "Claude installer finished, but Claude is still on package {} instead of latest {}",
            installed_version,
            latest_version
        );
    }
    Ok(())
}

fn wire_update_ui(ui: &AppWindow, ctx: InstalledUpdateContext) -> anyhow::Result<()> {
    let ctx = std::sync::Arc::new(std::sync::Mutex::new(ctx));

    {
        let ui_weak = ui.as_weak();
        ui.on_request_quit(move || {
            finish_ui_session(&ui_weak);
        });
    }

    {
        let ui_weak = ui.as_weak();
        let ctx = ctx.clone();
        ui.on_request_launch(move || {
            if let Err(e) = claude::launch_registered_claude() {
                let language = ctx
                    .lock()
                    .ok()
                    .map(|guard| guard.cfg.language)
                    .unwrap_or_default();
                show_dialog_error(language, GuiErrorContext::Launch, &e);
            }
            finish_ui_session(&ui_weak);
        });
    }

    {
        let ui_weak = ui.as_weak();
        let ctx = ctx.clone();
        ui.on_request_update(move |action| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let Some(ctx_snapshot) = ctx.lock().ok().map(|guard| (*guard).clone()) else {
                return;
            };

            if action == 0 {
                begin_update_action(&ui_weak, &ui, ctx_snapshot);
                return;
            }

            if let Err(e) = defer_update(action, &ctx_snapshot) {
                set_gui_error(&ui, ctx_snapshot.cfg.language, GuiErrorContext::General, &e);
                return;
            }
            let _ = claude::launch_registered_claude();
            finish_ui_session(&ui_weak);
        });
    }

    {
        let ui_weak = ui.as_weak();
        let ctx = ctx.clone();
        ui.on_request_error_retry(move |action| {
            if action != ErrorRetryAction::Update.as_i32() {
                return;
            }
            let Some(ui) = ui_weak.upgrade() else { return };
            let Some(ctx_snapshot) = ctx.lock().ok().map(|guard| (*guard).clone()) else {
                return;
            };
            begin_update_action(&ui_weak, &ui, ctx_snapshot);
        });
    }

    {
        let ui_weak = ui.as_weak();
        ui.on_request_copy_error(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let detail = ui.get_error_detail().to_string();
            if let Err(e) = copy_text_to_clipboard(&detail) {
                dialogs::error(&format!("Could not copy error details.\n\n{e:#}"));
            }
        });
    }

    Ok(())
}

fn begin_update_action(
    ui_weak: &slint::Weak<AppWindow>,
    ui: &AppWindow,
    ctx_snapshot: InstalledUpdateContext,
) {
    if !elevate::is_elevated() {
        match elevate::respawn_elevated("--auto-update") {
            Ok(()) => {
                finish_ui_session(ui_weak);
            }
            Err(e) => {
                let err = anyhow::anyhow!("Couldn't obtain admin rights: {e:#}");
                set_gui_error(ui, ctx_snapshot.cfg.language, GuiErrorContext::Update, &err);
            }
        }
        return;
    }

    let (phase, detail) = progress_text(
        ctx_snapshot.cfg.language,
        "Resolving official MSIX",
        "Downloading official installer",
    );
    ui.set_current_screen(4);
    ui.set_progress_phase(phase.into());
    ui.set_progress_detail(detail.into());
    ui.set_progress_indeterminate(true);
    start_gui_update(ui.as_weak(), ctx_snapshot);
}

fn start_gui_update(ui_weak: slint::Weak<AppWindow>, ctx: InstalledUpdateContext) {
    std::thread::spawn(move || {
        let language = ctx.cfg.language;
        let result = run_gui_update(&ctx, |phase, detail, fraction| {
            let weak = ui_weak.clone();
            let (phase, detail) = progress_text(language, phase, detail);
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak.upgrade() else { return };
                ui.set_progress_phase(phase.into());
                ui.set_progress_detail(detail.into());
                match fraction {
                    Some(fraction) => {
                        ui.set_progress_indeterminate(false);
                        ui.set_progress_fraction(fraction);
                    }
                    None => ui.set_progress_indeterminate(true),
                }
            });
        });

        let weak = ui_weak.clone();
        let latest_version = ctx.latest.version.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            match result {
                Ok(status) => {
                    ui.set_update_current_version(claude::display_version(&status.version).into());
                    ui.set_update_latest_version(claude::display_version(&latest_version).into());
                    ui.set_current_screen(13);
                }
                Err(e) => {
                    set_gui_error(&ui, language, GuiErrorContext::Update, &e);
                }
            }
        });
    });
}

fn run_gui_update(
    ctx: &InstalledUpdateContext,
    progress: impl Fn(&str, &str, Option<f32>),
) -> anyhow::Result<claude::ClaudePackageStatus> {
    progress(
        "Resolving official MSIX",
        "Downloading official installer",
        None,
    );
    let status = update_from_winget_with_progress(ctx.cfg.keep_downloads, false, &progress)?;
    progress("Updating launcher", "Writing updater state", None);
    install_updater_files(
        &GuiOptions {
            mode: ctx.cfg.install_mode,
            root: ctx.root.clone(),
            create_shortcut: ctx.cfg.create_shortcut,
            register_uninstall: ctx.cfg.register_uninstall,
            keep_downloads: ctx.cfg.keep_downloads,
            language: ctx.cfg.language,
        },
        &status,
    )?;
    Ok(status)
}

fn defer_update(action: i32, ctx: &InstalledUpdateContext) -> anyhow::Result<()> {
    let mut cfg = ctx.cfg.clone();
    cfg.known_latest = Some(ctx.latest.version.clone());
    cfg.last_check_unix = Some(match action {
        4 => now_unix() + 6 * 24 * 60 * 60,
        _ => now_unix(),
    });
    match action {
        2 => cfg.skipped_version = Some(ctx.latest.version.clone()),
        5 => cfg.update_policy = UpdatePolicy::Never,
        _ => {}
    }
    cfg.save_install(&ctx.root)
}

fn sync_installed_config(
    root: &Path,
    cfg: &mut Config,
    status: &claude::ClaudePackageStatus,
    known_latest: Option<String>,
) -> anyhow::Result<()> {
    cfg.current_package_version = status.version.clone();
    cfg.current_app_version = read_app_version(status);
    cfg.arch = status.architecture.to_ascii_lowercase();
    if let Some(latest) = known_latest {
        cfg.known_latest = Some(latest);
        cfg.last_check_unix = Some(now_unix());
    }
    cfg.save_install(root)?;
    junction::ensure_versions_layout(root, status)
}

fn update_check_due(cfg: &Config) -> bool {
    let Some(last_check) = cfg.last_check_unix else {
        return true;
    };
    let now = now_unix();
    match cfg.update_policy {
        UpdatePolicy::Always => true,
        UpdatePolicy::Daily => now >= last_check.saturating_add(24 * 60 * 60),
        UpdatePolicy::Weekly => now >= last_check.saturating_add(7 * 24 * 60 * 60),
        UpdatePolicy::Never => false,
    }
}

fn verify_claude_registration() -> anyhow::Result<()> {
    let status = claude::query_package_status()?;
    if status.package_family_name != claude::CLAUDE_PACKAGE_FAMILY {
        anyhow::bail!(
            "unexpected Claude package family: {}",
            status.package_family_name
        );
    }
    let app_id_registered = claude::query_start_apps_registered()?;
    let protocol_registered = claude::query_protocol_registered()?;
    let manifest = claude::query_manifest_integrations(&status)?;
    if claude::registration_needs_repair(app_id_registered, protocol_registered, manifest) {
        claude::run_powershell(&claude::register_manifest_command(&status))?;
        if !claude::query_start_apps_registered()? {
            anyhow::bail!("Claude AppID was not registered after repair");
        }
        if !claude::query_protocol_registered()? {
            anyhow::bail!("Claude URL protocol was not registered after repair");
        }
        if !claude::query_manifest_integrations(&status)?.complete() {
            anyhow::bail!(
                "Claude AppxManifest.xml is missing startup task, service, or firewall declarations"
            );
        }
    }
    Ok(())
}

fn download_file(
    url: &str,
    dest: &Path,
    progress: impl Fn(u64, Option<u64>),
) -> anyhow::Result<()> {
    if dest.exists() {
        let len = dest.metadata().map(|m| m.len()).ok();
        progress(len.unwrap_or(0), len);
        return Ok(());
    }

    let partial = dest.with_extension(format!(
        "{}.partial",
        dest.extension()
            .and_then(|s| s.to_str())
            .unwrap_or("download")
    ));
    let _ = std::fs::remove_file(&partial);

    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(20 * 60))
        .build()?;
    let mut response = client.get(url).send()?;
    if !response.status().is_success() {
        anyhow::bail!("download failed with HTTP {}", response.status());
    }
    let total = response.content_length();

    let mut file = std::fs::File::create(&partial)?;
    let mut buf = [0u8; 128 * 1024];
    let mut downloaded = 0u64;
    progress(downloaded, total);
    loop {
        let read = response.read(&mut buf)?;
        if read == 0 {
            break;
        }
        file.write_all(&buf[..read])?;
        downloaded += read as u64;
        progress(downloaded, total);
        if total.is_some_and(|t| downloaded >= t) {
            break;
        }
    }
    file.flush()?;
    drop(file);

    std::fs::rename(&partial, dest)
        .with_context(|| format!("moving download into place at {}", dest.display()))?;
    Ok(())
}

fn format_download_detail(done: u64, total: Option<u64>) -> String {
    match total {
        Some(total) if total > 0 => format!(
            "{} / {} ({:.0}%)",
            format_bytes(done),
            format_bytes(total),
            (done as f64 / total as f64 * 100.0).clamp(0.0, 100.0)
        ),
        _ => format!("{} downloaded", format_bytes(done)),
    }
}

fn format_bytes(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    const KIB: f64 = 1024.0;
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / MIB)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / KIB)
    } else {
        format!("{bytes} B")
    }
}

#[derive(Debug, Clone)]
struct GuiOptions {
    mode: InstallMode,
    root: PathBuf,
    create_shortcut: bool,
    register_uninstall: bool,
    keep_downloads: bool,
    language: AppLanguage,
}

fn wire_installer_ui(ui: &AppWindow, auto: Option<GuiOptions>) -> anyhow::Result<()> {
    let default_mode = auto.as_ref().map(|a| a.mode).unwrap_or(InstallMode::User);
    ui.set_current_screen(0);
    ui.set_install_mode(install_mode_to_int(default_mode));
    ui.set_install_path(
        auto.as_ref()
            .map(|a| a.root.clone())
            .unwrap_or_else(|| default_path(default_mode))
            .to_string_lossy()
            .into_owned()
            .into(),
    );
    ui.set_create_shortcut(auto.as_ref().map(|a| a.create_shortcut).unwrap_or(true));
    ui.set_register_uninstall(auto.as_ref().map(|a| a.register_uninstall).unwrap_or(true));
    ui.set_language(language_to_int(
        auto.as_ref().map(|a| a.language).unwrap_or_default(),
    ));

    {
        let ui_weak = ui.as_weak();
        ui.on_mode_selected(move |m| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mode = int_to_install_mode(m);
            let portable = matches!(mode, InstallMode::Portable);
            ui.set_install_path(default_path(mode).to_string_lossy().into_owned().into());
            ui.set_create_shortcut(!portable);
            ui.set_register_uninstall(!portable);
        });
    }

    {
        let ui_weak = ui.as_weak();
        ui.on_path_browse(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            match path_dialog::pick_folder() {
                Ok(Some(path)) => ui.set_install_path(
                    normalize_install_root(path)
                        .to_string_lossy()
                        .into_owned()
                        .into(),
                ),
                Ok(None) => {}
                Err(e) => dialogs::error(&format!("Folder picker failed: {e:#}")),
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        ui.on_request_quit(move || {
            finish_ui_session(&ui_weak);
        });
    }

    {
        let ui_weak = ui.as_weak();
        ui.on_request_launch(move || {
            if let Err(e) = claude::launch_registered_claude() {
                let language = ui_weak
                    .upgrade()
                    .map(|ui| int_to_language(ui.get_language()))
                    .unwrap_or_default();
                show_dialog_error(language, GuiErrorContext::Launch, &e);
            }
            finish_ui_session(&ui_weak);
        });
    }

    {
        let ui_weak = ui.as_weak();
        ui.on_request_install(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let opts = collect_gui_options(&ui);
            begin_install_action(&ui_weak, &ui, opts);
        });
    }

    {
        let ui_weak = ui.as_weak();
        ui.on_request_error_retry(move |action| {
            if action != ErrorRetryAction::Install.as_i32() {
                return;
            }
            let Some(ui) = ui_weak.upgrade() else { return };
            let opts = collect_gui_options(&ui);
            begin_install_action(&ui_weak, &ui, opts);
        });
    }

    {
        let ui_weak = ui.as_weak();
        ui.on_request_copy_error(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let detail = ui.get_error_detail().to_string();
            if let Err(e) = copy_text_to_clipboard(&detail) {
                dialogs::error(&format!("Could not copy error details.\n\n{e:#}"));
            }
        });
    }

    if let Some(auto) = auto {
        begin_install_action(&ui.as_weak(), ui, auto);
    }

    Ok(())
}

fn collect_gui_options(ui: &AppWindow) -> GuiOptions {
    GuiOptions {
        mode: int_to_install_mode(ui.get_install_mode()),
        root: normalize_install_root(PathBuf::from(ui.get_install_path().to_string())),
        create_shortcut: ui.get_create_shortcut(),
        register_uninstall: ui.get_register_uninstall(),
        keep_downloads: ui.get_keep_downloads(),
        language: int_to_language(ui.get_language()),
    }
}

fn normalize_install_root(path: PathBuf) -> PathBuf {
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(APP_DATA_DIR_NAME))
    {
        path
    } else {
        path.join(APP_DATA_DIR_NAME)
    }
}

fn install_requires_elevation(_opts: &GuiOptions) -> bool {
    true
}

fn begin_install_action(ui_weak: &slint::Weak<AppWindow>, ui: &AppWindow, opts: GuiOptions) {
    if install_requires_elevation(&opts) && !elevate::is_elevated() {
        match elevate::respawn_elevated(&auto_install_args(&opts)) {
            Ok(()) => {
                finish_ui_session(ui_weak);
            }
            Err(e) => {
                let err = anyhow::anyhow!("Couldn't obtain admin rights: {e:#}");
                set_gui_error(ui, opts.language, GuiErrorContext::Install, &err);
            }
        }
        return;
    }

    ui.set_current_screen(4);
    let (phase, detail) = progress_text(opts.language, "Resolving official MSIX", "");
    ui.set_progress_phase(phase.into());
    ui.set_progress_detail(detail.into());
    ui.set_progress_indeterminate(true);
    start_gui_install(ui.as_weak(), opts);
}

fn start_gui_install(ui_weak: slint::Weak<AppWindow>, opts: GuiOptions) {
    std::thread::spawn(move || {
        let language = opts.language;
        let result = run_gui_install(&opts, |phase, detail, fraction| {
            let weak = ui_weak.clone();
            let (phase, detail) = progress_text(language, phase, detail);
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak.upgrade() else { return };
                ui.set_progress_phase(phase.into());
                ui.set_progress_detail(detail.into());
                match fraction {
                    Some(fraction) => {
                        ui.set_progress_indeterminate(false);
                        ui.set_progress_fraction(fraction);
                    }
                    None => ui.set_progress_indeterminate(true),
                }
            });
        });

        let weak = ui_weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            match result {
                Ok(version) => {
                    ui.set_installed_version(claude::display_version(&version).into());
                    ui.set_current_screen(5);
                }
                Err(e) => {
                    set_gui_error(&ui, language, GuiErrorContext::Install, &e);
                }
            }
        });
    });
}

fn run_gui_install(
    opts: &GuiOptions,
    progress: impl Fn(&str, &str, Option<f32>),
) -> anyhow::Result<String> {
    std::fs::create_dir_all(&opts.root)?;

    progress(
        "Resolving official MSIX",
        "Downloading official installer",
        None,
    );
    let status = update_from_winget_with_progress(opts.keep_downloads, false, &progress)?;

    progress(
        "Installing updater",
        "Writing launcher and registration",
        None,
    );
    install_updater_files(opts, &status)?;
    Ok(status.version)
}

fn install_updater_files(
    opts: &GuiOptions,
    status: &claude::ClaudePackageStatus,
) -> anyhow::Result<()> {
    let updater_exe = opts.root.join(UPDATER_EXE_NAME);
    let current = std::env::current_exe()?;
    if !same_path(&current, &updater_exe) {
        std::fs::copy(&current, &updater_exe)?;
    }

    let legacy_exe = opts.root.join(LEGACY_UPDATER_EXE_NAME);
    if legacy_exe.is_file() && !same_path(&current, &legacy_exe) {
        let _ = std::fs::remove_file(&legacy_exe);
    }

    let cfg = Config {
        install_mode: opts.mode,
        current_package_version: status.version.clone(),
        current_app_version: read_app_version(status),
        known_latest: claude::query_official_msix_metadata()
            .ok()
            .map(|m| m.version),
        update_policy: UpdatePolicy::default(),
        last_check_unix: Some(now_unix()),
        skipped_version: None,
        arch: status.architecture.to_ascii_lowercase(),
        post_update_register: true,
        keep_downloads: opts.keep_downloads,
        register_uninstall: opts.register_uninstall,
        create_shortcut: opts.create_shortcut,
        language: opts.language,
    };
    cfg.save_install(&opts.root)?;
    junction::ensure_versions_layout(&opts.root, status)?;

    let icon = claude::claude_exe_path(status);
    if opts.create_shortcut {
        if let Some(link) = shortcut::link_path(opts.mode)? {
            shortcut::create_or_update(
                &link,
                &updater_exe,
                &icon,
                "Claude Desktop Updater",
                &opts.root,
            )?;
        }
    }
    if opts.register_uninstall {
        registry::write(
            opts.mode,
            &registry::UninstallEntry {
                display_name: "Claude Desktop Updater",
                display_version: &status.version,
                publisher: "vaportail",
                install_location: &opts.root,
                uninstall_string: format!("\"{}\" --uninstall", updater_exe.display()),
                display_icon: &icon,
            },
        )?;
    }
    Ok(())
}

fn run_uninstall_ui() -> anyhow::Result<()> {
    let ctx = match uninstall::load_context() {
        Ok(c) => c,
        Err(e) => {
            dialogs::error(&format!(
                "Couldn't read install state: {e:#}\n\n\
                 This launcher doesn't appear to be a valid Claude Desktop Updater install. \
                 No action taken."
            ));
            return Ok(());
        }
    };

    if uninstall::need_elevation(&ctx) {
        elevate::respawn_elevated("--uninstall")?;
        return Ok(());
    }

    dark_window::install();
    let ui = AppWindow::new()?;
    prepare_window(&ui);
    ui.set_language(language_to_int(ctx.cfg.language));
    wire_uninstall_ui(&ui, ctx)?;
    ui.set_current_screen(20);
    show_when_ready(&ui);
    slint::run_event_loop()?;
    Ok(())
}

fn wire_uninstall_ui(ui: &AppWindow, ctx: uninstall::UninstallContext) -> anyhow::Result<()> {
    let ctx_holder = std::sync::Arc::new(std::sync::Mutex::new(Some(ctx)));

    {
        let ui_weak = ui.as_weak();
        ui.on_request_quit(move || {
            finish_ui_session(&ui_weak);
        });
    }

    {
        let ui_weak = ui.as_weak();
        let ctx_holder = ctx_holder.clone();
        ui.on_request_uninstall_start(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let Some(ctx) = ctx_holder.lock().unwrap().take() else {
                return;
            };
            let language = ctx.cfg.language;
            ui.set_current_screen(21);
            let (phase, detail) = progress_text(language, "Starting", "");
            ui.set_progress_phase(phase.into());
            ui.set_progress_detail(detail.into());
            ui.set_progress_indeterminate(true);
            let ui_weak_inner = ui_weak.clone();
            std::thread::spawn(move || {
                uninstall::run_worker(ctx, move |msg| {
                    let weak = ui_weak_inner.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = weak.upgrade() else { return };
                        apply_uninstall_msg(&ui, msg, language);
                    });
                });
            });
        });
    }

    Ok(())
}

fn apply_uninstall_msg(ui: &AppWindow, msg: uninstall::UninstallMsg, language: AppLanguage) {
    match msg {
        uninstall::UninstallMsg::Phase { phase, detail } => {
            let (phase, detail) = progress_text(language, &phase, &detail);
            ui.set_progress_phase(phase.into());
            ui.set_progress_detail(detail.into());
            ui.set_progress_indeterminate(true);
        }
        uninstall::UninstallMsg::Progress(Some(f)) => {
            ui.set_progress_indeterminate(false);
            ui.set_progress_fraction(f);
        }
        uninstall::UninstallMsg::Progress(None) => {
            ui.set_progress_indeterminate(true);
        }
        uninstall::UninstallMsg::Done { log_path } => {
            ui.set_uninstall_log_path(log_path.into());
            ui.set_current_screen(22);
        }
        uninstall::UninstallMsg::Error(e) => {
            ui.set_error_text(e.into());
            ui.set_current_screen(23);
        }
    }
}

fn prepare_window(ui: &AppWindow) {
    ui.window()
        .set_position(slint::LogicalPosition::new(220.0, 160.0));
    start_motion_loop(ui.as_weak());
}

fn start_motion_loop(ui_weak: slint::Weak<AppWindow>) {
    std::thread::spawn(move || {
        let mut tick = 0.0f32;
        loop {
            std::thread::sleep(Duration::from_millis(33));
            tick += 0.018;
            if tick >= 1.0 {
                tick -= 1.0;
            }

            let weak = ui_weak.clone();
            if slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_motion_tick(tick);
                }
            })
            .is_err()
            {
                break;
            }
        }
    });
}

fn show_when_ready(ui: &AppWindow) {
    let ui_weak = ui.as_weak();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            let _ = ui.show();
            ui.window().request_redraw();
        }
    });
}

fn default_path(mode: InstallMode) -> PathBuf {
    match mode {
        InstallMode::Portable => std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("ClaudeDesktopUpdater"),
        InstallMode::User => directories::BaseDirs::new()
            .map(|d| d.data_local_dir().join("ClaudeDesktopUpdater"))
            .unwrap_or_else(|| PathBuf::from(r"C:\Users\Public\ClaudeDesktopUpdater")),
        InstallMode::System => PathBuf::from(r"C:\Program Files\ClaudeDesktopUpdater"),
    }
}

fn missing_appx_label(language: AppLanguage) -> &'static str {
    match language {
        AppLanguage::ZhCn => "未安装",
        AppLanguage::EnUs => "Not installed",
    }
}

fn install_mode_to_int(mode: InstallMode) -> i32 {
    match mode {
        InstallMode::Portable => 0,
        InstallMode::User => 1,
        InstallMode::System => 2,
    }
}

fn int_to_install_mode(i: i32) -> InstallMode {
    match i {
        0 => InstallMode::Portable,
        2 => InstallMode::System,
        _ => InstallMode::User,
    }
}

fn language_to_int(language: AppLanguage) -> i32 {
    match language {
        AppLanguage::ZhCn => 0,
        AppLanguage::EnUs => 1,
    }
}

fn int_to_language(i: i32) -> AppLanguage {
    match i {
        1 => AppLanguage::EnUs,
        _ => AppLanguage::ZhCn,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorRetryAction {
    None,
    Install,
    Update,
}

impl ErrorRetryAction {
    fn as_i32(self) -> i32 {
        match self {
            ErrorRetryAction::None => 0,
            ErrorRetryAction::Install => 1,
            ErrorRetryAction::Update => 2,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum GuiErrorContext {
    Install,
    Update,
    Launch,
    General,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ErrorViewModel {
    title: String,
    summary: String,
    detail: String,
    retry_action: ErrorRetryAction,
}

fn error_view_model(
    language: AppLanguage,
    context: GuiErrorContext,
    err: &anyhow::Error,
) -> ErrorViewModel {
    let detail = repair_error_detail_mojibake(&format!("{err:#}"));
    let detail_lower = detail.to_ascii_lowercase();
    let retry_action = match context {
        GuiErrorContext::Install => ErrorRetryAction::Install,
        GuiErrorContext::Update => ErrorRetryAction::Update,
        GuiErrorContext::Launch | GuiErrorContext::General => ErrorRetryAction::None,
    };

    let kind = if detail_lower.contains("admin rights")
        || detail_lower.contains("administrator")
        || detail_lower.contains("elevation")
        || detail_lower.contains("uac")
        || detail_lower.contains("access is denied")
        || detail_lower.contains("os error 5")
        || detail.contains("拒绝访问")
    {
        "elevation"
    } else if detail_lower.contains("official claude msix redirect")
        || detail_lower.contains("http ")
        || detail_lower.contains("network")
        || detail_lower.contains("connection")
        || detail_lower.contains("dns")
        || detail_lower.contains("timeout")
        || detail_lower.contains("download")
    {
        "network"
    } else if detail_lower.contains("add-appxpackage")
        || detail_lower.contains("installing official msix")
        || detail_lower.contains("deployment")
    {
        "appx-install"
    } else if detail_lower.contains("registered")
        || detail_lower.contains("registration")
        || detail_lower.contains("appxmanifest")
        || detail_lower.contains("startapps")
        || detail_lower.contains("url protocol")
    {
        "registration"
    } else if matches!(context, GuiErrorContext::Launch) || detail_lower.contains("launch") {
        "launch"
    } else {
        "general"
    };

    let (title, summary) = match (language, kind) {
        (AppLanguage::ZhCn, "elevation") => (
            "需要管理员权限",
            "安装 Claude 官方 MSIX 需要管理员权限。请在 Windows 权限确认窗口中允许后重试。",
        ),
        (AppLanguage::EnUs, "elevation") => (
            "Administrator rights are required",
            "Installing the official Claude MSIX requires administrator rights. Allow the Windows prompt, then try again.",
        ),
        (AppLanguage::ZhCn, "network") => (
            "无法连接官方 MSIX 更新源",
            "可能是网络、代理、公司防火墙或 Anthropic 下载源暂时不可用。请检查网络后重试。",
        ),
        (AppLanguage::EnUs, "network") => (
            "Could not reach the official MSIX source",
            "Your network, proxy, company firewall, or Anthropic's download source may be unavailable. Check the connection and try again.",
        ),
        (AppLanguage::ZhCn, "appx-install") => (
            "Claude 官方 MSIX 安装失败",
            "Windows Appx 安装没有完成。请重试；如果仍失败，可复制错误详情排查 Add-AppxPackage 输出。",
        ),
        (AppLanguage::EnUs, "appx-install") => (
            "Claude official MSIX install failed",
            "Windows Appx installation did not finish. Try again, or copy the details to inspect the Add-AppxPackage output.",
        ),
        (AppLanguage::ZhCn, "registration") => (
            "Claude Appx 注册验证失败",
            "Claude 已尝试安装或修复注册，但 Windows 集成仍不完整。请重试修复注册，或复制错误详情排查。",
        ),
        (AppLanguage::EnUs, "registration") => (
            "Claude Appx registration check failed",
            "Claude was installed or registration was repaired, but Windows integrations are still incomplete. Try again or copy the details.",
        ),
        (AppLanguage::ZhCn, "launch") => (
            "无法启动 Claude",
            "Windows 没有成功通过已注册的 Appx 入口启动 Claude。请确认 Claude 已安装并完成注册。",
        ),
        (AppLanguage::EnUs, "launch") => (
            "Could not launch Claude",
            "Windows could not start Claude through the registered Appx entry. Confirm Claude is installed and registered.",
        ),
        (AppLanguage::ZhCn, _) => (
            "操作失败",
            "操作没有完成。你可以重试，或复制错误详情继续排查。",
        ),
        (AppLanguage::EnUs, _) => (
            "Operation failed",
            "The operation did not finish. Try again, or copy the error details for troubleshooting.",
        ),
    };

    ErrorViewModel {
        title: title.into(),
        summary: summary.into(),
        detail,
        retry_action,
    }
}

fn repair_error_detail_mojibake(detail: &str) -> String {
    let repaired = repair_known_windows_mojibake(detail);
    if repaired != detail {
        return repaired;
    }

    if !looks_like_utf8_decoded_as_gbk(detail) {
        return detail.to_string();
    }

    let (bytes, _, had_errors) = encoding_rs::GBK.encode(detail);
    if had_errors {
        return detail.to_string();
    }
    String::from_utf8(bytes.into_owned()).unwrap_or_else(|_| detail.to_string())
}

fn repair_known_windows_mojibake(detail: &str) -> String {
    detail
        .replace("鎸囧畾鐨勬湇鍔℃湭瀹夎銆?", "指定的服务未安装。")
        .replace("鎸囧畾鐨勬湇鍔℃湭瀹夎銆�", "指定的服务未安装。")
        .replace("鎷掔粷璁块棶銆?", "拒绝访问。")
        .replace("鎷掔粷璁块棶銆�", "拒绝访问。")
}

fn looks_like_utf8_decoded_as_gbk(text: &str) -> bool {
    const MARKERS: &[char] = &[
        '鎷', '鎸', '銆', '鐨', '湇', '鍔', '湭', '瀹', '闂', '棶', '粷', '璁',
    ];
    text.chars().filter(|ch| MARKERS.contains(ch)).count() >= 2
}

fn set_gui_error(
    ui: &AppWindow,
    language: AppLanguage,
    context: GuiErrorContext,
    err: &anyhow::Error,
) {
    let view = error_view_model(language, context, err);
    ui.set_error_title(view.title.into());
    ui.set_error_summary(view.summary.into());
    ui.set_error_detail(view.detail.clone().into());
    ui.set_error_text(view.detail.into());
    ui.set_error_retry_action(view.retry_action.as_i32());
    ui.set_current_screen(6);
}

fn show_dialog_error(language: AppLanguage, context: GuiErrorContext, err: &anyhow::Error) {
    let view = error_view_model(language, context, err);
    dialogs::error(&format!(
        "{}\n\n{}\n\n{}",
        view.title, view.summary, view.detail
    ));
}

fn copy_text_to_clipboard(text: &str) -> anyhow::Result<()> {
    let mut child = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Set-Clipboard -Value ([Console]::In.ReadToEnd())",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting PowerShell clipboard helper")?;
    {
        let mut stdin = child
            .stdin
            .take()
            .context("opening clipboard helper stdin")?;
        stdin
            .write_all(text.as_bytes())
            .context("writing clipboard text")?;
    }
    let output = child
        .wait_with_output()
        .context("waiting for clipboard helper")?;
    if !output.status.success() {
        anyhow::bail!(
            "Set-Clipboard failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn progress_text(language: AppLanguage, phase: &str, detail: &str) -> (String, String) {
    (
        localize_progress_text(language, phase),
        localize_progress_text(language, detail),
    )
}

fn localize_progress_text(language: AppLanguage, text: &str) -> String {
    if !matches!(language, AppLanguage::ZhCn) || text.is_empty() {
        return text.to_string();
    }

    match text {
        "Starting" => "正在开始".into(),
        "Resolving official MSIX" => "正在连接官方 MSIX 源".into(),
        "Updating Claude" => "正在更新 Claude".into(),
        "Downloading official installer" => "正在下载官方安装器".into(),
        "Downloading Claude" => "正在下载 Claude".into(),
        "Closing Claude" => "正在关闭 Claude".into(),
        "Stopping running Claude processes before update" => "正在关闭运行中的 Claude".into(),
        "Running Claude installer" => "正在安装 Claude Appx".into(),
        "Waiting for Anthropic's installer to finish" => "等待 Anthropic 安装器完成".into(),
        "Installing official MSIX package" => "正在安装官方 MSIX 包".into(),
        "Preparing Appx registration" => "正在准备 Appx 注册".into(),
        "Removing developer-registered Claude package" => "正在移除开发者注册的 Claude 包".into(),
        "Verifying registration" => "正在验证注册".into(),
        "Claude is already current" => "Claude 已是最新版本".into(),
        "Checking Windows Appx registration" => "正在检查 Windows Appx 注册".into(),
        "Updating launcher" => "正在更新更新器".into(),
        "Writing updater state" => "正在写入更新器状态".into(),
        "Installing Claude MSIX" => "正在安装 Claude MSIX".into(),
        "Installing updater" => "正在安装更新器".into(),
        "Writing launcher and registration" => "正在写入更新器和注册信息".into(),
        "Validating install" => "正在验证安装".into(),
        "Removing Start Menu shortcut" => "正在移除开始菜单快捷方式".into(),
        "Removing registry entries" => "正在移除注册表项".into(),
        "Deleting updater files" => "正在删除更新器文件".into(),
        "Finalizing" => "正在完成".into(),
        _ => {
            if let Some(version) = text.strip_prefix("Preparing official installer ") {
                return format!("准备官方 MSIX 版本 {version}");
            }
            if let Some(bytes) = text.strip_suffix(" downloaded") {
                return format!("已下载 {bytes}");
            }
            text.to_string()
        }
    }
}

fn parse_auto_install(args: &[String]) -> Option<GuiOptions> {
    if !args.iter().any(|a| a == "--auto-install") {
        return None;
    }
    let mode = parse_string_flag(args, "--mode")
        .as_deref()
        .map(parse_install_mode)
        .unwrap_or(InstallMode::User);
    Some(GuiOptions {
        mode,
        root: parse_string_flag(args, "--path")
            .map(PathBuf::from)
            .unwrap_or_else(|| default_path(mode)),
        create_shortcut: !args.iter().any(|a| a == "--no-shortcut"),
        register_uninstall: !args.iter().any(|a| a == "--no-register-uninstall"),
        keep_downloads: args.iter().any(|a| a == "--keep-downloads"),
        language: parse_string_flag(args, "--language")
            .as_deref()
            .and_then(AppLanguage::parse)
            .unwrap_or_default(),
    })
}

fn auto_install_args(opts: &GuiOptions) -> String {
    let mode = match opts.mode {
        InstallMode::Portable => "portable",
        InstallMode::User => "user",
        InstallMode::System => "system",
    };
    let mut args = format!(
        "--auto-install --mode {} --path \"{}\"",
        mode,
        opts.root.display()
    );
    if !opts.create_shortcut {
        args.push_str(" --no-shortcut");
    }
    if !opts.register_uninstall {
        args.push_str(" --no-register-uninstall");
    }
    if opts.keep_downloads {
        args.push_str(" --keep-downloads");
    }
    let language = match opts.language {
        AppLanguage::ZhCn => "zh-CN",
        AppLanguage::EnUs => "en-US",
    };
    args.push_str(&format!(" --language {}", language));
    args
}

fn parse_install_mode(s: &str) -> InstallMode {
    match s.to_ascii_lowercase().as_str() {
        "portable" => InstallMode::Portable,
        "system" => InstallMode::System,
        _ => InstallMode::User,
    }
}

fn parse_string_flag(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn installed_config_path() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let root = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("exe has no parent directory"))?;
    Ok(root.join(CONFIG_FILENAME))
}

fn claude_download_dir() -> PathBuf {
    std::env::var("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(APP_DATA_DIR_NAME)
        .join("downloads")
}

fn read_app_version(status: &claude::ClaudePackageStatus) -> Option<String> {
    let version_path = PathBuf::from(&status.install_location)
        .join("app")
        .join("version");
    std::fs::read_to_string(version_path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_download_progress_with_total() {
        assert_eq!(
            format_download_detail(1024 * 1024, Some(2 * 1024 * 1024)),
            "1.0 MB / 2.0 MB (50%)"
        );
    }

    #[test]
    fn localizes_progress_text_for_chinese_ui() {
        assert_eq!(
            progress_text(
                AppLanguage::ZhCn,
                "Resolving official MSIX",
                "Preparing official installer 1.2.3"
            ),
            (
                "正在连接官方 MSIX 源".into(),
                "准备官方 MSIX 版本 1.2.3".into()
            )
        );
        assert_eq!(
            progress_text(AppLanguage::EnUs, "Updating Claude", "1.0 MB downloaded"),
            ("Updating Claude".into(), "1.0 MB downloaded".into())
        );
        assert_eq!(
            progress_text(
                AppLanguage::ZhCn,
                "Updating launcher",
                "Writing updater state"
            ),
            ("正在更新更新器".into(), "正在写入更新器状态".into())
        );
    }

    #[test]
    fn friendly_error_keeps_http_redirect_detail() {
        let view = error_view_model(
            AppLanguage::ZhCn,
            GuiErrorContext::Update,
            &anyhow::anyhow!("official Claude MSIX redirect returned HTTP 403 Forbidden"),
        );

        assert_eq!(view.title, "无法连接官方 MSIX 更新源");
        assert!(view.summary.contains("网络、代理"));
        assert!(view.detail.contains("HTTP 403 Forbidden"));
        assert_eq!(view.retry_action, ErrorRetryAction::Update);
    }

    #[test]
    fn friendly_error_classifies_appx_install_failure() {
        let view = error_view_model(
            AppLanguage::ZhCn,
            GuiErrorContext::Install,
            &anyhow::anyhow!("Add-AppxPackage failed: deployment failed"),
        );

        assert_eq!(view.title, "Claude 官方 MSIX 安装失败");
        assert!(view.summary.contains("Windows Appx"));
        assert!(view.detail.contains("Add-AppxPackage failed"));
        assert_eq!(view.retry_action, ErrorRetryAction::Install);
    }

    #[test]
    fn friendly_error_classifies_elevation_failure() {
        let view = error_view_model(
            AppLanguage::ZhCn,
            GuiErrorContext::Install,
            &anyhow::anyhow!("Couldn't obtain admin rights: canceled"),
        );

        assert_eq!(view.title, "需要管理员权限");
        assert!(view.summary.contains("管理员权限"));
        assert_eq!(view.retry_action, ErrorRetryAction::Install);
    }

    #[test]
    fn friendly_error_classifies_access_denied_as_elevation_failure() {
        let view = error_view_model(
            AppLanguage::ZhCn,
            GuiErrorContext::Update,
            &anyhow::anyhow!("running PowerShell command: Stop-Service: 拒绝访问。 (os error 5)"),
        );

        assert_eq!(view.title, "需要管理员权限");
        assert_eq!(view.retry_action, ErrorRetryAction::Update);
    }

    #[test]
    fn friendly_error_repairs_mojibake_detail_before_display_and_copy() {
        let view = error_view_model(
            AppLanguage::ZhCn,
            GuiErrorContext::Update,
            &anyhow::anyhow!("opening CoworkVMService: 鎸囧畾鐨勬湇鍔℃湭瀹夎銆?(0x80070424)"),
        );

        assert!(
            view.detail.contains("指定的服务未安装。"),
            "{}",
            view.detail
        );
        assert!(!view.detail.contains("鎸囧畾"), "{}", view.detail);
    }

    #[test]
    fn friendly_error_classifies_mojibake_access_denied_as_elevation_failure() {
        let view = error_view_model(
            AppLanguage::ZhCn,
            GuiErrorContext::Update,
            &anyhow::anyhow!(
                "running PowerShell command: Stop-Service: 鎷掔粷璁块棶銆?(os error 5)"
            ),
        );

        assert_eq!(view.title, "需要管理员权限");
        assert!(view.detail.contains("拒绝访问。"), "{}", view.detail);
        assert_eq!(view.retry_action, ErrorRetryAction::Update);
    }

    #[test]
    fn friendly_error_classifies_registration_failure() {
        let view = error_view_model(
            AppLanguage::ZhCn,
            GuiErrorContext::Update,
            &anyhow::anyhow!("Claude URL protocol was not registered after repair"),
        );

        assert_eq!(view.title, "Claude Appx 注册验证失败");
        assert!(view.summary.contains("修复注册"));
        assert_eq!(view.retry_action, ErrorRetryAction::Update);
    }

    #[test]
    fn friendly_error_launch_has_no_retry_action() {
        let view = error_view_model(
            AppLanguage::ZhCn,
            GuiErrorContext::Launch,
            &anyhow::anyhow!("Could not launch Claude"),
        );

        assert_eq!(view.title, "无法启动 Claude");
        assert_eq!(view.retry_action, ErrorRetryAction::None);
    }

    #[test]
    fn cached_latest_newer_than_current_shows_update_available() {
        let appx = claude::ClaudePackageStatus {
            package_full_name: "Claude_1.9659.2.0_x64__pzs8sxrjxfjjc".into(),
            package_family_name: claude::CLAUDE_PACKAGE_FAMILY.into(),
            version: "1.9659.2.0".into(),
            architecture: "X64".into(),
            install_location: r"C:\Program Files\WindowsApps\Claude_1.9659.2.0_x64__pzs8sxrjxfjjc"
                .into(),
            signature_kind: Some("Developer".into()),
        };

        assert_eq!(
            cached_update_screen(&appx, Some("1.15962.1"), None),
            Some(12)
        );
    }

    #[test]
    fn missing_appx_install_requires_official_msix_install() {
        let cfg = Config {
            install_mode: InstallMode::User,
            current_package_version: "1.15962.1.0".into(),
            current_app_version: Some("42.4.0".into()),
            known_latest: Some("1.15962.1".into()),
            update_policy: UpdatePolicy::Daily,
            last_check_unix: Some(now_unix()),
            skipped_version: None,
            arch: "x64".into(),
            post_update_register: true,
            keep_downloads: false,
            register_uninstall: true,
            create_shortcut: true,
            language: AppLanguage::ZhCn,
        };

        assert_eq!(
            update_screen_for_status(None, cfg.known_latest.as_deref().unwrap(), None),
            Some(14)
        );
        assert_eq!(
            update_screen_for_status(
                None,
                cfg.known_latest.as_deref().unwrap(),
                cfg.known_latest.as_deref()
            ),
            Some(14)
        );
    }

    #[test]
    fn missing_appx_label_is_localized() {
        assert_eq!(missing_appx_label(AppLanguage::ZhCn), "未安装");
        assert_eq!(missing_appx_label(AppLanguage::EnUs), "Not installed");
    }

    #[test]
    fn rejects_success_when_installed_version_is_still_older_than_latest() {
        let err = ensure_installed_version("1.9659.2.0", "1.15962.1").unwrap_err();
        assert!(
            err.to_string().contains("Claude installer finished"),
            "{err:#}"
        );
    }

    #[test]
    fn appx_install_satisfies_official_msix_when_version_is_current() {
        let appx = claude::ClaudePackageStatus {
            package_full_name: "Claude_1.15962.1.0_x64__pzs8sxrjxfjjc".into(),
            package_family_name: claude::CLAUDE_PACKAGE_FAMILY.into(),
            version: "1.15962.1.0".into(),
            architecture: "X64".into(),
            install_location: r"C:\Program Files\WindowsApps\Claude_1.15962.1.0_x64__pzs8sxrjxfjjc"
                .into(),
            signature_kind: Some("Store".into()),
        };

        assert!(current_install_satisfies_official_msix(&appx, "1.15962.1"));
    }

    #[test]
    fn official_msix_install_requires_elevation() {
        let mut opts = GuiOptions {
            mode: InstallMode::User,
            root: PathBuf::from(r"C:\Users\me\AppData\Local\ClaudeDesktopUpdater"),
            create_shortcut: true,
            register_uninstall: true,
            keep_downloads: false,
            language: AppLanguage::ZhCn,
        };

        assert!(install_requires_elevation(&opts));
        opts.mode = InstallMode::System;
        assert!(install_requires_elevation(&opts));
    }

    #[test]
    fn selected_install_directory_contains_claude_desktop_updater_folder() {
        assert_eq!(
            normalize_install_root(PathBuf::from(r"D:\Tools")),
            PathBuf::from(r"D:\Tools\ClaudeDesktopUpdater")
        );
        assert_eq!(
            normalize_install_root(PathBuf::from(r"D:\Tools\ClaudeDesktopUpdater")),
            PathBuf::from(r"D:\Tools\ClaudeDesktopUpdater")
        );
    }

    #[test]
    fn legacy_msix_cli_args_are_unsupported() {
        let args = vec![
            "--msix".to_string(),
            r"D:\Downloads\Claude.msix".to_string(),
        ];
        let err = reject_unsupported_legacy_cli(&args).unwrap_err();
        assert!(err.to_string().contains("no longer supported"));
        assert!(err.to_string().contains("--update"));

        let args = vec![
            "--extract-msix".to_string(),
            r"D:\Downloads\Claude.msix".to_string(),
        ];
        assert!(reject_unsupported_legacy_cli(&args).is_err());
    }
}
