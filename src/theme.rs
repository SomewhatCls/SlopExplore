//! Fluent 2 / Windows 11 look: colour tokens, accent colour, Mica backdrop, DPI-aware metrics.
#[cfg(target_os = "windows")]
use eframe::egui::{
    FontDefinitions, FontData
};

use eframe::egui::{
    self, style::ScrollStyle, Color32, CornerRadius, FontId, Stroke, FontFamily, TextStyle, Theme, Visuals
};

use std::collections::BTreeMap;
// ----------------------------------------------------------------------------------------------
// Accent colour (Windows "Accent color" setting)

#[derive(Clone, Copy, PartialEq)]
pub struct Accent {
    pub base: Color32,
    /// Fill used in dark theme (WinUI: SystemAccentColorLight2).
    pub light: Color32,
    /// Fill used in light theme (WinUI: SystemAccentColorDark1).
    pub dark: Color32,
}

impl Accent {
    pub fn from_base(base: Color32) -> Self {
        Self { base, light: mix(base, Color32::WHITE, 0.30), dark: mix(base, Color32::BLACK, 0.15) }
    }
    pub fn fallback() -> Self {
        Self::from_base(Color32::from_rgb(0x00, 0x78, 0xD4))
    }
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

/// Reads the accent colour the user picked in Windows settings. None on other platforms.
#[cfg(windows)]
pub fn system_accent() -> Option<Accent> {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Accent")
        .ok()?;
    let abgr: u32 = key.get_value("AccentColorMenu").ok()?;
    let base = Color32::from_rgb((abgr & 0xFF) as u8, ((abgr >> 8) & 0xFF) as u8, ((abgr >> 16) & 0xFF) as u8);
    let mut accent = Accent::from_base(base);
    // AccentPalette: 8 x RGBA -> Light3, Light2, Light1, Base, Dark1, Dark2, Dark3, (unused)
    if let Ok(raw) = key.get_raw_value("AccentPalette") {
        let b = raw.bytes.to_vec();
        if b.len() >= 32 {
            let at = |i: usize| Color32::from_rgb(b[i * 4], b[i * 4 + 1], b[i * 4 + 2]);
            accent.light = at(1);
            accent.dark = at(4);
        }
    }
    Some(accent)
}
#[cfg(not(windows))]
pub fn system_accent() -> Option<Accent> {
    None
}

/// Windows "Accessibility > Text size" factor (1.0 – 2.25).
#[cfg(windows)]
pub fn system_text_scale() -> f32 {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Accessibility")
        .ok()
        .and_then(|k| k.get_value::<u32, _>("TextScaleFactor").ok())
        .map(|v| (v as f32 / 100.0).clamp(1.0, 2.25))
        .unwrap_or(1.0)
}
#[cfg(not(windows))]
pub fn system_text_scale() -> f32 {
    1.0
}

/// False when Windows has transparency effects switched off ("Transparency effects" setting) or
/// Battery Saver is on. Mica is not drawn then, so the app must paint opaque backgrounds itself.
#[cfg(windows)]
pub fn system_transparency() -> bool {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    #[repr(C)]
    struct PowerStatus {
        ac_line: u8,
        battery_flag: u8,
        battery_percent: u8,
        system_status_flag: u8, // 1 = Battery Saver is on
        life_time: u32,
        full_life_time: u32,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetSystemPowerStatus(s: *mut PowerStatus) -> i32;
    }
    let enabled = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .ok()
        .and_then(|k| k.get_value::<u32, _>("EnableTransparency").ok())
        .map(|v| v != 0)
        .unwrap_or(true);
    let mut st = PowerStatus {
        ac_line: 0,
        battery_flag: 0,
        battery_percent: 0,
        system_status_flag: 0,
        life_time: 0,
        full_life_time: 0,
    };
    let saver = unsafe { GetSystemPowerStatus(&mut st) } != 0 && st.system_status_flag & 1 != 0;
    enabled && !saver
}
#[cfg(not(windows))]
pub fn system_transparency() -> bool {
    true
}

/// Hides the window at once, so closing looks instant even if saving takes a moment.
#[cfg(windows)]
pub fn hide_window(window: &impl raw_window_handle::HasWindowHandle) {
    use raw_window_handle::RawWindowHandle;
    #[link(name = "user32")]
    extern "system" {
        fn ShowWindow(hwnd: isize, cmd: i32) -> i32;
    }
    if let Ok(handle) = window.window_handle() {
        if let RawWindowHandle::Win32(w) = handle.as_raw() {
            unsafe {
                ShowWindow(w.hwnd.get(), 0); // SW_HIDE
            }
        }
    }
}
#[cfg(not(windows))]
pub fn hide_window(_window: &impl raw_window_handle::HasWindowHandle) {}

/// Ends the process immediately. Normal exit runs every DLL's detach code (GPU drivers, DWM
/// helpers), which is what can stall or freeze a transparent window while it is being destroyed.
#[cfg(windows)]
pub fn terminate_now() -> ! {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> isize;
        fn TerminateProcess(h: isize, code: u32) -> i32;
    }
    unsafe {
        TerminateProcess(GetCurrentProcess(), 0);
    }
    std::process::exit(0)
}
#[cfg(not(windows))]
pub fn terminate_now() -> ! {
    std::process::exit(0)
}

