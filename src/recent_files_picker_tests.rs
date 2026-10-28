//! UI integration coverage using the real Command, dialog, and panel loader.

use std::path::{Path, PathBuf};

use gpui_kit::component::{IndexPath, Root};
use gpui_kit::test::{ElementSnapshot, TestSupportExt, TestWindowExt};
use gpui_kit::{
    AnyWindowHandle, App, AppContext, Context, Entity, FocusHandle, InteractiveElement, IntoElement, ParentElement, Render, Role, Styled, TestAppContext,
    Window, div, px, size,
};

use super::{OpenRecentFiles, display_directory, open};
use crate::raster_panel::RasterPanel;

#[test]
fn home_directory_is_abbreviated_only_at_a_path_boundary() {
    let home = Some(Path::new("/home/user"));
    assert_eq!(display_directory(Path::new("/home/user/maps"), home), "~/maps");
    assert_eq!(display_directory(Path::new("/home/user"), home), "~");
    assert_eq!(display_directory(Path::new("/home/username/maps"), home), "/home/username/maps");
    assert_eq!(display_directory(Path::new("/data/maps"), None), "/data/maps");
}

// MapView's renderer-free constructor is private. This fixture supplies only its
// workspace action seam; the keymap, popup, and panel behavior remain production.
struct Workspace {
    panel: Entity<RasterPanel>,
    focus: FocusHandle,
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("recent-files-workspace")
            .test_support()
            .track_focus(&self.focus)
            .key_context("RasterWorkspace")
            .on_action(cx.listener(|this, _: &OpenRecentFiles, window, cx| {
                this.panel.update(cx, |panel, cx| panel.open_recent_files(window, cx));
            }))
            .size_full()
            .child(self.panel.clone())
    }
}

struct Harness {
    _directory: tempfile::TempDir,
    paths: Vec<PathBuf>,
    workspace: Entity<Workspace>,
    panel: Entity<RasterPanel>,
    opened: Entity<Vec<PathBuf>>,
    window: AnyWindowHandle,
}

impl Harness {
    fn new(cx: &mut TestAppContext) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let paths = ["north", "south", "west"]
            .map(|region| {
                let folder = directory.path().join(region);
                std::fs::create_dir(&folder).unwrap();
                let path = folder.join("elevation.tif");
                crate::raster_source::test_support::write_raster(&path, 4326, 1).unwrap();
                path
            })
            .to_vec();
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::map_view::init(cx).unwrap();
            cx.set_reduce_motion(true);
        });
        let mut workspace = None;
        let window = cx.open_window(size(px(900.), px(760.)), |window, cx| {
            let view = cx.new(|cx| {
                let focus = cx.focus_handle();
                focus.focus(window, cx);
                Workspace {
                    panel: cx.new(|cx| RasterPanel::new(window, cx)),
                    focus,
                }
            });
            workspace = Some(view.clone());
            Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        let panel = cx.update(|cx| workspace.read(cx).panel.clone());
        let opened = cx.new(|cx: &mut Context<Vec<PathBuf>>| {
            cx.subscribe(&panel, |opened, _, event, _| {
                opened.extend(event.source().map(|source| source.metadata().path.clone()));
            })
            .detach();
            Vec::new()
        });
        Self {
            _directory: directory,
            paths,
            workspace,
            panel,
            opened,
            window: window.into(),
        }
    }

    fn ui(&self, cx: &mut TestAppContext, interact: impl FnOnce(&mut Window, &mut App)) {
        cx.update_window(self.window, |_, window, cx| {
            window.render_frame(cx);
            interact(window, cx);
        })
        .unwrap();
        cx.run_until_parked();
    }

    fn show(&self, paths: Vec<PathBuf>, cx: &mut TestAppContext) {
        self.ui(cx, |window, cx| open(paths, self.panel.downgrade(), window, cx));
        self.ui(cx, |window, _| {
            assert!(window.find("dialog").visible());
            assert_eq!(query(window).focused(), Some(true));
        });
    }

    fn assert_opened(&self, index: usize, cx: &mut TestAppContext) {
        self.ui(cx, |window, cx| {
            assert!(window.try_find("dialog").is_none());
            assert!(!self.panel.read(cx).is_loading());
            assert_eq!(self.panel.read(cx).load_error(), None);
            assert_eq!(self.opened.read(cx).as_slice(), &[self.paths[index].clone()]);
        });
    }

    fn assert_dismissed(&self, cx: &mut TestAppContext) {
        self.ui(cx, |window, cx| {
            assert!(window.try_find("dialog").is_none());
            assert!(self.workspace.read(cx).focus.is_focused(window));
            assert!(self.opened.read(cx).is_empty());
            assert!(!self.panel.read(cx).is_loading());
        });
    }
}

fn query(window: &mut Window) -> ElementSnapshot {
    let mut inputs = window.within("command").find_all_by_role(Role::TextInput);
    assert_eq!(inputs.len(), 1, "The popup must have one search input");
    inputs.remove(0)
}

