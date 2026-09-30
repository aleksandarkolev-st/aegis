use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{
        Block, BorderType, Borders, List, ListItem, ListState, Padding, Paragraph, StatefulWidget,
        Widget,
    },
};
use unicode_width::UnicodeWidthStr;

use crate::ui::Palette;

fn rgb(channels: [u8; 3]) -> Color {
    Color::Rgb(channels[0], channels[1], channels[2])
}

fn text_lines(lines: &[String]) -> Vec<Line<'static>> {
    lines
        .iter()
        .map(|text| Line::from(crate::text::clean(text).replace('\n', " ")))
        .collect()
}

pub fn home(
    content: &[String],
    recent: &[String],
    workspace: Option<&str>,
    width: u16,
    palette: &Palette,
) -> Buffer {
    let wide = width >= 72;
    let height = if wide {
        content.len().max(recent.len())
    } else {
        content.len() + recent.len() + 1
    } as u16;
    let area = Rect::new(
        0,
        0,
        width,
        height + 4 + if workspace.is_some() { 2 } else { 0 },
    );
    let mut buffer = Buffer::empty(area);
    let border = Block::bordered()
        .border_type(BorderType::Rounded)
        .padding(Padding::uniform(1))
        .title(format!(" aegis {} ", env!("CARGO_PKG_VERSION")))
        .border_style(Style::new().fg(rgb(palette.quiet)))
        .title_style(
            Style::new()
                .fg(rgb(palette.accent))
                .add_modifier(Modifier::BOLD),
        );
    let inner = border.inner(area);
    border.render(area, &mut buffer);
    let body = Rect::new(inner.x, inner.y, inner.width, height);
    let quiet = Style::new().fg(rgb(palette.quiet));
    if wide {
        let [left, divider, right] = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(3),
            Constraint::Fill(1),
        ])
        .areas(body);
        Paragraph::new(text_lines(content))
            .style(quiet)
            .render(left, &mut buffer);
        Paragraph::new(text_lines(recent))
            .style(quiet)
            .render(right, &mut buffer);
        Block::new()
            .borders(Borders::LEFT)
            .border_style(quiet)
            .render(
                Rect::new(divider.x + 1, divider.y, 1, divider.height),
                &mut buffer,
            );
        if let Some(title) = recent.first() {
            Paragraph::new(crate::text::clean(title))
                .style(
                    Style::new()
                        .fg(rgb(palette.accent))
                        .add_modifier(Modifier::BOLD),
                )
                .render(Rect::new(right.x, right.y, right.width, 1), &mut buffer);
        }
    } else {
        let mut lines = text_lines(content);
        lines.push(Line::default());
        lines.extend(text_lines(recent));
        Paragraph::new(lines).style(quiet).render(body, &mut buffer);
    }
    if let Some(workspace) = workspace {
        Paragraph::new(crate::text::clean(workspace))
            .style(quiet)
            .render(
                Rect::new(inner.x, inner.y + height + 1, inner.width, 1),
                &mut buffer,
            );
    }
    buffer
}

pub fn composer(
    prefix: &str,
    lines: &[String],
    status: &str,
    width: u16,
    palette: &Palette,
) -> (Buffer, u16) {
    let input_height = lines.len().max(1).min(u16::MAX as usize - 2) as u16;
    let area = Rect::new(0, 0, width, input_height + 2);
    let mut buffer = Buffer::empty(area);
    let border = Block::bordered()
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .title_bottom(format!(" {} ", crate::text::clean(status)))
        .border_style(Style::new().fg(rgb(palette.quiet)));
    let inner = border.inner(area);
    border.render(area, &mut buffer);
    let prefix = crate::text::clean(prefix).trim_start().to_owned();
    Paragraph::new(prefix.clone())
        .style(Style::new().fg(rgb(palette.accent)))
        .render(Rect::new(inner.x, inner.y, inner.width, 1), &mut buffer);
    let input = composer_input_area(&prefix, area);
    let empty = lines.is_empty() || (lines.len() == 1 && lines[0].is_empty());
    let visible = if empty {
        vec!["Type a message…".to_owned()]
    } else {
        lines.to_vec()
    };
    let style = Style::new().fg(if empty {
        rgb(palette.quiet)
    } else {
        Color::Reset
    });
    for (index, line) in visible.iter().take(input.height as usize).enumerate() {
        Paragraph::new(crate::text::clean(line))
            .style(style)
            .render(
                Rect::new(input.x, input.y + index as u16, input.width, 1),
                &mut buffer,
            );
    }
    (buffer, input.x)
}

pub fn composer_input_area(prefix: &str, area: Rect) -> Rect {
    let inner = Block::bordered()
        .padding(Padding::horizontal(1))
        .inner(area);
    let offset = crate::text::clean(prefix)
        .trim_start()
        .width()
        .min(inner.width as usize) as u16;
    Rect::new(
        inner.x + offset,
        inner.y,
        inner.width.saturating_sub(offset),
        inner.height,
    )
}

