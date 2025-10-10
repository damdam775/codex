use crossterm::event::KeyEvent;
use crossterm::terminal;
use lazy_static::lazy_static;
use ratatui::buffer::Buffer;
use ratatui::layout::Alignment;
use ratatui::layout::Constraint;
use ratatui::layout::Direction;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::BorderType;
use ratatui::widgets::Borders;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use ratatui::widgets::WidgetRef;
use regex_lite::Regex;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::thread;
use std::time::Duration;
use tui_textarea::Input;
use tui_textarea::Key;
use tui_textarea::TextArea;

use super::chat_composer_history::ChatComposerHistory;
use super::command_popup::CommandPopup;

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;

/// Minimum number of visible text rows inside the textarea.
const MIN_TEXTAREA_ROWS: usize = 1;
/// Rows consumed by the border.
const BORDER_LINES: u16 = 2;
/// Height reserved for rendering detected image references.
const MATCH_SUMMARY_ROWS: u16 = 2;
/// Delay in milliseconds before automatically hiding the preview window.
const PREVIEW_RELEASE_DELAY_MS: u64 = 200;

const PREVIEW_SCRIPT: &str = r#"
import sys
import os
import tkinter as tk
from urllib.parse import urlparse
from urllib.request import urlopen
from io import BytesIO

try:
    from PIL import Image, ImageTk  # type: ignore
    PIL_AVAILABLE = True
except Exception:  # pragma: no cover
    PIL_AVAILABLE = False

path = sys.argv[1]
label_text = sys.argv[2]
max_w = int(sys.argv[3])
max_h = int(sys.argv[4])

temp_path = None


def ensure_local(source: str):
    global temp_path
    if source.startswith("http://") or source.startswith("https://"):
        data = urlopen(source, timeout=10).read()
        if PIL_AVAILABLE:
            return data
        suffix = os.path.splitext(urlparse(source).path)[1] or ".img"
        import tempfile

        fd, temp_path = tempfile.mkstemp(suffix=suffix)
        with os.fdopen(fd, "wb") as fh:
            fh.write(data)
        return temp_path
    return source


def load_image():
    source = ensure_local(path)
    if PIL_AVAILABLE:
        if isinstance(source, bytes):
            img = Image.open(BytesIO(source))
        else:
            img = Image.open(source)
        img.thumbnail((max_w, max_h))
        return ImageTk.PhotoImage(img)
    photo = tk.PhotoImage(file=source)
    w = photo.width()
    h = photo.height()
    scale = 1
    while w // scale > max_w or h // scale > max_h:
        scale += 1
    if scale > 1:
        photo = photo.subsample(scale, scale)
    return photo


root = tk.Tk()
root.title(label_text)

try:
    image = load_image()
    container = tk.Label(root, image=image)
    container.image = image
    container.pack()
except Exception as exc:  # pragma: no cover
    tk.Label(
        root,
        text=f"Unable to preview image: {exc}",
        padx=12,
        pady=12,
    ).pack()

tk.Label(root, text=label_text, pady=6).pack()


def on_close():
    root.destroy()


root.protocol("WM_DELETE_WINDOW", on_close)
root.mainloop()

if temp_path and os.path.exists(temp_path):
    try:
        os.remove(temp_path)
    except OSError:
        pass
"#;

const IMAGE_EXTENSION_PATTERN: &str = r"(?:png|jpe?g|gif|bmp|webp|svg)";

lazy_static! {
    static ref PLACEHOLDER_REGEX: Regex = Regex::new(r"(?i)\[img_(\d+)\]").unwrap();
    static ref MARKDOWN_IMAGE_REGEX: Regex = Regex::new(r"!\[[^\]]*?\]\(([^)]+)\)").unwrap();
    static ref BRACKET_IMAGE_REGEX: Regex =
        Regex::new(r"(?i)\[image[^\]]*?\](?:\(([^)]+)\))?").unwrap();
    static ref QUOTED_IMAGE_REGEX: Regex = Regex::new(&format!(
        r#"['"]([^'"]+?\.{ext})['"]"#,
        ext = IMAGE_EXTENSION_PATTERN
    ))
    .unwrap();
    static ref IMAGE_URL_REGEX: Regex = Regex::new(&format!(
        r"https?://[^\s)]+?\.{ext}",
        ext = IMAGE_EXTENSION_PATTERN
    ))
    .unwrap();
    static ref IMAGE_PATH_REGEX: Regex = Regex::new(&format!(
        r"\b(?:\.[/\\]|[/\\]|[A-Za-z]:[/\\])[\w.-]+(?:[/\\][\w.-]+)*\.{ext}\b",
        ext = IMAGE_EXTENSION_PATTERN
    ))
    .unwrap();
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DetectedImage {
    key: String,
    label: String,
    original: String,
    path: Option<String>,
    start: usize,
}

/// Result returned when the user interacts with the text area.
pub enum InputResult {
    Submitted(String),
    None,
}

pub(crate) struct ChatComposer<'a> {
    textarea: TextArea<'a>,
    command_popup: Option<CommandPopup>,
    app_event_tx: AppEventSender,
    history: ChatComposerHistory,
    has_input_focus: bool,
    image_matches: Vec<DetectedImage>,
    selected_image_index: Option<usize>,
    previewing_index: Option<usize>,
    preview_process: Option<Child>,
    preview_release_token: u64,
    interrupt_mode_enabled: bool,
}

