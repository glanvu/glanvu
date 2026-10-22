// SPDX-License-Identifier: Apache-2.0

//! Lightweight EXIF metadata extraction for raster images (JPEG, TIFF, WebP, PNG).
//!
//! Uses pure-Rust `kamadak-exif` (zero unsafe, no system C libraries) to parse camera and
//! shooting parameters from file containers without fully decoding pixel data.

use std::fs::File;
use std::io::{BufReader, Cursor};
use std::path::Path;

use exif::{In, Reader, Tag, Value};

/// Extracted photographic and camera metadata from an image's EXIF header.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExifMeta {
    /// Camera manufacturer (e.g. "Sony", "Apple", "Nikon", "Canon").
    pub make: Option<String>,
    /// Camera model (e.g. "ILCE-7M4", "iPhone 15 Pro", "Z 6_2").
    pub model: Option<String>,
    /// Lens model (e.g. "FE 24-70mm F2.8 GM II", "iPhone 15 Pro back camera 6.78mm f/1.78").
    pub lens: Option<String>,
    /// Exposure time / shutter speed (e.g. "1/500 s", "2.5 s").
    pub exposure_time: Option<String>,
    /// F-number / aperture (e.g. "f/2.8").
    pub f_number: Option<String>,
    /// ISO sensitivity rating (e.g. "ISO 200").
    pub iso: Option<String>,
    /// Focal length (e.g. "35 mm").
    pub focal_length: Option<String>,
    /// Focal length in 35mm film equivalent (e.g. "50 mm").
    pub focal_length_35mm: Option<String>,
    /// Exposure bias value (e.g. "+0.3 EV", "-0.7 EV", "0 EV").
    pub exposure_bias: Option<String>,
    /// Original capture date and time (e.g. "2024-08-15 14:32:10").
    pub date_time_original: Option<String>,
    /// Human-readable GPS coordinates (e.g. "40°25'01\" N, 3°42'12\" W").
    pub gps_coords: Option<String>,
    /// Flash firing status (e.g. "Flash fired", "Flash did not fire").
    pub flash: Option<String>,
    /// Raw EXIF orientation flag (1..=8).
    pub orientation: Option<u8>,
}

impl ExifMeta {
    /// Returns `true` if no meaningful EXIF fields were found.
    pub fn is_empty(&self) -> bool {
        self.make.is_none()
            && self.model.is_none()
            && self.lens.is_none()
            && self.exposure_time.is_none()
            && self.f_number.is_none()
            && self.iso.is_none()
            && self.focal_length.is_none()
            && self.focal_length_35mm.is_none()
            && self.exposure_bias.is_none()
            && self.date_time_original.is_none()
            && self.gps_coords.is_none()
            && self.flash.is_none()
            && self.orientation.is_none()
    }

    /// Combined camera make and model (e.g. "Sony ILCE-7M4", "Canon EOS R5"), avoiding repetition
    /// if the model string already starts with or contains the manufacturer name.
    pub fn camera_name(&self) -> Option<String> {
        match (&self.make, &self.model) {
            (Some(make), Some(model)) => {
                let make_trimmed = make.trim();
                let model_trimmed = model.trim();
                let make_lower = make_trimmed.to_lowercase();
                let model_lower = model_trimmed.to_lowercase();
                if model_lower.starts_with(&make_lower) || model_lower.contains(&make_lower) {
                    Some(model_trimmed.to_string())
                } else {
                    Some(format!("{make_trimmed} {model_trimmed}"))
                }
            }
            (None, Some(model)) => Some(model.trim().to_string()),
            (Some(make), None) => Some(make.trim().to_string()),
            (None, None) => None,
        }
    }

