use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use std::sync::LazyLock;

pub const NIGHT: Color = Color::Rgb(0x0B, 0x09, 0x06);
pub const RULE: Color = Color::Rgb(0x2E, 0x24, 0x18);
pub const FAINT: Color = Color::Rgb(0x5A, 0x48, 0x30);
pub const DIM: Color = Color::Rgb(0x8C, 0x76, 0x57);
pub const TEXT: Color = Color::Rgb(0xF2, 0xE2, 0xC4);
pub const STATE: Color = Color::Rgb(0xE3, 0xC4, 0x8E);
pub const SECONDARY: Color = Color::Rgb(0xB8, 0x92, 0x5E);
pub const LAMP: Color = Color::Rgb(0xFF, 0xB5, 0x47);
pub const DANGER: Color = Color::Rgb(0xE5, 0x55, 0x3F);
pub const BORDER: Color = Color::Rgb(0x7A, 0x62, 0x40);
pub const SHADOW: Color = Color::Rgb(0x15, 0x0F, 0x08);

pub fn text() -> Style {
    Style::new().fg(TEXT)
}

pub fn bold() -> Style {
    text().add_modifier(Modifier::BOLD)
}

pub fn dim() -> Style {
    Style::new().fg(DIM)
}

pub fn faint() -> Style {
    Style::new().fg(FAINT)
}

pub fn lamp() -> Style {
    Style::new().fg(LAMP)
}

pub fn danger() -> Style {
    Style::new().fg(DANGER)
}

pub fn state() -> Style {
    Style::new().fg(STATE)
}

pub fn secondary() -> Style {
    Style::new().fg(SECONDARY)
}

pub fn lit_block() -> Style {
    Style::new().fg(NIGHT).bg(LAMP).add_modifier(Modifier::BOLD)
}

static TRUECOLOR: LazyLock<bool> = LazyLock::new(|| {
    std::env::var("COLORTERM").is_ok_and(|value| value.contains("truecolor") || value.contains("24bit"))
});

fn channels(color: Color) -> (u32, u32, u32) {
    match color {
        Color::Rgb(red, green, blue) => (u32::from(red), u32::from(green), u32::from(blue)),
        _ => (0, 0, 0),
    }
}

fn mix(base: Color, light: Color, per_mille: u32) -> Color {
    let (base_red, base_green, base_blue) = channels(base);
    let (light_red, light_green, light_blue) = channels(light);
    let blend = |from: u32, to: u32| {
        let value = (from * (1000 - per_mille) + to * per_mille) / 1000;
        u8::try_from(value).unwrap_or(u8::MAX)
    };

    Color::Rgb(blend(base_red, light_red), blend(base_green, light_green), blend(base_blue, light_blue))
}

pub fn glow(buffer: &mut Buffer, row: Rect) {
    let width = row.width.max(1);

    for offset in 0..row.width {
        let fade = u32::from(width - offset) * 1000 / u32::from(width);
        let strength = 20 + fade * fade * 240 / 1_000_000;
        let color = if *TRUECOLOR { mix(NIGHT, LAMP, strength) } else { SHADOW };

        for y in row.top()..row.bottom() {
            if let Some(cell) = buffer.cell_mut((row.x + offset, y)) {
                cell.set_bg(color);
            }
        }
    }
}

pub fn cut(text: &str, width: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

    if text.width() <= width {
        return text.to_owned();
    }

    let mut used = 0;
    let mut kept = String::new();

    for character in text.chars() {
        let size = character.width().unwrap_or(0);

        if used + size + 1 > width {
            break;
        }

        used += size;
        kept.push(character);
    }

    if width > 0 {
        kept.push('…');
    }

    kept
}

pub fn fit(text: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthStr;

    let kept = cut(text, width);
    let pad = width.saturating_sub(kept.width());

    format!("{kept}{}", " ".repeat(pad))
}
