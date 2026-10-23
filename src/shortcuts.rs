use gpui_kit::component::{ActiveTheme, WindowExt, kbd::Kbd};
use gpui_kit::{Action, App, FontWeight, InteractiveElement, IntoElement, ParentElement, Styled, TestSupportExt, Window, div};

gpui_kit::actions!(shortcuts, [ShowShortcuts]);

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
                            "Open raster…",
                            &crate::raster_panel::OpenRaster,
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