// ----------------------------------------------------------------------------------------------
// Mica

/// Applies Mica Alt (Win 11 22H2+), falling back to Mica (Win 11 21H2). Returns false if unavailable.
#[cfg(windows)]
pub fn apply_backdrop(window: &impl raw_window_handle::HasWindowHandle, dark: bool) -> bool {
    window_vibrancy::apply_tabbed(window, Some(dark)).is_ok() || window_vibrancy::apply_mica(window, Some(dark)).is_ok()
}
#[cfg(not(windows))]
pub fn apply_backdrop(_window: &impl raw_window_handle::HasWindowHandle, _dark: bool) -> bool {
    false
}

// ----------------------------------------------------------------------------------------------
// Palette (Fluent 2 tokens, approximated)

#[derive(Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    pub mica: bool,
    pub accent: Color32,
    pub on_accent: Color32,
    pub base: Color32,       // window background (only painted when Mica is unavailable)
    pub layer: Color32,      // content card / active tab
    pub flyout: Color32,     // menus, dialogs
    pub text: Color32,
    pub text_secondary: Color32,
    pub text_disabled: Color32,
    pub subtle_hover: Color32,
    pub subtle_pressed: Color32,
    pub control: Color32,
    pub control_hover: Color32,
    pub control_stroke: Color32,
    pub divider: Color32,
    pub selected: Color32,
    pub selected_hover: Color32,
    /// Accent-tinted fill for list rows / navigation items under the pointer.
    pub hover: Color32,
    pub danger: Color32,
    pub danger_bg: Color32,
}

fn a(c: u8, alpha: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(c, c, c, (alpha * 255.0).round() as u8)
}
fn with_alpha(c: Color32, alpha: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (alpha * 255.0).round() as u8)
}
fn luminance(c: Color32) -> f32 {
    (0.299 * c.r() as f32 + 0.587 * c.g() as f32 + 0.114 * c.b() as f32) / 255.0
}

