//! Follows the Omarchy desktop theme: reads the active palette, maps it onto the GPUI Kit
//! theme (UI colours, editor syntax highlighting, fonts), and re-applies it live when the
//! user switches themes or fonts.
//!
//! `omarchy theme set` stages the new theme and then replaces
//! `~/.local/state/omarchy/current/theme/` wholesale (`rm -rf` + `mv`), writing
//! `current/theme.name` last. So we watch the *parent* directory, debounce, and re-read
//! `theme/colors.toml`. `omarchy font set` rewrites `~/.config/fontconfig/fonts.conf`, and
//! the font is resolved the same way Omarchy does: `fc-match monospace`.
//!
//! Without Omarchy the app keeps GPUI Kit's default light/dark themes, following the
//! system appearance.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use gpui_kit::component::theme::{Theme, ThemeConfig, ThemeMode, ThemeRegistry};
use gpui_kit::*;
use notify::{RecursiveMode, Watcher as _};
use serde::Deserialize;
use serde_json::{Value, json};

/// Points the app at a specific theme directory instead of the active Omarchy theme,
/// e.g. `/usr/share/omarchy/themes/catppuccin-latte`. For previewing and development.
pub const THEME_DIR_OVERRIDE: &str = "COURIER_THEME_DIR";

const DEBOUNCE: Duration = Duration::from_millis(250);

// MARK: Palette

/// An sRGB colour parsed from `#rrggbb`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub fn parse(hex: &str) -> Result<Self> {
        let hex = hex.trim().trim_start_matches('#');
        if hex.len() != 6 && hex.len() != 8 {
            return Err(anyhow!("expected #rrggbb, got #{hex}"));
        }
        let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).with_context(|| format!("bad colour #{hex}"));
        Ok(Self(channel(0)?, channel(2)?, channel(4)?))
    }

    pub fn hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.0, self.1, self.2)
    }

    /// `#rrggbbaa` with the given opacity.
    pub fn alpha(self, alpha: f32) -> String {
        format!("{}{:02x}", self.hex(), (alpha.clamp(0.0, 1.0) * 255.0).round() as u8)
    }

    /// Linear blend: `t = 0` is `self`, `t = 1` is `other`.
    pub fn mix(self, other: Rgb, t: f32) -> Rgb {
        let lerp = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
        Rgb(lerp(self.0, other.0), lerp(self.1, other.1), lerp(self.2, other.2))
    }

    /// WCAG relative luminance.
    pub fn luminance(self) -> f32 {
        let linear = |c: u8| {
            let c = c as f32 / 255.0;
            if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * linear(self.0) + 0.7152 * linear(self.1) + 0.0722 * linear(self.2)
    }

    /// WCAG contrast ratio, 1.0 to 21.0.
    pub fn contrast(self, other: Rgb) -> f32 {
        let (a, b) = (self.luminance(), other.luminance());
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }
}

/// Whichever candidate reads best on `background`.
fn most_legible(background: Rgb, candidates: &[Rgb]) -> Rgb {
    candidates
        .iter()
        .copied()
        .max_by(|a, b| a.contrast(background).total_cmp(&b.contrast(background)))
        .expect("at least one candidate")
}

/// The first candidate that meets `min` contrast on `background`, else the most legible.
fn first_legible(background: Rgb, min: f32, candidates: &[Rgb]) -> Rgb {
    candidates
        .iter()
        .copied()
        .find(|c| c.contrast(background) >= min)
        .unwrap_or_else(|| most_legible(background, candidates))
}

#[derive(Deserialize)]
struct ColorsToml {
    #[serde(default)]
    mode: Option<String>,
    accent: String,
    selection: String,
    muted: String,
    background: String,
    dark_background: String,
    darker_background: String,
    lighter_background: String,
    foreground: String,
    dark_foreground: String,
    light_foreground: String,
    bright_foreground: String,
    red: String,
    yellow: String,
    orange: Option<String>,
    green: String,
    cyan: String,
    blue: String,
    magenta: String,
    bright_red: Option<String>,
    bright_yellow: Option<String>,
    bright_green: Option<String>,
    bright_cyan: Option<String>,
    bright_blue: Option<String>,
    bright_magenta: Option<String>,
}

