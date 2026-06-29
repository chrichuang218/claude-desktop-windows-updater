#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod claude;
mod cleanup;
mod config;
mod dark_window;
mod dialogs;
mod elevate;
mod extract;
mod junction;
mod path_dialog;
mod registry;
mod safety;
mod shortcut;
mod uninstall;

use config::{
    AppLanguage, Config, Fetcher, InstallMode, UpdatePolicy, APP_DATA_DIR_NAME, CONFIG_FILENAME,
    LEGACY_UPDATER_EXE_NAME, UPDATER_EXE_NAME,
};
use sha2::{Digest, Sha256};
use slint::ComponentHandle;
use std::cmp::Ordering;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

slint::include_modules!();

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--self-test") {
        return Ok(());
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

#[derive(Debug, Clone)]
struct InstalledUpdateContext {
    root: PathBuf,
    cfg: Config,
    status: claude::ClaudePackageStatus,
    latest: claude::WingetInstallerMetadata,
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

    if !update_check_due(&cfg) {
        if !matches!(cfg.update_policy, UpdatePolicy::Never) {
            if let (Some(status), Some(known_latest)) =
                (current_status.as_ref(), cfg.known_latest.clone())
            {
                let skipped = cfg.skipped_version.as_deref() == Some(known_latest.as_str());
                let already_current =
                    claude::compare_versions(&known_latest, &status.version) != Ordering::Greater;
                if !skipped && already_current {
                    return show_update_prompt(
                        InstalledUpdateContext {
                            root,
                            cfg,
                            status: status.clone(),
                            latest: claude::WingetInstallerMetadata {
                                version: known_latest,
                                installer_url: String::new(),
                                sha256: String::new(),
                            },
                        },
                        13,
                    );
                }
            }
        }
        return claude::launch_registered_claude();
    }

    let status = match current_status {
        Some(status) => status,
        None => claude::query_package_status()?,
    };
    let latest = claude::query_winget_metadata()?;
    sync_installed_config(&root, &mut cfg, &status, Some(latest.version.clone()))?;

    if cfg.skipped_version.as_deref() == Some(latest.version.as_str()) {
        return claude::launch_registered_claude();
    }

    let current_screen =
        if claude::compare_versions(&latest.version, &status.version) == Ordering::Greater {
            12
        } else {
            13
        };

    show_update_prompt(
        InstalledUpdateContext {
            root,
            cfg,
            status,
            latest,
        },
        current_screen,
    )
}

fn show_update_prompt(ctx: InstalledUpdateContext, current_screen: i32) -> anyhow::Result<()> {
    dark_window::install();
    let ui = AppWindow::new()?;
    prepare_window(&ui);
    ui.set_language(language_to_int(ctx.cfg.language));
    ui.set_current_screen(current_screen);
    ui.set_update_current_version(claude::display_version(&ctx.status.version).into());
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
    if let Some(msix) = parse_string_flag(args, "--msix") {
        return run_msix(Path::new(&msix), true).map(|_| true);
    }
    if args.iter().any(|a| a == "--update") {
        return run_update_cli().map(|_| true);
    }
    if let Some(msix) = parse_string_flag(args, "--extract-msix") {
        return run_extract_diagnostic(Path::new(&msix), &std::env::current_dir()?).map(|_| true);
    }
    if args.iter().any(|a| a == "--launch") {
        return claude::launch_registered_claude().map(|_| true);
    }
    if args.iter().any(|a| a == "--uninstall") {
        return run_uninstall_ui().map(|_| true);
    }
    Ok(false)
}

