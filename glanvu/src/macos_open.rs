// SPDX-License-Identifier: Apache-2.0

//! macOS-specific: patch winit's `WinitApplicationDelegate` to handle
//! `applicationOpenFile:` (the Apple Event that Finder sends when the user
//! does "Open With" on a file while the app is not running, or sends a file
//! to an already-running app).
//!
//! Winit 0.30 does not implement this delegate method, so macOS would show
//! "Glanvu cannot open files in the 'Image' format." and return an error.
//! By adding the method at runtime via the Objective-C runtime, we:
//!   1. Return YES so macOS doesn't show the error dialog.
//!   2. Queue the file path in PENDING_OPEN_PATHS so the viewer processes it
//!      the next time `about_to_wait` runs.

use std::path::PathBuf;
use std::sync::Mutex;

use objc2::ffi::{class_addMethod, objc_getClass};
use objc2::runtime::Bool;
use objc2::sel;

/// Paths queued by `applicationOpenFile:` waiting to be processed by the viewer.
pub static PENDING_OPEN_PATHS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Patch `WinitApplicationDelegate` to implement `applicationOpenFile:`.
///
/// Must be called BEFORE the winit event loop starts (before `EventLoop::new()`).
///
/// # Safety
/// Uses the Objective-C runtime directly. The class and method types must match
/// exactly; a mismatch would be UB. The encoding `B@:@@` means:
///   B = BOOL return, @ = id self, : = SEL, @ = NSApplication*, @ = NSString* filename
#[allow(unsafe_code)]
pub fn install() {
    use std::ffi::CStr;

    // The function we inject as `applicationOpenFile:`.
    unsafe extern "C" fn application_open_file(
        _self: *mut objc2::runtime::AnyObject,
        _cmd: objc2::runtime::Sel,
        _app: *mut objc2::runtime::AnyObject,
        filename: *mut objc2::runtime::AnyObject,
    ) -> Bool {
        // filename is an NSString*.  Convert to Rust String via CFString bridge.
        // We use the objc2 raw msg_send approach to call -[NSString UTF8String].
        let utf8: *const std::ffi::c_char = unsafe { objc2::msg_send![filename, UTF8String] };
        if !utf8.is_null() {
            if let Ok(s) = unsafe { CStr::from_ptr(utf8) }.to_str() {
                let path = PathBuf::from(s);
                if let Ok(mut pending) = PENDING_OPEN_PATHS.lock() {
                    pending.push(path);
                }
            }
        }
        Bool::YES
    }

    let debug = std::env::var_os("GLANVU_PERF").is_some();
    unsafe {
        let cls = objc_getClass(c"WinitApplicationDelegate".as_ptr() as *const std::ffi::c_char);
        if cls.is_null() {
            if debug {
                eprintln!("glanvu: WinitApplicationDelegate not found — file-open patch skipped");
            }
            return;
        }
        if debug {
            eprintln!("glanvu: WinitApplicationDelegate found, adding application:openFile:");
        }
        // Encoding: BOOL return, id self, SEL, NSApplication*, NSString*
        let types = c"B@:@@";
        // Imp = unsafe extern "C-unwind" fn() (objc2 runtime type alias)
        let imp: objc2::runtime::Imp = std::mem::transmute::<
            unsafe extern "C" fn(
                *mut objc2::runtime::AnyObject,
                objc2::runtime::Sel,
                *mut objc2::runtime::AnyObject,
                *mut objc2::runtime::AnyObject,
            ) -> Bool,
            objc2::runtime::Imp,
        >(application_open_file);
        let added = class_addMethod(
            cls as *mut _,
            sel!(application:openFile:),
            imp,
            types.as_ptr() as *const std::ffi::c_char,
        );
        if debug {
            eprintln!("glanvu: application:openFile: added = {}", added.as_bool());
        }
    }
}
