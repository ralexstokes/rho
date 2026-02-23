use std::sync::OnceLock;

use base64::Engine;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageProtocol {
    Kitty,
    Iterm2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalCapabilities {
    pub images: Option<ImageProtocol>,
    pub true_color: bool,
    pub hyperlinks: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageDimensions {
    pub width_px: u32,
    pub height_px: u32,
}

static CAPABILITIES_CACHE: OnceLock<TerminalCapabilities> = OnceLock::new();

pub fn detect_capabilities() -> TerminalCapabilities {
    let term_program = std::env::var("TERM_PROGRAM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let term = std::env::var("TERM").unwrap_or_default().to_ascii_lowercase();
    let color_term = std::env::var("COLORTERM")
        .unwrap_or_default()
        .to_ascii_lowercase();

    if std::env::var("KITTY_WINDOW_ID").is_ok() || term_program == "kitty" {
        return TerminalCapabilities {
            images: Some(ImageProtocol::Kitty),
            true_color: true,
            hyperlinks: true,
        };
    }
    if term_program == "ghostty" || term_program == "wezterm" || std::env::var("WEZTERM_PANE").is_ok() {
        return TerminalCapabilities {
            images: Some(ImageProtocol::Kitty),
            true_color: true,
            hyperlinks: true,
        };
    }
    if std::env::var("ITERM_SESSION_ID").is_ok() || term_program == "iterm.app" {
        return TerminalCapabilities {
            images: Some(ImageProtocol::Iterm2),
            true_color: true,
            hyperlinks: true,
        };
    }

    TerminalCapabilities {
        images: None,
        true_color: color_term == "truecolor" || color_term == "24bit" || term.contains("256color"),
        hyperlinks: true,
    }
}

pub fn capabilities() -> TerminalCapabilities {
    *CAPABILITIES_CACHE.get_or_init(detect_capabilities)
}

pub fn image_fallback(mime_type: &str, dimensions: Option<ImageDimensions>, filename: Option<&str>) -> String {
    let mut parts = Vec::new();
    if let Some(filename) = filename {
        parts.push(filename.to_string());
    }
    parts.push(format!("[{mime_type}]"));
    if let Some(dimensions) = dimensions {
        parts.push(format!("{}x{}", dimensions.width_px, dimensions.height_px));
    }
    format!("[Image: {}]", parts.join(" "))
}

pub fn render_image_sequence(
    base64_data: &str,
    mime_type: &str,
    columns: u16,
) -> Option<(String, u16)> {
    let caps = capabilities();
    let dimensions = get_image_dimensions(base64_data, mime_type);
    let rows = dimensions
        .map(|dim| calculate_rows(dim, columns))
        .unwrap_or(1);

    match caps.images {
        Some(ImageProtocol::Kitty) => {
            let sequence = encode_kitty(base64_data, columns, rows);
            Some((sequence, rows))
        }
        Some(ImageProtocol::Iterm2) => {
            let sequence = encode_iterm2(base64_data, columns);
            Some((sequence, rows))
        }
        None => None,
    }
}

pub fn get_image_dimensions(base64_data: &str, mime_type: &str) -> Option<ImageDimensions> {
    match mime_type {
        "image/png" => get_png_dimensions(base64_data),
        "image/gif" => get_gif_dimensions(base64_data),
        _ => None,
    }
}

pub fn get_png_dimensions(base64_data: &str) -> Option<ImageDimensions> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(base64_data).ok()?;
    if bytes.len() < 24 {
        return None;
    }
    if &bytes[0..4] != b"\x89PNG" {
        return None;
    }
    Some(ImageDimensions {
        width_px: u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]),
        height_px: u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]),
    })
}

pub fn get_gif_dimensions(base64_data: &str) -> Option<ImageDimensions> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(base64_data).ok()?;
    if bytes.len() < 10 {
        return None;
    }
    if &bytes[0..3] != b"GIF" {
        return None;
    }
    Some(ImageDimensions {
        width_px: u16::from_le_bytes([bytes[6], bytes[7]]) as u32,
        height_px: u16::from_le_bytes([bytes[8], bytes[9]]) as u32,
    })
}

fn calculate_rows(dimensions: ImageDimensions, columns: u16) -> u16 {
    let target_width_px = u32::from(columns) * 9;
    if dimensions.width_px == 0 {
        return 1;
    }
    let scaled_height = dimensions.height_px.saturating_mul(target_width_px) / dimensions.width_px;
    ((scaled_height + 17) / 18).max(1) as u16
}

fn encode_kitty(base64_data: &str, columns: u16, rows: u16) -> String {
    format!("\u{1b}_Ga=T,f=100,q=2,c={columns},r={rows};{base64_data}\u{1b}\\")
}

fn encode_iterm2(base64_data: &str, columns: u16) -> String {
    format!("\u{1b}]1337;File=inline=1;width={columns};height=auto:{base64_data}\u{7}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_includes_mime() {
        let rendered = image_fallback("image/png", None, None);
        assert!(rendered.contains("image/png"));
    }

    #[test]
    fn png_dimensions_parse_from_header() {
        // 1x1 transparent png
        let data = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO6X3N0AAAAASUVORK5CYII=";
        let dims = get_png_dimensions(data).expect("png dims");
        assert_eq!(dims.width_px, 1);
        assert_eq!(dims.height_px, 1);
    }
}
