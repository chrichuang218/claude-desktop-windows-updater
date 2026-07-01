use anyhow::{anyhow, Context, Result};
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use std::cmp::Ordering;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

pub const CLAUDE_PACKAGE_NAME: &str = "Claude";
pub const CLAUDE_PACKAGE_FAMILY: &str = "Claude_pzs8sxrjxfjjc";
pub const CLAUDE_APP_ID: &str = "Claude_pzs8sxrjxfjjc!Claude";
pub const CLAUDE_PROTOCOL: &str = "claude";
pub const CLAUDE_STARTUP_TASK: &str = "ClaudeStartup";
pub const CLAUDE_SERVICE_NAME: &str = "CoworkVMService";
pub const CLAUDE_EXE: &str = r"app\Claude.exe";
pub const CLAUDE_COWORK_EXE: &str = r"app\resources\cowork-svc.exe";
pub const CLAUDE_MSIX_LATEST_X64_URL: &str =
    "https://api.anthropic.com/api/desktop/win32/x64/msix/latest/redirect";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfficialMsixMetadata {
    pub version: String,
    pub msix_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudePackageStatus {
    pub package_full_name: String,
    pub package_family_name: String,
    pub version: String,
    pub architecture: String,
    pub install_location: String,
    pub signature_kind: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClaudeManifestIntegrations {
    pub startup_task: bool,
    pub protocol: bool,
    pub service: bool,
    pub claude_firewall: bool,
    pub cowork_firewall: bool,
}

impl ClaudeManifestIntegrations {
    pub fn complete(self) -> bool {
        self.startup_task
            && self.protocol
            && self.service
            && self.claude_firewall
            && self.cowork_firewall
    }
}

pub fn parse_appx_package(output: &str) -> Result<ClaudePackageStatus> {
    let package_full_name = find_value(output, &["PackageFullName"])
        .ok_or_else(|| anyhow!("missing PackageFullName"))?;
    let package_family_name = find_value(output, &["PackageFamilyName"])
        .ok_or_else(|| anyhow!("missing PackageFamilyName"))?;
    let version = find_value(output, &["Version"]).ok_or_else(|| anyhow!("missing Version"))?;
    let architecture =
        find_value(output, &["Architecture"]).ok_or_else(|| anyhow!("missing Architecture"))?;
    let install_location = find_value(output, &["InstallLocation"])
        .ok_or_else(|| anyhow!("missing InstallLocation"))?;
    let signature_kind = find_value(output, &["SignatureKind"]);

    Ok(ClaudePackageStatus {
        package_full_name,
        package_family_name,
        version,
        architecture,
        install_location,
        signature_kind,
    })
}

pub fn start_apps_has_claude(output: &str) -> bool {
    output
        .lines()
        .any(|line| line.split_whitespace().any(|part| part == CLAUDE_APP_ID))
}

pub fn parse_manifest_integrations(raw: &str) -> Result<ClaudeManifestIntegrations> {
    let mut reader = Reader::from_str(raw);
    reader.config_mut().trim_text(true);
    let mut integrations = ClaudeManifestIntegrations::default();

    loop {
        match reader.read_event() {
            Ok(Event::Empty(element)) | Ok(Event::Start(element)) => {
                collect_manifest_element(&element, &mut integrations)?;
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(anyhow!("invalid AppxManifest.xml: {e}")),
            _ => {}
        }
    }

    Ok(integrations)
}

pub fn query_manifest_integrations(
    status: &ClaudePackageStatus,
) -> Result<ClaudeManifestIntegrations> {
    let manifest = Path::new(&status.install_location).join("AppxManifest.xml");
    let raw = std::fs::read_to_string(&manifest)
        .with_context(|| format!("reading {}", manifest.display()))?;
    parse_manifest_integrations(&raw)
}

pub fn compare_versions(left: &str, right: &str) -> Ordering {
    let left = version_parts(left);
    let right = version_parts(right);
    let max = left.len().max(right.len());
    for i in 0..max {
        let a = left.get(i).copied().unwrap_or(0);
        let b = right.get(i).copied().unwrap_or(0);
        match a.cmp(&b) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    Ordering::Equal
}

pub fn display_version(version: &str) -> String {
    let mut parts = version_parts(version);
    if parts.len() < 4 || parts.len() != version.split('.').count() {
        return version.to_string();
    }

    while parts.len() > 3 && parts.last() == Some(&0) {
        parts.pop();
    }

    parts
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(".")
}

pub fn registration_needs_repair(
    app_id_registered: bool,
    protocol_registered: bool,
    manifest: ClaudeManifestIntegrations,
) -> bool {
    !app_id_registered || !protocol_registered || !manifest.complete()
}

pub fn powershell_bool_is_true(output: &str) -> bool {
    output
        .lines()
        .any(|line| line.trim().eq_ignore_ascii_case("true"))
}

fn collect_manifest_element(
    element: &BytesStart<'_>,
    integrations: &mut ClaudeManifestIntegrations,
) -> Result<()> {
    match element.local_name().as_ref() {
        b"StartupTask" if attr_eq(element, b"TaskId", CLAUDE_STARTUP_TASK)? => {
            integrations.startup_task = true;
        }
        b"Protocol" if attr_eq(element, b"Name", CLAUDE_PROTOCOL)? => {
            integrations.protocol = true;
        }
        b"Service" if attr_eq(element, b"Name", CLAUDE_SERVICE_NAME)? => {
            integrations.service = true;
        }
        b"FirewallRules" => {
            if attr_path_eq(element, b"Executable", CLAUDE_EXE)? {
                integrations.claude_firewall = true;
            }
            if attr_path_eq(element, b"Executable", CLAUDE_COWORK_EXE)? {
                integrations.cowork_firewall = true;
            }
        }
        _ => {}
    }
    Ok(())
}

fn attr_eq(element: &BytesStart<'_>, name: &[u8], expected: &str) -> Result<bool> {
    for attr in element.attributes() {
        let attr = attr.context("reading AppxManifest.xml attribute")?;
        if attr.key.as_ref() == name {
            return Ok(attr
                .unescape_value()
                .context("decoding AppxManifest.xml attribute")?
                .eq_ignore_ascii_case(expected));
        }
    }
    Ok(false)
}

fn attr_path_eq(element: &BytesStart<'_>, name: &[u8], expected: &str) -> Result<bool> {
    for attr in element.attributes() {
        let attr = attr.context("reading AppxManifest.xml attribute")?;
        if attr.key.as_ref() == name {
            let value = attr
                .unescape_value()
                .context("decoding AppxManifest.xml path attribute")?
                .replace('/', "\\");
            return Ok(value.eq_ignore_ascii_case(expected));
        }
    }
    Ok(false)
}

pub fn register_manifest_command(status: &ClaudePackageStatus) -> String {
    let manifest = format!(r"{}\AppxManifest.xml", status.install_location);
    format!(
        "Add-AppxPackage -Path '{}' -Register -DisableDevelopmentMode -ForceApplicationShutdown",
        escape_powershell_single_quoted(&manifest)
    )
}

pub fn msix_install_command(msix: &Path) -> String {
    format!(
        "Add-AppxPackage -Path '{}' -ForceApplicationShutdown -ForceUpdateFromAnyVersion",
        escape_powershell_single_quoted(&msix.display().to_string())
    )
}

pub fn remove_package_command(status: &ClaudePackageStatus) -> String {
    format!(
        "$ErrorActionPreference = 'Stop'; Remove-AppxPackage -Package '{}' -ErrorAction Stop",
        escape_powershell_single_quoted(&status.package_full_name)
    )
}

pub fn query_package_status() -> Result<ClaudePackageStatus> {
    let command = format!(
        "Get-AppxPackage -Name {} | Select-Object Name, PackageFullName, PackageFamilyName, Version, Architecture, InstallLocation, SignatureKind | Format-List",
        CLAUDE_PACKAGE_NAME
    );
    let output = windows_powershell()
        .args(["-NoProfile", "-Command", &command])
        .output()
        .context("running Get-AppxPackage")?;

    if !output.status.success() {
        return Err(anyhow!(
            "Get-AppxPackage failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    parse_appx_package(&String::from_utf8_lossy(&output.stdout))
}

pub fn parse_msix_version_from_url(url: &str) -> Result<String> {
    let marker = "/releases/win32/x64/";
    let Some((_, tail)) = url.split_once(marker) else {
        anyhow::bail!("MSIX URL does not contain {marker}: {url}");
    };
    let version = tail
        .split('/')
        .next()
        .filter(|part| !part.trim().is_empty())
        .ok_or_else(|| anyhow!("MSIX URL is missing version: {url}"))?;
    Ok(version.to_string())
}

pub fn package_is_appx(status: &ClaudePackageStatus) -> bool {
    status.package_family_name == CLAUDE_PACKAGE_FAMILY
}

pub fn package_is_developer_signed(status: &ClaudePackageStatus) -> bool {
    status
        .signature_kind
        .as_deref()
        .is_some_and(|kind| kind.eq_ignore_ascii_case("Developer"))
}

pub fn claude_exe_path(status: &ClaudePackageStatus) -> PathBuf {
    PathBuf::from(&status.install_location)
        .join("app")
        .join("Claude.exe")
}

pub fn query_start_apps_registered() -> Result<bool> {
    let output = windows_powershell()
        .args([
            "-NoProfile",
            "-Command",
            "Get-StartApps | Where-Object { $_.AppID -eq 'Claude_pzs8sxrjxfjjc!Claude' } | Format-Table -AutoSize",
        ])
        .output()
        .context("running Get-StartApps")?;

    if !output.status.success() {
        return Err(anyhow!(
            "Get-StartApps failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(start_apps_has_claude(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

pub fn query_protocol_registered() -> Result<bool> {
    let command = format!(
        "$key = Get-Item -LiteralPath 'Registry::HKEY_CLASSES_ROOT\\{}' -ErrorAction SilentlyContinue; if ($null -eq $key) {{ 'False' }} else {{ [bool]($key.GetValue('URL Protocol', $null) -ne $null) }}",
        CLAUDE_PROTOCOL
    );
    let output = windows_powershell()
        .args(["-NoProfile", "-Command", &command])
        .output()
        .context("querying Claude URL protocol registration")?;

    if !output.status.success() {
        return Err(anyhow!(
            "Claude URL protocol query failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(powershell_bool_is_true(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

pub fn run_powershell(command: &str) -> Result<()> {
    let output = windows_powershell()
        .args(run_powershell_args(command))
        .output()
        .with_context(|| format!("running PowerShell command: {command}"))?;

    if output.status.success() {
        return Ok(());
    }

    Err(anyhow!(
        "PowerShell command failed\ncommand: {}\nstdout: {}\nstderr: {}",
        command,
        String::from_utf8_lossy(&output.stdout).trim(),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

fn run_powershell_args(command: &str) -> [&str; 3] {
    ["-NoProfile", "-Command", command]
}

pub fn launch_registered_claude() -> Result<()> {
    command_no_window("explorer.exe")
        .arg(format!(r"shell:appsFolder\{CLAUDE_APP_ID}"))
        .spawn()
        .context("launching Claude via shell:appsFolder")?;
    Ok(())
}

pub fn query_official_msix_metadata() -> Result<OfficialMsixMetadata> {
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let response = client.get(CLAUDE_MSIX_LATEST_X64_URL).send()?;
    if !response.status().is_redirection() {
        anyhow::bail!(
            "official Claude MSIX redirect returned HTTP {}",
            response.status()
        );
    }
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .ok_or_else(|| anyhow!("official Claude MSIX redirect is missing Location"))?
        .to_str()
        .context("decoding official Claude MSIX redirect Location")?;
    let msix_url = if location.starts_with("https://") {
        location.to_string()
    } else if location.starts_with('/') {
        format!("https://downloads.claude.ai{location}")
    } else {
        anyhow::bail!("unsupported official Claude MSIX redirect Location: {location}");
    };
    let version = parse_msix_version_from_url(&msix_url)?;
    Ok(OfficialMsixMetadata { version, msix_url })
}

pub fn stop_running_claude() -> Result<()> {
    stop_claude_with_native_windows_api()
}

#[cfg(windows)]
fn stop_claude_with_native_windows_api() -> Result<()> {
    stop_cowork_service()?;
    terminate_official_claude_processes()?;
    wait_until_claude_stopped(Duration::from_secs(5))
}

#[cfg(not(windows))]
fn stop_claude_with_native_windows_api() -> Result<()> {
    anyhow::bail!("stopping Claude is only supported on Windows")
}

#[cfg(windows)]
fn stop_cowork_service() -> Result<()> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Services::{
        CloseServiceHandle, ControlService, OpenSCManagerW, OpenServiceW, SC_MANAGER_CONNECT,
        SERVICE_CONTROL_STOP, SERVICE_QUERY_STATUS, SERVICE_STATUS, SERVICE_STOP, SERVICE_STOPPED,
    };

    unsafe {
        let manager = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT)
            .context("opening Windows service manager")?;
        let service_name = wide_utf16(CLAUDE_SERVICE_NAME);
        let service = match OpenServiceW(
            manager,
            PCWSTR(service_name.as_ptr()),
            SERVICE_QUERY_STATUS | SERVICE_STOP,
        ) {
            Ok(service) => service,
            Err(err) => {
                let _ = CloseServiceHandle(manager);
                if is_service_not_installed_error(err.code().0) {
                    return Ok(());
                }
                return Err(anyhow!("opening {CLAUDE_SERVICE_NAME}: {err}"));
            }
        };

        let result = (|| -> Result<()> {
            if service_state(service)? == SERVICE_STOPPED {
                return Ok(());
            }

            let mut status = SERVICE_STATUS::default();
            ControlService(service, SERVICE_CONTROL_STOP, &mut status)
                .with_context(|| format!("stopping {CLAUDE_SERVICE_NAME}"))?;

            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(15) {
                if service_state(service)? == SERVICE_STOPPED {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            anyhow::bail!("timed out stopping {CLAUDE_SERVICE_NAME}")
        })();

        let _ = CloseServiceHandle(service);
        let _ = CloseServiceHandle(manager);
        result
    }
}

#[cfg(windows)]
fn terminate_official_claude_processes() -> Result<()> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    };

    unsafe {
        let snapshot =
            CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).context("listing processes")?;
        let result = (|| -> Result<()> {
            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            if Process32FirstW(snapshot, &mut entry).is_err() {
                return Ok(());
            }

            loop {
                if process_name_eq(&entry, "claude.exe")
                    && is_official_claude_process(entry.th32ProcessID)
                {
                    let process = match OpenProcess(
                        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
                        false,
                        entry.th32ProcessID,
                    ) {
                        Ok(process) => process,
                        Err(err) if is_process_already_exited_error(err.code().0) => {
                            continue;
                        }
                        Err(err) => {
                            return Err(err).with_context(|| {
                                format!("opening Claude process {}", entry.th32ProcessID)
                            });
                        }
                    };
                    let terminate_result = TerminateProcess(process, 1).with_context(|| {
                        format!("terminating Claude process {}", entry.th32ProcessID)
                    });
                    let _ = CloseHandle(process);
                    if let Err(err) = terminate_result {
                        if !windows_error_chain_contains(&err, is_process_already_exited_error) {
                            return Err(err);
                        }
                    }
                }

                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
            Ok(())
        })();
        let _ = CloseHandle(snapshot);
        return result;
    }

    fn process_name_eq(entry: &PROCESSENTRY32W, expected: &str) -> bool {
        nul_terminated_utf16_to_string(&entry.szExeFile).eq_ignore_ascii_case(expected)
    }

    unsafe fn is_official_claude_process(pid: u32) -> bool {
        process_image_path(pid).is_some_and(|path| is_official_claude_path(&path))
    }
}

#[cfg(windows)]
fn wait_until_claude_stopped(timeout: Duration) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        let running = official_claude_process_count()?;
        if running == 0 {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let running = official_claude_process_count()?;
    if running == 0 {
        Ok(())
    } else {
        anyhow::bail!("failed to stop {running} Claude process(es)")
    }
}

#[cfg(windows)]
fn official_claude_process_count() -> Result<usize> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    unsafe {
        let snapshot =
            CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).context("listing processes")?;
        let result = (|| -> Result<usize> {
            let mut count = 0usize;
            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            if Process32FirstW(snapshot, &mut entry).is_err() {
                return Ok(0);
            }

            loop {
                if nul_terminated_utf16_to_string(&entry.szExeFile)
                    .eq_ignore_ascii_case("claude.exe")
                    && process_image_path(entry.th32ProcessID)
                        .is_some_and(|path| is_official_claude_path(&path))
                {
                    count += 1;
                }

                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
            Ok(count)
        })();
        let _ = CloseHandle(snapshot);
        result
    }
}

#[cfg(windows)]
unsafe fn service_state(
    service: windows::Win32::System::Services::SC_HANDLE,
) -> Result<windows::Win32::System::Services::SERVICE_STATUS_CURRENT_STATE> {
    use std::mem::size_of;
    use windows::Win32::System::Services::{
        QueryServiceStatusEx, SC_STATUS_PROCESS_INFO, SERVICE_STATUS_PROCESS,
    };

    let mut status = SERVICE_STATUS_PROCESS::default();
    let mut needed = 0u32;
    let buffer = std::slice::from_raw_parts_mut(
        &mut status as *mut _ as *mut u8,
        size_of::<SERVICE_STATUS_PROCESS>(),
    );
    QueryServiceStatusEx(service, SC_STATUS_PROCESS_INFO, Some(buffer), &mut needed)
        .with_context(|| format!("querying {CLAUDE_SERVICE_NAME} status"))?;
    Ok(status.dwCurrentState)
}

#[cfg(windows)]
unsafe fn process_image_path(pid: u32) -> Option<String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
        return None;
    };
    let mut buffer = vec![0u16; 32768];
    let mut size = buffer.len() as u32;
    let ok = QueryFullProcessImageNameW(
        process,
        PROCESS_NAME_WIN32,
        windows::core::PWSTR(buffer.as_mut_ptr()),
        &mut size,
    )
    .is_ok();
    let _ = CloseHandle(process);
    if !ok || size == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buffer[..size as usize]).replace('/', "\\"))
}

fn is_official_claude_path(path: &str) -> bool {
    let path = path.replace('/', "\\").to_ascii_lowercase();
    path.contains(r"\windowsapps\claude_") && path.ends_with(r"__pzs8sxrjxfjjc\app\claude.exe")
}

fn is_service_not_installed_error(code: i32) -> bool {
    const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
    const HRESULT_FROM_ERROR_SERVICE_DOES_NOT_EXIST: i32 = 0x80070424u32 as i32;
    code == ERROR_SERVICE_DOES_NOT_EXIST || code == HRESULT_FROM_ERROR_SERVICE_DOES_NOT_EXIST
}

fn is_process_already_exited_error(code: i32) -> bool {
    const ERROR_INVALID_PARAMETER: i32 = 87;
    const HRESULT_FROM_ERROR_INVALID_PARAMETER: i32 = 0x80070057u32 as i32;
    code == ERROR_INVALID_PARAMETER || code == HRESULT_FROM_ERROR_INVALID_PARAMETER
}

fn windows_error_chain_contains(err: &anyhow::Error, predicate: impl Fn(i32) -> bool) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<windows::core::Error>()
            .is_some_and(|windows_err| predicate(windows_err.code().0))
    })
}

#[cfg(windows)]
fn wide_utf16(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn nul_terminated_utf16_to_string(buffer: &[u16]) -> String {
    let len = buffer
        .iter()
        .position(|ch| *ch == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..len])
}

fn windows_powershell() -> Command {
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    command_no_window(format!(
        r"{}\System32\WindowsPowerShell\v1.0\powershell.exe",
        system_root
    ))
}

fn command_no_window(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

fn find_value(output: &str, keys: &[&str]) -> Option<String> {
    for line in output.lines() {
        let normalized = line.replace('：', ":");
        let Some((key, value)) = normalized.split_once(':') else {
            continue;
        };
        if keys
            .iter()
            .any(|candidate| key.trim().eq_ignore_ascii_case(candidate))
        {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn escape_powershell_single_quoted(value: &str) -> String {
    value.replace('\'', "''")
}

fn version_parts(version: &str) -> Vec<u64> {
    version
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .map(|part| part.parse::<u64>().unwrap_or(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_official_msix_version_from_redirect_url() {
        let version = parse_msix_version_from_url(
            "https://downloads.claude.ai/releases/win32/x64/1.15962.1/Claude-1e236d9fa9efd21a5a0a66a7b70c028f48848604.msix",
        )
        .expect("msix version");

        assert_eq!(version, "1.15962.1");
    }

    #[test]
    fn official_msix_redirect_uses_programmatic_api_host() {
        assert!(CLAUDE_MSIX_LATEST_X64_URL.starts_with("https://api.anthropic.com/"));
    }

    #[test]
    fn parses_appx_package_status_from_powershell_output() {
        let output = r#"
Name              : Claude
PackageFullName   : Claude_1.15962.1.0_x64__pzs8sxrjxfjjc
PackageFamilyName : Claude_pzs8sxrjxfjjc
Version           : 1.15962.1.0
Architecture      : X64
InstallLocation   : C:\Program Files\WindowsApps\Claude_1.15962.1.0_x64__pzs8sxrjxfjjc
SignatureKind     : Developer
"#;

        let status = parse_appx_package(output).expect("appx status");

        assert_eq!(
            status.package_full_name,
            "Claude_1.15962.1.0_x64__pzs8sxrjxfjjc"
        );
        assert_eq!(status.package_family_name, CLAUDE_PACKAGE_FAMILY);
        assert_eq!(status.version, "1.15962.1.0");
        assert_eq!(status.architecture, "X64");
        assert!(status
            .install_location
            .ends_with("Claude_1.15962.1.0_x64__pzs8sxrjxfjjc"));
        assert_eq!(status.signature_kind.as_deref(), Some("Developer"));
        assert!(package_is_developer_signed(&status));
    }

    #[test]
    fn detects_registered_start_app_id() {
        let output = r#"
Name           AppID
----           -----
Claude         electron.app.Claude
Claude         Claude_pzs8sxrjxfjjc!Claude
"#;

        assert!(start_apps_has_claude(output));
    }

    #[test]
    fn parses_claude_manifest_integrations() {
        let raw = r#"
<Package xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10"
         xmlns:uap3="http://schemas.microsoft.com/appx/manifest/uap/windows10/3"
         xmlns:desktop="http://schemas.microsoft.com/appx/manifest/desktop/windows10"
         xmlns:desktop2="http://schemas.microsoft.com/appx/manifest/desktop/windows10/2"
         xmlns:desktop6="http://schemas.microsoft.com/appx/manifest/desktop/windows10/6">
  <Applications>
    <Application Id="Claude" Executable="app\Claude.exe">
      <Extensions>
        <desktop:StartupTask TaskId="ClaudeStartup" Enabled="false" DisplayName="Claude" />
        <uap3:Protocol Name="claude" Parameters="&quot;%1&quot;" />
        <desktop6:Service Name="CoworkVMService" StartupType="auto" StartAccount="localSystem" />
      </Extensions>
    </Application>
  </Applications>
  <Extensions>
    <desktop2:FirewallRules Executable="app\Claude.exe" />
    <desktop2:FirewallRules Executable="app/resources/cowork-svc.exe" />
  </Extensions>
</Package>
"#;

        let integrations = parse_manifest_integrations(raw).expect("manifest integrations");

        assert!(integrations.complete());
    }

    #[test]
    fn compares_versions_with_trailing_zero_equivalence() {
        assert_eq!(
            compare_versions("1.15962.1", "1.15962.1.0"),
            std::cmp::Ordering::Equal
        );
        assert_eq!(
            compare_versions("1.15962.1", "1.9659.2.0"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            compare_versions("1.15963.0", "1.15962.9"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            compare_versions("1.15962.1", "1.15962.2"),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn formats_display_version_without_msix_padding_zero() {
        assert_eq!(display_version("1.15962.1.0"), "1.15962.1");
        assert_eq!(display_version("26.623.8305.0"), "26.623.8305");
        assert_eq!(display_version("1.2.0.0"), "1.2.0");
        assert_eq!(display_version("1.15962.1"), "1.15962.1");
        assert_eq!(display_version("1.15962.1-beta"), "1.15962.1-beta");
    }

    #[test]
    fn appx_install_uses_packaged_claude_exe() {
        let status = ClaudePackageStatus {
            package_full_name: "Claude_1.15962.1.0_x64__pzs8sxrjxfjjc".into(),
            package_family_name: CLAUDE_PACKAGE_FAMILY.into(),
            version: "1.15962.1.0".into(),
            architecture: "X64".into(),
            install_location: r"C:\Program Files\WindowsApps\Claude_1.15962.1.0_x64__pzs8sxrjxfjjc"
                .into(),
            signature_kind: Some("Store".into()),
        };

        assert_eq!(
            claude_exe_path(&status),
            PathBuf::from(
                r"C:\Program Files\WindowsApps\Claude_1.15962.1.0_x64__pzs8sxrjxfjjc\app\Claude.exe"
            )
        );
    }

    #[test]
    fn registration_health_requires_all_windows_integrations() {
        let complete = ClaudeManifestIntegrations {
            startup_task: true,
            protocol: true,
            service: true,
            claude_firewall: true,
            cowork_firewall: true,
        };
        let missing_service = ClaudeManifestIntegrations {
            service: false,
            ..complete
        };

        assert!(!registration_needs_repair(true, true, complete));
        assert!(registration_needs_repair(false, true, complete));
        assert!(registration_needs_repair(true, false, complete));
        assert!(registration_needs_repair(true, true, missing_service));
    }

    #[test]
    fn parses_powershell_true_output() {
        assert!(powershell_bool_is_true("\r\nTrue\r\n"));
        assert!(!powershell_bool_is_true("\r\nFalse\r\n"));
        assert!(!powershell_bool_is_true(""));
    }

    #[test]
    fn recognizes_only_official_claude_process_paths() {
        assert!(is_official_claude_path(
            r"C:\Program Files\WindowsApps\Claude_1.15962.1.0_x64__pzs8sxrjxfjjc\app\Claude.exe"
        ));
        assert!(is_official_claude_path(
            r"C:/Program Files/WindowsApps/Claude_1.15962.1.0_x64__pzs8sxrjxfjjc/app/Claude.exe"
        ));
        assert!(!is_official_claude_path(
            r"D:\Tools\Claude_1.15962.1.0_x64__pzs8sxrjxfjjc\app\Claude.exe"
        ));
        assert!(!is_official_claude_path(
            r"C:\Program Files\WindowsApps\Other_1.0.0.0_x64__pzs8sxrjxfjjc\app\Claude.exe"
        ));
    }

    #[test]
    fn recognizes_service_not_installed_errors() {
        assert!(is_service_not_installed_error(1060));
        assert!(is_service_not_installed_error(0x80070424u32 as i32));
        assert!(!is_service_not_installed_error(5));
    }

    #[test]
    fn recognizes_process_already_exited_errors() {
        assert!(is_process_already_exited_error(87));
        assert!(is_process_already_exited_error(0x80070057u32 as i32));
        assert!(!is_process_already_exited_error(5));
    }

    #[test]
    fn run_powershell_does_not_request_execution_policy_bypass() {
        let args = run_powershell_args("exit 0");

        assert_eq!(args, ["-NoProfile", "-Command", "exit 0"]);
        assert!(!args.iter().any(|arg| arg.eq_ignore_ascii_case("Bypass")));
    }

    #[test]
    fn builds_register_command_for_existing_manifest() {
        let status = ClaudePackageStatus {
            package_full_name: "Claude_1.15962.1.0_x64__pzs8sxrjxfjjc".into(),
            package_family_name: CLAUDE_PACKAGE_FAMILY.into(),
            version: "1.15962.1.0".into(),
            architecture: "X64".into(),
            install_location: r"C:\Program Files\WindowsApps\Claude_1.15962.1.0_x64__pzs8sxrjxfjjc"
                .into(),
            signature_kind: Some("Developer".into()),
        };

        let command = register_manifest_command(&status);

        assert!(command.contains("Add-AppxPackage"));
        assert!(command.contains("-Register"));
        assert!(command.contains("AppxManifest.xml"));
        assert!(command.contains("-ForceApplicationShutdown"));
    }

    #[test]
    fn builds_msix_install_command_with_force_update() {
        let command = msix_install_command(Path::new(r"D:\Downloads\Claude.msix"));

        assert!(command.contains("Add-AppxPackage"));
        assert!(command.contains("-ForceUpdateFromAnyVersion"));
        assert!(command.contains(r"D:\Downloads\Claude.msix"));
    }

    #[test]
    fn builds_remove_package_command_for_developer_registration() {
        let status = ClaudePackageStatus {
            package_full_name: "Claude_1.9659.2.0_x64__pzs8sxrjxfjjc".into(),
            package_family_name: CLAUDE_PACKAGE_FAMILY.into(),
            version: "1.9659.2.0".into(),
            architecture: "X64".into(),
            install_location: r"C:\Program Files\WindowsApps\Claude_1.9659.2.0_x64__pzs8sxrjxfjjc"
                .into(),
            signature_kind: Some("Developer".into()),
        };
        let command = remove_package_command(&status);

        assert!(command.contains("Remove-AppxPackage"));
        assert!(command.contains("Claude_1.9659.2.0_x64__pzs8sxrjxfjjc"));
    }
}
