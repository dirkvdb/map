use std::io::{self, Write};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::{ActiveTheme, Sizable, button::Button, button::ButtonVariants};
use gpui_kit::{AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render, Styled, Task, TestSupportExt, Window, div};

gpui_kit::actions!(logs, [ToggleLogs]);

const MAX_LOG_BYTES: usize = 128 * 1024;

#[derive(Default)]
struct LogBuffer {
    text: String,
    revision: u64,
}

impl LogBuffer {
    fn append(&mut self, bytes: &[u8]) {
        self.text.push_str(&String::from_utf8_lossy(bytes));
        if self.text.len() > MAX_LOG_BYTES {
            let mut start = self.text.len() - MAX_LOG_BYTES;
            while !self.text.is_char_boundary(start) {
                start += 1;
            }
            self.text.drain(..start);
        }
        self.revision = self.revision.wrapping_add(1);
    }
}

fn buffer() -> Arc<Mutex<LogBuffer>> {
    static BUFFER: OnceLock<Arc<Mutex<LogBuffer>>> = OnceLock::new();
    BUFFER.get_or_init(|| Arc::new(Mutex::new(LogBuffer::default()))).clone()
}

/// Mirrors env_logger's formatted output without changing its filtering.
pub(crate) struct LogWriter;

impl Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        buffer().lock().unwrap_or_else(|error| error.into_inner()).append(bytes);
        // The UI must still receive logs when stderr is unavailable (GUI launches).
        let _ = io::stderr().write_all(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stderr().flush()
    }
}

pub(crate) struct LogOutput {
    output: Entity<TextareaState>,
    buffer: Arc<Mutex<LogBuffer>>,
    revision: Option<u64>,
    _refresh_task: Task<()>,
}

impl LogOutput {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::with_buffer(buffer(), window, cx)
    }

    fn with_buffer(buffer: Arc<Mutex<LogBuffer>>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let output = cx.new(|cx| TextareaState::new(window, cx).placeholder("No log output yet. Warnings and errors appear here by default."));
        let task = cx.spawn_in(window, async |this, cx| {
            while this.update_in(cx, |view, window, cx| view.refresh(window, cx)).is_ok() {
                cx.background_executor().timer(Duration::from_millis(250)).await;
            }
        });
        let mut view = Self {
            output,
            buffer,
            revision: None,
            _refresh_task: task,
        };
        view.refresh(window, cx);
        view
    }

    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let buffer = self.buffer.lock().unwrap_or_else(|error| error.into_inner());
        if self.revision == Some(buffer.revision) {
            return;
        }
        self.revision = Some(buffer.revision);
        self.output.update(cx, |output, cx| {
            let offset = output.scroll_offset();
            output.set_value(buffer.text.clone(), window, cx);
            output.set_scroll_offset(offset, cx);
        });
    }
}

impl Render for LogOutput {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("log-panel")
            .test_support()
            .occlude()
            .flex()
            .flex_col()
            .h_48()
            .max_h_full()
            .flex_shrink_0()
            .on_scroll(|_, _, cx| cx.stop_propagation())
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                div().flex().items_center().justify_between().px_3().py_1().child("Application logs").child(
                    Button::new("close-logs")
                        .small()
                        .ghost()
                        .icon(gpui_kit::assets::IconName::X)
                        .accessibility_label("Hide logs")
                        .on_click(|_, window, cx| window.dispatch_action(Box::new(ToggleLogs), cx)),
                ),
            )
            .child(
                div().flex_1().min_h_0().font_family(cx.theme().mono_font_family.clone()).child(
                    Textarea::new(&self.output)
                        .accessibility_id("log-output")
                        .aria_label("Application log output")
                        .readonly(true)
                        .bordered(false)
                        .h_full(),
                ),
            )
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::TestAppContext;
    use gpui_kit::test::TestWindowExt;

    use super::*;

    #[gpui_kit::test]
    fn log_output_displays_history_and_refreshes_without_allowing_edits(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let buffer = Arc::new(Mutex::new(LogBuffer::default()));
        buffer.lock().unwrap().append(b"startup warning\n");
        let handle = cx.open_window(gpui_kit::size(gpui_kit::px(640.), gpui_kit::px(480.)), |window, cx| {
            let view = cx.new(|cx| LogOutput::with_buffer(buffer.clone(), window, cx));
            gpui_kit::component::Root::new(view, window, cx)
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("input-state").value(), Some("startup warning\n"));
            window.click("input-state", cx);
            window.input("cannot edit", cx);
            assert_eq!(window.find("input-state").value(), Some("startup warning\n"));
        })
        .unwrap();
        cx.run_until_parked();
        buffer.lock().unwrap().append(b"live error\n");
        cx.executor().advance_clock(Duration::from_millis(250));
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("input-state").value(), Some("startup warning\nlive error\n"));
        })
        .unwrap();
    }

    #[test]
    fn buffer_retains_recent_output_with_a_utf8_safe_size_limit() {
        let mut buffer = LogBuffer::default();
        buffer.append("é".repeat(MAX_LOG_BYTES).as_bytes());
        buffer.append(b"\nlatest warning\n");
        assert!(buffer.text.len() <= MAX_LOG_BYTES);
        assert!(buffer.text.ends_with("\nlatest warning\n"));
        assert_eq!(buffer.revision, 2);
    }

    #[test]
    fn buffer_preserves_multiline_output() {
        let mut buffer = LogBuffer::default();
        buffer.append(b"[WARN map] first\n  caused by: second\n");
        assert_eq!(buffer.text, "[WARN map] first\n  caused by: second\n");
    }

    #[test]
    fn logger_captures_formatted_records_and_respects_filters() {
        use log::Log as _;
        let logger = crate::logging_builder().parse_filters("off,map=warn").build();
        logger.log(
            &log::Record::builder()
                .level(log::Level::Warn)
                .target("map::log_capture_test")
                .args(format_args!("captured diagnostic"))
                .build(),
        );
        logger.log(
            &log::Record::builder()
                .level(log::Level::Info)
                .target("map::log_capture_test")
                .args(format_args!("filtered diagnostic"))
                .build(),
        );
        let buffer = buffer();
        let buffer = buffer.lock().unwrap();
        assert!(buffer.text.contains("WARN"));
        assert!(buffer.text.contains("map::log_capture_test"));
        assert!(buffer.text.contains("captured diagnostic"));
        assert!(!buffer.text.contains("filtered diagnostic"));
        assert!(!buffer.text.contains('\u{1b}'));
    }
}
