//! Colours and widget styles of the editor window, light or dark like
//! Windows' app setting.

use iced::widget::{button, container, pick_list, text_input};
use iced::{Background, Border, Color, Font, Shadow, Theme, font};
use std::sync::atomic::{AtomicBool, Ordering};

static DARK: AtomicBool = AtomicBool::new(false);

/// The colours of the current scheme.
#[derive(Clone, Copy)]
pub struct Colors {
    pub bg: Color,
    pub sidebar: Color,
    pub surface: Color,
    pub input: Color,
    pub border: Color,
    pub text: Color,
    pub muted: Color,
    pub accent: Color,
    pub accent_text: Color,
    pub accent_soft: Color,
    pub hover: Color,
    pub success: Color,
    pub warning: Color,
    pub danger: Color,
}

const fn rgb(hex: u32) -> Color {
    Color::from_rgb8((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

const fn alpha(c: Color, a: f32) -> Color {
    Color { a, ..c }
}

const LIGHT: Colors = Colors {
    bg: rgb(0xf5f6f8),
    sidebar: rgb(0xeef0f3),
    surface: rgb(0xffffff),
    input: rgb(0xffffff),
    border: rgb(0xd9dde3),
    text: rgb(0x1f2328),
    muted: rgb(0x656d76),
    // pine green
    accent: rgb(0x01796f),
    accent_text: rgb(0xffffff),
    accent_soft: alpha(rgb(0x01796f), 0.12),
    hover: alpha(rgb(0x1f2328), 0.06),
    success: rgb(0x1a7f37),
    warning: rgb(0x9a6700),
    danger: rgb(0xcf222e),
};

const DARK_COLORS: Colors = Colors {
    bg: rgb(0x16181c),
    sidebar: rgb(0x1b1e23),
    surface: rgb(0x1f2329),
    input: rgb(0x16181c),
    border: rgb(0x30363d),
    text: rgb(0xe6e8eb),
    muted: rgb(0x8b949e),
    // pine green, lighter for a dark background (dark text on it reads better)
    accent: rgb(0x3dae8f),
    accent_text: rgb(0x08201a),
    accent_soft: alpha(rgb(0x3dae8f), 0.16),
    hover: alpha(rgb(0xffffff), 0.06),
    success: rgb(0x3fb950),
    warning: rgb(0xd29922),
    danger: rgb(0xf85149),
};

pub fn colors() -> Colors {
    if DARK.load(Ordering::Relaxed) { DARK_COLORS } else { LIGHT }
}

/// Follow Windows' "app mode" (light or dark), read once at start, unless
/// `NGINX_EDITOR_THEME` is `light` or `dark`.
pub fn init() -> Theme {
    let dark = match std::env::var("NGINX_EDITOR_THEME").as_deref() {
        Ok("light") => false,
        Ok("dark") => true,
        _ => system_dark(),
    };
    DARK.store(dark, Ordering::Relaxed);
    let c = colors();
    Theme::custom(
        if dark { "nginx dark" } else { "nginx light" },
        iced::theme::Palette {
            background: c.bg,
            text: c.text,
            primary: c.accent,
            success: c.success,
            warning: c.warning,
            danger: c.danger,
        },
    )
}

fn system_dark() -> bool {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    let key: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize\0".encode_utf16().collect();
    let name: Vec<u16> = "AppsUseLightTheme\0".encode_utf16().collect();
    let (mut value, mut size) = (1u32, 4u32);
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut value as *mut u32).cast(),
            &mut size,
        )
    };
    r == 0 && value == 0
}

pub const SEMIBOLD: Font = Font { weight: font::Weight::Semibold, ..Font::DEFAULT };
pub const ITALIC: Font = Font { style: font::Style::Italic, ..Font::DEFAULT };

fn radius(r: f32) -> Border {
    Border { radius: r.into(), ..Border::default() }
}

// ------------------------------------------------------------ containers

pub fn page(_: &Theme) -> container::Style {
    let c = colors();
    container::Style { background: Some(c.bg.into()), text_color: Some(c.text), ..Default::default() }
}

pub fn bar(_: &Theme) -> container::Style {
    let c = colors();
    container::Style { background: Some(c.surface.into()), ..Default::default() }
}

pub fn sidebar(_: &Theme) -> container::Style {
    let c = colors();
    container::Style { background: Some(c.sidebar.into()), ..Default::default() }
}

pub fn card(_: &Theme) -> container::Style {
    let c = colors();
    container::Style {
        background: Some(c.surface.into()),
        border: Border { radius: 10.0.into(), width: 1.0, color: c.border },
        ..Default::default()
    }
}

pub fn pill(_: &Theme) -> container::Style {
    let c = colors();
    container::Style {
        background: Some(c.accent_soft.into()),
        text_color: Some(c.accent),
        border: radius(99.0),
        ..Default::default()
    }
}

pub fn pill_muted(_: &Theme) -> container::Style {
    let c = colors();
    container::Style { background: Some(c.hover.into()), text_color: Some(c.muted), border: radius(99.0), ..Default::default() }
}

pub fn pill_warn(_: &Theme) -> container::Style {
    let c = colors();
    container::Style {
        background: Some(alpha(c.warning, 0.14).into()),
        text_color: Some(c.warning),
        border: radius(99.0),
        ..Default::default()
    }
}

pub fn segmented(_: &Theme) -> container::Style {
    let c = colors();
    container::Style {
        background: Some(c.hover.into()),
        border: Border { radius: 8.0.into(), width: 1.0, color: c.border },
        ..Default::default()
    }
}