    /// Compact summary of core shooting parameters (e.g. "1/500 s · f/2.8 · ISO 200 · 35 mm").
    pub fn shot_summary(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(t) = &self.exposure_time {
            parts.push(t.clone());
        }
        if let Some(f) = &self.f_number {
            parts.push(f.clone());
        }
        if let Some(iso) = &self.iso {
            parts.push(iso.clone());
        }
        if let Some(focal) = &self.focal_length {
            if let Some(eq) = &self.focal_length_35mm {
                if eq != focal {
                    parts.push(format!("{focal} (eq. {eq})"));
                } else {
                    parts.push(focal.clone());
                }
            } else {
                parts.push(focal.clone());
            }
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" · "))
        }
    }

    /// Formatted multi-line text block suitable for the info overlay card.
    pub fn display_lines(&self) -> Vec<String> {
        self.display_lines_with_defaults(false)
    }

    /// Formatted multi-line text block. If `include_default_orientation` is true,
    /// includes orientation even when it is standard `Normal (0°)` (Orientation 1).
    pub fn display_lines_with_defaults(&self, include_default_orientation: bool) -> Vec<String> {
        let mut lines = Vec::new();

        if let Some(cam) = self.camera_name() {
            lines.push(format!("Camera: {cam}"));
        }
        if let Some(lens) = &self.lens {
            lines.push(format!("Lens: {lens}"));
        }
        if let Some(shot) = self.shot_summary() {
            lines.push(format!("Shot: {shot}"));
        }
        if let Some(bias) = &self.exposure_bias {
            if bias != "0 EV" && bias != "+0.0 EV" && bias != "0.0 EV" {
                lines.push(format!("Exp. Bias: {bias}"));
            }
        }
        if let Some(date) = &self.date_time_original {
            lines.push(format!("Captured: {date}"));
        }
        if let Some(flash) = &self.flash {
            lines.push(format!("Flash: {flash}"));
        }
        if let Some(orient) = self.orientation_description() {
            if include_default_orientation || self.orientation != Some(1) {
                lines.push(format!("Orientation: {orient}"));
            }
        }
        if let Some(gps) = &self.gps_coords {
            lines.push(format!("GPS: {gps}"));
        }

        lines
            .into_iter()
            .map(|l| sanitize_text(&l))
            .filter(|l| !l.is_empty())
            .collect()
    }

    /// Human-friendly description of the EXIF orientation transformation.
    pub fn orientation_description(&self) -> Option<&'static str> {
        match self.orientation {
            Some(1) => Some("Normal (0°)"),
            Some(2) => Some("Flipped horizontal"),
            Some(3) => Some("Rotate 180°"),
            Some(4) => Some("Flipped vertical"),
            Some(5) => Some("Flipped & Rotate 90° CW"),
            Some(6) => Some("Rotate 90° CW"),
            Some(7) => Some("Flipped & Rotate 270° CW"),
            Some(8) => Some("Rotate 270° CW"),
            _ => None,
        }
    }
}

/// Extract EXIF metadata from an image file on disk. Returns `None` if the file contains no EXIF data.
pub fn read_exif_path<P: AsRef<Path>>(path: P) -> Option<ExifMeta> {
    let file = File::open(path.as_ref()).ok()?;
    let mut reader = BufReader::new(file);
    let exif = Reader::new().read_from_container(&mut reader).ok()?;
    let meta = parse_exif(&exif);
    if meta.is_empty() {
        None
    } else {
        Some(meta)
    }
}

/// Extract EXIF metadata from in-memory image bytes. Returns `None` if the bytes contain no EXIF data.
pub fn read_exif_bytes(bytes: &[u8]) -> Option<ExifMeta> {
    let mut cursor = Cursor::new(bytes);
    let exif = Reader::new().read_from_container(&mut cursor).ok()?;
    let meta = parse_exif(&exif);
    if meta.is_empty() {
        None
    } else {
        Some(meta)
    }
}