fn run_status() -> anyhow::Result<()> {
    let status = claude::query_package_status()?;
    let app_id_registered = claude::query_start_apps_registered()?;
    let protocol_registered = claude::query_protocol_registered()?;
    let manifest = claude::query_manifest_integrations(&status)?;

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
    let metadata = claude::query_winget_metadata()?;
    println!("Latest Claude from winget:");
    println!("  Version       : {}", metadata.version);
    println!("  Installer URL : {}", metadata.installer_url);
    println!("  SHA256        : {}", metadata.sha256);
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

fn run_msix(msix: &Path, print: bool) -> anyhow::Result<claude::ClaudePackageStatus> {
    if !msix.exists() {
        anyhow::bail!("MSIX file does not exist: {}", msix.display());
    }
    if print {
        println!("Installing Claude MSIX: {}", msix.display());
    }
    claude::run_powershell(&claude::msix_install_command(msix))?;
    verify_claude_registration()?;
    let status = claude::query_package_status()?;
    if print {
        println!("Claude MSIX installed and registered.");
    }
    Ok(status)
}

fn run_update_cli() -> anyhow::Result<()> {
    let before = claude::query_package_status().ok();
    let latest = claude::query_winget_metadata().ok();
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
    let metadata = claude::query_winget_metadata()?;

    if let Ok(current) = claude::query_package_status() {
        if claude::compare_versions(&metadata.version, &current.version) != Ordering::Greater {
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
    let installer = download_dir.join(format!("Claude-{}.exe", metadata.version));

    if print {
        println!("Downloading Claude {}...", metadata.version);
    }
    progress(
        "Downloading Claude",
        &format!("Preparing official installer {}", metadata.version),
        Some(0.0),
    );
    download_with_sha256(
        &metadata.installer_url,
        &installer,
        &metadata.sha256,
        |done, total| {
            progress(
                "Downloading Claude",
                &format_download_detail(done, total),
                total.map(|t| (done as f32 / t as f32).clamp(0.0, 1.0)),
            );
        },
    )?;
    if print {
        println!("Running installer: {}", installer.display());
    }
    progress(
        "Running Claude installer",
        "Waiting for Anthropic's installer to finish",
        None,
    );
    let status = Command::new(&installer).status()?;
    if !status.success() {
        anyhow::bail!("Claude installer exited with status {}", status);
    }
    if !keep_downloads {
        let _ = std::fs::remove_file(&installer);
    }
    progress(
        "Verifying registration",
        "Checking Windows Appx registration",
        None,
    );
    verify_claude_registration()?;
    claude::query_package_status()
}

fn run_extract_diagnostic(msix: &Path, root: &Path) -> anyhow::Result<PathBuf> {
    if !msix.exists() {
        anyhow::bail!("MSIX file does not exist: {}", msix.display());
    }
    let version = msix
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.split('_').nth(1))
        .unwrap_or("diagnostic");
    let out_root = root.join("diagnostic_extract");
    std::fs::create_dir_all(&out_root)?;
    let mut progress = |_done: u64, _total: Option<u64>| {};
    let out = extract::extract_app(msix, &out_root, version, &mut progress)?;
    println!("Extracted diagnostic copy to {}", out.display());
    println!("This is not a registered Claude install.");
    Ok(out)
}

fn wire_update_ui(ui: &AppWindow, ctx: InstalledUpdateContext) -> anyhow::Result<()> {
    let ctx = std::sync::Arc::new(std::sync::Mutex::new(ctx));

    {
        let ui_weak = ui.as_weak();
        ui.on_request_quit(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let _ = ui.window().hide();
            }
            let _ = slint::quit_event_loop();
        });
    }

    {
        let ui_weak = ui.as_weak();
        ui.on_request_launch(move || {
            if let Err(e) = claude::launch_registered_claude() {
                dialogs::error(&format!("Could not launch Claude.\n\n{e:#}"));
            }
            if let Some(ui) = ui_weak.upgrade() {
                let _ = ui.window().hide();
            }
            let _ = slint::quit_event_loop();
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
                let (phase, detail) = progress_text(
                    ctx_snapshot.cfg.language,
                    "Updating Claude",
                    "Downloading official installer",
                );
                ui.set_current_screen(4);
                ui.set_progress_phase(phase.into());
                ui.set_progress_detail(detail.into());
                ui.set_progress_indeterminate(true);
                start_gui_update(ui.as_weak(), ctx_snapshot);
                return;
            }

            if let Err(e) = defer_update(action, &ctx_snapshot) {
                ui.set_error_text(format!("{e:#}").into());
                ui.set_current_screen(6);
                return;
            }
            let _ = claude::launch_registered_claude();
            let _ = ui.window().hide();
            let _ = slint::quit_event_loop();
        });
    }

    Ok(())
}

fn start_gui_update(ui_weak: slint::Weak<AppWindow>, ctx: InstalledUpdateContext) {
    std::thread::spawn(move || {
        let result = run_gui_update(&ctx, |phase, detail, fraction| {
            let weak = ui_weak.clone();
            let (phase, detail) = progress_text(ctx.cfg.language, phase, detail);
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
                Ok(status) => {
                    ui.set_update_current_version(claude::display_version(&status.version).into());
                    ui.set_current_screen(13);
                }
                Err(e) => {
                    ui.set_error_text(format!("{e:#}").into());
                    ui.set_current_screen(6);
                }
            }
        });
    });
}

