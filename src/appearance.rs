use std::rc::Rc;

use anyhow::{Context as _, bail};
use gpui_kit::component::{Theme, ThemeConfig, ThemeMode, ThemeRegistry, highlighter::HighlightTheme};
use gpui_kit::{App, Global, Window};
use serde::{Deserialize, Serialize};

use crate::config::ConfigPaths;

const DEFAULT_LIGHT_THEME: &str = "Ayu Light";
const DEFAULT_DARK_THEME: &str = "Ayu Dark";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AppearanceMode {
    #[default]
    System,
    Light,
    Dark,
}

impl AppearanceMode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Appearance {
    mode: AppearanceMode,
    light_theme: String,
    dark_theme: String,
    #[serde(skip)]
    paths: Option<ConfigPaths>,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            mode: AppearanceMode::System,
            light_theme: DEFAULT_LIGHT_THEME.into(),
            dark_theme: DEFAULT_DARK_THEME.into(),
            paths: None,
        }
    }
}

impl Global for Appearance {}

impl Appearance {
    pub(crate) fn mode(cx: &App) -> AppearanceMode {
        cx.global::<Self>().mode
    }

    pub(crate) fn selected_theme(mode: ThemeMode, cx: &App) -> &str {
        let preferences = cx.global::<Self>();
        match mode {
            ThemeMode::Light => &preferences.light_theme,
            ThemeMode::Dark => &preferences.dark_theme,
        }
    }
}

pub(crate) fn init(cx: &mut App) {
    register_bundled(cx);
    let paths = ConfigPaths::discover().map_err(|error| log::warn!("Could not locate settings: {error:#}")).ok();
    cx.set_global(load(paths, cx));
}

fn register_bundled(cx: &mut App) {
    for (name, content) in crate::bundled_themes::THEMES {
        if let Err(error) = ThemeRegistry::global_mut(cx).load_themes_from_str(content) {
            log::error!("Could not load bundled theme {name}: {error:#}");
        }
    }
}

fn load(paths: Option<ConfigPaths>, cx: &App) -> Appearance {
    let mut preferences: Appearance = paths
        .as_ref()
        .and_then(|paths| {
            paths
                .read()
                .and_then(|value| value.map(serde_json::from_value).transpose().context("Could not parse appearance settings"))
                .map_err(|error| log::warn!("Could not load appearance settings: {error:#}"))
                .ok()
                .flatten()
        })
        .unwrap_or_default();
    preferences.paths = paths;
    for (mode, name, default) in [
        (ThemeMode::Light, &mut preferences.light_theme, DEFAULT_LIGHT_THEME),
        (ThemeMode::Dark, &mut preferences.dark_theme, DEFAULT_DARK_THEME),
    ] {
        if theme_config(mode, name, cx).is_err() {
            let registry = ThemeRegistry::global(cx);
            let fallback = theme_config(mode, default, cx).unwrap_or_else(|_| match mode {
                ThemeMode::Light => registry.default_light_theme().clone(),
                ThemeMode::Dark => registry.default_dark_theme().clone(),
            });
            log::warn!("Theme {name} is unavailable for {mode:?}; using {}", fallback.name);
            *name = fallback.name.to_string();
        }
    }
    preferences
}

fn theme_config(mode: ThemeMode, name: &str, cx: &App) -> anyhow::Result<Rc<ThemeConfig>> {
    let config = ThemeRegistry::global(cx)
        .themes()
        .get(name)
        .cloned()
        .with_context(|| format!("Theme {name} is unavailable"))?;
    if config.mode != mode {
        bail!("Theme {name} is not a {mode:?} theme");
    }
    Ok(config)
}

pub(crate) fn apply(window: &mut Window, cx: &mut App) {
    let preferences = cx.global::<Appearance>();
    let mode = match preferences.mode {
        AppearanceMode::System => window.appearance().into(),
        AppearanceMode::Light => ThemeMode::Light,
        AppearanceMode::Dark => ThemeMode::Dark,
    };
    let pair =
        theme_config(ThemeMode::Light, &preferences.light_theme, cx).and_then(|light| Ok((light, theme_config(ThemeMode::Dark, &preferences.dark_theme, cx)?)));
    let (light, dark) = match pair {
        Ok(pair) => pair,
        Err(error) => {
            log::error!("Could not apply appearance: {error:#}");
            return;
        }
    };
    Theme::update(cx, |theme| {
        theme.light_theme = light;
        theme.dark_theme = dark;
        // Omitted metrics inherit the previous theme unless explicitly reset.
        let defaults = Theme::default();
        theme.font_family = defaults.font_family;
        theme.font_size = defaults.font_size;
        theme.mono_font_family = defaults.mono_font_family;
        theme.mono_font_size = defaults.mono_font_size;
        theme.radius = defaults.radius;
        theme.radius_lg = defaults.radius_lg;
        theme.shadow = defaults.shadow;
        theme.highlight_theme = if mode.is_dark() {
            HighlightTheme::default_dark()
        } else {
            HighlightTheme::default_light()
        };
        let config = if mode.is_dark() {
            theme.dark_theme.clone()
        } else {
            theme.light_theme.clone()
        };
        theme.apply_config(&config);
    });
}