pub fn tooltip(_: &Theme) -> container::Style {
    let c = colors();
    container::Style {
        background: Some(c.surface.into()),
        text_color: Some(c.text),
        border: Border { radius: 6.0.into(), width: 1.0, color: c.border },
        shadow: Shadow { color: alpha(Color::BLACK, 0.2), offset: iced::Vector::new(0.0, 2.0), blur_radius: 8.0 },
        ..Default::default()
    }
}

pub fn dialog(_: &Theme) -> container::Style {
    let c = colors();
    container::Style {
        background: Some(c.surface.into()),
        text_color: Some(c.text),
        border: Border { radius: 12.0.into(), width: 1.0, color: c.border },
        shadow: Shadow { color: alpha(Color::BLACK, 0.35), offset: iced::Vector::new(0.0, 8.0), blur_radius: 24.0 },
        ..Default::default()
    }
}

pub fn backdrop(_: &Theme) -> container::Style {
    container::Style { background: Some(alpha(Color::BLACK, 0.4).into()), ..Default::default() }
}

// --------------------------------------------------------------- buttons

fn base_button(bg: Option<Color>, fg: Color, border: Color, r: f32) -> button::Style {
    button::Style {
        background: bg.map(Background::Color),
        text_color: fg,
        border: Border { radius: r.into(), width: if border.a > 0.0 { 1.0 } else { 0.0 }, color: border },
        shadow: Shadow::default(),
        snap: true,
    }
}

pub fn primary(_: &Theme, s: button::Status) -> button::Style {
    let c = colors();
    let bg = match s {
        button::Status::Hovered => mix(c.accent, Color::WHITE, 0.12),
        button::Status::Pressed => mix(c.accent, Color::BLACK, 0.12),
        button::Status::Disabled => alpha(c.accent, 0.45),
        button::Status::Active => c.accent,
    };
    base_button(Some(bg), c.accent_text, Color::TRANSPARENT, 6.0)
}

pub fn secondary(_: &Theme, s: button::Status) -> button::Style {
    let c = colors();
    match s {
        button::Status::Hovered | button::Status::Pressed => base_button(Some(c.hover), c.text, c.border, 6.0),
        button::Status::Disabled => base_button(None, alpha(c.muted, 0.6), c.border, 6.0),
        button::Status::Active => base_button(Some(c.surface), c.text, c.border, 6.0),
    }
}

pub fn danger(_: &Theme, s: button::Status) -> button::Style {
    let c = colors();
    match s {
        button::Status::Hovered | button::Status::Pressed => base_button(Some(c.danger), Color::WHITE, c.danger, 6.0),
        _ => base_button(None, c.danger, alpha(c.danger, 0.5), 6.0),
    }
}

/// Icon buttons: no frame until hovered.
pub fn ghost(_: &Theme, s: button::Status) -> button::Style {
    let c = colors();
    match s {
        button::Status::Hovered | button::Status::Pressed => base_button(Some(c.hover), c.text, Color::TRANSPARENT, 6.0),
        button::Status::Disabled => base_button(None, alpha(c.muted, 0.35), Color::TRANSPARENT, 6.0),
        button::Status::Active => base_button(None, c.muted, Color::TRANSPARENT, 6.0),
    }
}

/// A tab of the Form / Text switch.
pub fn tab(on: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, s| {
        let c = colors();
        if on {
            base_button(Some(c.surface), c.text, c.border, 6.0)
        } else {
            let fg = if matches!(s, button::Status::Hovered) { c.text } else { c.muted };
            base_button(None, fg, Color::TRANSPARENT, 6.0)
        }
    }
}

/// A block in the tree on the left.
pub fn node(on: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, s| {
        let c = colors();
        if on {
            base_button(Some(c.accent_soft), c.accent, Color::TRANSPARENT, 6.0)
        } else if matches!(s, button::Status::Hovered | button::Status::Pressed) {
            base_button(Some(c.hover), c.text, Color::TRANSPARENT, 6.0)
        } else {
            base_button(None, c.text, Color::TRANSPARENT, 6.0)
        }
    }
}

// ---------------------------------------------------------------- fields

pub fn input(bad: bool) -> impl Fn(&Theme, text_input::Status) -> text_input::Style {
    move |_, s| {
        let c = colors();
        let border = match s {
            _ if bad => c.danger,
            text_input::Status::Focused { .. } => c.accent,
            text_input::Status::Hovered => mix(c.border, c.text, 0.25),
            _ => c.border,
        };
        text_input::Style {
            background: c.input.into(),
            border: Border { radius: 6.0.into(), width: 1.0, color: border },
            icon: c.muted,
            placeholder: alpha(c.muted, 0.8),
            value: c.text,
            selection: alpha(c.accent, 0.3),
        }
    }
}

pub fn select(_: &Theme, s: pick_list::Status) -> pick_list::Style {
    let c = colors();
    let border = match s {
        pick_list::Status::Opened { .. } => c.accent,
        pick_list::Status::Hovered => mix(c.border, c.text, 0.25),
        pick_list::Status::Active => c.border,
    };
    pick_list::Style {
        text_color: c.text,
        placeholder_color: alpha(c.muted, 0.8),
        handle_color: c.muted,
        background: c.input.into(),
        border: Border { radius: 6.0.into(), width: 1.0, color: border },
    }
}

fn mix(a: Color, b: Color, t: f32) -> Color {
    Color { r: a.r + (b.r - a.r) * t, g: a.g + (b.g - a.g) * t, b: a.b + (b.b - a.b) * t, a: a.a }
}
