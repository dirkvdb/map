use std::rc::Rc;

use gpui_kit::base::{Radio, RadioGroup};
use gpui_kit::component::{
    ActiveTheme, Icon, IconName, Theme, ThemeColor, ThemeConfig, ThemeMode, ThemeRegistry, WindowExt,
    button::Button,
    dialog::{Confirm, DialogEntrance, DialogFooter},
    notification::Notification,
};
use gpui_kit::{
    App, Axis, Div, ElementId, FontWeight, InteractiveElement, ParentElement, Pixels, StatefulInteractiveElement, Styled, TestSupportExt, Window, div,
    prelude::FluentBuilder, relative, rems,
};

use crate::appearance::{self, Appearance, AppearanceMode};

gpui_kit::actions!(settings, [OpenSettings]);

pub(crate) fn open(window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, |dialog, window, cx| {
        let margin = cx.theme().spacing_tokens().lg;
        let height = rems(42.)
            .to_pixels(window.rem_size())
            .min((window.viewport_size().height - margin * 2.).max(Pixels::ZERO));
        let width = rems(52.)
            .to_pixels(window.rem_size())
            .min((window.viewport_size().width - margin * 2.).max(Pixels::ZERO));
        let columns = if width >= rems(44.).to_pixels(window.rem_size()) {
            4
        } else if width >= rems(32.).to_pixels(window.rem_size()) {
            3
        } else if width >= rems(22.).to_pixels(window.rem_size()) {
            2
        } else {
            1
        };
        dialog
            .title("Settings")
            .w(width)
            .h(height)
            .margin_top((window.viewport_size().height - height) / 2.)
            .entrance(DialogEntrance::None)
            .child(
                div()
                    .id("appearance-settings")
                    .test_support()
                    .flex()
                    .flex_col()
                    .gap_6()
                    .py_2()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(section_heading(
                                "Appearance mode",
                                "Choose a light or dark appearance, or follow your system.",
                                cx,
                            ))
                            .child(mode_choices(columns == 1, cx)),
                    )
                    .child(theme_section(ThemeMode::Light, columns, cx))
                    .child(theme_section(ThemeMode::Dark, columns, cx)),
            )
            .footer(DialogFooter::new().child(Button::new("settings-done").label("Done").on_click(|_, window, cx| window.close_dialog(cx))))
    });
}

fn section_heading(title: &'static str, description: &'static str, cx: &App) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
        .child(div().text_sm().text_color(cx.theme().muted_foreground).child(description))
}

fn mode_choices(stacked: bool, cx: &App) -> RadioGroup {
    let light = resolved_colors(&cx.theme().light_theme);
    let dark = resolved_colors(&cx.theme().dark_theme);
    RadioGroup::new("appearance-mode")
        .aria_label("Appearance mode")
        .axis(if stacked { Axis::Vertical } else { Axis::Horizontal })
        .flex()
        .flex_wrap()
        .when(stacked, |group| group.flex_col().items_start())
        .gap_4()
        .children([
            mode_card("mode-light", AppearanceMode::Light.label(), AppearanceMode::Light, &light, &dark, cx),
            mode_card("mode-dark", AppearanceMode::Dark.label(), AppearanceMode::Dark, &light, &dark, cx),
            mode_card("mode-system", "Follow system", AppearanceMode::System, &light, &dark, cx),
        ])
}

fn mode_card(id: &'static str, label: &'static str, mode: AppearanceMode, light: &ThemeColor, dark: &ThemeColor, cx: &App) -> Radio {
    let selected = Appearance::mode(cx) == mode;
    let accent = cx.theme().primary;
    let preview = div()
        .id("mode-preview")
        .test_support()
        .flex()
        .w_full()
        .h_20()
        .rounded(cx.theme().radius)
        .overflow_hidden()
        .border_2()
        .border_color(if selected { accent } else { cx.theme().border })
        .map(|preview| match mode {
            AppearanceMode::Light => preview.child(mini_window(light)),
            AppearanceMode::Dark => preview.child(mini_window(dark)),
            AppearanceMode::System => preview
                .child(
                    div()
                        .flex()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .overflow_hidden()
                        .child(mini_window(light).w(relative(2.))),
                )
                .child(
                    div()
                        .flex()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .justify_end()
                        .overflow_hidden()
                        .child(mini_window(dark).w(relative(2.))),
                ),
        });
    Radio::new(id)
        .accessibility_label(label)
        .checked(selected)
        .flex()
        .flex_col()
        .items_center()
        .w(rems(9.))
        .gap_2()
        .p_1()
        .rounded(cx.theme().radius_lg)
        .border_1()
        .border_color(cx.theme().border.opacity(0.))
        .cursor_default()
        .hover(|style| style.bg(cx.theme().muted))
        .active(|style| style.bg(cx.theme().accent))
        .focus_visible(move |style| style.border_color(accent))
        .child(preview)
        .child(
            div()
                .id("mode-label")
                .test_support()
                .flex()
                .items_center()
                .gap_2()
                .text_sm()
                .child(radio_indicator(selected, cx))
                .child(label),
        )
        .on_change(move |_, _, window, cx| report(appearance::select_mode(mode, window, cx), window, cx))
        .on_action(move |_: &Confirm, window, cx| {
            cx.stop_propagation();
            report(appearance::select_mode(mode, window, cx), window, cx);
        })
}