impl ChatComposer<'_> {
    pub fn new(has_input_focus: bool, app_event_tx: AppEventSender) -> Self {
        let mut textarea = TextArea::default();
        textarea.set_placeholder_text("send a message");
        textarea.set_cursor_line_style(ratatui::style::Style::default());

        let mut this = Self {
            textarea,
            command_popup: None,
            app_event_tx,
            history: ChatComposerHistory::new(),
            has_input_focus,
            image_matches: Vec::new(),
            selected_image_index: None,
            previewing_index: None,
            preview_process: None,
            preview_release_token: 0,
            interrupt_mode_enabled: false,
        };
        this.update_border(has_input_focus);
        this
    }

    /// Record the history metadata advertised by `SessionConfiguredEvent` so
    /// that the composer can navigate cross-session history.
    pub(crate) fn set_history_metadata(&mut self, log_id: u64, entry_count: usize) {
        self.history.set_metadata(log_id, entry_count);
    }

    /// Integrate an asynchronous response to an on-demand history lookup. If
    /// the entry is present and the offset matches the current cursor we
    /// immediately populate the textarea.
    pub(crate) fn on_history_entry_response(
        &mut self,
        log_id: u64,
        offset: usize,
        entry: Option<String>,
    ) -> bool {
        let updated = self
            .history
            .on_entry_response(log_id, offset, entry, &mut self.textarea);
        if updated {
            self.update_image_matches();
        }
        updated
    }

    pub fn set_input_focus(&mut self, has_focus: bool) {
        self.has_input_focus = has_focus;
        self.update_border(has_focus);
    }

    pub(crate) fn captures_tab(&self) -> bool {
        !self.image_matches.is_empty()
    }

    pub(crate) fn set_interrupt_mode(&mut self, enabled: bool) {
        if self.interrupt_mode_enabled != enabled {
            self.interrupt_mode_enabled = enabled;
            self.update_border(self.has_input_focus);
        }
    }

    pub(crate) fn stop_image_preview(&mut self, token: u64) -> bool {
        if token != self.preview_release_token {
            return false;
        }
        let had_preview = self.preview_process.is_some() || self.previewing_index.is_some();
        if had_preview {
            self.stop_preview_internal();
        }
        had_preview
    }

    /// Handle a key event coming from the main UI.
    pub fn handle_key_event(&mut self, key_event: KeyEvent) -> (InputResult, bool) {
        let (input_result, mut needs_redraw) = match self.command_popup {
            Some(_) => self.handle_key_event_with_popup(key_event),
            None => self.handle_key_event_without_popup(key_event),
        };

        // Update (or hide/show) popup after processing the key.
        self.sync_command_popup();
        if self.update_image_matches() {
            needs_redraw = true;
        }

        (input_result, needs_redraw)
    }

    /// Handle key event when the slash-command popup is visible.
    fn handle_key_event_with_popup(&mut self, key_event: KeyEvent) -> (InputResult, bool) {
        let Some(popup) = self.command_popup.as_mut() else {
            tracing::error!("handle_key_event_with_popup called without an active popup");
            return (InputResult::None, false);
        };

        match key_event.into() {
            Input { key: Key::Up, .. } => {
                popup.move_up();
                (InputResult::None, true)
            }
            Input { key: Key::Down, .. } => {
                popup.move_down();
                (InputResult::None, true)
            }
            Input { key: Key::Tab, .. } => {
                if let Some(cmd) = popup.selected_command() {
                    let first_line = self
                        .textarea
                        .lines()
                        .first()
                        .map(|s| s.as_str())
                        .unwrap_or("");

                    let starts_with_cmd = first_line
                        .trim_start()
                        .starts_with(&format!("/{}", cmd.command()));

                    if !starts_with_cmd {
                        self.textarea.select_all();
                        self.textarea.cut();
                        let _ = self.textarea.insert_str(format!("/{} ", cmd.command()));
                    }
                }
                (InputResult::None, true)
            }
            Input {
                key: Key::Enter,
                shift: false,
                alt: false,
                ctrl: false,
            } => {
                if let Some(cmd) = popup.selected_command() {
                    // Send command to the app layer.
                    self.app_event_tx.send(AppEvent::DispatchCommand(*cmd));

                    // Clear textarea so no residual text remains.
                    self.textarea.select_all();
                    self.textarea.cut();

                    // Hide popup since the command has been dispatched.
                    self.command_popup = None;
                    return (InputResult::None, true);
                }
                // Fallback to default newline handling if no command selected.
                self.handle_key_event_without_popup(key_event)
            }
            input => self.handle_input_basic(input),
        }
    }

    /// Handle key event when no popup is visible.
    fn handle_key_event_without_popup(&mut self, key_event: KeyEvent) -> (InputResult, bool) {
        let input: Input = key_event.into();

        let is_tab = matches!(input, Input { key: Key::Tab, .. });
        let is_right_arrow = matches!(
            input,
            Input {
                key: Key::Right,
                ..
            }
        ) && !input.ctrl
            && !input.alt;

        if self.selected_image_index.is_some() && !is_tab && !is_right_arrow {
            self.clear_image_selection();
        }

        if is_tab {
            let backwards = input.shift;
            if self.cycle_image_selection(backwards) {
                return (InputResult::None, true);
            }
        }

        if is_right_arrow {
            if self.preview_selected_image() {
                return (InputResult::None, true);
            }
        }

        match input {
            // -------------------------------------------------------------
            // History navigation (Up / Down) – only when the composer is not
            // empty or when the cursor is at the correct position, to avoid
            // interfering with normal cursor movement.
            // -------------------------------------------------------------
            Input { key: Key::Up, .. } => {
                if self.history.should_handle_navigation(&self.textarea) {
                    let consumed = self
                        .history
                        .navigate_up(&mut self.textarea, &self.app_event_tx);
                    if consumed {
                        return (InputResult::None, true);
                    }
                }
                self.handle_input_basic(input)
            }
            Input { key: Key::Down, .. } => {
                if self.history.should_handle_navigation(&self.textarea) {
                    let consumed = self
                        .history
                        .navigate_down(&mut self.textarea, &self.app_event_tx);
                    if consumed {
                        return (InputResult::None, true);
                    }
                }
                self.handle_input_basic(input)
            }
            Input {
                key: Key::Enter,
                shift: false,
                alt: false,
                ctrl: false,
            } => {
                let text = self.textarea.lines().join("\n");
                self.textarea.select_all();
                self.textarea.cut();

                if text.is_empty() {
                    (InputResult::None, true)
                } else {
                    self.history.record_local_submission(&text);
                    (InputResult::Submitted(text), true)
                }
            }
            Input {
                key: Key::Enter, ..
            }
            | Input {
                key: Key::Char('j'),
                ctrl: true,
                alt: false,
                shift: false,
            } => {
                self.textarea.insert_newline();
                (InputResult::None, true)
            }
            Input { key: Key::Tab, .. } => (InputResult::None, false),
            input => self.handle_input_basic(input),
        }
    }

    /// Handle generic Input events that modify the textarea content.
    fn handle_input_basic(&mut self, input: Input) -> (InputResult, bool) {
        self.textarea.input(input);
        (InputResult::None, true)
    }

    fn clear_image_selection(&mut self) {
        self.selected_image_index = None;
        self.stop_preview_internal();
    }

    fn cycle_image_selection(&mut self, backwards: bool) -> bool {
        if self.image_matches.is_empty() {
            return false;
        }

        let len = self.image_matches.len();
        let next_index = match self.selected_image_index {
            Some(current) => {
                if backwards {
                    (current + len - 1) % len
                } else {
                    (current + 1) % len
                }
            }
            None => {
                if backwards {
                    len - 1
                } else {
                    0
                }
            }
        };

        if self.selected_image_index != Some(next_index) {
            self.selected_image_index = Some(next_index);
            self.previewing_index = None;
            self.stop_preview_internal();
        }

        true
    }

    fn preview_selected_image(&mut self) -> bool {
        let Some(idx) = self.selected_image_index else {
            return false;
        };

        if !self.start_preview_for_index(idx) {
            return false;
        }

        self.schedule_preview_release();
        true
    }

    fn start_preview_for_index(&mut self, idx: usize) -> bool {
        if idx >= self.image_matches.len() {
            return false;
        }

        let Some(image) = self.image_matches.get(idx).cloned() else {
            return false;
        };

        let Some(path) = image.path.clone() else {
            return false;
        };

        if self.previewing_index == Some(idx) && self.preview_process.is_some() {
            return true;
        }

        self.stop_preview_internal();

        let (max_w, max_h) = self.compute_preview_bounds();
        let label = image.label.replace('\n', " ");

        if let Some(child) = spawn_preview_process(&path, &label, max_w, max_h) {
            self.preview_process = Some(child);
            self.previewing_index = Some(idx);
            true
        } else {
            false
        }
    }

    fn schedule_preview_release(&mut self) {
        self.preview_release_token = self.preview_release_token.wrapping_add(1);
        let token = self.preview_release_token;
        let sender = self.app_event_tx.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(PREVIEW_RELEASE_DELAY_MS));
            sender.send(AppEvent::ComposerStopPreview { token });
        });
    }

    fn compute_preview_bounds(&self) -> (usize, usize) {
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        let approx_width = ((cols as usize * 8) / 4).max(120);
        let approx_height = ((rows as usize * 16) / 4).max(120);
        (approx_width, approx_height)
    }

    fn stop_preview_internal(&mut self) {
        if let Some(mut child) = self.preview_process.take() {
            let _ = child.kill();
        }
        self.previewing_index = None;
    }

    fn update_image_matches(&mut self) -> bool {
        let text = self.textarea.lines().join("\n");
        let new_matches = detect_image_matches(&text);

        if new_matches == self.image_matches {
            return false;
        }

        let selected_key = self
            .selected_image_index
            .and_then(|idx| self.image_matches.get(idx))
            .map(|m| m.key.clone());
        let preview_key = self
            .previewing_index
            .and_then(|idx| self.image_matches.get(idx))
            .map(|m| m.key.clone());

        self.image_matches = new_matches;

        self.selected_image_index =
            selected_key.and_then(|key| self.image_matches.iter().position(|m| m.key == key));
        self.previewing_index =
            preview_key.and_then(|key| self.image_matches.iter().position(|m| m.key == key));

        if self.image_matches.is_empty() || self.selected_image_index.is_none() {
            self.clear_image_selection();
        } else if self.previewing_index.is_none() {
            self.stop_preview_internal();
        }

        true
    }

    fn render_image_matches(&self, area: Rect, buf: &mut Buffer) {
        if self.image_matches.is_empty() || area.height == 0 {
            return;
        }

        let mut spans: Vec<Span<'static>> = Vec::new();
        for (idx, image) in self.image_matches.iter().enumerate() {
            if idx > 0 {
                spans.push(Span::raw("  ".to_string()));
            }

            let trimmed = image.original.trim();
            let has_brackets = trimmed.starts_with('[') && trimmed.contains(']');
            let default_display = if has_brackets {
                trimmed.to_string()
            } else {
                format!("[{}]", image.label)
            };

            let display_text = if Some(idx) == self.selected_image_index {
                let arrow = '›';
                if Some(idx) == self.previewing_index {
                    format!("{default_display}{arrow}")
                } else {
                    format!("[SHOW]{arrow}")
                }
            } else {
                default_display
            };

            let color = if Some(idx) == self.selected_image_index {
                Color::Rgb(255, 169, 77)
            } else {
                Color::Rgb(96, 165, 250)
            };

            spans.push(Span::styled(display_text, Style::default().fg(color)));
        }

        let paragraph = Paragraph::new(Line::from(spans)).alignment(Alignment::Left);
        paragraph.render_ref(area, buf);
    }

    fn build_border_spans(&self) -> Vec<Span<'static>> {
        let mut spans = vec![Span::raw(
            "Enter to send | Ctrl+D to quit | Ctrl+J for newline".to_string(),
        )];

        if self.interrupt_mode_enabled {
            spans.push(Span::raw(" | ".to_string()));
            spans.push(Span::styled(
                "interrupt mode: ON".to_string(),
                Style::default().fg(Color::Rgb(255, 169, 77)),
            ));
        }

        spans
    }
    /// Synchronize `self.command_popup` with the current text in the
    /// textarea. This must be called after every modification that can change
    /// the text so the popup is shown/updated/hidden as appropriate.
    fn sync_command_popup(&mut self) {
        // Inspect only the first line to decide whether to show the popup. In
        // the common case (no leading slash) we avoid copying the entire
        // textarea contents.
        let first_line = self
            .textarea
            .lines()
            .first()
            .map(|s| s.as_str())
            .unwrap_or("");

        if first_line.starts_with('/') {
            // Create popup lazily when the user starts a slash command.
            let popup = self.command_popup.get_or_insert_with(CommandPopup::new);

            // Forward *only* the first line since `CommandPopup` only needs
            // the command token.
            popup.on_composer_text_change(first_line.to_string());
        } else if self.command_popup.is_some() {
            // Remove popup when '/' is no longer the first character.
            self.command_popup = None;
        }
    }

    pub fn calculate_required_height(&self, area: &Rect) -> u16 {
        let rows = self.textarea.lines().len().max(MIN_TEXTAREA_ROWS);
        let num_popup_rows = if let Some(popup) = &self.command_popup {
            popup.calculate_required_height(area)
        } else {
            0
        };
        let match_rows = if self.image_matches.is_empty() {
            0
        } else {
            MATCH_SUMMARY_ROWS
        };

        rows as u16 + BORDER_LINES + num_popup_rows + match_rows
    }

    fn update_border(&mut self, has_focus: bool) {
        struct BlockState {
            right_title: Line<'static>,
            border_style: Style,
        }

        let spans = self.build_border_spans();
        let focused_line = Line::from(spans.clone()).alignment(Alignment::Right);
        let unfocused_line = Line::from(spans).alignment(Alignment::Right);

        let bs = if has_focus {
            BlockState {
                right_title: focused_line,
                border_style: Style::default(),
            }
        } else {
            BlockState {
                right_title: unfocused_line,
                border_style: Style::default().dim(),
            }
        };

        self.textarea.set_block(
            ratatui::widgets::Block::default()
                .title_bottom(bs.right_title)
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(bs.border_style),
        );
    }

    pub(crate) fn is_command_popup_visible(&self) -> bool {
        self.command_popup.is_some()
    }
}

