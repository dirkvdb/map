use std::path::Path;

use gpui_kit::component::{
    ActiveTheme, WindowExt,
    button::{Button, ButtonVariants},
    dialog::{Cancel, Confirm, DialogFooter},
    input::{Input, InputEvent, InputState},
    tooltip::Tooltip,
};
use gpui_kit::{
    App, AppContext, Context, Entity, Focusable, InteractiveElement, IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled,
    Subscription, TestSupportExt, WeakEntity, Window, div,
};

use super::RasterPanel;

struct ProjectionPrompt {
    input: Entity<InputState>,
    error: Option<SharedString>,
    epsg: Option<u32>,
    _subscription: Subscription,
}

pub(super) fn open(path: &Path, generation: u64, panel: WeakEntity<RasterPanel>, window: &mut Window, cx: &mut App) {
    let prompt = cx.new(|cx| {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("e.g. 4326"));
        let subscription = cx.subscribe_in(&input, window, |this: &mut ProjectionPrompt, _, event, window, cx| match event {
            InputEvent::PressEnter { .. } => window.dispatch_action(Box::new(Confirm { secondary: false }), cx),
            InputEvent::Change => {
                this.error = None;
                cx.notify();
            }
            _ => {}
        });
        ProjectionPrompt {
            input,
            error: None,
            epsg: None,
            _subscription: subscription,
        }
    });
    let focus = prompt.read(cx).input.clone();
    let path = path.to_string_lossy().into_owned();
    window.open_dialog(cx, move |dialog, _, cx| {
        let confirm = prompt.clone();
        let closed = prompt.clone();
        let panel = panel.clone();
        let full_path = path.clone();
        dialog
            .title("Set raster projection")
            .w_112()
            .close_button(false)
            .overlay_closable(false)
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("This raster has no projection information. Enter its source EPSG code."),
            )
            .child(
                div()
                    .id("projection-file")
                    .test_support()
                    .aria_label(path.clone())
                    .w_full()
                    .min_w_0()
                    .text_sm()
                    .truncate()
                    .tooltip(move |window, cx| Tooltip::new(full_path.clone()).build(window, cx))
                    .child(path.clone()),
            )
            .child(prompt.clone())
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("projection-cancel")
                            .label("Cancel")
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(Cancel), cx)),
                    )
                    .child(
                        Button::new("projection-open")
                            .primary()
                            .label("Open raster")
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(Confirm { secondary: false }), cx)),
                    ),
            )
            .on_ok(move |_, window, cx| confirm.update(cx, |this, cx| this.validate(window, cx)))
            .on_close(move |_, window, cx| {
                let epsg = closed.read(cx).epsg;
                let panel = panel.clone();
                // Continue after the old overlay closes, so the next file never
                // opens a nested dialog or restores focus through a closing one.
                window.defer(cx, move |window, cx| {
                    let _ = panel.update(cx, |panel, cx| panel.complete_projection_prompt(generation, epsg, window, cx));
                });
            })
    });
    window.defer(cx, move |window, cx| focus.focus_handle(cx).focus(window, cx));
}

impl ProjectionPrompt {
    fn validate(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        match parse_epsg(&self.input.read(cx).value()) {
            Ok(code) => {
                self.epsg = Some(code);
                true
            }
            Err(error) => {
                self.error = Some(error.into());
                self.input.focus_handle(cx).focus(window, cx);
                cx.notify();
                false
            }
        }
    }
}

impl Render for ProjectionPrompt {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child("EPSG code")
            .child(Input::new(&self.input).id("projection-epsg").aria_label("EPSG code"))
            .children(self.error.as_ref().map(|error| {
                div()
                    .id("projection-error")
                    .test_support()
                    .aria_label(error.clone())
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(error.clone())
            }))
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("Remembered in recent files after opening. The original file is not changed."),
            )
    }
}

fn parse_epsg(value: &str) -> Result<u32, &'static str> {
    let value = value.trim();
    let value = value
        .get(..5)
        .filter(|prefix| prefix.eq_ignore_ascii_case("EPSG:"))
        .map_or(value, |_| value[5..].trim());
    let code = value
        .parse::<u32>()
        .ok()
        .filter(|&code| code > 0)
        .ok_or("Enter a positive EPSG code, such as 4326.")?;
    let reference = gdal::spatial_ref::SpatialRef::from_epsg(code).map_err(|_| "Unknown EPSG code. Check the code and try again.")?;
    if !reference.is_geographic() && !reference.is_projected() {
        return Err("Use a geographic or projected EPSG code.");
    }
    Ok(code)
}

#[cfg(test)]
#[path = "raster_projection_prompt_tests.rs"]
mod tests;
