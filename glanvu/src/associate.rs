// SPDX-License-Identifier: Apache-2.0

//! `glanvu set-default` — register Glanvu as the default application for image types.
//!
//! macOS uses Launch Services through `NSWorkspace` (macOS 12+, non-deprecated API).
//! The OS owns the confirmation flow: it shows a permission dialog per UTType and
//! manages state. We fire the requests and return immediately — no backup, no polling.
//!
//! Windows registers the app in `HKCU\Software\Classes` so it appears under "Open with"
//! and as a selectable default, then opens the Settings → Default apps page. Windows
//! protects the real default (UserChoice) behind a per-user hash, so — like macOS — the
//! OS owns the final confirmation; we register and hand off to the system UI.
//!
//! Linux is stubbed until its installer lands.

use std::process::ExitCode;
use std::sync::Mutex;

/// Image extensions Glanvu can decode. Keep in sync with `CFBundleTypeExtensions` in
/// `scripts/build-macos-app.sh` and the `MimeType` line in `scripts/build-linux-pkg.sh`'s
/// `.desktop` entry.
pub const SUPPORTED_EXTS: &[&str] =
    &["jpg", "jpeg", "png", "gif", "bmp", "tif", "tiff", "webp", "svg", "pdf"];

/// Result of a background set/unset operation, posted here for the viewer to pick up.
pub static ASSOC_RESULT: Mutex<Option<String>> = Mutex::new(None);

enum Mode {
    Set,
    Unset,
    List,
    Reset,
}

