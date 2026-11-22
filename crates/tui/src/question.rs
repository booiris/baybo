//! Keyboard interaction for the structured questions in one AskUser call.

use baybo_channels::wire::QuestionReply;
use baybo_model::human_ask::{AskId, Question, QuestionAnswer};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::markdown::{Gutter, wrap};

pub(crate) struct QuestionForm {
    pub id: AskId,
    questions: Vec<Question>,
    answers: Vec<QuestionAnswer>,
    active: usize,
    editing: bool,
    scroll: usize,
    pub submitting: bool,
}

impl QuestionForm {
    pub fn new(id: AskId, questions: Vec<Question>) -> Self {
        let answers = questions
            .iter()
            .map(|q| QuestionAnswer {
                header: q.header.clone(),
                selected: Vec::new(),
                other: None,
            })
            .collect();
        Self {
            id,
            questions,
            answers,
            active: 0,
            editing: false,
            scroll: 0,
            submitting: false,
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Option<QuestionReply> {
        if self.submitting
            || key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        if key.kind == KeyEventKind::Repeat
            && !matches!(key.code, KeyCode::Up | KeyCode::Down)
            && !(self.editing && matches!(key.code, KeyCode::Char(_) | KeyCode::Backspace))
        {
            return None;
        }
        let q = self.questions.get(self.active)?;
        let answer = &mut self.answers[self.active];
        let last = self.questions.len() - 1;
        if self.editing {
            match key.code {
                KeyCode::Char(c) => {
                    answer.other.get_or_insert_with(String::new).push(c);
                    if !q.multi_select && !c.is_whitespace() {
                        answer.selected.clear();
                    }
                }
                KeyCode::Backspace => {
                    if let Some(text) = &mut answer.other {
                        text.pop();
                    }
                }
                KeyCode::Enter | KeyCode::Esc => {
                    self.editing = false;
                    self.scroll = 0;
                }
                _ => {}
            }
            return None;
        }
        match key.code {
            KeyCode::Char(c @ '1'..='9') => {
                let opt = q.options.get(c as usize - '1' as usize)?;
                if q.multi_select {
                    if let Some(index) = answer.selected.iter().position(|v| v == &opt.label) {
                        answer.selected.remove(index);
                    } else {
                        answer.selected.push(opt.label.clone());
                    }
                } else {
                    answer.selected = vec![opt.label.clone()];
                    answer.other = None;
                }
            }
            KeyCode::Char('o') => self.editing = true,
            KeyCode::Right | KeyCode::Tab => {
                self.active = (self.active + 1) % self.questions.len();
                self.scroll = 0;
            }
            KeyCode::Left | KeyCode::BackTab => {
                self.active = self.active.checked_sub(1).unwrap_or(last);
                self.scroll = 0;
            }
            KeyCode::Down => self.scroll = self.scroll.saturating_add(1),
            KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::Esc => return Some(QuestionReply::Cancelled),
            KeyCode::Enter => {
                if let Some(unanswered) = self.unanswered() {
                    self.active = unanswered;
                    self.scroll = 0;
                } else {
                    let mut answers = self.answers.clone();
                    for answer in &mut answers {
                        answer.other = answer
                            .other
                            .as_deref()
                            .map(str::trim)
                            .filter(|v| !v.is_empty())
                            .map(str::to_owned);
                    }
                    return Some(QuestionReply::Answered { answers });
                }
            }
            _ => {}
        }
        None
    }

    fn unanswered(&self) -> Option<usize> {
        self.answers.iter().position(|a| {
            a.selected.is_empty() && a.other.as_deref().unwrap_or_default().trim().is_empty()
        })
    }

    fn rows(&self, width: u16) -> (Vec<Line<'static>>, Vec<Line<'static>>) {
        let mut body = Vec::new();
        let mut add = |text: &str, style| {
            for line in text.split('\n') {
                body.extend(wrap(
                    &[Span::styled(line.to_owned(), style)],
                    usize::from(width),
                    &Gutter::none(),
                ));
            }
        };
        let q = &self.questions[self.active];
        let a = &self.answers[self.active];
        add(
            &format!(
                "Question {}/{} · {}",
                self.active + 1,
                self.questions.len(),
                q.header
            ),
            Style::default().fg(Color::Cyan),
        );
        add(&q.question, Style::default());
        for (index, opt) in q.options.iter().enumerate() {
            let selected = a.selected.contains(&opt.label);
            let marker = if selected { "[x]" } else { "[ ]" };
            add(
                &format!("{} {marker} {} — {}", index + 1, opt.label, opt.description),
                Style::default().fg(if selected { Color::Green } else { Color::Reset }),
            );
        }
        let caret = if self.editing { "▏" } else { "" };
        add(
            &format!("Other: {}{caret}", a.other.as_deref().unwrap_or_default()),
            Style::default(),
        );
        let hint = if self.submitting {
            "Waiting for the gateway…".to_owned()
        } else if self.editing {
            "Enter/Esc finish editing".to_owned()
        } else {
            let choose = if q.multi_select { "toggle" } else { "choose" };
            let tabs = if self.questions.len() > 1 {
                " · Tab change question"
            } else {
                ""
            };
            let enter = if self.unanswered().is_none() {
                "submit"
            } else {
                "continue"
            };
            format!(
                "1–{} {choose} · o other{tabs} · Enter {enter} · Esc cancel · ↑↓ scroll · Ctrl-X stop",
                q.options.len()
            )
        };
        (
            body,
            wrap(
                &[Span::styled(hint, Style::default().fg(Color::DarkGray))],
                usize::from(width),
                &Gutter::none(),
            ),
        )
    }

    pub fn height(&self, width: u16) -> u16 {
        let (body, hint) = self.rows(width);
        u16::try_from(body.len() + hint.len()).unwrap_or(u16::MAX)
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let (body, hint) = self.rows(area.width);
        let available = usize::from(area.height);
        let body_height = available.saturating_sub(hint.len()).max(1);
        let max_scroll = body.len().saturating_sub(body_height);
        self.scroll = if self.editing {
            max_scroll
        } else {
            self.scroll.min(max_scroll)
        };
        let lines: Vec<_> = body
            .into_iter()
            .skip(self.scroll)
            .take(body_height)
            .chain(hint)
            .take(available)
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::human_ask::QuestionOption;

    fn form() -> QuestionForm {
        let questions = [("Build", false), ("Targets", true)]
            .into_iter()
            .map(|(header, multi_select)| Question {
                header: header.into(),
                question: format!("Choose {header}"),
                options: ["One", "Two"]
                    .map(|label| QuestionOption {
                        label: label.into(),
                        description: "说明".into(),
                    })
                    .into(),
                multi_select,
            })
            .collect();
        QuestionForm::new("ask".parse().unwrap(), questions)
    }

    fn press(form: &mut QuestionForm, code: KeyCode) -> Option<QuestionReply> {
        form.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn answers_every_tab_and_keeps_other_exclusive_only_for_single_select() {
        let mut form = form();
        for ch in "1o自".chars() {
            press(&mut form, KeyCode::Char(ch));
        }
        press(&mut form, KeyCode::Enter);
        assert!(press(&mut form, KeyCode::Enter).is_none());
        assert_eq!(form.active, 1);
        for ch in "121o备注 ".chars() {
            press(&mut form, KeyCode::Char(ch));
        }
        press(&mut form, KeyCode::Esc);
        press(&mut form, KeyCode::BackTab);
        assert_eq!(
            press(&mut form, KeyCode::Enter),
            Some(QuestionReply::Answered {
                answers: vec![
                    QuestionAnswer {
                        header: "Build".into(),
                        selected: vec![],
                        other: Some("自".into())
                    },
                    QuestionAnswer {
                        header: "Targets".into(),
                        selected: vec!["Two".into()],
                        other: Some("备注".into())
                    },
                ]
            })
        );
    }

    #[test]
    fn selecting_a_single_option_replaces_free_text_and_cancel_sends_no_answers() {
        let mut form = form();
        for c in "o自由".chars() {
            press(&mut form, KeyCode::Char(c));
        }
        press(&mut form, KeyCode::Enter);
        press(&mut form, KeyCode::Char('2'));
        assert_eq!(form.answers[0].other, None);
        assert_eq!(
            press(&mut form, KeyCode::Esc),
            Some(QuestionReply::Cancelled)
        );
        form.submitting = true;
        assert_eq!(press(&mut form, KeyCode::Esc), None);
    }
}