fn run_gui_update(
    ctx: &InstalledUpdateContext,
    progress: impl Fn(&str, &str, Option<f32>),
) -> anyhow::Result<claude::ClaudePackageStatus> {
    progress("Updating Claude", "Downloading official installer", None);
    let status = update_from_winget_with_progress(ctx.cfg.keep_downloads, false, &progress)?;
    progress("Updating launcher", "Writing updater state", None);
    install_updater_files(
        &GuiOptions {
            mode: ctx.cfg.install_mode,
            root: ctx.root.clone(),
            source: Fetcher::Winget,
            msix_path: None,
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
    cfg.save_install(root)
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

fn download_with_sha256(
    url: &str,
    dest: &Path,
    expected_sha256: &str,
    progress: impl Fn(u64, Option<u64>),
) -> anyhow::Result<()> {
    if dest.exists() {
        let actual = file_sha256(dest)?;
        if actual.eq_ignore_ascii_case(expected_sha256.trim()) {
            let len = dest.metadata().map(|m| m.len()).ok();
            progress(len.unwrap_or(0), len);
            return Ok(());
        }
        let _ = std::fs::remove_file(dest);
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
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 128 * 1024];
    let mut downloaded = 0u64;
    progress(downloaded, total);
    loop {
        let read = response.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        file.write_all(&buf[..read])?;
        downloaded += read as u64;
        progress(downloaded, total);
        if total.is_some_and(|t| downloaded >= t) {
            break;
        }
    }
    file.flush()?;
    drop(file);

    let actual = format!("{:x}", hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected_sha256.trim()) {
        let _ = std::fs::remove_file(&partial);
        anyhow::bail!(
            "SHA256 mismatch for {}\nexpected: {}\nactual:   {}",
            dest.display(),
            expected_sha256,
            actual
        );
    }
    std::fs::rename(&partial, dest)?;
    Ok(())
}

fn file_sha256(path: &Path) -> anyhow::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 128 * 1024];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
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
    source: Fetcher,
    msix_path: Option<PathBuf>,
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
    ui.set_fetcher(fetcher_to_int(
        auto.as_ref().map(|a| a.source).unwrap_or_default(),
    ));
    ui.set_msix_path(
        auto.as_ref()
            .and_then(|a| a.msix_path.as_ref())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
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
                Ok(Some(path)) => ui.set_install_path(path.to_string_lossy().into_owned().into()),
                Ok(None) => {}
                Err(e) => dialogs::error(&format!("Folder picker failed: {e:#}")),
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        ui.on_msix_browse(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            match path_dialog::pick_file() {
                Ok(Some(path)) => ui.set_msix_path(path.to_string_lossy().into_owned().into()),
                Ok(None) => {}
                Err(e) => dialogs::error(&format!("File picker failed: {e:#}")),
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        ui.on_request_quit(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let _ = ui.window().hide();
            }
            let _ = slint::quit_event_loop();
        });
    }

    {
        let ui_weak = ui.as_weak();
        ui.on_request_launch(move || {
            if let Err(e) = claude::launch_registered_claude() {
                dialogs::error(&format!("Could not launch Claude.\n\n{e:#}"));
            }
            if let Some(ui) = ui_weak.upgrade() {
                let _ = ui.window().hide();
            }
            let _ = slint::quit_event_loop();
        });
    }

    {
        let ui_weak = ui.as_weak();
        ui.on_request_install(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let opts = collect_gui_options(&ui);

            if matches!(opts.mode, InstallMode::System) && !elevate::is_elevated() {
                match elevate::respawn_elevated(&auto_install_args(&opts)) {
                    Ok(()) => {
                        let _ = ui.window().hide();
                        let _ = slint::quit_event_loop();
                    }
                    Err(e) => {
                        ui.set_error_text(format!("Couldn't obtain admin rights: {e:#}").into());
                        ui.set_current_screen(6);
                    }
                }
                return;
            }

            ui.set_current_screen(4);
            let (phase, detail) = progress_text(opts.language, "Starting", "");
            ui.set_progress_phase(phase.into());
            ui.set_progress_detail(detail.into());
            ui.set_progress_indeterminate(true);
            start_gui_install(ui.as_weak(), opts);
        });
    }

    if let Some(auto) = auto {
        ui.set_current_screen(4);
        let (phase, detail) = progress_text(auto.language, "Starting", "");
        ui.set_progress_phase(phase.into());
        ui.set_progress_detail(detail.into());
        ui.set_progress_indeterminate(true);
        start_gui_install(ui.as_weak(), auto);
    }

    Ok(())
}