pub(crate) fn sync_system(window: &mut Window, cx: &mut App) {
    if Appearance::mode(cx) == AppearanceMode::System {
        apply(window, cx);
    }
}

pub(crate) fn select_mode(mode: AppearanceMode, window: &mut Window, cx: &mut App) -> anyhow::Result<()> {
    cx.global_mut::<Appearance>().mode = mode;
    apply(window, cx);
    save(cx)
}

pub(crate) fn select_theme(mode: ThemeMode, name: &str, window: &mut Window, cx: &mut App) -> anyhow::Result<()> {
    let config = theme_config(mode, name, cx)?;
    let preferences = cx.global_mut::<Appearance>();
    match mode {
        ThemeMode::Light => preferences.light_theme = name.into(),
        ThemeMode::Dark => preferences.dark_theme = name.into(),
    }
    if Theme::global(cx).mode == mode {
        apply(window, cx);
    } else {
        Theme::update(cx, |theme| match mode {
            ThemeMode::Light => theme.light_theme = config,
            ThemeMode::Dark => theme.dark_theme = config,
        });
    }
    save(cx)
}

fn save(cx: &App) -> anyhow::Result<()> {
    let preferences = cx.global::<Appearance>();
    preferences
        .paths
        .as_ref()
        .context("The configuration directory is unavailable")?
        .update(preferences)
}

#[cfg(test)]
pub(crate) fn setup_test(path: Option<std::path::PathBuf>, cx: &mut App) {
    register_bundled(cx);
    cx.set_global(load(path.map(|path| ConfigPaths::new(path.clone(), path)), cx));
}

#[cfg(test)]
mod tests {
    use gpui_kit::{AppContext, Empty, TestAppContext, px};
    use std::{fs, path::PathBuf};

    use super::*;

    fn load_path(path: PathBuf, cx: &App) -> Appearance {
        load(Some(ConfigPaths::new(path.clone(), path)), cx)
    }

    #[test]
    fn defaults_use_system_mode_and_the_same_theme_pair_as_topq() {
        assert_eq!(
            serde_json::to_value(Appearance::default()).unwrap(),
            serde_json::json!({"mode": "system", "light_theme": "Ayu Light", "dark_theme": "Ayu Dark"})
        );
    }

