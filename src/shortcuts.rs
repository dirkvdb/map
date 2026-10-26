use gpui_kit::component::{ActiveTheme, WindowExt, kbd::Kbd};
use gpui_kit::{
    Action, App, FontWeight, InteractiveElement, IntoElement, KeyBinding, KeyBindingContextPredicate, ParentElement, Styled, TestSupportExt, Window, div,
};

gpui_kit::actions!(shortcuts, [ShowShortcuts]);

pub(crate) fn key_binding(keystrokes: &str, action: impl Action, context: Option<&str>, cx: &App) -> anyhow::Result<KeyBinding> {
    let predicate = context.map(KeyBindingContextPredicate::parse).transpose()?.map(Into::into);
    Ok(KeyBinding::load(keystrokes, Box::new(action), predicate, false, None, cx)?)
}

pub(crate) fn open(window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, |dialog, window, cx| {
        let title = div().id("shortcuts-title").test_support().w_full().text_center().child("Keyboard shortcuts");
        dialog.title(title).w_96().max_w_full().child(
            div()
                .id("keyboard-shortcuts")
                .test_support()
                .flex()
                .flex_col()
                .gap_4()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(section_heading("Workspace", cx))
                        .child(shortcut(
                            "shortcut-open",
                            "Open dataset…",
                            &crate::raster_panel::OpenDataset,
                            "RasterWorkspace",
                            window,
                        ))
                        .child(shortcut(
                            "shortcut-fit",
                            "Fit raster",
                            &crate::raster_panel::FitRaster,
                            "RasterWorkspace",
                            window,
                        ))
                        .child(shortcut(
                            "shortcut-controls",
                            "Show or hide controls",
                            &crate::raster_panel::ToggleRasterControls,
                            "RasterWorkspace",
                            window,
                        ))
                        .child(shortcut(
                            "shortcut-settings",
                            "Settings…",
                            &crate::settings::OpenSettings,
                            "RasterWorkspace",
                            window,
                        ))
                        .child(shortcut(
                            "shortcut-tile-boundaries",
                            "Toggle debug mode",
                            &crate::map_view::ToggleTileBoundaries,
                            "RasterWorkspace",
                            window,
                        ))
                        .child(shortcut("shortcut-help", "Keyboard shortcuts", &ShowShortcuts, "RasterWorkspace", window)),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(section_heading("Map navigation", cx))
                        .child(shortcut("shortcut-zoom-in", "Zoom in", &crate::map_view::ZoomIn, "MapNavigation", window))
                        .child(shortcut("shortcut-zoom-out", "Zoom out", &crate::map_view::ZoomOut, "MapNavigation", window))
                        .child(shortcut("shortcut-pan-left", "Pan left", &crate::map_view::PanLeft, "MapNavigation", window))
                        .child(shortcut("shortcut-pan-right", "Pan right", &crate::map_view::PanRight, "MapNavigation", window))
                        .child(shortcut("shortcut-pan-up", "Pan up", &crate::map_view::PanUp, "MapNavigation", window))
                        .child(shortcut("shortcut-pan-down", "Pan down", &crate::map_view::PanDown, "MapNavigation", window)),
                ),
        )
    });
}

fn section_heading(title: &'static str, cx: &App) -> impl IntoElement {
    div()
        .text_base()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(cx.theme().primary)
        .pb_2()
        .child(title)
}

fn shortcut(id: &'static str, label: &'static str, action: &dyn Action, context: &str, window: &Window) -> impl IntoElement {
    div()
        .id(id)
        .test_support()
        .flex()
        .items_center()
        .gap_4()
        .text_sm()
        .child(div().flex_1().min_w_0().child(label))
        .children(Kbd::binding_for_action(action, Some(context), window))
}

#[cfg(test)]
mod tests {
    use gpui_kit::{Keystroke, Modifiers, TestAppContext};

    use super::*;

    #[gpui_kit::test]
    fn key_binding_resolves_secondary_using_the_app(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let binding = key_binding("secondary-q", crate::Quit, None, cx).unwrap();
            let keystroke = Keystroke {
                modifiers: Modifiers::secondary_key(cx),
                key: "q".into(),
                ..Default::default()
            };
            assert_eq!(binding.match_keystrokes(&[keystroke]), Some(false));
        });
    }

    #[gpui_kit::test]
    fn key_binding_returns_an_error_for_invalid_keystrokes(cx: &mut TestAppContext) {
        cx.update(|cx| {
            assert!(key_binding("unknown-q", crate::Quit, None, cx).is_err());
        });
    }
}