/// The parts of an Omarchy `colors.toml` this app uses.
#[derive(Clone, Debug, PartialEq)]
pub struct Palette {
    pub name: String,
    pub dark: bool,
    pub accent: Rgb,
    pub selection: Rgb,
    pub muted: Rgb,
    pub background: Rgb,
    pub dark_background: Rgb,
    pub darker_background: Rgb,
    pub lighter_background: Rgb,
    pub foreground: Rgb,
    pub dark_foreground: Rgb,
    pub light_foreground: Rgb,
    pub bright_foreground: Rgb,
    pub red: Rgb,
    pub yellow: Rgb,
    pub orange: Rgb,
    pub green: Rgb,
    pub cyan: Rgb,
    pub blue: Rgb,
    pub magenta: Rgb,
    pub bright_red: Rgb,
    pub bright_yellow: Rgb,
    pub bright_green: Rgb,
    pub bright_cyan: Rgb,
    pub bright_blue: Rgb,
    pub bright_magenta: Rgb,
}

impl Palette {
    pub fn parse(name: &str, text: &str) -> Result<Self> {
        let raw: ColorsToml = toml::from_str(text).context("parsing colors.toml")?;
        let c = |hex: &str| Rgb::parse(hex);
        let background = c(&raw.background)?;
        let dark = match raw.mode.as_deref() {
            Some("light") => false,
            Some("dark") => true,
            // Older generated palettes may omit `mode`; judge by the background.
            _ => background.luminance() < 0.4,
        };
        let yellow = c(&raw.yellow)?;
        let red = c(&raw.red)?;
        let bright = |value: &Option<String>, base: Rgb| -> Result<Rgb> {
            value.as_deref().map(Rgb::parse).transpose().map(|v| v.unwrap_or(base))
        };
        let (green, cyan, blue, magenta) = (c(&raw.green)?, c(&raw.cyan)?, c(&raw.blue)?, c(&raw.magenta)?);
        Ok(Self {
            name: name.to_string(),
            dark,
            accent: c(&raw.accent)?,
            selection: c(&raw.selection)?,
            muted: c(&raw.muted)?,
            background,
            dark_background: c(&raw.dark_background)?,
            darker_background: c(&raw.darker_background)?,
            lighter_background: c(&raw.lighter_background)?,
            foreground: c(&raw.foreground)?,
            dark_foreground: c(&raw.dark_foreground)?,
            light_foreground: c(&raw.light_foreground)?,
            bright_foreground: c(&raw.bright_foreground)?,
            orange: raw.orange.as_deref().map(Rgb::parse).transpose()?.unwrap_or_else(|| red.mix(yellow, 0.5)),
            bright_red: bright(&raw.bright_red, red)?,
            bright_yellow: bright(&raw.bright_yellow, yellow)?,
            bright_green: bright(&raw.bright_green, green)?,
            bright_cyan: bright(&raw.bright_cyan, cyan)?,
            bright_blue: bright(&raw.bright_blue, blue)?,
            bright_magenta: bright(&raw.bright_magenta, magenta)?,
            red,
            yellow,
            green,
            cyan,
            blue,
            magenta,
        })
    }

    /// Loads `colors.toml` from a theme directory, naming it from `../theme.name` when present.
    pub fn load(theme_dir: &Path) -> Result<Self> {
        let colors = theme_dir.join("colors.toml");
        let text = std::fs::read_to_string(&colors).with_context(|| format!("reading {}", colors.display()))?;
        let name = std::fs::read_to_string(theme_dir.with_file_name("theme.name"))
            .ok()
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .or_else(|| theme_dir.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "omarchy".into());
        Self::parse(&name, &text)
    }