impl WidgetRef for &ChatComposer<'_> {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        if let Some(popup) = &self.command_popup {
            let popup_height = popup.calculate_required_height(&area);

            // Split the provided rect so that the popup is rendered at the
            // *top* and the textarea occupies the remaining space below.
            let popup_rect = Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: popup_height.min(area.height),
            };

            let remaining_height = area.height.saturating_sub(popup_rect.height);
            let remaining_rect = Rect {
                x: area.x,
                y: area.y + popup_rect.height,
                width: area.width,
                height: remaining_height,
            };

            popup.render(popup_rect, buf);

            if !self.image_matches.is_empty() && remaining_rect.height > 0 {
                let match_height = MATCH_SUMMARY_ROWS.min(remaining_rect.height);
                let layout = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(match_height), Constraint::Min(1)])
                    .split(remaining_rect);

                if let Some(match_rect) = layout.get(0) {
                    self.render_image_matches(*match_rect, buf);
                }
                if let Some(text_rect) = layout.get(1) {
                    self.textarea.render(*text_rect, buf);
                }
            } else {
                self.textarea.render(remaining_rect, buf);
            }
        } else {
            if !self.image_matches.is_empty() && area.height > 0 {
                let match_height = MATCH_SUMMARY_ROWS.min(area.height);
                let layout = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(match_height), Constraint::Min(1)])
                    .split(area);

                if let Some(match_rect) = layout.get(0) {
                    self.render_image_matches(*match_rect, buf);
                }
                if let Some(text_rect) = layout.get(1) {
                    self.textarea.render(*text_rect, buf);
                }
            } else {
                self.textarea.render(area, buf);
            }
        }
    }
}

