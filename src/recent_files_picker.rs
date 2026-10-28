use std::path::{Path, PathBuf};

use gpui_kit::assets::IconName;
use gpui_kit::component::command::{Command, CommandItem, CommandState};
use gpui_kit::component::{ActiveTheme, Icon, WindowExt};
use gpui_kit::{App, AppContext, Focusable, InteractiveElement, ParentElement, StatefulInteractiveElement, Styled, TestSupportExt, WeakEntity, Window, div};

use crate::raster_panel::RasterPanel;

gpui_kit::actions!(recent_files, [OpenRecentFiles]);

pub(crate) fn open(paths: Vec<PathBuf>, panel: WeakEntity<RasterPanel>, window: &mut Window, cx: &mut App) {
    let state = cx.new(|cx| CommandState::new(window, cx));
    let focus = state.clone();
    let empty_history = paths.is_empty();
    let home = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf());
    window.open_dialog(cx, move |dialog, _, _| {
        let paths = paths.clone();
        let panel = panel.clone();
        dialog.close_button(false).w_128().max_w_full().p_0().child(
            Command::new(&state)
                .bordered(false)
                .placeholder("Search recent files…")
                .items(paths.iter().map(|path| file_item(path, home.as_deref())))
                .empty(move |_, _, cx| {
                    div()
                        .p_4()
                        .text_color(cx.theme().muted_foreground)
                        .child(if empty_history { "No recent files" } else { "No matching recent files" })
                })
                .on_confirm(move |index, window, cx| {
                    if let Some(path) = paths.get(index.row) {
                        let path = path.clone();
                        window.close_dialog(cx);
                        let _ = panel.update(cx, |panel, cx| panel.open_paths(vec![path], cx));
                    }
                }),
        )
    });
    window.defer(cx, move |window, cx| focus.focus_handle(cx).focus(window, cx));
}

fn file_item(path: &Path, home: Option<&Path>) -> CommandItem {
    let name = path.file_name().unwrap_or(path.as_os_str()).to_string_lossy().into_owned();
    let directory = display_directory(path.parent().unwrap_or(path), home);
    CommandItem::new()
        .label(name.clone())
        .keywords([path.to_string_lossy().into_owned(), directory.clone()])
        .child(move |_, cx| {
            div()
                .flex()
                .items_center()
                .gap_3()
                .w_full()
                .min_w_0()
                .py_1()
                .child(Icon::new(IconName::FolderOpen).size_4().text_color(cx.theme().muted_foreground))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w_0()
                        .flex_1()
                        .child(div().id("file-name").test_support().aria_label(name.clone()).truncate().child(name.clone()))
                        .child(
                            div()
                                .id("file-directory")
                                .test_support()
                                .aria_label(directory.clone())
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .truncate()
                                .child(directory.clone()),
                        ),
                )
        })
}

fn display_directory(directory: &Path, home: Option<&Path>) -> String {
    if let Some(relative) = home.and_then(|home| directory.strip_prefix(home).ok()) {
        return if relative.as_os_str().is_empty() {
            "~".into()
        } else {
            Path::new("~").join(relative).to_string_lossy().into_owned()
        };
    }
    directory.to_string_lossy().into_owned()
}

#[cfg(test)]
#[path = "recent_files_picker_tests.rs"]
mod tests;