impl Palette {
    pub fn new(theme: Theme, accent: Accent, mica: bool) -> Self {
        let dark = theme == Theme::Dark;
        let acc = if dark { accent.light } else { accent.dark };
        let on_accent = if luminance(acc) > 0.6 { Color32::BLACK } else { Color32::WHITE };
        if dark {
            Self {
                dark, mica, accent: acc, on_accent,
                base: Color32::from_rgb(0x20, 0x20, 0x20),
                layer: if mica { a(0x3A, 0.30) } else { Color32::from_rgb(0x28, 0x28, 0x28) },
                flyout: Color32::from_rgb(0x2C, 0x2C, 0x2C),
                text: Color32::WHITE,
                text_secondary: a(255, 0.785),
                text_disabled: a(255, 0.36),
                subtle_hover: a(255, 0.10),
                subtle_pressed: a(255, 0.065),
                control: a(140, 0.06),
                control_hover: a(255, 0.13),
                control_stroke: a(190, 0.07),
                divider: a(255, 0.0835),
                selected: with_alpha(acc, 0.075),
                selected_hover: with_alpha(acc, 0.09),
                hover: with_alpha(acc, 0.06),
                danger: Color32::from_rgb(0xFF, 0x99, 0xA4),
                danger_bg: Color32::from_rgb(0x44, 0x27, 0x26),
            }
        } else {
            Self {
                dark, mica, accent: acc, on_accent,
                base: Color32::from_rgb(215, 215, 215),
                layer: if mica { Color32::from_rgba_unmultiplied(215, 215, 215, 200) } else { Color32::from_rgb(0xFB, 0xFB, 0xFB) },
                flyout: Color32::from_rgb(245, 245, 245),
                text: Color32::from_rgb(0x1A, 0x1A, 0x1A),
                text_secondary: a(0, 0.606),
                text_disabled: a(0, 0.36),
                subtle_hover: a(0, 0.075),
                subtle_pressed: a(0, 0.05),
                control: a(215, 0.5),
                control_hover: a(0xF9, 0.50),
                control_stroke: a(0, 0.0578),
                divider: a(0, 0.15),
                selected: with_alpha(acc, 0.12),
                selected_hover: with_alpha(acc, 0.15),
                hover: with_alpha(acc, 0.10),
                danger: Color32::from_rgb(0xC4, 0x2B, 0x1C),
                danger_bg: Color32::from_rgb(0xFD, 0xE7, 0xE9),
            }
        }
    }
}

// ----------------------------------------------------------------------------------------------
// Metrics: everything derives from the text size so icons, rows and paddings scale together.

#[derive(Clone, Copy)]
pub struct Metrics {
    pub font: f32,
    pub s: f32,
    pub row_h: f32,
    pub icon: f32,
    pub small_icon: f32,
    pub ctl_h: f32,
    pub tab_h: f32,
    pub nav_h: f32,
    pub pad: f32,
    pub radius: u8,
}

impl Metrics {
    pub fn new(font: f32, ppp: f32) -> Self {
        let snap = |v: f32| (v * ppp).round() / ppp; // align to physical pixels so SVGs stay crisp
        Self {
            font,
            s: font / 14.0,
            row_h: snap(font * 2.35),
            icon: snap(font * 1.35),
            small_icon: snap(font * 0.95),
            ctl_h: snap(font * 2.4),
            tab_h: snap(font * 2.6),
            nav_h: snap(font * 2.5),
            pad: snap(font * 0.6),
            radius: ((font * 0.3).round() as u8).max(3),
        }
    }
}

// ----------------------------------------------------------------------------------------------
// Fonts + style

pub fn install_fonts(ctx: &egui::Context) {
    #[cfg(windows)]
    {
        let dir = std::env::var_os("WINDIR").map(std::path::PathBuf::from).unwrap_or_else(|| "C:\\Windows".into());
        if let Ok(bytes) = std::fs::read(dir.join("Fonts").join("segoeui.ttf")) {
            let mut fonts = FontDefinitions::default();
            fonts.font_data.insert("segoe".to_owned(), FontData::from_owned(bytes).into());
            fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "segoe".to_owned());
            ctx.set_fonts(fonts);
        }
    }
    #[cfg(not(windows))]
    let _ = ctx;
}