impl Drop for ChatComposer<'_> {
    fn drop(&mut self) {
        self.stop_preview_internal();
    }
}

fn spawn_preview_process(path: &str, label: &str, max_w: usize, max_h: usize) -> Option<Child> {
    for program in ["python3", "python"] {
        let child = Command::new(program)
            .arg("-c")
            .arg(PREVIEW_SCRIPT)
            .arg(path)
            .arg(label)
            .arg(max_w.to_string())
            .arg(max_h.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();

        match child {
            Ok(child) => return Some(child),
            Err(_) => {
                // Try the next interpreter.
                continue;
            }
        }
    }

    None
}

fn detect_image_matches(value: &str) -> Vec<DetectedImage> {
    let mut matches: Vec<DetectedImage> = Vec::new();
    let mut occupied: Vec<(usize, usize)> = Vec::new();

    for caps in PLACEHOLDER_REGEX.captures_iter(value) {
        if let Some(m) = caps.get(0) {
            let token = m.as_str();
            let start = m.start();
            let end = m.end();
            if overlaps(&occupied, start, end) {
                continue;
            }
            push_detected_image(
                &mut matches,
                &mut occupied,
                format!("clipboard-{start}-{token}"),
                token.to_string(),
                token.to_string(),
                None,
                start,
            );
        }
    }

    for caps in MARKDOWN_IMAGE_REGEX.captures_iter(value) {
        if let (Some(full), Some(path_match)) = (caps.get(0), caps.get(1)) {
            let original = full.as_str();
            let start = full.start();
            let end = full.end();
            if overlaps(&occupied, start, end) {
                continue;
            }
            let path = path_match.as_str();
            if path.is_empty() {
                continue;
            }
            let label = label_from_path(path);
            push_detected_image(
                &mut matches,
                &mut occupied,
                format!("markdown-{start}-{label}"),
                label,
                original.to_string(),
                Some(path.to_string()),
                start,
            );
        }
    }

    for caps in BRACKET_IMAGE_REGEX.captures_iter(value) {
        if let Some(m) = caps.get(0) {
            let original = m.as_str().to_string();
            let start = m.start();
            let end = m.end();
            if overlaps(&occupied, start, end) {
                continue;
            }
            let captured_path = caps.get(1).map(|p| p.as_str().to_string());
            let inside = original
                .split_once(']')
                .map(|(inside, _)| inside.trim_start_matches('[').trim().to_string())
                .unwrap_or_default();
            let label = if !inside.is_empty() {
                inside.clone()
            } else if let Some(path) = captured_path.as_ref() {
                if path.is_empty() {
                    original.clone()
                } else {
                    label_from_path(path)
                }
            } else {
                original.clone()
            };

            let path = captured_path.filter(|p| !p.is_empty());

            push_detected_image(
                &mut matches,
                &mut occupied,
                format!("bracket-{start}-{label}"),
                label,
                original,
                path,
                start,
            );
        }
    }

    for caps in QUOTED_IMAGE_REGEX.captures_iter(value) {
        if let (Some(full), Some(path_match)) = (caps.get(0), caps.get(1)) {
            let original = full.as_str();
            let path = path_match.as_str();
            if path.is_empty() {
                continue;
            }
            let start = full.start();
            let end = full.end();
            if overlaps(&occupied, start, end) {
                continue;
            }
            let label = label_from_path(path);
            push_detected_image(
                &mut matches,
                &mut occupied,
                format!("quoted-{start}-{label}"),
                label,
                original.to_string(),
                Some(path.to_string()),
                start,
            );
        }
    }

    for m in IMAGE_URL_REGEX.find_iter(value) {
        let start = m.start();
        let end = m.end();
        if overlaps(&occupied, start, end) {
            continue;
        }
        let url = m.as_str();
        push_detected_image(
            &mut matches,
            &mut occupied,
            format!("url-{start}"),
            url.to_string(),
            url.to_string(),
            Some(url.to_string()),
            start,
        );
    }

    for m in IMAGE_PATH_REGEX.find_iter(value) {
        let start = m.start();
        let end = m.end();
        if overlaps(&occupied, start, end) {
            continue;
        }
        let path = m.as_str();
        let label = label_from_path(path);
        push_detected_image(
            &mut matches,
            &mut occupied,
            format!("path-{start}-{label}"),
            label,
            path.to_string(),
            Some(path.to_string()),
            start,
        );
    }

    matches.sort_by_key(|m| m.start);
    matches
}

fn push_detected_image(
    matches: &mut Vec<DetectedImage>,
    occupied: &mut Vec<(usize, usize)>,
    key: String,
    label: String,
    original: String,
    path: Option<String>,
    start: usize,
) {
    let end = start + original.len();
    occupied.push((start, end));
    matches.push(DetectedImage {
        key,
        label,
        original,
        path,
        start,
    });
}

fn overlaps(occupied: &[(usize, usize)], start: usize, end: usize) -> bool {
    occupied.iter().any(|&(s, e)| !(end <= s || start >= e))
}

fn label_from_path(path: &str) -> String {
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_string()
}
