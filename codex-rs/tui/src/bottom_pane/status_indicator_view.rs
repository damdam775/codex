use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use ratatui::buffer::Buffer;
use ratatui::layout::Constraint;
use ratatui::layout::Direction;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;
use ratatui::widgets::WidgetRef;

use crate::app_event::AppEvent;
use crate::app_event::InterjectionMode;
use crate::app_event_sender::AppEventSender;
use crate::status_indicator_widget::StatusIndicatorWidget;

use super::BottomPaneView;
use super::bottom_pane_view::ConditionalUpdate;

pub(crate) struct StatusIndicatorView {
    view: StatusIndicatorWidget,
    app_event_tx: AppEventSender,
    interrupt_mode_enabled: bool,
    interjection_draft: String,
    interjection_mode: InterjectionMode,
}

impl StatusIndicatorView {
    pub fn new(app_event_tx: AppEventSender, height: u16, interrupt_mode_enabled: bool) -> Self {
        Self {
            view: StatusIndicatorWidget::new(app_event_tx.clone(), height),
            app_event_tx,
            interrupt_mode_enabled,
            interjection_draft: String::new(),
            interjection_mode: InterjectionMode::Add,
        }
    }

    pub fn update_text(&mut self, text: String) {
        self.view.update_text(text);
    }

    fn submit_interjection(&mut self) {
        let trimmed = self.interjection_draft.trim();
        if trimmed.is_empty() {
            self.reset_interjection_prompt();
            return;
        }

        let mut mode = self.interjection_mode;
        if mode == InterjectionMode::Add && (trimmed.starts_with('!') || trimmed.ends_with('!')) {
            mode = InterjectionMode::Interrupt;
        }

        let normalized = trimmed.trim_matches('!').trim();
        if normalized.is_empty() {
            self.reset_interjection_prompt();
            return;
        }

        self.app_event_tx.send(AppEvent::SubmitInterjection {
            text: normalized.to_string(),
            mode,
        });
        self.reset_interjection_prompt();
    }
}

impl<'a> BottomPaneView<'a> for StatusIndicatorView {
    fn handle_key_event(&mut self, _pane: &mut super::BottomPane<'a>, key_event: KeyEvent) {
        if !self.interrupt_mode_enabled {
            return;
        }

        if key_event.modifiers.contains(KeyModifiers::CONTROL)
            || key_event.modifiers.contains(KeyModifiers::ALT)
        {
            return;
        }

        match key_event.code {
            KeyCode::Enter => {
                self.submit_interjection();
            }
            KeyCode::Backspace => {
                self.interjection_draft.pop();
            }
            KeyCode::Char('!') if self.interjection_draft.is_empty() => {
                self.interjection_mode = if self.interjection_mode == InterjectionMode::Interrupt {
                    InterjectionMode::Add
                } else {
                    InterjectionMode::Interrupt
                };
            }
            KeyCode::Char(c) => {
                self.interjection_draft.push(c);
            }
            _ => {}
        }
    }

    fn update_status_text(&mut self, text: String) -> ConditionalUpdate {
        self.update_text(text);
        ConditionalUpdate::NeedsRedraw
    }

    fn should_hide_when_task_is_done(&mut self) -> bool {
        true
    }

    fn calculate_required_height(&self, _area: &Rect) -> u16 {
        let base = self.view.get_height();
        if self.interrupt_mode_enabled {
            base.saturating_add(2)
        } else {
            base
        }
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        if self.interrupt_mode_enabled {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(self.view.get_height()),
                    Constraint::Min(1),
                ])
                .split(area);

            if let Some(main) = chunks.get(0) {
                self.view.render_ref(*main, buf);
            }

            if let Some(prompt_area) = chunks.get(1) {
                let mode_label = if self.interjection_mode == InterjectionMode::Interrupt {
                    "Interrupt"
                } else {
                    "Add"
                };
                let color = if self.interjection_mode == InterjectionMode::Interrupt {
                    Color::Rgb(255, 169, 77)
                } else {
                    Color::Yellow
                };

                let spans = vec![
                    Span::styled(format!("[{mode_label}]"), Style::default().fg(color)),
                    Span::raw(" "),
                    Span::raw(self.interjection_draft.clone()),
                ];
                let paragraph = Paragraph::new(Line::from(spans));
                paragraph.render_ref(*prompt_area, buf);
            }
        } else {
            self.view.render_ref(area, buf);
        }
    }

    fn set_interrupt_mode(&mut self, enabled: bool) {
        self.interrupt_mode_enabled = enabled;
        if !enabled {
            self.reset_interjection_prompt();
        }
    }

    fn reset_interjection_prompt(&mut self) {
        self.interjection_draft.clear();
        self.interjection_mode = InterjectionMode::Add;
    }
}
