use anyhow::{anyhow, Context, Result};
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use std::cmp::Ordering;
use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;

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
pub const CLAUDE_WINGET_ID: &str = "Anthropic.Claude";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WingetInstallerMetadata {
    pub version: String,
    pub installer_url: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudePackageStatus {
    pub package_full_name: String,
    pub package_family_name: String,
    pub version: String,
    pub architecture: String,
    pub install_location: String,
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

pub fn parse_winget_show(output: &str) -> Result<WingetInstallerMetadata> {
    let version = find_value(output, &["版本"]).ok_or_else(|| anyhow!("missing version"))?;
    let installer_url = find_value(output, &["Installer Url", "Installer URL", "安装程序 URL"])
        .ok_or_else(|| anyhow!("missing installer url"))?;
    let sha256 = find_value(
        output,
        &["Installer Sha256", "Installer SHA256", "安装程序 SHA256"],
    )
    .ok_or_else(|| anyhow!("missing installer sha256"))?;

    Ok(WingetInstallerMetadata {
        version,
        installer_url,
        sha256,
    })
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

    Ok(ClaudePackageStatus {
        package_full_name,
        package_family_name,
        version,
        architecture,
        install_location,
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

pub fn query_package_status() -> Result<ClaudePackageStatus> {
    let command = format!(
        "Get-AppxPackage -Name {} | Select-Object Name, PackageFullName, PackageFamilyName, Version, Architecture, InstallLocation | Format-List",
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

pub fn query_winget_metadata() -> Result<WingetInstallerMetadata> {
    let output = command_no_window("winget")
        .args(["show", "--id", CLAUDE_WINGET_ID, "--source", "winget"])
        .output()
        .context("running winget show")?;

    if !output.status.success() {
        return Err(anyhow!(
            "winget show failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    parse_winget_show(&String::from_utf8_lossy(&output.stdout))
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
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            command,
        ])
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

pub fn launch_registered_claude() -> Result<()> {
    command_no_window("explorer.exe")
        .arg(format!(r"shell:appsFolder\{CLAUDE_APP_ID}"))
        .spawn()
        .context("launching Claude via shell:appsFolder")?;
    Ok(())
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
    fn parses_winget_show_output_for_installer_metadata() {
        let output = r#"
已找到 Claude [Anthropic.Claude]
版本: 1.15962.1
安装：
  安装程序类型： exe
  安装程序 URL： https://downloads.claude.ai/releases/win32/x64/1.15962.1/Claude.exe
  安装程序 SHA256： 9e17f7dc732595b59cc07cbfec9bc3bc826355cafdbbed12475eb6f084dd6d16
"#;

        let metadata = parse_winget_show(output).expect("winget metadata");

        assert_eq!(metadata.version, "1.15962.1");
        assert_eq!(
            metadata.installer_url,
            "https://downloads.claude.ai/releases/win32/x64/1.15962.1/Claude.exe"
        );
        assert_eq!(
            metadata.sha256,
            "9e17f7dc732595b59cc07cbfec9bc3bc826355cafdbbed12475eb6f084dd6d16"
        );
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
    fn builds_register_command_for_existing_manifest() {
        let status = ClaudePackageStatus {
            package_full_name: "Claude_1.15962.1.0_x64__pzs8sxrjxfjjc".into(),
            package_family_name: CLAUDE_PACKAGE_FAMILY.into(),
            version: "1.15962.1.0".into(),
            architecture: "X64".into(),
            install_location: r"C:\Program Files\WindowsApps\Claude_1.15962.1.0_x64__pzs8sxrjxfjjc"
                .into(),
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
}