    #[gpui_kit::test]
    fn selections_persist_and_inactive_themes_do_not_change_the_palette(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("map/appearance.json");
        cx.update(|cx| {
            gpui_kit::init(cx);
            setup_test(Some(path.clone()), cx);
        });
        let handle = cx.add_window(|_, _| Empty);
        cx.update_window(handle.into(), |_, window, cx| {
            select_mode(AppearanceMode::Dark, window, cx).unwrap();
            let before = Theme::global(cx).clone();
            select_theme(ThemeMode::Light, "Gruvbox Light", window, cx).unwrap();
            assert_eq!(Theme::global(cx).colors, before.colors);
            assert_eq!(Theme::global(cx).tokens, before.tokens);
            assert_eq!(Theme::global(cx).font_size, before.font_size);
            select_theme(ThemeMode::Dark, "Tokyo Night", window, cx).unwrap();
            assert_eq!(Theme::global(cx).theme_name().as_str(), "Tokyo Night");
            select_mode(AppearanceMode::Light, window, cx).unwrap();
            assert_eq!(Theme::global(cx).theme_name().as_str(), "Gruvbox Light");
            let reloaded = load_path(path, cx);
            assert_eq!(reloaded.mode, AppearanceMode::Light);
            assert_eq!(reloaded.light_theme, "Gruvbox Light");
            assert_eq!(reloaded.dark_theme, "Tokyo Night");
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn system_sync_respects_manual_mode_and_uses_the_selected_pair(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_kit::init(cx);
            setup_test(Some(directory.path().join("appearance.json")), cx);
        });
        let handle = cx.add_window(|_, _| Empty);
        cx.update_window(handle.into(), |_, window, cx| {
            select_mode(AppearanceMode::Dark, window, cx).unwrap();
            select_theme(ThemeMode::Dark, "Tokyo Night", window, cx).unwrap();
            sync_system(window, cx);
            assert_eq!(Theme::global(cx).theme_name().as_str(), "Tokyo Night");
            select_theme(ThemeMode::Light, "Gruvbox Light", window, cx).unwrap();
            select_mode(AppearanceMode::System, window, cx).unwrap();
            Theme::update(cx, |theme| theme.font_size = px(40.));
            sync_system(window, cx);
            let mode = ThemeMode::from(window.appearance());
            assert_eq!(Theme::global(cx).mode, mode);
            assert_eq!(Theme::global(cx).theme_name().as_str(), Appearance::selected_theme(mode, cx));
            assert_eq!(Theme::global(cx).font_size, Theme::default().font_size);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn missing_malformed_and_unavailable_preferences_recover_without_writing(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("appearance.json");
        cx.update(|cx| {
            gpui_kit::init(cx);
            setup_test(Some(path.clone()), cx);
            assert!(!path.exists());
            for content in ["{invalid", r#"{"mode":"light","light_theme":"Ayu Dark","dark_theme":"Removed"}"#] {
                fs::write(&path, content).unwrap();
                let preferences = load_path(path.clone(), cx);
                assert_eq!(preferences.light_theme, DEFAULT_LIGHT_THEME);
                assert_eq!(preferences.dark_theme, DEFAULT_DARK_THEME);
                assert_eq!(fs::read_to_string(&path).unwrap(), content);
            }
        });
    }

    #[gpui_kit::test]
    fn invalid_theme_selection_preserves_preferences_and_disk(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("appearance.json");
        cx.update(|cx| {
            gpui_kit::init(cx);
            setup_test(Some(path.clone()), cx);
        });
        let handle = cx.add_window(|_, _| Empty);
        cx.update_window(handle.into(), |_, window, cx| {
            select_mode(AppearanceMode::Dark, window, cx).unwrap();
            let before = fs::read(&path).unwrap();
            for name in ["Removed", "Ayu Light"] {
                assert!(select_theme(ThemeMode::Dark, name, window, cx).is_err());
                assert_eq!(Appearance::selected_theme(ThemeMode::Dark, cx), DEFAULT_DARK_THEME);
                assert_eq!(fs::read(&path).unwrap(), before);
            }
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn every_bundled_theme_keeps_component_and_base_tokens_in_sync(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            setup_test(None, cx);
        });
        let handle = cx.add_window(|_, _| Empty);
        cx.update_window(handle.into(), |_, window, cx| {
            let configs: Vec<_> = ThemeRegistry::global(cx).sorted_themes().into_iter().cloned().collect();
            for config in configs {
                let preferences = cx.global_mut::<Appearance>();
                if config.mode.is_dark() {
                    preferences.mode = AppearanceMode::Dark;
                    preferences.dark_theme = config.name.to_string();
                } else {
                    preferences.mode = AppearanceMode::Light;
                    preferences.light_theme = config.name.to_string();
                }
                apply(window, cx);
                let theme = Theme::global(cx);
                assert_eq!(theme.theme_name(), &config.name);
                assert_eq!(theme.mode, theme.highlight_theme.appearance);
                assert_eq!(theme.background, theme.tokens.background.color);
                assert_eq!(theme.primary, theme.tokens.primary.color);
                assert_eq!(theme.radius, config.radius.map_or(Theme::default().radius, |radius| px(radius as f32)));
                assert_eq!(gpui_kit::base::Theme::global(cx).tokens.colors.background, theme.background);
            }
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn save_failure_keeps_the_live_selection(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("appearance.json");
        fs::create_dir(&path).unwrap();
        cx.update(|cx| {
            gpui_kit::init(cx);
            setup_test(Some(path), cx);
        });
        let handle = cx.add_window(|_, _| Empty);
        cx.update_window(handle.into(), |_, window, cx| {
            assert!(select_mode(AppearanceMode::Dark, window, cx).is_err());
            assert_eq!(Appearance::mode(cx), AppearanceMode::Dark);
            assert_eq!(Theme::global(cx).mode, ThemeMode::Dark);
        })
        .unwrap();
    }
}