#[gpui_kit::test]
fn filtering_by_directory_activates_the_original_path_not_the_filtered_row(cx: &mut TestAppContext) {
    let harness = Harness::new(cx);
    harness.show(harness.paths.clone(), cx);
    harness.ui(cx, |window, cx| {
        let mut command = window.within("command");
        let row = command.within(IndexPath::new(1));
        assert_eq!(row.find("file-name").label(), Some("elevation.tif"));
        assert_eq!(row.find("file-directory").label(), harness.paths[1].parent().unwrap().to_str());
        window.input("SOUTH/", cx);
    });
    harness.ui(cx, |window, cx| {
        assert_eq!(query(window).value(), Some("SOUTH/"));
        assert!(window.within("command").try_find(IndexPath::new(0)).is_none());
        assert_eq!(window.within("command").find(IndexPath::new(1)).selected(), Some(true));
        assert!(window.within("command").try_find(IndexPath::new(2)).is_none());
        window.press("enter", cx);
    });
    harness.assert_opened(1, cx);
}

#[gpui_kit::test]
fn arrow_keys_select_and_enter_opens_the_highlighted_path(cx: &mut TestAppContext) {
    let harness = Harness::new(cx);
    harness.show(harness.paths.clone(), cx);
    harness.ui(cx, |window, cx| {
        assert_eq!(window.within("command").find(IndexPath::new(0)).selected(), Some(true));
        window.press("down", cx);
        window.press("down", cx);
        assert_eq!(window.within("command").find(IndexPath::new(2)).selected(), Some(true));
        window.press("up", cx);
        assert_eq!(window.within("command").find(IndexPath::new(1)).selected(), Some(true));
        assert_eq!(query(window).focused(), Some(true));
        window.press("enter", cx);
    });
    harness.assert_opened(1, cx);
}

#[gpui_kit::test]
fn clicking_a_noninitial_row_opens_that_path(cx: &mut TestAppContext) {
    let harness = Harness::new(cx);
    harness.show(harness.paths.clone(), cx);
    harness.ui(cx, |window, cx| window.within("command").click(IndexPath::new(2), cx));
    harness.assert_opened(2, cx);
}

#[gpui_kit::test]
fn empty_history_has_no_activatable_rows_and_escape_restores_focus(cx: &mut TestAppContext) {
    let harness = Harness::new(cx);
    harness.ui(cx, |window, cx| window.press("secondary-alt-o", cx));
    harness.ui(cx, |window, cx| {
        assert!(window.find("dialog").visible());
        assert_eq!(query(window).focused(), Some(true));
        assert!(window.within("command").find_all_by_role(Role::ListBoxOption).is_empty());
        window.press("down", cx);
        window.press("enter", cx);
    });
    harness.ui(cx, |window, cx| {
        assert!(window.find("dialog").visible());
        assert!(harness.opened.read(cx).is_empty());
        assert!(!harness.panel.read(cx).is_loading());
        window.press("escape", cx);
    });
    harness.assert_dismissed(cx);
}

#[gpui_kit::test]
fn unmatched_query_cannot_open_and_escape_clears_before_dismissing(cx: &mut TestAppContext) {
    let harness = Harness::new(cx);
    harness.show(harness.paths.clone(), cx);
    harness.ui(cx, |window, cx| window.input("no-such-region", cx));
    harness.ui(cx, |window, cx| {
        assert!(window.within("command").find_all_by_role(Role::ListBoxOption).is_empty());
        window.press("enter", cx);
    });
    harness.ui(cx, |window, cx| {
        assert!(window.find("dialog").visible());
        assert!(harness.opened.read(cx).is_empty());
        assert!(!harness.panel.read(cx).is_loading());
        window.press("escape", cx);
    });
    harness.ui(cx, |window, cx| {
        assert!(window.find("dialog").visible());
        assert_eq!(query(window).value(), Some(""));
        assert_eq!(query(window).focused(), Some(true));
        assert_eq!(window.within("command").find_all_by_role(Role::ListBoxOption).len(), 3);
        window.press("escape", cx);
    });
    harness.assert_dismissed(cx);
}

#[gpui_kit::test]
fn actual_workspace_shortcut_reopens_successfully_loaded_panel_history(cx: &mut TestAppContext) {
    let harness = Harness::new(cx);
    harness.ui(cx, |_, cx| {
        harness.panel.update(cx, |panel, cx| panel.open_paths(vec![harness.paths[2].clone()], cx));
    });
    harness.ui(cx, |_, cx| {
        assert_eq!(harness.opened.read(cx).as_slice(), &[harness.paths[2].clone()]);
        harness.opened.update(cx, |opened, _| opened.clear());
    });
    harness.ui(cx, |window, cx| {
        window.press("alt-o", cx);
        assert!(window.try_find("dialog").is_none());
        window.press("secondary-alt-o", cx);
    });
    harness.ui(cx, |window, cx| {
        assert!(window.find("dialog").visible());
        assert_eq!(query(window).focused(), Some(true));
        assert_eq!(window.within("command").find_all_by_role(Role::ListBoxOption).len(), 1);
        window.press("enter", cx);
    });
    harness.assert_opened(2, cx);
}