    /// Secondary text: Omarchy's `muted` where it is readable, else the dimmed foreground.
    fn secondary_text(&self, on: Rgb) -> Rgb {
        first_legible(on, 4.5, &[self.muted, self.dark_foreground, self.foreground])
    }

    /// Text on a filled `fill` colour (buttons, badges).
    fn text_on(&self, fill: Rgb) -> Rgb {
        most_legible(fill, &[self.darker_background, self.bright_foreground, Rgb(0, 0, 0), Rgb(255, 255, 255)])
    }

    /// Hover and pressed shades of a fill, moving toward the foreground.
    fn hover(&self, fill: Rgb) -> String {
        fill.mix(self.foreground, 0.10).hex()
    }

    fn active(&self, fill: Rgb) -> String {
        fill.mix(self.foreground, 0.18).hex()
    }

    /// A GPUI Kit theme config for this palette.
    pub fn theme_config(&self, font_family: Option<&str>) -> Value {
        let p = self;
        let border = p.background.mix(p.foreground, if p.dark { 0.16 } else { 0.18 });
        let raised = p.lighter_background;
        let secondary_text = p.secondary_text(p.background);
        let semantic = |name: &str, color: Rgb, config: &mut serde_json::Map<String, Value>| {
            config.insert(format!("{name}.background"), json!(color.hex()));
            config.insert(format!("{name}.foreground"), json!(p.text_on(color).hex()));
            config.insert(format!("{name}.hover.background"), json!(p.hover(color)));
            config.insert(format!("{name}.active.background"), json!(p.active(color)));
            config.insert(format!("button.{name}.background"), json!(color.hex()));
            config.insert(format!("button.{name}.foreground"), json!(p.text_on(color).hex()));
            config.insert(format!("button.{name}.hover.background"), json!(p.hover(color)));
            config.insert(format!("button.{name}.active.background"), json!(p.active(color)));
        };

        let mut colors = serde_json::Map::new();
        for (key, value) in [
            ("background", p.background.hex()),
            ("foreground", p.foreground.hex()),
            ("border", border.hex()),
            ("input.border", border.hex()),
            ("window.border", border.hex()),
            ("ring", p.accent.hex()),
            ("caret", p.accent.hex()),
            ("selection.background", p.selection.alpha(if p.dark { 0.85 } else { 0.6 })),
            ("link", p.accent.hex()),
            ("link.hover", p.accent.mix(p.foreground, 0.2).hex()),
            ("link.active", p.accent.mix(p.foreground, 0.3).hex()),
            // Hover wash for menu items, list rows and similar.
            ("accent.background", raised.hex()),
            ("accent.foreground", p.bright_foreground.hex()),
            ("muted.background", raised.hex()),
            ("muted.foreground", secondary_text.hex()),
            ("popover.background", if p.dark { p.dark_background } else { p.background }.hex()),
            ("popover.foreground", p.foreground.hex()),
            ("overlay", p.darker_background.alpha(0.6)),
            ("secondary.background", raised.hex()),
            ("secondary.foreground", p.foreground.hex()),
            ("secondary.hover.background", p.hover(raised)),
            ("secondary.active.background", p.active(raised)),
            ("button.background", p.background.hex()),
            ("button.foreground", p.foreground.hex()),
            ("button.hover.background", raised.hex()),
            ("button.active.background", p.active(raised)),
            ("button.secondary.background", raised.hex()),
            ("button.secondary.foreground", p.foreground.hex()),
            ("button.secondary.hover.background", p.hover(raised)),
            ("button.secondary.active.background", p.active(raised)),
            ("list.background", p.background.hex()),
            ("list.even.background", p.background.hex()),
            ("list.head.background", p.dark_background.hex()),
            ("list.hover.background", raised.hex()),
            ("list.active.background", p.accent.alpha(0.18)),
            ("list.active.border", p.accent.hex()),
            ("table.background", p.background.hex()),
            ("table.even.background", p.background.mix(raised, 0.35).hex()),
            ("table.head.background", p.dark_background.hex()),
            ("table.head.foreground", secondary_text.hex()),
            ("table.hover.background", raised.hex()),
            ("table.active.background", p.accent.alpha(0.18)),
            ("table.active.border", p.accent.hex()),
            ("table.row.border", border.hex()),
            ("sidebar.background", p.dark_background.hex()),
            ("sidebar.foreground", p.foreground.hex()),
            ("sidebar.border", border.hex()),
            ("sidebar.accent.background", raised.hex()),
            ("sidebar.accent.foreground", p.bright_foreground.hex()),
            ("sidebar.primary.background", p.accent.hex()),
            ("sidebar.primary.foreground", p.text_on(p.accent).hex()),
            ("tab.background", p.background.hex()),
            ("tab.foreground", p.secondary_text(p.background).hex()),
            ("tab.active.background", raised.hex()),
            ("tab.active.foreground", p.bright_foreground.hex()),
            ("tab_bar.background", p.background.hex()),
            ("tab_bar.segmented.background", p.dark_background.hex()),
            ("title_bar.background", p.dark_background.hex()),
            ("title_bar.border", border.hex()),
            ("status_bar.background", p.dark_background.hex()),
            ("status_bar.border", border.hex()),
            ("scrollbar.background", p.background.alpha(0.0)),
            ("scrollbar.thumb.background", p.muted.alpha(0.55)),
            ("scrollbar.thumb.hover.background", p.muted.hex()),
            ("switch.background", raised.hex()),
            ("switch.thumb.background", p.foreground.hex()),
            ("slider.background", p.accent.hex()),
            ("slider.thumb.background", p.foreground.hex()),
            ("progress.bar.background", p.accent.hex()),
            ("skeleton.background", raised.hex()),
            ("accordion.background", p.background.hex()),
            ("group_box.background", p.dark_background.hex()),
            ("group_box.foreground", p.foreground.hex()),
            ("group_box.title.foreground", secondary_text.hex()),
            ("description_list.label.background", p.dark_background.hex()),
            ("description_list.label.foreground", secondary_text.hex()),
            ("drag.border", p.accent.hex()),
            ("drop_target.background", p.accent.alpha(0.15)),
            ("tiles.background", p.dark_background.hex()),
            ("chart_bullish", p.green.hex()),
            ("chart_bearish", p.red.hex()),
            ("base.red", p.red.hex()),
            ("base.red.light", p.bright_red.hex()),
            ("base.yellow", p.yellow.hex()),
            ("base.yellow.light", p.bright_yellow.hex()),
            ("base.green", p.green.hex()),
            ("base.green.light", p.bright_green.hex()),
            ("base.cyan", p.cyan.hex()),
            ("base.cyan.light", p.bright_cyan.hex()),
            ("base.blue", p.blue.hex()),
            ("base.blue.light", p.bright_blue.hex()),
            ("base.magenta", p.magenta.hex()),
            ("base.magenta.light", p.bright_magenta.hex()),
        ] {
            colors.insert(key.to_string(), json!(value));
        }
        semantic("primary", p.accent, &mut colors);
        // Status colours double as text colours in the app (e.g. "200 OK" in green), so
        // nudge each toward the foreground until it reads on the background.
        semantic("danger", p.readable_status(p.red, p.bright_red), &mut colors);
        semantic("warning", p.readable_status(p.yellow, p.bright_yellow), &mut colors);
        semantic("success", p.readable_status(p.green, p.bright_green), &mut colors);
        semantic("info", p.readable_status(p.blue, p.bright_blue), &mut colors);
        colors.remove("primary.hover.background");
        colors.insert("primary.hover.background".into(), json!(p.hover(p.accent)));

        let style = |color: Rgb| json!({ "color": color.hex() });
        let italic = |color: Rgb| json!({ "color": color.hex(), "font_style": "italic" });
        let syntax: serde_json::Map<String, Value> = [
            ("attribute", style(p.cyan)),
            ("boolean", style(p.orange)),
            ("comment", italic(p.muted)),
            ("comment.doc", italic(p.muted)),
            ("constant", style(p.orange)),
            ("constructor", style(p.yellow)),
            ("embedded", style(p.foreground)),
            ("emphasis", json!({ "font_style": "italic" })),
            ("emphasis.strong", json!({ "font_weight": 700 })),
            ("enum", style(p.yellow)),
            ("function", style(p.blue)),
            ("keyword", style(p.magenta)),
            ("label", style(p.yellow)),
            ("link_text", style(p.accent)),
            ("link_uri", style(p.cyan)),
            ("number", style(p.orange)),
            ("operator", style(p.cyan)),
            ("property", style(p.blue)),
            ("punctuation", style(p.dark_foreground)),
            ("punctuation.bracket", style(p.dark_foreground)),
            ("punctuation.delimiter", style(p.dark_foreground)),
            ("punctuation.list_marker", style(p.accent)),
            ("punctuation.special", style(p.cyan)),
            ("string", style(p.green)),
            ("string.escape", style(p.cyan)),
            ("string.regex", style(p.cyan)),
            ("string.special", style(p.cyan)),
            ("string.special.symbol", style(p.cyan)),
            ("tag", style(p.red)),
            ("text.literal", style(p.green)),
            ("title", json!({ "color": p.accent.hex(), "font_weight": 700 })),
            ("type", style(p.cyan)),
            ("variable", style(p.foreground)),
            ("variable.special", style(p.red)),
            ("variant", style(p.yellow)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();

        let mut highlight: serde_json::Map<String, Value> = [
            ("editor.background", p.background.hex()),
            ("editor.foreground", p.foreground.hex()),
            ("editor.active_line.background", raised.alpha(if p.dark { 0.55 } else { 0.7 })),
            ("editor.line_number", p.muted.hex()),
            ("editor.active_line_number", p.foreground.hex()),
            ("editor.invisible", p.muted.alpha(0.45)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), json!(v)))
        .collect();
        for (status, color) in [("error", p.red), ("warning", p.yellow), ("info", p.blue), ("success", p.green), ("hint", p.magenta)] {
            highlight.insert(status.into(), json!(color.hex()));
            highlight.insert(format!("{status}.background"), json!(color.alpha(0.15)));
            highlight.insert(format!("{status}.border"), json!(color.hex()));
        }
        highlight.insert("syntax".into(), Value::Object(syntax));

        let mut config = json!({
            "is_default": false,
            "name": format!("Omarchy · {}", p.name),
            "mode": if p.dark { "dark" } else { "light" },
            "colors": colors,
            "highlight": highlight,
        });
        if let Some(font) = font_family {
            config["font.family"] = json!(font);
            config["mono_font.family"] = json!(font);
        }
        config
    }

    fn readable_status(&self, base: Rgb, bright: Rgb) -> Rgb {
        let mut color = first_legible(self.background, 4.5, &[base, bright]);
        for step in 1..=5 {
            if color.contrast(self.background) >= 4.5 {
                break;
            }
            color = base.mix(self.foreground, step as f32 * 0.15);
        }
        color
    }
}

// MARK: Locations

/// The active Omarchy theme directory (or the override), if there is one.
pub fn theme_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(THEME_DIR_OVERRIDE) {
        return Some(PathBuf::from(dir));
    }
    // `omarchy-theme-set` uses `$HOME/.local/state` directly, not `$XDG_STATE_HOME`.
    let dir = PathBuf::from(std::env::var_os("HOME")?).join(".local/state/omarchy/current/theme");
    dir.parent().is_some_and(Path::is_dir).then_some(dir)
}

