// SPDX-License-Identifier: Apache-2.0

//! The `glanvu convert` batch CLI: convert/resize many images in parallel, headless (no GPU).
//!
//! Usage: `glanvu convert --to <fmt> [--resize WxH] [--out DIR] <inputs...>`
//! Globs are expanded by the shell, so `<inputs...>` is just a list of file paths.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};

use rayon::prelude::*;

use glanvu_core::{ConvertOptions, Rotation, SourceFormat};

pub fn run(args: &[String]) -> ExitCode {
    let mut to: Option<String> = None;
    let mut resize: Option<(u32, u32)> = None;
    let mut crop: Option<(u32, u32, u32, u32)> = None;
    let mut rotate = Rotation::None;
    let mut quality: Option<u8> = None;
    let mut rename: Option<String> = None;
    let mut out: Option<PathBuf> = None;
    let mut inputs: Vec<PathBuf> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--to" | "-t" => {
                i += 1;
                to = args.get(i).cloned();
            }
            "--resize" | "-r" => {
                i += 1;
                match args.get(i).and_then(|s| parse_wxh(s)) {
                    Some(r) => resize = Some(r),
                    None => {
                        eprintln!("glanvu convert: --resize expects WxH (e.g. 1920x1080)");
                        return ExitCode::from(2);
                    }
                }
            }
            "--crop" | "-c" => {
                i += 1;
                match args.get(i).and_then(|s| parse_crop(s)) {
                    Some(c) => crop = Some(c),
                    None => {
                        eprintln!("glanvu convert: --crop expects X,Y,WxH (e.g. 0,0,800x600)");
                        return ExitCode::from(2);
                    }
                }
            }
            "--rotate" => {
                i += 1;
                match args.get(i).and_then(|s| parse_rotate(s)) {
                    Some(r) => rotate = r,
                    None => {
                        eprintln!("glanvu convert: --rotate expects 90, 180, 270 or -90");
                        return ExitCode::from(2);
                    }
                }
            }
            "--quality" | "-q" => {
                i += 1;
                match args.get(i).and_then(|s| s.parse::<u32>().ok()) {
                    Some(q) if (1..=100).contains(&q) => quality = Some(q as u8),
                    _ => {
                        eprintln!("glanvu convert: --quality expects a number 1-100");
                        return ExitCode::from(2);
                    }
                }
            }
            "--rename" => {
                i += 1;
                rename = args.get(i).cloned();
            }
            "--out" | "-o" => {
                i += 1;
                out = args.get(i).map(PathBuf::from);
            }
            "--help" | "-h" => {
                print_help();
                return ExitCode::SUCCESS;
            }
            s if s.starts_with('-') => {
                eprintln!("glanvu convert: unknown option '{s}' (try --help)");
                return ExitCode::from(2);
            }
            s => inputs.push(PathBuf::from(s)),
        }
        i += 1;
    }

    let Some(to) = to else {
        eprintln!("glanvu convert: --to <format> is required (jpg/png/gif/bmp/tiff/webp)");
        return ExitCode::from(2);
    };
    let Some(target) = SourceFormat::from_extension(&to) else {
        eprintln!("glanvu convert: unsupported target format '{to}' (jpg/png/gif/bmp/tiff/webp)");
        return ExitCode::from(2);
    };
    let ext = to.to_ascii_lowercase();

    if quality.is_some() && target != SourceFormat::Jpeg {
        eprintln!("glanvu convert: --quality only applies to JPEG output; ignoring it for {to}");
        quality = None;
    }

    if inputs.is_empty() {
        eprintln!("glanvu convert: no input files (try --help)");
        return ExitCode::from(2);
    }
    if let Some(dir) = &out {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!(
                "glanvu convert: cannot create output dir {}: {e}",
                dir.display()
            );
            return ExitCode::FAILURE;
        }
    }

    // Plan each output path up front, and refuse if two inputs map to the same output (that would
    // silently overwrite one with the other).
    let planned: Vec<(PathBuf, PathBuf)> = inputs
        .iter()
        .enumerate()
        .map(|(idx, input)| {
            let out_dir = out
                .clone()
                .or_else(|| input.parent().map(Path::to_path_buf))
                .unwrap_or_else(|| PathBuf::from("."));
            let stem = input
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "out".to_string());
            let name = match &rename {
                Some(pattern) => render_pattern(pattern, &stem, idx + 1),
                None => stem,
            };
            (input.clone(), out_dir.join(format!("{name}.{ext}")))
        })
        .collect();

    let mut by_output: std::collections::HashMap<&PathBuf, usize> =
        std::collections::HashMap::new();
    for (_, output) in &planned {
        *by_output.entry(output).or_insert(0) += 1;
    }
    let mut collisions: Vec<&PathBuf> = by_output
        .iter()
        .filter(|(_, n)| **n > 1)
        .map(|(o, _)| *o)
        .collect();
    if !collisions.is_empty() {
        collisions.sort();
        eprintln!("glanvu convert: several inputs would write to the same output (refusing, to avoid data loss):");
        for o in collisions {
            eprintln!("  {}", o.display());
        }
        eprintln!("hint: convert them in separate runs, or to different --out folders.");
        return ExitCode::from(2);
    }

    let opts = ConvertOptions {
        crop,
        rotate,
        resize,
        quality,
    };

    let ok = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);

    planned.par_iter().for_each(|(input, output)| {
        if output == input {
            eprintln!(
                "skip {} (output would overwrite the input; pass --out DIR)",
                input.display()
            );
            failed.fetch_add(1, Ordering::Relaxed);
            return;
        }

        match glanvu_core::convert_file(input, output, target, &opts) {
            Ok(()) => {
                println!("{} -> {}", input.display(), output.display());
                ok.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                eprintln!("error converting {}: {e}", input.display());
                failed.fetch_add(1, Ordering::Relaxed);
            }
        }
    });

    let (ok, failed) = (ok.load(Ordering::Relaxed), failed.load(Ordering::Relaxed));
    println!("converted {ok}, failed {failed}");
    if failed > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn parse_wxh(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once(['x', 'X'])?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

/// Parse a crop spec `X,Y,WxH` into `(x, y, w, h)`.
fn parse_crop(s: &str) -> Option<(u32, u32, u32, u32)> {
    let mut parts = s.split(',');
    let x = parts.next()?.trim().parse().ok()?;
    let y = parts.next()?.trim().parse().ok()?;
    let (w, h) = parse_wxh(parts.next()?)?;
    if parts.next().is_some() {
        return None; // too many comma-separated fields
    }
    Some((x, y, w, h))
}

/// Parse a rotation in degrees. Accepts clockwise and negative (counter-clockwise) forms.
fn parse_rotate(s: &str) -> Option<Rotation> {
    match s.trim() {
        "0" => Some(Rotation::None),
        "90" | "-270" => Some(Rotation::Cw90),
        "180" | "-180" => Some(Rotation::Cw180),
        "270" | "-90" => Some(Rotation::Cw270),
        _ => None,
    }
}

/// Render an output filename stem (extension is appended by the caller) from a `--rename` pattern.
/// Tokens: `{stem}` (input stem), `{n}` (1-based index), `{n:0N}` (index zero-padded to N digits).
/// Unknown tokens are left literal.
fn render_pattern(pattern: &str, stem: &str, n: usize) -> String {
    let mut out = String::with_capacity(pattern.len() + 8);
    let mut rest = pattern;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            // No closing brace: copy the rest verbatim.
            out.push_str(&rest[open..]);
            return out;
        };
        let token = &after[..close];
        match token {
            "stem" => out.push_str(stem),
            "n" => out.push_str(&n.to_string()),
            t if t.starts_with("n:0") => match t[3..].parse::<usize>() {
                Ok(width) => out.push_str(&format!("{n:0width$}")),
                Err(_) => out.push_str(&format!("{{{token}}}")),
            },
            other => out.push_str(&format!("{{{other}}}")), // unknown: keep literal
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

fn print_help() {
    println!(
        "glanvu convert - batch image conversion (headless, parallel)\n\
         \n\
         USAGE:\n\
         \x20   glanvu convert --to <FMT> [OPTIONS] <FILES...>\n\
         \n\
         OPTIONS:\n\
         \x20   -t, --to <FMT>       target format: jpg, png, gif, bmp, tiff, webp (required)\n\
         \x20   -c, --crop X,Y,WxH   crop a region before other steps (e.g. 0,0,800x600)\n\
         \x20       --rotate <DEG>   rotate 90, 180, 270 or -90 (clockwise)\n\
         \x20   -r, --resize <WxH>   fit within WxH, preserving aspect ratio (e.g. 1920x1080)\n\
         \x20   -q, --quality <N>    JPEG quality 1-100 (JPEG output only)\n\
         \x20       --rename <PAT>   output name pattern: {{stem}}, {{n}}, {{n:04}} (ext added)\n\
         \x20   -o, --out <DIR>      output directory (default: next to each input)\n\
         \x20   -h, --help           print this help\n\
         \n\
         Pipeline order: crop -> rotate -> resize -> encode.\n\
         \n\
         EXAMPLES:\n\
         \x20   glanvu convert --to webp --resize 2000x2000 --out out/ photos/*.jpg\n\
         \x20   glanvu convert --to jpg --quality 85 --out out/ photos/*.png\n\
         \x20   glanvu convert --to png --crop 100,100,512x512 --rotate 90 shot.png\n\
         \x20   glanvu convert --to webp --rename \"holiday_{{n:03}}\" --out out/ *.jpg"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_crop_valid_and_invalid() {
        assert_eq!(parse_crop("0,0,800x600"), Some((0, 0, 800, 600)));
        assert_eq!(parse_crop("10, 20, 30X40"), Some((10, 20, 30, 40)));
        assert_eq!(parse_crop("0,0"), None);
        assert_eq!(parse_crop("0,0,800x600,extra"), None);
        assert_eq!(parse_crop("a,b,cxd"), None);
    }

    #[test]
    fn parse_rotate_forms() {
        assert_eq!(parse_rotate("90"), Some(Rotation::Cw90));
        assert_eq!(parse_rotate("-90"), Some(Rotation::Cw270));
        assert_eq!(parse_rotate("180"), Some(Rotation::Cw180));
        assert_eq!(parse_rotate("-270"), Some(Rotation::Cw90));
        assert_eq!(parse_rotate("45"), None);
    }

    #[test]
    fn render_pattern_tokens() {
        assert_eq!(render_pattern("{stem}", "photo", 1), "photo");
        assert_eq!(render_pattern("shot_{n}", "x", 7), "shot_7");
        assert_eq!(render_pattern("shot_{n:03}", "x", 7), "shot_007");
        assert_eq!(render_pattern("{stem}_{n:04}", "img", 42), "img_0042");
        // Unknown token kept literal.
        assert_eq!(render_pattern("a{bogus}b", "s", 1), "a{bogus}b");
    }
}