fn collect_gui_options(ui: &AppWindow) -> GuiOptions {
    let msix_path = ui.get_msix_path().to_string();
    GuiOptions {
        mode: int_to_install_mode(ui.get_install_mode()),
        root: PathBuf::from(ui.get_install_path().to_string()),
        source: int_to_fetcher(ui.get_fetcher()),
        msix_path: (!msix_path.trim().is_empty()).then(|| PathBuf::from(msix_path)),
        create_shortcut: ui.get_create_shortcut(),
        register_uninstall: ui.get_register_uninstall(),
        keep_downloads: ui.get_keep_downloads(),
        language: int_to_language(ui.get_language()),
    }
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
                    ui.set_error_text(format!("{e:#}").into());
                    ui.set_current_screen(6);
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

    let status = match opts.source {
        Fetcher::Winget => {
            progress("Updating Claude", "Downloading official installer", None);
            update_from_winget_with_progress(opts.keep_downloads, false, &progress)?
        }
        Fetcher::LocalMsix => {
            let msix = opts
                .msix_path
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("Local MSIX source requires a file path"))?;
            progress("Installing Claude MSIX", &msix.display().to_string(), None);
            run_msix(msix, false)?
        }
        Fetcher::ExtractDiagnostic => {
            let msix = opts
                .msix_path
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("Diagnostic extract requires an MSIX file path"))?;
            progress(
                "Extracting diagnostic copy",
                &msix.display().to_string(),
                None,
            );
            let out = run_extract_diagnostic(msix, &opts.root)?;
            return Ok(format!("diagnostic: {}", out.display()));
        }
    };

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
        known_latest: claude::query_winget_metadata().ok().map(|m| m.version),
        update_policy: UpdatePolicy::default(),
        last_check_unix: Some(now_unix()),
        skipped_version: None,
        fetcher: opts.source,
        arch: status.architecture.to_ascii_lowercase(),
        post_update_register: true,
        keep_downloads: opts.keep_downloads,
        register_uninstall: opts.register_uninstall,
        create_shortcut: opts.create_shortcut,
        language: opts.language,
    };
    cfg.save_install(&opts.root)?;
    junction::ensure_versions_layout(&opts.root, status)?;

    let icon = PathBuf::from(&status.install_location)
        .join("app")
        .join("Claude.exe");
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
            if let Some(ui) = ui_weak.upgrade() {
                let _ = ui.window().hide();
            }
            let _ = slint::quit_event_loop();
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

fn fetcher_to_int(f: Fetcher) -> i32 {
    match f {
        Fetcher::Winget => 0,
        Fetcher::LocalMsix => 1,
        Fetcher::ExtractDiagnostic => 2,
    }
}

fn int_to_fetcher(i: i32) -> Fetcher {
    match i {
        1 => Fetcher::LocalMsix,
        2 => Fetcher::ExtractDiagnostic,
        _ => Fetcher::Winget,
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
        "Updating Claude" => "正在更新 Claude".into(),
        "Downloading official installer" => "正在下载官方安装器".into(),
        "Downloading Claude" => "正在下载 Claude".into(),
        "Running Claude installer" => "正在运行 Claude 安装器".into(),
        "Waiting for Anthropic's installer to finish" => "等待 Anthropic 安装器完成".into(),
        "Verifying registration" => "正在验证注册".into(),
        "Claude is already current" => "Claude 已是最新版本".into(),
        "Checking Windows Appx registration" => "正在检查 Windows Appx 注册".into(),
        "Updating launcher" => "正在更新启动器".into(),
        "Writing updater state" => "正在写入更新器状态".into(),
        "Installing Claude MSIX" => "正在安装 Claude MSIX".into(),
        "Extracting diagnostic copy" => "正在解包诊断副本".into(),
        "Installing updater" => "正在安装更新器".into(),
        "Writing launcher and registration" => "正在写入启动器和注册信息".into(),
        "Validating install" => "正在验证安装".into(),
        "Removing Start Menu shortcut" => "正在移除开始菜单快捷方式".into(),
        "Removing registry entries" => "正在移除注册表项".into(),
        "Deleting updater files" => "正在删除更新器文件".into(),
        "Finalizing" => "正在完成".into(),
        _ => {
            if let Some(version) = text.strip_prefix("Preparing official installer ") {
                return format!("准备官方安装器 {version}");
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
    let source = parse_string_flag(args, "--source")
        .as_deref()
        .and_then(Fetcher::parse)
        .unwrap_or_default();
    Some(GuiOptions {
        mode,
        root: parse_string_flag(args, "--path")
            .map(PathBuf::from)
            .unwrap_or_else(|| default_path(mode)),
        source,
        msix_path: parse_string_flag(args, "--msix-path").map(PathBuf::from),
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
    let source = match opts.source {
        Fetcher::Winget => "winget",
        Fetcher::LocalMsix => "local_msix",
        Fetcher::ExtractDiagnostic => "extract_diagnostic",
    };
    let mut args = format!(
        "--auto-install --mode {} --path \"{}\" --source {}",
        mode,
        opts.root.display(),
        source
    );
    if let Some(msix) = &opts.msix_path {
        args.push_str(&format!(" --msix-path \"{}\"", msix.display()));
    }
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
                "Updating Claude",
                "Preparing official installer 1.2.3"
            ),
            ("正在更新 Claude".into(), "准备官方安装器 1.2.3".into())
        );
        assert_eq!(
            progress_text(AppLanguage::EnUs, "Updating Claude", "1.0 MB downloaded"),
            ("Updating Claude".into(), "1.0 MB downloaded".into())
        );
    }
}