fn parse_exif(exif: &exif::Exif) -> ExifMeta {
    let make = get_string_field(exif, Tag::Make);
    let model = get_string_field(exif, Tag::Model);
    let lens = get_string_field(exif, Tag::LensModel);

    let exposure_time = exif
        .get_field(Tag::ExposureTime, In::PRIMARY)
        .map(|f| f.display_value().to_string())
        .filter(|s| !s.is_empty())
        .map(|disp| {
            if disp.ends_with('s') || disp.ends_with("sec") {
                disp
            } else {
                format!("{disp} s")
            }
        });

    let f_number = exif
        .get_field(Tag::FNumber, In::PRIMARY)
        .map(|f| f.display_value().to_string())
        .filter(|s| !s.is_empty())
        .map(|disp| {
            if disp.starts_with('f') || disp.starts_with('F') {
                disp
            } else {
                format!("f/{disp}")
            }
        });

    let iso = exif
        .get_field(Tag::PhotographicSensitivity, In::PRIMARY)
        .or_else(|| exif.get_field(Tag::ISOSpeed, In::PRIMARY))
        .map(|f| f.display_value().to_string())
        .filter(|s| !s.is_empty())
        .map(|disp| {
            if disp.starts_with("ISO") || disp.starts_with("iso") {
                disp
            } else {
                format!("ISO {disp}")
            }
        });

    let focal_length = exif
        .get_field(Tag::FocalLength, In::PRIMARY)
        .map(|f| f.display_value().to_string())
        .filter(|s| !s.is_empty())
        .map(|disp| {
            if disp.ends_with("mm") {
                disp
            } else {
                format!("{disp} mm")
            }
        });

    let focal_length_35mm = exif
        .get_field(Tag::FocalLengthIn35mmFilm, In::PRIMARY)
        .map(|f| f.display_value().to_string())
        .filter(|s| !s.is_empty())
        .map(|disp| {
            if disp.ends_with("mm") {
                disp
            } else {
                format!("{disp} mm")
            }
        });

    let exposure_bias = exif
        .get_field(Tag::ExposureBiasValue, In::PRIMARY)
        .map(|f| f.display_value().to_string())
        .filter(|s| !s.is_empty())
        .map(|disp| {
            if disp.ends_with("EV") || disp.ends_with("ev") {
                disp
            } else {
                format!("{disp} EV")
            }
        });

    let date_time_original = exif
        .get_field(Tag::DateTimeOriginal, In::PRIMARY)
        .map(|f| f.display_value().to_string())
        .filter(|s| !s.is_empty());

    let flash = exif
        .get_field(Tag::Flash, In::PRIMARY)
        .map(|f| f.display_value().to_string())
        .filter(|s| !s.is_empty());

    let orientation = parse_orientation(exif);

    let gps_coords = parse_gps_coords(exif);

    ExifMeta {
        make,
        model,
        lens,
        exposure_time,
        f_number,
        iso,
        focal_length,
        focal_length_35mm,
        exposure_bias,
        date_time_original,
        flash,
        gps_coords,
        orientation,
    }
}

