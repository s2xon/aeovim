//! Colour palette, ported from the user's active Neovim theme "lilac"
//! (`~/.config/nvim/colors/lilac.lua`). Background is transparent (Color::Reset)
//! so the terminal wallpaper shows through, matching the nvim setup.

#![allow(dead_code)] // some palette entries are reserved for split panes / future UI

use ratatui::style::Color;

pub const BG: Color = Color::Reset; // transparent
pub const PANEL: Color = Color::Rgb(0x21, 0x1d, 0x26); // statusline / popups
pub const FG: Color = Color::Rgb(0xe0, 0xce, 0xed); // primary text
pub const DIM: Color = Color::Rgb(0x61, 0x4e, 0x6e); // comments / secondary
pub const GUTTER: Color = Color::Rgb(0x3b, 0x34, 0x42); // faint rules
pub const BORDER: Color = Color::Rgb(0x8e, 0x6d, 0xa6); // pane / float borders
pub const SELECTION: Color = Color::Rgb(0x3f, 0x2a, 0x57); // selected row bg
pub const CURSORLINE: Color = Color::Rgb(0x10, 0x0e, 0x12); // hovered row bg

pub const PURPLE: Color = Color::Rgb(0xb6, 0x57, 0xff); // PRIMARY accent
pub const PINK: Color = Color::Rgb(0xf5, 0xb0, 0xef); // secondary accent
pub const PERI: Color = Color::Rgb(0xa2, 0x9d, 0xfa); // info / added
pub const MAGENTA: Color = Color::Rgb(0xf2, 0x5a, 0xe6); // numbers / badges
pub const RED: Color = Color::Rgb(0xf0, 0x3e, 0x5f); // error
pub const AMBER: Color = Color::Rgb(0xe0, 0xa4, 0x4e); // warning / running

// Statusline mode colours (mirror lualine make_theme against lilac).
pub const MODE_NORMAL: Color = PINK;
pub const MODE_INSERT: Color = PINK;
pub const MODE_COMMAND: Color = AMBER;
pub const MODE_VISUAL: Color = PURPLE;
