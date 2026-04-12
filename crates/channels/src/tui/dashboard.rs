//! Dashboard view renderer: titled table with footer hint.

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Cell, Padding, Row, Table};

use crate::DashboardSnapshot;
use crate::tui::app::{AppState, ViewMode};

pub(crate) fn render(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let ViewMode::Dashboard {
        snapshot,
        table_state,
        ..
    } = &mut state.mode
    else {
        return;
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .padding(Padding::horizontal(1))
        .title(format!(" {} ", snapshot.title))
        .title_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );

    render_table(frame, area, block, snapshot, table_state);
}

fn render_table(
    frame: &mut Frame,
    area: Rect,
    block: Block<'_>,
    snapshot: &DashboardSnapshot,
    table_state: &mut ratatui::widgets::TableState,
) {
    let header_cells = snapshot
        .columns
        .iter()
        .map(|c| Cell::from(c.clone()).style(Style::default().add_modifier(Modifier::BOLD)));
    let header = Row::new(header_cells).height(1);

    let rows: Vec<Row> = snapshot
        .rows
        .iter()
        .map(|r| Row::new(r.iter().map(|c| Cell::from(c.clone()))).height(1))
        .collect();

    let widths: Vec<Constraint> = if snapshot.columns.is_empty() {
        vec![Constraint::Percentage(100)]
    } else {
        let per = 100u16 / snapshot.columns.len() as u16;
        snapshot
            .columns
            .iter()
            .map(|_| Constraint::Percentage(per))
            .collect()
    };

    let footer = snapshot
        .footer
        .clone()
        .unwrap_or_else(|| "↑/↓ select · PgUp/PgDn page · r refresh · Esc back".to_string());

    let table = Table::new(rows, widths)
        .header(header)
        .block(block.title_bottom(format!(" {footer} ")))
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );

    frame.render_stateful_widget(table, area, table_state);
}