fn mini_window(colors: &ThemeColor) -> Div {
    let dot = |color| div().size_1().rounded_full().bg(color);
    div().w_full().h_full().flex_none().p_2().bg(colors.muted).child(
        div()
            .flex()
            .flex_col()
            .size_full()
            .rounded_sm()
            .overflow_hidden()
            .bg(colors.background)
            .child(
                div()
                    .flex()
                    .items_center()
                    .h_3()
                    .flex_none()
                    .px_1()
                    .gap_0p5()
                    .bg(colors.title_bar)
                    .child(dot(colors.danger))
                    .child(dot(colors.warning))
                    .child(dot(colors.success)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .p_1p5()
                    .gap_1()
                    .child(div().w_10().h_1().rounded_full().bg(colors.primary))
                    .child(div().w_16().max_w_full().h_1().rounded_full().bg(colors.foreground.opacity(0.35)))
                    .child(div().w_12().max_w_full().h_1().rounded_full().bg(colors.foreground.opacity(0.2))),
            ),
    )
}

fn radio_indicator(selected: bool, cx: &App) -> Div {
    div()
        .size_3()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .border_2()
        .border_color(if selected { cx.theme().primary } else { cx.theme().muted_foreground })
        .when(selected, |indicator| indicator.child(div().size_1().rounded_full().bg(cx.theme().primary)))
}

fn theme_section(mode: ThemeMode, columns: u16, cx: &App) -> Div {
    let (id, title) = if mode.is_dark() {
        ("dark-themes", "Dark themes")
    } else {
        ("light-themes", "Light themes")
    };
    div()
        .flex()
        .flex_col()
        .gap_3()
        .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
        .child(
            RadioGroup::new(id).aria_label(title).grid().grid_cols(columns).gap_3().children(
                ThemeRegistry::global(cx)
                    .sorted_themes()
                    .into_iter()
                    .filter(|theme| theme.mode == mode)
                    .map(|theme| theme_card(theme, cx)),
            ),
        )
}

fn resolved_colors(config: &Rc<ThemeConfig>) -> ThemeColor {
    let base = if config.mode.is_dark() { ThemeColor::dark() } else { ThemeColor::light() };
    let mut preview = Theme::from(base.as_ref());
    preview.apply_config(config);
    preview.colors
}

fn theme_card(config: &Rc<ThemeConfig>, cx: &App) -> Radio {
    let colors = resolved_colors(config);
    let name = config.name.clone();
    let keyboard_name = name.clone();
    let mode = config.mode;
    let mode_label = if mode.is_dark() { "Dark" } else { "Light" };
    let selected = &name == cx.theme().theme_name();
    let accent = cx.theme().primary;
    let hover_border = cx.theme().muted_foreground;
    Radio::new((ElementId::from("theme"), name.clone()))
        .accessibility_label(format!("{name}, {mode_label} theme"))
        .checked(selected)
        .flex()
        .flex_col()
        .min_w_0()
        .w_full()
        .h(rems(9.))
        .p_2()
        .gap_2()
        .rounded(cx.theme().radius_lg)
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().group_box)
        .styles(move |styles| styles.checked(|style| style.border_color(accent)))
        .cursor_default()
        .hover(move |style| style.border_color(if selected { accent } else { hover_border }))
        .active(|style| style.bg(cx.theme().accent))
        .focus_visible(|style| style.border_color(accent).bg(cx.theme().muted))
        .child(
            div()
                .id("theme-swatch")
                .test_support()
                .flex()
                .flex_col()
                .h_16()
                .flex_none()
                .w_full()
                .p_2()
                .gap_1()
                .rounded(cx.theme().radius)
                .overflow_hidden()
                .bg(colors.background)
                .child(div().w_10().h_1().rounded_full().bg(colors.primary))
                .child(div().w_20().max_w_full().h_1().rounded_full().bg(colors.foreground.opacity(0.7)))
                .child(div().w_12().max_w_full().h_1().rounded_full().bg(colors.foreground.opacity(0.4))),
        )
        .child(
            div()
                .id("theme-label")
                .test_support()
                .flex()
                .items_start()
                .min_w_0()
                .gap_1()
                .text_sm()
                .child(div().flex_1().min_w_0().h_10().line_clamp(2).child(name.clone()))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_end()
                        .gap_1()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(mode_label)
                        .when(selected, |label| label.child(Icon::new(IconName::Check).size_3().text_color(accent))),
                ),
        )
        .on_change(move |_, _, window, cx| choose_theme(mode, &name, window, cx))
        .on_action(move |_: &Confirm, window, cx| {
            cx.stop_propagation();
            choose_theme(mode, &keyboard_name, window, cx);
        })
}

fn choose_theme(mode: ThemeMode, name: &str, window: &mut Window, cx: &mut App) {
    let result = if Appearance::selected_theme(mode, cx) == name {
        Ok(())
    } else {
        appearance::select_theme(mode, name, window, cx)
    };
    // Configuring the inactive palette must not disable system following.
    let mode_result = if Appearance::mode(cx) != AppearanceMode::System && cx.theme().mode != mode {
        appearance::select_mode(if mode.is_dark() { AppearanceMode::Dark } else { AppearanceMode::Light }, window, cx)
    } else {
        Ok(())
    };
    report(result.and(mode_result), window, cx);
}

fn report(result: anyhow::Result<()>, window: &mut Window, cx: &mut App) {
    if let Err(error) = result {
        log::error!("Could not save appearance preference: {error:#}");
        window.push_notification(Notification::error("Appearance changed, but the preference could not be saved."), cx);
    }
}
