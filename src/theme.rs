//! Colour tokens — colour-agnostic, defaulting to the user's Neovim theme
//! "lilac" (`~/.config/nvim/colors/lilac.lua`).
//!
//! Ten base tokens (bg · fg · dim · accent · accent2 · info · ok · warn · err ·
//! num) drive everything; panels, borders, selection, gutters are DERIVED by
//! mixing, so swapping the ten rethemes the whole app. Override any token in
//! `~/.config/aeovim/theme.toml` (`accent = "#b657ff"` lines). The terminal
//! background stays transparent (Color::Reset) so the wallpaper shows through —
//! `bg` is only the mix base for derived panels.

use std::sync::OnceLock;

use ratatui::style::Color;

pub struct Theme {
    /// Transparent — the terminal wallpaper shows through. Reserved (with
    /// `faint`) for painted-background / timestamp rendering.
    #[allow(dead_code)]
    pub bg: Color,
    // -- the ten base tokens --
    pub fg: Color,
    pub dim: Color,
    pub accent: Color,  // primary (lilac purple)
    pub accent2: Color, // secondary (pink) — cursor, INSERT
    pub info: Color,    // periwinkle — user role, code
    pub num: Color,     // magenta — numbers / badges
    pub err: Color,
    pub warn: Color,
    pub ok: Color,
    // -- derived --
    pub gutter: Color,     // faint rules
    pub panel: Color,      // statusline / popup bg
    #[allow(dead_code)]
    pub panel2: Color,     // darker statusline segment (reserved)
    pub sel: Color,        // selected row bg
    pub cursorline: Color, // hovered row bg
    pub border: Color,     // pane / float borders
    pub fgdim: Color,      // de-emphasised names
    #[allow(dead_code)]
    pub faint: Color,      // timestamps (reserved)
    // -- mode pills (mirror lualine against the base tokens) --
    pub mode_normal: Color,
    pub mode_insert: Color,
    pub mode_command: Color,
    pub mode_visual: Color,
}

#[derive(Clone, Copy)]
struct Rgb(u8, u8, u8);

impl Rgb {
    fn color(self) -> Color {
        Color::Rgb(self.0, self.1, self.2)
    }
}

/// `a` at `pct`% over `b` — the same color-mix(in srgb, …) the design canvas
/// derives its panels with.
fn mix(a: Rgb, pct: u32, b: Rgb) -> Rgb {
    let m = |x: u8, y: u8| ((x as u32 * pct + y as u32 * (100 - pct)) / 100) as u8;
    Rgb(m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

fn parse_hex(s: &str) -> Option<Rgb> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let b = |i| u8::from_str_radix(&s[i..i + 2], 16).ok();
    Some(Rgb(b(0)?, b(2)?, b(4)?))
}

/// The ten base tokens, lilac by default, overridable one line per token in
/// `~/.config/aeovim/theme.toml`. Parsing is deliberately dumb: `name = "#hex"`.
fn base_tokens() -> [Rgb; 10] {
    // bg fg dim accent accent2 info num err warn ok
    let mut t = [
        Rgb(0x0d, 0x0b, 0x11),
        Rgb(0xe0, 0xce, 0xed),
        Rgb(0x61, 0x4e, 0x6e),
        Rgb(0xb6, 0x57, 0xff),
        Rgb(0xf5, 0xb0, 0xef),
        Rgb(0xa2, 0x9d, 0xfa),
        Rgb(0xf2, 0x5a, 0xe6),
        Rgb(0xf0, 0x3e, 0x5f),
        Rgb(0xe0, 0xa4, 0x4e),
        Rgb(0x7c, 0xd6, 0x9a),
    ];
    const NAMES: [&str; 10] = [
        "bg", "fg", "dim", "accent", "accent2", "info", "num", "err", "warn", "ok",
    ];
    let Some(home) = std::env::var_os("HOME") else { return t };
    let path = std::path::PathBuf::from(home).join(".config/aeovim/theme.toml");
    let Ok(text) = std::fs::read_to_string(path) else { return t };
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let key = k.trim();
        let val = v.trim().trim_matches(|c| c == '"' || c == '\'');
        if let (Some(i), Some(rgb)) = (NAMES.iter().position(|n| *n == key), parse_hex(val)) {
            t[i] = rgb;
        }
    }
    t
}

fn build() -> Theme {
    let [bg, fg, dim, accent, accent2, info, num, err, warn, ok] = base_tokens();
    let black = Rgb(0, 0, 0);
    Theme {
        bg: Color::Reset, // transparent — wallpaper shows through
        fg: fg.color(),
        dim: dim.color(),
        accent: accent.color(),
        accent2: accent2.color(),
        info: info.color(),
        num: num.color(),
        err: err.color(),
        warn: warn.color(),
        ok: ok.color(),
        gutter: mix(fg, 20, bg).color(),
        panel: mix(fg, 9, bg).color(),
        panel2: mix(fg, 5, bg).color(),
        sel: mix(accent, 30, bg).color(),
        cursorline: mix(bg, 92, black).color(),
        border: mix(accent, 52, dim).color(),
        fgdim: mix(fg, 60, dim).color(),
        faint: mix(dim, 70, bg).color(),
        mode_normal: accent2.color(),
        mode_insert: accent2.color(),
        mode_command: warn.color(),
        mode_visual: accent.color(),
    }
}

/// The loaded theme. First call reads the config; later calls are free.
pub fn get() -> &'static Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(build)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_parses_and_mix_derives() {
        let p = parse_hex("#b657ff").unwrap();
        assert_eq!((p.0, p.1, p.2), (0xb6, 0x57, 0xff));
        assert!(parse_hex("nope").is_none());
        assert!(parse_hex("#b657f").is_none());
        // mix at 100/0 is identity on either side
        let a = Rgb(10, 20, 30);
        let b = Rgb(200, 100, 0);
        assert_eq!(mix(a, 100, b).0, 10);
        assert_eq!(mix(a, 0, b).0, 200);
    }

    #[test]
    fn theme_builds_with_lilac_defaults() {
        let th = get();
        assert_eq!(th.accent, Color::Rgb(0xb6, 0x57, 0xff));
        assert_eq!(th.bg, Color::Reset);
        // derived panel ≈ #201c25 (fg 9% over bg) — the canvas formula
        match th.panel {
            Color::Rgb(r, g, b) => {
                assert!((0x1c..=0x24).contains(&r), "panel r {r:#x}");
                assert!((0x18..=0x20).contains(&g), "panel g {g:#x}");
                assert!((0x20..=0x29).contains(&b), "panel b {b:#x}");
            }
            other => panic!("panel not rgb: {other:?}"),
        }
    }
}