/// Entry point for `glanvu set-default [EXT...] [--list] [--unset] [--reset]`.
pub fn run(args: &[String]) -> ExitCode {
    let mut mode = Mode::Set;
    let mut exts: Vec<String> = Vec::new();

    for arg in args {
        match arg.as_str() {
            "--list" => mode = Mode::List,
            "--unset" => mode = Mode::Unset,
            "--reset" => mode = Mode::Reset,
            "--help" | "-h" => {
                print_help();
                return ExitCode::SUCCESS;
            }
            other if other.starts_with('-') => {
                eprintln!("glanvu set-default: unknown option '{other}'");
                return ExitCode::from(2);
            }
            other => {
                let norm = other.trim_start_matches('.').to_ascii_lowercase();
                if !SUPPORTED_EXTS.contains(&norm.as_str()) {
                    eprintln!(
                        "glanvu set-default: unsupported extension '{other}'.\n\
                         Supported: {}",
                        SUPPORTED_EXTS.join(", ")
                    );
                    return ExitCode::from(2);
                }
                exts.push(norm);
            }
        }
    }

    if exts.is_empty() {
        exts = SUPPORTED_EXTS.iter().map(|s| s.to_string()).collect();
    } else {
        exts.dedup();
    }

    match mode {
        Mode::Set => {
            let msg = set_default_blocking();
            println!("{msg}");
            if msg.starts_with("Glanvu.app") || msg.starts_with("Could") {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Mode::Unset => {
            let msg = unset_default_blocking();
            println!("{msg}");
            ExitCode::SUCCESS
        }
        _ => platform::run(mode, &exts),
    }
}

/// Fire set-default requests for all supported types. The OS shows permission
/// dialogs; returns immediately without waiting for confirmation.
pub fn set_default_blocking() -> String {
    platform::do_set()
}

/// Fire set-default-to-Preview requests for all supported types. The OS shows
/// permission dialogs; returns immediately without waiting for confirmation.
pub fn unset_default_blocking() -> String {
    platform::do_unset()
}

fn print_help() {
    println!(
        "glanvu set-default - make Glanvu the default app for image files\n\
         \n\
         USAGE:\n\
         \x20   glanvu set-default                 set Glanvu as default for all supported types\n\
         \x20   glanvu set-default jpg png webp    set only the listed extensions\n\
         \x20   glanvu set-default --list          show the current default app per type\n\
         \x20   glanvu set-default --unset         restore to Apple Preview\n\
         \x20   glanvu set-default --reset         same as --unset\n\
         \n\
         Supported extensions: {}\n\
         \n\
         macOS uses Launch Services; Windows registers the app and opens Settings.\n\
         You can also press D / U inside the viewer to set / restore interactively.",
        SUPPORTED_EXTS.join(", ")
    );
}

// ── macOS implementation ────────────────────────────────────────────────────

// The entire module is Objective-C runtime FFI (NSWorkspace / UTType via msg_send!),
// so the workspace `unsafe_code = deny` lint is lifted here.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod platform {
    use super::{ExitCode, Mode, SUPPORTED_EXTS};
    use std::ffi::{c_char, CStr, CString};

    use objc2::class;
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    const BUNDLE_ID: &str = "com.glanvu.app";
    const PREVIEW_ID: &str = "com.apple.Preview";

    // UTType lives in UniformTypeIdentifiers, which neither winit nor wgpu link.
    #[link(name = "UniformTypeIdentifiers", kind = "framework")]
    extern "C" {}

    type Id = *mut AnyObject;

    unsafe fn nsstring(s: &str) -> Id {
        let c = CString::new(s).unwrap_or_default();
        let cls = class!(NSString);
        msg_send![cls, stringWithUTF8String: c.as_ptr()]
    }

    unsafe fn read_nsstring(s: Id) -> Option<String> {
        if s.is_null() {
            return None;
        }
        let utf8: *const c_char = msg_send![s, UTF8String];
        if utf8.is_null() {
            return None;
        }
        CStr::from_ptr(utf8).to_str().ok().map(str::to_owned)
    }

    unsafe fn shared_workspace() -> Id {
        let cls = class!(NSWorkspace);
        msg_send![cls, sharedWorkspace]
    }

    unsafe fn url_for_bundle_id(ws: Id, bundle_id: &str) -> Id {
        let bid = nsstring(bundle_id);
        msg_send![ws, URLForApplicationWithBundleIdentifier: bid]
    }

    unsafe fn url_path(url: Id) -> Option<String> {
        if url.is_null() {
            return None;
        }
        let p: Id = msg_send![url, path];
        read_nsstring(p)
    }

    unsafe fn uttype_for_ext(ext: &str) -> Id {
        let cls = class!(UTType);
        let e = nsstring(ext);
        msg_send![cls, typeWithFilenameExtension: e]
    }

    unsafe fn current_handler_path(ws: Id, uttype: Id) -> Option<String> {
        let url: Id = msg_send![ws, URLForApplicationToOpenContentType: uttype];
        url_path(url)
    }

    /// Fire a set-default request. macOS shows a permission dialog if needed;
    /// the OS owns the confirmation flow — we do not wait for the result.
    unsafe fn request(ws: Id, app_url: Id, utt: Id) {
        let _: () = msg_send![
            ws,
            setDefaultApplicationAtURL: app_url,
            toOpenContentType: utt,
            completionHandler: std::ptr::null_mut::<AnyObject>(),
        ];
    }

    // ── Public functions ─────────────────────────────────────────────────────

    pub fn do_set() -> String {
        unsafe {
            let ws = shared_workspace();
            let app_url = url_for_bundle_id(ws, BUNDLE_ID);
            if app_url.is_null() {
                return "Glanvu.app is not installed. Run: make install-app".to_string();
            }
            let app_path = match url_path(app_url) {
                Some(p) => p,
                None => return "Could not resolve Glanvu.app path.".to_string(),
            };
            let mut sent = 0usize;
            let mut already = 0usize;
            for ext in SUPPORTED_EXTS {
                let utt = uttype_for_ext(ext);
                if utt.is_null() {
                    continue;
                }
                if current_handler_path(ws, utt).as_deref() == Some(app_path.as_str()) {
                    already += 1;
                } else {
                    request(ws, app_url, utt);
                    sent += 1;
                }
            }
            if sent == 0 {
                format!("Already the default for all {} types.", already)
            } else if already == 0 {
                format!("Approve the system dialogs ({sent} types).")
            } else {
                format!("Approve the system dialogs ({sent} types; {already} already set).")
            }
        }
    }

    pub fn do_unset() -> String {
        unsafe {
            let ws = shared_workspace();
            let preview_url = url_for_bundle_id(ws, PREVIEW_ID);
            if preview_url.is_null() {
                return "Preview.app not found on this system.".to_string();
            }
            for ext in SUPPORTED_EXTS {
                let utt = uttype_for_ext(ext);
                if utt.is_null() {
                    continue;
                }
                request(ws, preview_url, utt);
            }
            format!(
                "Approve the system dialogs to restore Preview ({} types).",
                SUPPORTED_EXTS.len()
            )
        }
    }

    fn base_name(path: &str) -> &str {
        path.rsplit('/').next().unwrap_or(path)
    }

    pub fn run(mode: Mode, exts: &[String]) -> ExitCode {
        unsafe {
            let ws = shared_workspace();
            match mode {
                Mode::List => {
                    println!("Default application per image type:");
                    for ext in exts {
                        let utt = uttype_for_ext(ext);
                        if utt.is_null() {
                            println!("  {ext:<6} (no type registered on this system)");
                            continue;
                        }
                        let name = current_handler_path(ws, utt)
                            .map(|p| base_name(&p).to_string())
                            .unwrap_or_else(|| "(none)".to_string());
                        println!("  {ext:<6} → {name}");
                    }
                    ExitCode::SUCCESS
                }

                Mode::Reset => {
                    let preview_url = url_for_bundle_id(ws, PREVIEW_ID);
                    if preview_url.is_null() {
                        eprintln!("Could not find Apple Preview on this system.");
                        return ExitCode::FAILURE;
                    }
                    for ext in exts {
                        let utt = uttype_for_ext(ext);
                        if !utt.is_null() {
                            request(ws, preview_url, utt);
                        }
                    }
                    println!("Sent reset requests for {} types.", exts.len());
                    ExitCode::SUCCESS
                }

                Mode::Set | Mode::Unset => ExitCode::SUCCESS,
            }
        }
    }
}

// ── Windows implementation ────────────────────────────────────────────────────

// Registers Glanvu under HKCU\Software\Classes so it shows up in "Open with" and as a
// selectable default, then opens Settings → Default apps. Windows guards the actual
// default (UserChoice) with a per-user hash, so the user confirms in the system UI.
#[cfg(target_os = "windows")]
mod platform {
    use super::{ExitCode, Mode, SUPPORTED_EXTS};
    use std::io;

    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
    use winreg::RegKey;

    const PROGID: &str = "Glanvu.Image";

    fn exe_path() -> io::Result<String> {
        Ok(std::env::current_exe()?.to_string_lossy().into_owned())
    }

    /// Write the registry entries that make Glanvu visible to the shell:
    ///   1. A ProgID (`Glanvu.Image`) with an open command + icon.
    ///   2. `Applications\glanvu.exe` + `SupportedTypes` → appears in the "Open with" list.
    ///   3. `.<ext>\OpenWithProgids` → Glanvu offered for each supported extension.
    fn register(exe: &str) -> io::Result<()> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let classes =
            hkcu.open_subkey_with_flags(r"Software\Classes", KEY_READ | KEY_WRITE)?;

        // 1) ProgID with open command + icon.
        let (progid, _) = classes.create_subkey(PROGID)?;
        progid.set_value("", &"Glanvu Image")?;
        let (icon, _) = progid.create_subkey("DefaultIcon")?;
        icon.set_value("", &format!("{exe},0"))?;
        let (cmd, _) = progid.create_subkey(r"shell\open\command")?;
        cmd.set_value("", &format!("\"{exe}\" \"%1\""))?;

        // 2) Applications\glanvu.exe — surfaces the app in the "Open with" picker.
        let (app, _) = classes.create_subkey(r"Applications\glanvu.exe")?;
        let (app_cmd, _) = app.create_subkey(r"shell\open\command")?;
        app_cmd.set_value("", &format!("\"{exe}\" \"%1\""))?;
        let (supported, _) = app.create_subkey("SupportedTypes")?;
        for ext in SUPPORTED_EXTS {
            supported.set_value(format!(".{ext}"), &"")?;
        }

        // 3) Per-extension OpenWithProgids so Glanvu is offered for each type.
        for ext in SUPPORTED_EXTS {
            let (owp, _) = classes.create_subkey(format!(r".{ext}\OpenWithProgids"))?;
            owp.set_value(PROGID, &"")?;
        }
        Ok(())
    }

    /// Remove Glanvu's per-extension OpenWithProgids entries. Windows then falls back
    /// to the remaining handlers. The app stays in the "Open with" list (Applications key).
    fn unregister_defaults() {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        if let Ok(classes) =
            hkcu.open_subkey_with_flags(r"Software\Classes", KEY_READ | KEY_WRITE)
        {
            for ext in SUPPORTED_EXTS {
                if let Ok(owp) = classes
                    .open_subkey_with_flags(format!(r".{ext}\OpenWithProgids"), KEY_WRITE)
                {
                    let _ = owp.delete_value(PROGID);
                }
            }
        }
    }

    /// Open Settings → Default apps. Windows blocks programmatic default changes
    /// (UserChoice hash), so the user confirms there — mirrors the macOS permission flow.
    fn open_default_apps_settings() {
        let _ = open::that("ms-settings:defaultapps");
    }

    pub fn do_set() -> String {
        let exe = match exe_path() {
            Ok(e) => e,
            Err(e) => return format!("Could not resolve glanvu.exe path: {e}"),
        };
        if let Err(e) = register(&exe) {
            return format!("Could not register Glanvu in the registry: {e}");
        }
        open_default_apps_settings();
        "Glanvu registered. In the Settings window that opened, choose Glanvu as the \
         default for the image types you want."
            .to_string()
    }

    pub fn do_unset() -> String {
        unregister_defaults();
        open_default_apps_settings();
        "Removed Glanvu from the default-handler list. Pick another app in the Settings \
         window if needed."
            .to_string()
    }

    pub fn run(mode: Mode, exts: &[String]) -> ExitCode {
        match mode {
            Mode::List => {
                let hkcu = RegKey::predef(HKEY_CURRENT_USER);
                println!("Current default ProgID per image type (Windows owns the real default):");
                for ext in exts {
                    let choice = hkcu
                        .open_subkey(format!(
                            r"Software\Classes\.{ext}\UserChoice"
                        ))
                        .ok()
                        .and_then(|k| k.get_value::<String, _>("ProgId").ok())
                        .unwrap_or_else(|| "(unset)".to_string());
                    println!("  {ext:<6} → {choice}");
                }
                ExitCode::SUCCESS
            }
            Mode::Reset => {
                println!("{}", do_unset());
                ExitCode::SUCCESS
            }
            // Set/Unset are handled by the outer run() via do_set/do_unset.
            Mode::Set | Mode::Unset => ExitCode::SUCCESS,
        }
    }
}

// ── Linux stub ────────────────────────────────────────────────────────────────

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
mod platform {
    use super::{ExitCode, Mode};

    const MSG: &str = "glanvu set-default: not yet supported on this platform.\n\
         Default-app association is implemented for macOS and Windows.\n\
         Linux (xdg-mime) support will land with its installer.";

    pub fn do_set() -> String {
        MSG.to_string()
    }
    pub fn do_unset() -> String {
        MSG.to_string()
    }
    pub fn run(_mode: Mode, _exts: &[String]) -> ExitCode {
        eprintln!("{MSG}");
        ExitCode::FAILURE
    }
}