/// The desktop monospace font, resolved the way `omarchy font current` does.
pub fn desktop_font() -> Option<String> {
    let output = Command::new("fc-match").args(["monospace", "-f", "%{family}"]).output().ok()?;
    let family = String::from_utf8(output.stdout).ok()?;
    let family = family.lines().next()?.split(',').next()?.trim().to_string();
    (!family.is_empty()).then_some(family)
}

// MARK: Applying

/// Loads the palette for `dir` and applies it, or restores the default themes when there
/// is no usable Omarchy theme. Returns the applied theme name, or the reason it fell back.
pub fn apply(dir: Option<&Path>, cx: &mut App) -> Result<String, String> {
    let palette = match dir.map(Palette::load) {
        Some(Ok(palette)) => palette,
        Some(Err(e)) => {
            restore_defaults(cx);
            return Err(format!("{e:#}"));
        }
        None => {
            restore_defaults(cx);
            return Err("no Omarchy theme found".into());
        }
    };
    let font = desktop_font();
    let config: ThemeConfig = match serde_json::from_value(palette.theme_config(font.as_deref())) {
        Ok(config) => config,
        Err(e) => {
            restore_defaults(cx);
            return Err(format!("building theme: {e}"));
        }
    };
    let mode = config.mode;
    let config = Rc::new(config);
    {
        let theme = Theme::global_mut(cx);
        if mode.is_dark() {
            theme.dark_theme = config;
        } else {
            theme.light_theme = config;
        }
    }
    Theme::change(mode, None, cx);
    cx.refresh_windows();
    Ok(palette.name)
}