pub fn apply_style(ctx: &egui::Context, m: &Metrics, accent: Accent) {
    let s = m.s;
    let font = m.font;
    ctx.all_styles_mut(|style| {
        let ts: BTreeMap<TextStyle, FontId> = BTreeMap::from([
            (TextStyle::Small, FontId::new(font * 0.86, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(font, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(font, FontFamily::Proportional)),
            (TextStyle::Heading, FontId::new(font * 1.45, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(font * 0.95, FontFamily::Monospace)),
        ]);
        style.text_styles = ts;
        style.override_font_id = None;
        style.spacing.item_spacing = egui::vec2(8.0 * s, 6.0 * s);
        style.spacing.button_padding = egui::vec2(12.0 * s, 5.0 * s);
        style.spacing.interact_size = egui::vec2(32.0 * s, m.ctl_h);
        style.spacing.menu_margin = egui::Margin::same((4.0 * s) as i8);
        style.spacing.window_margin = egui::Margin::same((20.0 * s) as i8);
        style.spacing.scroll = ScrollStyle::floating();
        style.spacing.scroll.bar_width = 6.0 * s;
        style.spacing.scroll.floating_width = 3.0 * s;
        style.interaction.tooltip_delay = 0.5;
    });
    for theme in [Theme::Dark, Theme::Light] {
        let pal = Palette::new(theme, accent, true);
        ctx.set_visuals_of(theme, visuals(&pal, m));
    }
}

/// One shadow for every popup: menus, dialogs, tooltips.
pub fn popup_shadow(dark: bool) -> egui::Shadow {
    // This is broken; While working on tiny popups such as the close buttons, it fails to draw proper shadows behind large popups such as the property window.
    egui::Shadow {
        offset: [0, 8],
        blur: 64,
        spread: 0,
        color: Color32::from_black_alpha(if dark { 115 } else { 42 }),
    }
}

fn visuals(p: &Palette, m: &Metrics) -> Visuals {
    let mut v = if p.dark { Visuals::dark() } else { Visuals::light() };
    let r = CornerRadius::same(m.radius);
    v.panel_fill = Color32::TRANSPARENT;
    v.window_fill = p.flyout;
    v.window_stroke = Stroke::new(1.0_f32, p.divider);
    v.window_corner_radius = CornerRadius::same(m.radius * 2);
    v.menu_corner_radius = CornerRadius::same(m.radius * 2);
    let shadow = popup_shadow(p.dark);
    v.window_shadow = shadow;
    v.popup_shadow = shadow;
    v.faint_bg_color = p.subtle_hover;
    v.extreme_bg_color = p.selected_hover;
    v.selection.bg_fill = with_alpha(p.accent, 0.40);
    v.selection.stroke = Stroke::new(1.0_f32, p.accent);
    v.hyperlink_color = p.accent;
    v.override_text_color = Some(p.text);

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = Color32::TRANSPARENT;
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, p.divider);
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, p.text);
    w.noninteractive.corner_radius = r;
    w.inactive.bg_fill = p.control;
    w.inactive.weak_bg_fill = p.control;
    w.inactive.bg_stroke = Stroke::new(1.0_f32, p.control_stroke);
    w.inactive.fg_stroke = Stroke::new(1.0_f32, p.text);
    w.inactive.corner_radius = r;
    w.hovered.bg_fill = p.control_hover;
    w.hovered.weak_bg_fill = p.control_hover;
    w.hovered.bg_stroke = Stroke::new(1.0_f32, p.control_stroke);
    w.hovered.fg_stroke = Stroke::new(1.0_f32, p.text);
    w.hovered.corner_radius = r;
    w.hovered.expansion = 0.0;
    w.active.bg_fill = p.subtle_pressed;
    w.active.weak_bg_fill = p.subtle_pressed;
    w.active.bg_stroke = Stroke::new(1.0_f32, p.control_stroke);
    w.active.fg_stroke = Stroke::new(1.0_f32, p.text_secondary);
    w.active.corner_radius = r;
    w.active.expansion = 0.0;
    w.open = w.hovered;
    v
}

/// Undecorated windows lose the DWM shadow and rounded corners; ask DWM to restore both.
#[cfg(windows)]
pub fn style_frameless(window: &impl raw_window_handle::HasWindowHandle) {
    use raw_window_handle::RawWindowHandle;
    #[repr(C)]
    struct Margins {
        left: i32,
        right: i32,
        top: i32,
        bottom: i32,
    }
    #[link(name = "dwmapi")]
    extern "system" {
        fn DwmExtendFrameIntoClientArea(hwnd: isize, margins: *const Margins) -> i32;
        fn DwmSetWindowAttribute(hwnd: isize, attr: u32, value: *const core::ffi::c_void, size: u32) -> i32;
    }
    if let Ok(handle) = window.window_handle() {
        if let RawWindowHandle::Win32(w) = handle.as_raw() {
            let hwnd = w.hwnd.get();
            unsafe {
                let margins = Margins { left: 1, right: 1, top: 1, bottom: 1 };
                DwmExtendFrameIntoClientArea(hwnd, &margins);
                let round: i32 = 2; // DWMWCP_ROUND
                DwmSetWindowAttribute(hwnd, 33, &round as *const i32 as *const core::ffi::c_void, 4);
            }
        }
    }
}
#[cfg(not(windows))]
pub fn style_frameless(_window: &impl raw_window_handle::HasWindowHandle) {}
