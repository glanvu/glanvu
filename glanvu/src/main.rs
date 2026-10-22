// SPDX-License-Identifier: Apache-2.0

// On Windows, release builds use the GUI subsystem so double-clicking the .exe (or
// "Open with") does not spawn a console window. CLI subcommands reattach to the
// parent terminal at startup (see `windows_console`), so `glanvu convert …` from a
// shell still prints normally. Debug builds keep the console subsystem for dev ergonomics.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

//! Glanvu entry point.
//!
//! Dispatch table:
//!   (no args)          → native file-open dialog, then viewer
//!   <FILE>             → open viewer with that file
//!   info <FILE>        → print format/dims
//!   convert …          → headless batch convert/resize

mod associate;
mod batch;
mod updater;
mod viewer;

#[cfg(target_os = "macos")]
mod macos_open;

use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// On Windows the binary is a GUI-subsystem app (no console allocated). When it is
/// launched from a terminal for a CLI subcommand, attach to the parent process's
/// console so stdout/stderr land there. Launched by double-click there is no parent
/// console and the call fails silently — which is exactly what we want (no window).
#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
fn windows_console() {
    use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    // SAFETY: FFI call with a documented constant; a failed attach is non-fatal.
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

fn main() -> ExitCode {
    #[cfg(target_os = "windows")]
    windows_console();

    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(String::as_str) {
        Some("--version" | "-V") => {
            println!("glanvu {VERSION} (core {})", glanvu_core::VERSION);
            ExitCode::SUCCESS
        }
        Some("--help" | "-h") => {
            print_help();
            ExitCode::SUCCESS
        }
        // No arguments: launched from .app, Dock, or Spotlight.
        // Start the event loop in empty mode — macOS will send files opened via "Open With"
        // as DroppedFile events once the window exists. The user can also press Enter to pick.
        None => viewer::run_empty(),
        Some("info") => run_info(&args[1..]),
        Some("convert") => batch::run(&args[1..]),
        Some("set-default") => associate::run(&args[1..]),
        Some(path) => viewer::run(path),
    }
}

/// Inspect a single image: detect format, read dimensions, size, and EXIF metadata.
fn run_info(args: &[String]) -> ExitCode {
    let mut json = false;
    let mut exif_only = false;
    let mut target_path: Option<&str> = None;

    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            "--exif" => exif_only = true,
            "-h" | "--help" => {
                println!(
                    "glanvu info - print format, dimensions, size, and EXIF metadata\n\n\
                     USAGE:\n    glanvu info [OPTIONS] <FILE>\n\n\
                     OPTIONS:\n    --exif       print only EXIF metadata\n    --json       output in JSON format\n    -h, --help   print help"
                );
                return ExitCode::SUCCESS;
            }
            s if s.starts_with('-') => {
                eprintln!("glanvu info: unrecognized option '{s}' (see: glanvu info --help)");
                return ExitCode::from(2);
            }
            path => {
                if target_path.is_some() {
                    eprintln!("glanvu info: only one file can be inspected at a time");
                    return ExitCode::from(2);
                }
                target_path = Some(path);
            }
        }
    }

    let Some(path) = target_path else {
        eprintln!("usage: glanvu info [--exif] [--json] <FILE>");
        return ExitCode::from(2);
    };

    let meta = match glanvu_core::read_meta_path(path) {
        Ok(meta) => meta,
        Err(e) => {
            eprintln!("glanvu: cannot read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let exif = glanvu_core::read_exif_path(path);

    if exif_only {
        let Some(exif) = exif.filter(|e| !e.is_empty()) else {
            eprintln!("glanvu: no EXIF metadata found in {path}");
            return ExitCode::FAILURE;
        };
        if json {
            print_exif_json(&exif);
        } else {
            println!("{path}");
            for line in exif.display_lines_with_defaults(true) {
                println!("  {line}");
            }
        }
        return ExitCode::SUCCESS;
    }

    let decoded = glanvu_core::decode_path(path);

    if json {
        let (rgba_len, d_w, d_h, err_msg) = match &decoded {
            Ok(img) => (
                Some(img.rgba.len()),
                Some(img.width),
                Some(img.height),
                None,
            ),
            Err(e) => (None, None, None, Some(e.to_string())),
        };
        print_full_json(
            path,
            &meta,
            rgba_len,
            d_w,
            d_h,
            exif.as_ref(),
            err_msg.as_deref(),
        );
        return match decoded {
            Ok(_) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("glanvu: read the header but full decode failed for {path}: {e}");
                ExitCode::FAILURE
            }
        };
    }

    match decoded {
        Ok(img) => {
            println!("{path}");
            println!("  format:     {}", meta.format.name());
            println!("  dimensions: {} x {} px", meta.width, meta.height);
            println!("  file size:  {} bytes", meta.file_size);
            println!(
                "  decoded:    {} RGBA bytes ({} x {} x 4)",
                img.rgba.len(),
                img.width,
                img.height
            );
            if let Some(exif) = exif.filter(|e| !e.is_empty()) {
                println!("  exif:");
                for line in exif.display_lines() {
                    println!("    {line}");
                }
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("glanvu: read the header but full decode failed for {path}: {e}");
            ExitCode::FAILURE
        }
    }
}

fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn opt_json_str(opt: &Option<String>) -> String {
    match opt {
        Some(s) => escape_json(s),
        None => "null".to_string(),
    }
}

fn print_exif_json(exif: &glanvu_core::ExifMeta) {
    let cam = opt_json_str(&exif.camera_name());
    let make = opt_json_str(&exif.make);
    let model = opt_json_str(&exif.model);
    let lens = opt_json_str(&exif.lens);
    let exposure_time = opt_json_str(&exif.exposure_time);
    let f_number = opt_json_str(&exif.f_number);
    let iso = opt_json_str(&exif.iso);
    let focal_length = opt_json_str(&exif.focal_length);
    let focal_length_35mm = opt_json_str(&exif.focal_length_35mm);
    let exposure_bias = opt_json_str(&exif.exposure_bias);
    let date_time = opt_json_str(&exif.date_time_original);
    let flash = opt_json_str(&exif.flash);
    let gps = opt_json_str(&exif.gps_coords);
    let orientation = exif
        .orientation
        .map(|o| o.to_string())
        .unwrap_or_else(|| "null".to_string());
    let orientation_desc = match exif.orientation_description() {
        Some(desc) => escape_json(desc),
        None => "null".to_string(),
    };

    println!(
        "{{\n  \
           \"camera\": {cam},\n  \
           \"make\": {make},\n  \
           \"model\": {model},\n  \
           \"lens\": {lens},\n  \
           \"exposure_time\": {exposure_time},\n  \
           \"f_number\": {f_number},\n  \
           \"iso\": {iso},\n  \
           \"focal_length\": {focal_length},\n  \
           \"focal_length_35mm\": {focal_length_35mm},\n  \
           \"exposure_bias\": {exposure_bias},\n  \
           \"date_time_original\": {date_time},\n  \
           \"flash\": {flash},\n  \
           \"gps_coords\": {gps},\n  \
           \"orientation\": {orientation},\n  \
           \"orientation_description\": {orientation_desc}\n\
         }}"
    );
}

fn print_full_json(
    path: &str,
    meta: &glanvu_core::ImageMeta,
    decoded_bytes: Option<usize>,
    decoded_w: Option<u32>,
    decoded_h: Option<u32>,
    exif: Option<&glanvu_core::ExifMeta>,
    error: Option<&str>,
) {
    let p = escape_json(path);
    let fmt = escape_json(meta.format.name());
    let d_bytes = decoded_bytes
        .map(|b| b.to_string())
        .unwrap_or_else(|| "null".to_string());
    let d_w = decoded_w
        .map(|w| w.to_string())
        .unwrap_or_else(|| "null".to_string());
    let d_h = decoded_h
        .map(|h| h.to_string())
        .unwrap_or_else(|| "null".to_string());
    let err_json = match error {
        Some(e) => escape_json(e),
        None => "null".to_string(),
    };

    let exif_json = match exif.filter(|e| !e.is_empty()) {
        None => "null".to_string(),
        Some(e) => {
            let cam = opt_json_str(&e.camera_name());
            let make = opt_json_str(&e.make);
            let model = opt_json_str(&e.model);
            let lens = opt_json_str(&e.lens);
            let exposure_time = opt_json_str(&e.exposure_time);
            let f_number = opt_json_str(&e.f_number);
            let iso = opt_json_str(&e.iso);
            let focal_length = opt_json_str(&e.focal_length);
            let focal_length_35mm = opt_json_str(&e.focal_length_35mm);
            let exposure_bias = opt_json_str(&e.exposure_bias);
            let date_time = opt_json_str(&e.date_time_original);
            let flash = opt_json_str(&e.flash);
            let gps = opt_json_str(&e.gps_coords);
            let orientation = e
                .orientation
                .map(|o| o.to_string())
                .unwrap_or_else(|| "null".to_string());
            let orientation_desc = match e.orientation_description() {
                Some(desc) => escape_json(desc),
                None => "null".to_string(),
            };
            format!(
                "{{\n    \
                   \"camera\": {cam},\n    \
                   \"make\": {make},\n    \
                   \"model\": {model},\n    \
                   \"lens\": {lens},\n    \
                   \"exposure_time\": {exposure_time},\n    \
                   \"f_number\": {f_number},\n    \
                   \"iso\": {iso},\n    \
                   \"focal_length\": {focal_length},\n    \
                   \"focal_length_35mm\": {focal_length_35mm},\n    \
                   \"exposure_bias\": {exposure_bias},\n    \
                   \"date_time_original\": {date_time},\n    \
                   \"flash\": {flash},\n    \
                   \"gps_coords\": {gps},\n    \
                   \"orientation\": {orientation},\n    \
                   \"orientation_description\": {orientation_desc}\n  \
                 }}"
            )
        }
    };

    println!(
        "{{\n  \
           \"path\": {p},\n  \
           \"format\": {fmt},\n  \
           \"width\": {},\n  \
           \"height\": {},\n  \
           \"file_size\": {},\n  \
           \"decoded_bytes\": {d_bytes},\n  \
           \"decoded_width\": {d_w},\n  \
           \"decoded_height\": {d_h},\n  \
           \"error\": {err_json},\n  \
           \"exif\": {exif_json}\n\
         }}",
        meta.width, meta.height, meta.file_size
    );
}

fn print_help() {
    println!(
        "glanvu {VERSION} - fast cross-platform universal image viewer & converter\n\
         \n\
         USAGE:\n\
         \x20   glanvu                            open file picker\n\
         \x20   glanvu <FILE>                     open viewer (arrows navigate the folder)\n\
         \x20   glanvu info [--exif] [--json] ... print an image's format, dimensions and EXIF\n\
         \x20   glanvu convert ...                batch convert/resize (see: glanvu convert --help)\n\
         \x20   glanvu set-default ...            make Glanvu the default image app (see: --help)\n\
         \n\
         OPTIONS:\n\
         \x20   -V, --version                     print version\n\
         \x20   -h, --help                        print this help"
    );
}