/// Strip ASCII and Unicode control characters (including escape sequences `\x1b`)
/// and collapse multiple spaces.
pub fn sanitize_text(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn get_string_field(exif: &exif::Exif, tag: Tag) -> Option<String> {
    let field = exif.get_field(tag, In::PRIMARY)?;
    match &field.value {
        Value::Ascii(vec) => {
            if let Some(first) = vec.first() {
                let s = std::str::from_utf8(first).ok()?.trim();
                let clean = sanitize_text(s);
                if !clean.is_empty() {
                    return Some(clean);
                }
            }
            None
        }
        _ => {
            let s = field.display_value().to_string();
            let trimmed = s.trim();
            if trimmed.is_empty() || trimmed == "\"\"" {
                None
            } else {
                let clean = sanitize_text(trimmed.trim_matches('"'));
                if clean.is_empty() {
                    None
                } else {
                    Some(clean)
                }
            }
        }
    }
}

fn parse_orientation(exif: &exif::Exif) -> Option<u8> {
    let field = exif.get_field(Tag::Orientation, In::PRIMARY)?;
    match &field.value {
        Value::Short(v) => v.first().copied().and_then(|val| u8::try_from(val).ok()),
        Value::Byte(v) => v.first().copied(),
        _ => None,
    }
}

fn parse_gps_coords(exif: &exif::Exif) -> Option<String> {
    let lat_field = exif.get_field(Tag::GPSLatitude, In::PRIMARY)?;
    let lon_field = exif.get_field(Tag::GPSLongitude, In::PRIMARY)?;
    let lat_ref = exif
        .get_field(Tag::GPSLatitudeRef, In::PRIMARY)
        .map(|f| f.display_value().to_string())
        .unwrap_or_else(|| "N".to_string());
    let lon_ref = exif
        .get_field(Tag::GPSLongitudeRef, In::PRIMARY)
        .map(|f| f.display_value().to_string())
        .unwrap_or_else(|| "E".to_string());

    let lat_str = format_dms(&lat_field.value)?;
    let lon_str = format_dms(&lon_field.value)?;

    let lat_hemi = lat_ref.trim().chars().next().unwrap_or('N');
    let lon_hemi = lon_ref.trim().chars().next().unwrap_or('E');

    Some(format!("{lat_str} {lat_hemi}, {lon_str} {lon_hemi}"))
}

fn format_dms(val: &Value) -> Option<String> {
    match val {
        Value::Rational(vec) if vec.len() >= 3 => {
            let d = vec[0].to_f64();
            let m = vec[1].to_f64();
            let s = vec[2].to_f64();
            let total_sec = d * 3600.0 + m * 60.0 + s;
            if !total_sec.is_finite() || total_sec < 0.0 {
                return None;
            }
            let mut deg = (total_sec / 3600.0).floor();
            let rem = total_sec - deg * 3600.0;
            let mut min = (rem / 60.0).floor();
            let mut sec = rem - min * 60.0;

            // Handle potential rounding rollover with 1 decimal place (e.g. 59.98 -> 60.0)
            if (sec * 10.0).round() >= 600.0 {
                sec = 0.0;
                min += 1.0;
                if min >= 60.0 {
                    min = 0.0;
                    deg += 1.0;
                }
            }

            Some(format!("{deg:.0}°{min:.0}'{sec:.1}\""))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera_name_deduplicates_make() {
        let meta = ExifMeta {
            make: Some("Sony".to_string()),
            model: Some("Sony ILCE-7M4".to_string()),
            ..Default::default()
        };
        assert_eq!(meta.camera_name(), Some("Sony ILCE-7M4".to_string()));

        let meta2 = ExifMeta {
            make: Some("Sony".to_string()),
            model: Some("ILCE-7M4".to_string()),
            ..Default::default()
        };
        assert_eq!(meta2.camera_name(), Some("Sony ILCE-7M4".to_string()));

        let meta3 = ExifMeta {
            make: Some("Canon".to_string()),
            model: Some("EOS R5".to_string()),
            ..Default::default()
        };
        assert_eq!(meta3.camera_name(), Some("Canon EOS R5".to_string()));
    }

    #[test]
    fn shot_summary_formats_cleanly() {
        let meta = ExifMeta {
            exposure_time: Some("1/500 s".to_string()),
            f_number: Some("f/2.8".to_string()),
            iso: Some("ISO 200".to_string()),
            focal_length: Some("35 mm".to_string()),
            focal_length_35mm: Some("50 mm".to_string()),
            ..Default::default()
        };
        assert_eq!(
            meta.shot_summary(),
            Some("1/500 s · f/2.8 · ISO 200 · 35 mm (eq. 50 mm)".to_string())
        );

        let meta_same_eq = ExifMeta {
            exposure_time: Some("1/250 s".to_string()),
            f_number: Some("f/4".to_string()),
            iso: Some("ISO 100".to_string()),
            focal_length: Some("50 mm".to_string()),
            focal_length_35mm: Some("50 mm".to_string()),
            ..Default::default()
        };
        assert_eq!(
            meta_same_eq.shot_summary(),
            Some("1/250 s · f/4 · ISO 100 · 50 mm".to_string())
        );
    }

    #[test]
    fn empty_meta_detection() {
        let empty = ExifMeta::default();
        assert!(empty.is_empty());

        let with_make = ExifMeta {
            make: Some("Nikon".to_string()),
            ..Default::default()
        };
        assert!(!with_make.is_empty());
    }

    #[test]
    fn display_lines_generation() {
        let meta = ExifMeta {
            make: Some("Apple".to_string()),
            model: Some("iPhone 15 Pro".to_string()),
            lens: Some("back camera 6.78mm f/1.78".to_string()),
            exposure_time: Some("1/120 s".to_string()),
            f_number: Some("f/1.8".to_string()),
            iso: Some("ISO 80".to_string()),
            focal_length: Some("24 mm".to_string()),
            focal_length_35mm: None,
            exposure_bias: Some("+0.3 EV".to_string()),
            date_time_original: Some("2024-05-12 18:22:01".to_string()),
            gps_coords: Some("40°25'1.2\" N, 3°42'12.0\" W".to_string()),
            flash: None,
            orientation: None,
        };
        let lines = meta.display_lines();
        assert!(lines.contains(&"Camera: Apple iPhone 15 Pro".to_string()));
        assert!(lines.contains(&"Lens: back camera 6.78mm f/1.78".to_string()));
        assert!(lines.contains(&"Shot: 1/120 s · f/1.8 · ISO 80 · 24 mm".to_string()));
        assert!(lines.contains(&"Exp. Bias: +0.3 EV".to_string()));
        assert!(lines.contains(&"Captured: 2024-05-12 18:22:01".to_string()));
        assert!(lines.contains(&"GPS: 40°25'1.2\" N, 3°42'12.0\" W".to_string()));
    }

    #[test]
    fn format_dms_handles_fractional_components_and_rounding() {
        use exif::Rational;

        // Standard: 40 deg, 25 min, 15 sec
        let val1 = Value::Rational(vec![
            Rational { num: 40, denom: 1 },
            Rational { num: 25, denom: 1 },
            Rational { num: 15, denom: 1 },
        ]);
        assert_eq!(format_dms(&val1), Some("40°25'15.0\"".to_string()));

        // Fractional minutes: 40 deg, 25.5 min (51/2), 0 sec -> 40°25'30.0"
        let val2 = Value::Rational(vec![
            Rational { num: 40, denom: 1 },
            Rational { num: 51, denom: 2 },
            Rational { num: 0, denom: 1 },
        ]);
        assert_eq!(format_dms(&val2), Some("40°25'30.0\"".to_string()));

        // Rounding rollover: 40 deg, 25 min, 59.98 sec -> 40°26'0.0"
        let val3 = Value::Rational(vec![
            Rational { num: 40, denom: 1 },
            Rational { num: 25, denom: 1 },
            Rational {
                num: 5998,
                denom: 100,
            },
        ]);
        assert_eq!(format_dms(&val3), Some("40°26'0.0\"".to_string()));
    }

    #[test]
    fn orientation_descriptions_and_display_lines() {
        let meta_normal = ExifMeta {
            orientation: Some(1),
            ..Default::default()
        };
        assert_eq!(meta_normal.orientation_description(), Some("Normal (0°)"));
        // Orientation 1 is not pushed to default display_lines to avoid cluttering normal images
        assert!(!meta_normal
            .display_lines()
            .iter()
            .any(|l| l.starts_with("Orientation:")));
        // But with include_default_orientation=true, it is included
        assert!(meta_normal
            .display_lines_with_defaults(true)
            .contains(&"Orientation: Normal (0°)".to_string()));

        let meta_rotated = ExifMeta {
            orientation: Some(6),
            ..Default::default()
        };
        assert_eq!(
            meta_rotated.orientation_description(),
            Some("Rotate 90° CW")
        );
        assert!(meta_rotated
            .display_lines()
            .contains(&"Orientation: Rotate 90° CW".to_string()));
    }

    #[test]
    fn sanitize_text_and_control_characters() {
        let dirty = "Canon\x1b[2J\x1b[H EOS\r\n 5D\t";
        assert_eq!(sanitize_text(dirty), "Canon [2J [H EOS 5D");

        let meta = ExifMeta {
            make: Some("Injected\x1b[2J".to_string()),
            ..Default::default()
        };
        let lines = meta.display_lines();
        assert_eq!(lines, vec!["Camera: Injected [2J".to_string()]);
    }
}