fn restore_defaults(cx: &mut App) {
    let registry = ThemeRegistry::global(cx);
    let (light, dark) = (registry.default_light_theme().clone(), registry.default_dark_theme().clone());
    let theme = Theme::global_mut(cx);
    theme.light_theme = light;
    theme.dark_theme = dark;
    let mode = match cx.window_appearance() {
        WindowAppearance::Dark | WindowAppearance::VibrantDark => ThemeMode::Dark,
        WindowAppearance::Light | WindowAppearance::VibrantLight => ThemeMode::Light,
    };
    Theme::change(mode, None, cx);
    cx.refresh_windows();
}

// MARK: Watching

/// Watches the directories whose changes mean the theme or font changed. Events arrive on
/// the returned channel; the watcher stops when it is dropped.
pub fn watch(theme_dir: &Path, font_config_dir: Option<&Path>) -> Result<(notify::RecommendedWatcher, async_channel::Receiver<()>)> {
    let (tx, rx) = async_channel::unbounded();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if event.is_ok_and(|e| !matches!(e.kind, notify::EventKind::Access(_))) {
            let _ = tx.try_send(());
        }
    })?;
    // The theme directory itself is replaced on every switch, so watch its parent.
    let parent = theme_dir.parent().context("theme directory has no parent")?;
    watcher.watch(parent, RecursiveMode::NonRecursive)?;
    if let Some(dir) = font_config_dir.filter(|d| d.is_dir()) {
        watcher.watch(dir, RecursiveMode::NonRecursive)?;
    }
    Ok((watcher, rx))
}