pub fn menu(
    title: &str,
    choices: &[String],
    matches: &[usize],
    selected: usize,
    rows: u16,
    query: &str,
    width: u16,
    palette: &Palette,
) -> Buffer {
    let area = Rect::new(0, 0, width, rows + 2);
    let mut buffer = Buffer::empty(area);
    let quiet = Style::new().fg(rgb(palette.quiet));
    Paragraph::new(crate::text::clean(title))
        .style(
            Style::new()
                .fg(rgb(palette.accent))
                .add_modifier(Modifier::BOLD),
        )
        .render(Rect::new(2, 0, width.saturating_sub(4), 1), &mut buffer);
    if matches.is_empty() {
        Paragraph::new("No matches · Backspace clears the search")
            .style(quiet)
            .render(Rect::new(2, 1, width.saturating_sub(4), rows), &mut buffer);
    } else {
        let items: Vec<_> = matches
            .iter()
            .map(|index| {
                ListItem::new(format!(
                    "{}  {}",
                    index + 1,
                    crate::text::clean(&choices[*index]).replace('\n', " ")
                ))
            })
            .collect();
        let offset = selected
            .saturating_sub(rows as usize / 2)
            .min(matches.len().saturating_sub(rows as usize));
        let mut state = ListState::default()
            .with_selected(Some(selected))
            .with_offset(offset);
        StatefulWidget::render(
            List::new(items)
                .style(quiet)
                .highlight_symbol("› ")
                .highlight_style(
                    Style::new()
                        .fg(rgb(palette.accent))
                        .add_modifier(Modifier::BOLD | Modifier::REVERSED),
                ),
            Rect::new(2, 1, width.saturating_sub(4), rows),
            &mut buffer,
            &mut state,
        );
    }
    Paragraph::new(format!(
        "↑↓ select · Enter choose · Esc back · {}/{} · search: {}",
        selected + usize::from(!matches.is_empty()),
        matches.len(),
        crate::text::clean(query)
    ))
    .style(quiet)
    .render(
        Rect::new(2, rows + 1, width.saturating_sub(4), 1),
        &mut buffer,
    );
    buffer
}

pub fn row_text(buffer: &Buffer, row: u16) -> String {
    let mut result = String::new();
    let mut column = 0;
    while column < buffer.area.width {
        let symbol = buffer[(column, row)].symbol();
        result.push_str(symbol);
        column += symbol.width().max(1) as u16;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widgets_clip_unicode_and_keep_selection_in_the_filtered_list() {
        let palette = Palette::default();
        let choices = ["hidden", "日本語 🦊", "selected"].map(str::to_owned);
        let buffer = menu("Models", &choices, &[1, 2], 1, 2, "", 40, &palette);
        assert!(row_text(&buffer, 2).contains("selected"));
        assert!(!row_text(&buffer, 1).contains("hidden"));
        assert!(buffer[(2, 2)].modifier.contains(Modifier::REVERSED));
        assert!((0..buffer.area.height).all(|row| row_text(&buffer, row).width() == 40));
        for width in [16, 40, 76, 96] {
            let (buffer, input_column) =
                composer("› ", &["日本語 🦊".into()], "model · low", width, &palette);
            assert_eq!(buffer.area.height, 3);
            assert!((0..3).all(|row| row_text(&buffer, row).width() == width as usize));
            assert_eq!(input_column, 4);
        }
    }

    #[test]
    fn composer_reports_the_rendered_input_column() {
        let palette = Palette::default();
        for prefix in ["› ", "日本語 ", "🦊 "] {
            let (buffer, input_column) =
                composer(prefix, &["x".into()], "model · low", 80, &palette);
            assert_eq!(buffer[(input_column, 1)].symbol(), "x");
            assert_eq!(input_column as usize, 2 + prefix.width());
        }
        let rows = vec![
            "first visual row".to_owned(),
            "second visual row".to_owned(),
        ];
        let (buffer, input_column) = composer("› ", &rows, "model · low", 80, &palette);
        assert_eq!(buffer.area.height, 4);
        assert_eq!(buffer[(input_column, 1)].symbol(), "f");
        assert!(row_text(&buffer, 1).contains("first visual row"));
        assert!(row_text(&buffer, 2).contains("second visual row"));
    }

    #[test]
    fn composer_input_area_matches_rendered_text_at_narrow_and_wide_widths() {
        let palette = Palette::default();
        for width in [16, 40, 80] {
            for prefix in ["› ", "日本語 ", "🦊 "] {
                let area = Rect::new(0, 0, width, 3);
                let input = composer_input_area(prefix, area);
                let (buffer, origin) =
                    composer(prefix, &["x".into()], "model · low", width, &palette);
                assert_eq!(origin, input.x);
                assert_eq!(buffer[(input.x, input.y)].symbol(), "x");
                assert_eq!(input.x + input.width, width - 2);
            }
        }
    }
}
