use gpui_kit::component::{
    ActiveTheme, ThemeMode, ThemeRegistry, WindowExt,
    button::Button,
    dialog::{DialogEntrance, DialogFooter},
    label::Label,
    menu::{DropdownMenu, PopupMenuItem},
    notification::Notification,
};
use gpui_kit::{App, FontWeight, IntoElement, ParentElement, Styled, Window, div, px, rems};

use crate::appearance::{self, Appearance, AppearanceMode};

gpui_kit::actions!(settings, [OpenSettings]);

pub(crate) fn open(window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, |dialog, window, cx| {
        let margin = cx.theme().spacing_tokens().lg;
        let height = rems(28.)
            .to_pixels(window.rem_size())
            .min((window.viewport_size().height - margin * 2.).max(px(0.)));
        dialog
            .title("Settings")
            .w(rems(32.).to_pixels(window.rem_size()))
            .h(height)
            .margin_top((window.viewport_size().height - height) / 2.)
            .entrance(DialogEntrance::None)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .py_4()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("Appearance"))
                    .child(setting("Mode", "Follow your system or choose Light or Dark.", mode_menu(cx), cx))
                    .child(setting(
                        "Light theme",
                        "Used when the appearance is light.",
                        theme_menu(ThemeMode::Light, cx),
                        cx,
                    ))
                    .child(setting("Dark theme", "Used when the appearance is dark.", theme_menu(ThemeMode::Dark, cx), cx))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("Changes apply immediately. The basemap stays dark."),
                    ),
            )
            .footer(DialogFooter::new().child(Button::new("settings-done").label("Done").on_click(|_, window, cx| window.close_dialog(cx))))
    });
}

fn setting(title: &'static str, description: &'static str, control: impl IntoElement, cx: &App) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_4()
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(Label::new(title))
                .child(div().text_sm().text_color(cx.theme().muted_foreground).child(description)),
        )
        .child(div().w_56().min_w_0().child(control))
}

fn report(result: anyhow::Result<()>, window: &mut Window, cx: &mut App) {
    if let Err(error) = result {
        log::error!("Could not save appearance preference: {error:#}");
        window.push_notification(Notification::error("Appearance changed, but the preference could not be saved."), cx);
    }
}

fn mode_menu(cx: &App) -> impl IntoElement {
    let label = Appearance::mode(cx).label();
    Button::new("appearance-mode")
        .w_full()
        .outline()
        .label(label)
        .accessibility_label(format!("Mode: {label}"))
        .dropdown_caret(true)
        .dropdown_menu(|mut menu, _, cx| {
            for mode in [AppearanceMode::System, AppearanceMode::Light, AppearanceMode::Dark] {
                menu = menu.item(
                    PopupMenuItem::new(mode.label())
                        .checked(Appearance::mode(cx) == mode)
                        .on_click(move |_, window, cx| {
                            report(appearance::select_mode(mode, window, cx), window, cx);
                        }),
                );
            }
            menu
        })
}

fn theme_menu(mode: ThemeMode, cx: &App) -> impl IntoElement {
    let label = Appearance::selected_theme(mode, cx).to_owned();
    Button::new(if mode.is_dark() { "dark-theme" } else { "light-theme" })
        .w_full()
        .outline()
        .label(label.clone())
        .accessibility_label(format!("{} theme: {label}", if mode.is_dark() { "Dark" } else { "Light" }))
        .dropdown_caret(true)
        .dropdown_menu(move |mut menu, window, cx| {
            menu = menu
                .min_w(rems(14.).to_pixels(window.rem_size()))
                .max_h(rems(24.).to_pixels(window.rem_size()))
                .scrollable(true);
            for theme in ThemeRegistry::global(cx).sorted_themes() {
                if theme.mode != mode {
                    continue;
                }
                let name = theme.name.clone();
                menu = menu.item(
                    PopupMenuItem::new(name.clone())
                        .checked(Appearance::selected_theme(mode, cx) == name.as_str())
                        .on_click(move |_, window, cx| {
                            report(appearance::select_theme(mode, &name, window, cx), window, cx);
                        }),
                );
            }
            menu
        })
}