/// Applies the Omarchy theme, or the defaults when following it is turned off.
pub fn set_following(follow: bool, cx: &mut App) {
    if follow {
        match apply(theme_dir().as_deref(), cx) {
            Ok(name) => eprintln!("using Omarchy theme {name}"),
            Err(reason) => eprintln!("using default theme: {reason}"),
        }
    } else {
        restore_defaults(cx);
    }
}

fn following(cx: &App) -> bool {
    crate::settings::AppSettings::get(cx).follow_omarchy_theme
}

/// Applies the current theme and keeps following changes for the life of the app.
pub fn init(cx: &mut App) {
    let dir = theme_dir();
    set_following(following(cx), cx);
    let Some(dir) = dir else {
        return;
    };
    let font_config = std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/fontconfig"));
    let (watcher, events) = match watch(&dir, font_config.as_deref()) {
        Ok(watching) => watching,
        Err(e) => {
            eprintln!("not following theme changes: {e:#}");
            return;
        }
    };
    cx.spawn(async move |cx| {
        let _watcher = watcher;
        while events.recv().await.is_ok() {
            // A switch is a burst of removes, renames and writes; apply once it settles.
            loop {
                cx.background_executor().timer(DEBOUNCE).await;
                if events.is_empty() {
                    break;
                }
                while events.try_recv().is_ok() {}
            }
            let dir = dir.clone();
            cx.update(|cx| {
                if !following(cx) {
                    return;
                }
                match apply(Some(&dir), cx) {
                    Ok(name) => eprintln!("theme changed to {name}"),
                    Err(reason) => eprintln!("theme change not applied: {reason}"),
                }
            });
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    // `gpui_kit::*` (via `super::*`) exports GPUI's test macro; keep Rust's for `#[test]`.
    #[allow(unused_imports)]
    use core::prelude::v1::test;

    const GATEPUNK: &str = r##"
mode = "dark"
accent    = "#47c57a"
selection = "#1c4e35"
muted     = "#566b61"
background         = "#0c1c18"
dark_background    = "#081310"
darker_background  = "#050d0b"
lighter_background = "#173029"
foreground        = "#c2d5c8"
dark_foreground   = "#6f887b"
light_foreground  = "#d9e9dd"
bright_foreground = "#eefaf1"
hyprland_active_border   = "rgba(47c57aee) rgba(aaeeb9ee) 45deg"
red     = "#d95f4a"
yellow  = "#d4b96a"
orange  = "#d1854a"
green   = "#47c57a"
cyan    = "#3fb8a6"
blue    = "#4f8ba8"
magenta = "#9b7fc4"
brown   = "#947457"
bright_red = "#ef8168"
"##;

    #[test]
    fn colour_math() {
        assert_eq!(Rgb::parse("#47c57a").unwrap(), Rgb(0x47, 0xc5, 0x7a));
        assert_eq!(Rgb::parse("0c1c18ff").unwrap().hex(), "#0c1c18");
        assert!(Rgb::parse("#abc").is_err());
        assert_eq!(Rgb(0, 0, 0).mix(Rgb(255, 255, 255), 0.5), Rgb(128, 128, 128));
        assert_eq!(Rgb(255, 0, 0).alpha(0.5), "#ff000080");
        assert!((Rgb(0, 0, 0).contrast(Rgb(255, 255, 255)) - 21.0).abs() < 0.01);
    }

    #[test]
    fn maps_a_palette_to_a_valid_gpui_theme() {
        let palette = Palette::parse("gatepunk", GATEPUNK).unwrap();
        assert!(palette.dark);
        assert_eq!(palette.bright_green, palette.green, "missing bright colours fall back");
        let value = palette.theme_config(Some("JetBrainsMono Nerd Font"));
        assert_eq!(value["colors"]["background"], "#0c1c18");
        assert_eq!(value["colors"]["sidebar.background"], "#081310");
        assert_eq!(value["colors"]["primary.background"], "#47c57a");
        assert_eq!(value["mono_font.family"], "JetBrainsMono Nerd Font");
        let config: ThemeConfig = serde_json::from_value(value).expect("GPUI Kit accepts the config");
        assert!(config.mode.is_dark());
        assert!(config.highlight.is_some());
    }

    fn assert_legible(theme: &str, what: &str, fg: &str, bg: &str, min: f32) {
        let (fg, bg) = (Rgb::parse(fg).unwrap(), Rgb::parse(bg).unwrap());
        let ratio = fg.contrast(bg);
        assert!(ratio >= min, "{theme}: {what} contrast {ratio:.2} < {min}");
    }

    /// Every theme installed on this machine (stock and user) maps to a config GPUI Kit
    /// accepts, and the colours this app derives stay readable. Skips where Omarchy isn't
    /// installed.
    #[test]
    fn installed_omarchy_themes_map_cleanly() {
        let mut dirs = Vec::new();
        for root in ["/usr/share/omarchy/themes".into(), std::env::var("HOME").unwrap_or_default() + "/.config/omarchy/themes"] {
            if let Ok(entries) = fs::read_dir(&root) {
                dirs.extend(entries.flatten().map(|e| e.path()).filter(|p| p.join("colors.toml").is_file()));
            }
        }
        if dirs.is_empty() {
            eprintln!("no Omarchy themes installed; skipping");
            return;
        }
        for dir in dirs {
            let name = dir.file_name().unwrap().to_string_lossy().into_owned();
            let palette = Palette::load(&dir).unwrap_or_else(|e| panic!("{name}: {e:#}"));
            let value = palette.theme_config(None);
            let colors = &value["colors"];
            let get = |key: &str| colors[key].as_str().unwrap_or_else(|| panic!("{name}: missing {key}")).to_string();
            serde_json::from_value::<ThemeConfig>(value.clone()).unwrap_or_else(|e| panic!("{name}: {e}"));

            assert_legible(&name, "primary button text", &get("primary.foreground"), &get("primary.background"), 3.0);
            assert_legible(&name, "secondary text", &get("muted.foreground"), &get("background"), 3.0);
            for status in ["danger", "warning", "success", "info"] {
                assert_legible(&name, status, &get(&format!("{status}.background")), &get("background"), 3.0);
            }
        }
    }

    #[gpui_kit::test]
    fn applies_the_palette_to_the_live_theme(cx: &mut gpui_kit::TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let theme = tmp.path().join("theme");
        fs::create_dir_all(&theme).unwrap();
        fs::write(theme.join("colors.toml"), GATEPUNK).unwrap();
        fs::write(tmp.path().join("theme.name"), "gatepunk").unwrap();

        cx.update(|cx| {
            gpui_kit::init(cx);
            assert_eq!(apply(Some(&theme), cx).as_deref(), Ok("gatepunk"));
            let applied = Theme::global(cx);
            assert!(applied.is_dark());
            assert_eq!(applied.background, gpui_kit::component::try_parse_color("#0c1c18").unwrap());
            assert_eq!(applied.sidebar, gpui_kit::component::try_parse_color("#081310").unwrap());

            // A broken theme falls back to the defaults instead of half-applying.
            fs::write(theme.join("colors.toml"), "mode = 'dark'").unwrap();
            assert!(apply(Some(&theme), cx).is_err());
            assert_ne!(Theme::global(cx).sidebar, gpui_kit::component::try_parse_color("#081310").unwrap());
        });
    }

    /// Mimics `omarchy theme set`: stage, `rm -rf` the old theme, `mv` the new one in,
    /// write theme.name. The watcher must report it.
    #[test]
    fn watcher_sees_an_omarchy_style_theme_switch() {
        let tmp = tempfile::tempdir().unwrap();
        let current = tmp.path().join("current");
        let theme = current.join("theme");
        fs::create_dir_all(&theme).unwrap();
        fs::write(theme.join("colors.toml"), GATEPUNK).unwrap();

        let (_watcher, events) = watch(&theme, None).unwrap();
        let next = current.join("next-theme");
        fs::create_dir_all(&next).unwrap();
        fs::write(next.join("colors.toml"), GATEPUNK.replace("#0c1c18", "#101010")).unwrap();
        fs::remove_dir_all(&theme).unwrap();
        fs::rename(&next, &theme).unwrap();
        fs::write(current.join("theme.name"), "other\n").unwrap();

        let mut seen = false;
        for _ in 0..200 {
            if events.try_recv().is_ok() {
                seen = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(seen, "no change event");
        let palette = Palette::load(&theme).unwrap();
        assert_eq!(palette.name, "other");
        assert_eq!(palette.background, Rgb(0x10, 0x10, 0x10));
    }
}
