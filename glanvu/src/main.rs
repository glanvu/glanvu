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
        Some("info") => match args.get(1) {
            Some(path) => run_info(path),
            None => {
                eprintln!("usage: glanvu info <FILE>");
                ExitCode::from(2)
            }
        },
        Some("convert") => batch::run(&args[1..]),
        Some("set-default") => associate::run(&args[1..]),
        Some(path) => viewer::run(path),
    }
}

/// Inspect a single image: detect format, read header dimensions, then fully decode it.
fn run_info(path: &str) -> ExitCode {
    let meta = match glanvu_core::read_meta_path(path) {
        Ok(meta) => meta,
        Err(e) => {
            eprintln!("glanvu: cannot read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };

    match glanvu_core::decode_path(path) {
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
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("glanvu: read the header but full decode failed for {path}: {e}");
            ExitCode::FAILURE
        }
    }
}

fn print_help() {
    println!(
        "glanvu {VERSION} - fast cross-platform universal image viewer & converter\n\
         \n\
         USAGE:\n\
         \x20   glanvu                   open file picker\n\
         \x20   glanvu <FILE>            open viewer (arrows navigate the folder)\n\
         \x20   glanvu info <FILE>       print an image's format, dimensions and size\n\
         \x20   glanvu convert ...       batch convert/resize (see: glanvu convert --help)\n\
         \x20   glanvu set-default ...   make Glanvu the default image app (see: --help)\n\
         \n\
         OPTIONS:\n\
         \x20   -V, --version            print version\n\
         \x20   -h, --help               print this help"
    );
}
