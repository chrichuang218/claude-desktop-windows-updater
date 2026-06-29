# Claude Desktop Updater

Unofficial Windows updater and registration repair tool for the **Claude Desktop** app.

The default path uses Anthropic's official Windows installer metadata from `winget`, then verifies that Windows still has Claude registered as an Appx/MSIX package. If registration is missing or stale, it re-registers the existing `AppxManifest.xml`, including StartApps, the `claude:` URL protocol, startup task, packaged service, and firewall rule declarations.

This project is based on `vaportail/codex-windows-updater`; the updater code is MIT licensed. This project is not affiliated with, endorsed by, or sponsored by Anthropic. Claude, Claude Desktop, Anthropic, and related branding are trademarks or assets of Anthropic.

## Build

Requires Rust 1.80+ and the MSVC toolchain on Windows.

```powershell
.\scripts\package-release.ps1
```

The packaged outputs are:

```text
target/release/Claude Desktop Updater.exe
target/release/ClaudeDesktopUpdater/Claude Desktop Updater.exe
```

Installed updater roots and Start Menu shortcuts use `Claude Desktop Updater.exe`
as the stable entrypoint.

## Install Layout

Installed updater roots use the same shape as the Codex updater:

```text
ClaudeDesktopUpdater/
├── Claude Desktop Updater.exe
├── updater.json
├── downloads/
└── versions/
    ├── <package-version>/  -> current Claude Appx InstallLocation
    └── current/            -> <package-version>
```

Claude still runs from the Windows Appx package. The `versions/` entries are local NTFS junctions so tools and shortcuts can inspect a stable Codex-style layout without replacing Windows package registration.
For Claude, the executable is under `versions/current/app/Claude.exe` because the junction targets the Appx package root.

Default install roots:

```text
Portable: <current directory>\ClaudeDesktopUpdater
User:     %LOCALAPPDATA%\ClaudeDesktopUpdater
System:   C:\Program Files\ClaudeDesktopUpdater
```

The UI supports Simplified Chinese and English. The selected language is saved in `updater.json` as `language: "zh-cn"` or `language: "en-us"` and is reused for future update and uninstall prompts.

## CLI

```powershell
& '.\Claude Desktop Updater.exe' --status
& '.\Claude Desktop Updater.exe' --check
& '.\Claude Desktop Updater.exe' --update
& '.\Claude Desktop Updater.exe' --repair-register
& '.\Claude Desktop Updater.exe' --msix D:\Downloads\Claude.msix
& '.\Claude Desktop Updater.exe' --extract-msix D:\Downloads\Claude.msix
& '.\Claude Desktop Updater.exe' --launch
```

### Commands

| Flag | Effect |
|---|---|
| `--status` | Shows the installed Claude Appx package, version, install location, StartApps registration, `claude:` protocol registration, and package-managed integration declarations. |
| `--check` | Queries `winget show --id Anthropic.Claude --source winget` and prints latest installer metadata. |
| `--update` | Downloads the official installer URL from winget metadata, verifies SHA-256, runs it, then verifies Appx registration. |
| `--repair-register` | Re-registers the current Claude `AppxManifest.xml` with `Add-AppxPackage -Register`. |
| `--msix <path>` | Installs or updates Claude from a local MSIX using `Add-AppxPackage`. |
| `--extract-msix <path>` | Extracts an MSIX into a diagnostic folder only; this is not a registered install. |
| `--launch` | Starts Claude via `shell:appsFolder\Claude_pzs8sxrjxfjjc!Claude`. |
| `--uninstall` | Removes this updater. It does not uninstall Anthropic Claude. |

## Registration Checks

The updater treats these values as the expected Claude package identity:

```text
Package name:   Claude
Package family: Claude_pzs8sxrjxfjjc
AppID:          Claude_pzs8sxrjxfjjc!Claude
URL protocol:   claude:
Startup task:   ClaudeStartup
Service:        CoworkVMService
Executable:     app\Claude.exe
Firewall apps:  app\Claude.exe, app\resources\cowork-svc.exe
```

After update or repair, it verifies:

```powershell
Get-AppxPackage -Name Claude
Get-StartApps | Where-Object { $_.AppID -eq 'Claude_pzs8sxrjxfjjc!Claude' }
Get-Item -LiteralPath 'Registry::HKEY_CLASSES_ROOT\claude'
```

It also reads the installed `AppxManifest.xml` and confirms that Claude still declares `ClaudeStartup`, `CoworkVMService`, and firewall rules for `app\Claude.exe` and `app\resources\cowork-svc.exe`.

If the AppID, URL protocol, or package integration declarations are missing, it runs:

```powershell
Add-AppxPackage -Path "<InstallLocation>\AppxManifest.xml" -Register -DisableDevelopmentMode -ForceApplicationShutdown
```

Claude's startup task, packaged service, and firewall rules are declared by `AppxManifest.xml`; re-registering the manifest is the repair path for those Windows integrations.

## Installed Launcher Behavior

When installed, `Claude Desktop Updater.exe` normally starts Claude through:

```text
shell:appsFolder\Claude_pzs8sxrjxfjjc!Claude
```

It also syncs `updater.json` from the current `Get-AppxPackage -Name Claude` result. When the configured update policy is due, it checks winget metadata; if a newer Claude package is available, it opens the update prompt. Deferring an update records the choice in `updater.json`; updating runs the official installer and then repeats the registration checks above.

## Notes

- The updater does not change Claude's own enterprise `disableAutoUpdates` policy.
- `claude.ai` direct MSIX endpoints can be blocked by Cloudflare for command-line clients, so the default update path relies on `winget` metadata and keeps local MSIX as a fallback.
- Start Menu shortcut and Add/Remove Programs entries belong to this updater only and intentionally do not overwrite Anthropic's official Claude entries.
