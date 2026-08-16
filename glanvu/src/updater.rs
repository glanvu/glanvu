// SPDX-License-Identifier: Apache-2.0

//! Lightweight background update checker.
//!
//! Checks for newer releases in a background thread and deposits the newest version string and URL
//! if available. Network errors and timeouts fail silently without blocking the application.

use std::sync::Mutex;
use std::time::Duration;

/// Holds the latest update information if a newer release was discovered: `(version_string, release_url)`.
pub static UPDATE_AVAILABLE: Mutex<Option<(String, String)>> = Mutex::new(None);

/// Compare two semantic version strings (e.g. "0.9.1" vs "0.9.2" or "v0.9.2").
/// Returns true if `remote` is strictly newer than `current`.
pub fn is_newer_version(current: &str, remote: &str) -> bool {
    let parse = |v: &str| -> Option<(u32, u32, u32)> {
        let clean = v.trim().trim_start_matches('v').trim_start_matches('V');
        let mut parts = clean.split('.');
        let maj = parts.next()?.parse::<u32>().ok()?;
        let min = parts.next()?.parse::<u32>().ok()?;
        let pat = parts.next().unwrap_or("0").parse::<u32>().ok()?;
        Some((maj, min, pat))
    };

    match (parse(current), parse(remote)) {
        (Some(c), Some(r)) => r > c,
        _ => false,
    }
}

/// Extract version string from JSON payload (supports GitHub release API and version.json format).
pub fn parse_version_json(json: &str) -> Option<(String, String)> {
    // 1. Try "tag_name": "v0.9.2" (GitHub Releases API)
    if let Some(pos) = json.find("\"tag_name\"") {
        let slice = &json[pos + 10..];
        if let Some(colon) = slice.find(':') {
            let after_colon = &slice[colon + 1..];
            if let Some(q1) = after_colon.find('"') {
                let after_q1 = &after_colon[q1 + 1..];
                if let Some(q2) = after_q1.find('"') {
                    let tag = &after_q1[..q2];
                    let ver = tag
                        .trim_start_matches('v')
                        .trim_start_matches('V')
                        .to_string();
                    let url = format!("https://github.com/glanvu/glanvu/releases/tag/{tag}");
                    return Some((ver, url));
                }
            }
        }
    }

    // 2. Try "version": "0.9.2" (glanvu.com/version.json format)
    if let Some(pos) = json.find("\"version\"") {
        let slice = &json[pos + 9..];
        if let Some(colon) = slice.find(':') {
            let after_colon = &slice[colon + 1..];
            if let Some(q1) = after_colon.find('"') {
                let after_q1 = &after_colon[q1 + 1..];
                if let Some(q2) = after_q1.find('"') {
                    let ver = after_q1[..q2].to_string();
                    let url = format!("https://github.com/glanvu/glanvu/releases/tag/v{ver}");
                    return Some((ver, url));
                }
            }
        }
    }

    None
}

/// Path to the update-ignore configuration file.
fn ignore_file_path() -> std::path::PathBuf {
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("C:\\glanvu_cache"));

    #[cfg(not(target_os = "windows"))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));

    base.join("glanvu").join("ignore_update.txt")
}

/// Check if a version string has been explicitly ignored by the user.
pub fn is_version_ignored(version: &str) -> bool {
    let path = ignore_file_path();
    if let Ok(content) = std::fs::read_to_string(path) {
        return content.lines().any(|l| l.trim() == version.trim());
    }
    false
}

/// Save a version string to the ignored versions list.
pub fn ignore_version(version: &str) {
    let path = ignore_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut current = std::fs::read_to_string(&path).unwrap_or_default();
    if !current.lines().any(|l| l.trim() == version.trim()) {
        current.push_str(version.trim());
        current.push('\n');
        let _ = std::fs::write(&path, current);
    }
}

/// Fetch version info via HTTPS using platform-native tools (curl / PowerShell) with strict timeout.
/// Silent failure on network errors or timeout.
fn fetch_latest_release() -> Option<(String, String)> {
    #[cfg(not(target_os = "windows"))]
    {
        let output = std::process::Command::new("curl")
            .args([
                "-s",
                "-m",
                "3",
                "-H",
                "User-Agent: Glanvu-Viewer",
                "https://api.github.com/repos/glanvu/glanvu/releases/latest",
            ])
            .output()
            .ok()?;

        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            if let Some((ver, _)) = parse_version_json(&text) {
                // Direct link to the website download section
                let url = "https://glanvu.com/#download".to_string();
                return Some((ver, url));
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        // On Windows 10/11 curl.exe is standard; fallback to powershell if needed.
        let output = std::process::Command::new("curl.exe")
            .args([
                "-s",
                "-m",
                "3",
                "-H",
                "User-Agent: Glanvu-Viewer",
                "https://api.github.com/repos/glanvu/glanvu/releases/latest",
            ])
            .output();

        if let Ok(out) = output {
            if out.status.success() {
                let text = String::from_utf8_lossy(&out.stdout);
                if let Some((ver, _)) = parse_version_json(&text) {
                    let url = "https://glanvu.com/#download".to_string();
                    return Some((ver, url));
                }
            }
        }
    }

    None
}

/// Spawn a background thread to check for updates against current compiled version.
pub fn spawn_update_check(current_version: &'static str) {
    std::thread::Builder::new()
        .name("glanvu-updater".into())
        .spawn(move || {
            // Small initial pause so the first frame renders instantly without any IO contention
            std::thread::sleep(Duration::from_millis(500));

            if let Some((remote_ver, url)) = fetch_latest_release() {
                if !is_version_ignored(&remote_ver)
                    && is_newer_version(current_version, &remote_ver)
                {
                    if let Ok(mut slot) = UPDATE_AVAILABLE.lock() {
                        *slot = Some((remote_ver, url));
                    }
                }
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_comparison_detects_newer() {
        assert!(is_newer_version("0.9.1", "0.9.2"));
        assert!(is_newer_version("0.9.1", "v0.9.2"));
        assert!(is_newer_version("0.9.1", "0.10.0"));
        assert!(is_newer_version("0.9.1", "1.0.0"));
        assert!(!is_newer_version("0.9.1", "0.9.1"));
        assert!(!is_newer_version("0.9.1", "v0.9.1"));
        assert!(!is_newer_version("0.9.1", "0.9.0"));
        assert!(!is_newer_version("1.0.0", "0.9.9"));
    }

    #[test]
    fn parse_github_release_json() {
        let json = r#"{"tag_name":"v0.9.2","name":"Glanvu v0.9.2"}"#;
        let parsed = parse_version_json(json);
        assert_eq!(
            parsed,
            Some((
                "0.9.2".to_string(),
                "https://github.com/glanvu/glanvu/releases/tag/v0.9.2".to_string()
            ))
        );
    }

    #[test]
    fn parse_version_json_file() {
        let json =
            r#"{"version":"0.9.2","url":"https://github.com/glanvu/glanvu/releases/tag/v0.9.2"}"#;
        let parsed = parse_version_json(json);
        assert_eq!(
            parsed,
            Some((
                "0.9.2".to_string(),
                "https://github.com/glanvu/glanvu/releases/tag/v0.9.2".to_string()
            ))
        );
    }
}
