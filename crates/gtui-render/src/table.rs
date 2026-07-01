use gtui_core::frame::Frame;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Row, Table};

pub struct TableRenderer;

impl TableRenderer {
    pub fn render<'a>(frame: &'a Frame) -> Table<'a> {
        let headers: Vec<String> = frame.fields.iter().map(|f| f.name.clone()).collect();

        let mut rows = Vec::new();
        if !frame.fields.is_empty() {
            // Find max length among all series
            let max_len = frame
                .fields
                .iter()
                .map(|f| f.series.len())
                .max()
                .unwrap_or(0);

            for i in 0..max_len {
                let mut row_data = Vec::new();
                for field in &frame.fields {
                    // MVP naive string formatting for the table cell
                    let val_str = match field.series.get(i) {
                        Ok(val) => val.to_string(),
                        Err(_) => "".to_string(),
                    };
                    row_data.push(val_str);
                }
                rows.push(Row::new(row_data));
            }
        }

        // Just use proportional widths for MVP
        let widths: Vec<ratatui::layout::Constraint> = headers
            .iter()
            .map(|_| ratatui::layout::Constraint::Ratio(1, headers.len() as u32))
            .collect();

        Table::new(rows, widths)
            .header(Row::new(headers).style(Style::default().fg(Color::Yellow)))
            .block(Block::default().borders(Borders::ALL).title("Table"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtui_core::frame::{Field, FieldType};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::widgets::Widget;

    #[test]
    fn test_table_render() {
        let field = Field::new("val", FieldType::Float64, vec![1.0, 5.0]);
        let frame = Frame::new(vec![field]);

        let t = TableRenderer::render(&frame);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 10));
        t.render(buffer.area, &mut buffer);

        // Header
        assert_eq!(buffer.cell((1, 1)).unwrap().symbol(), "v");
        assert_eq!(buffer.cell((2, 1)).unwrap().symbol(), "a");
        assert_eq!(buffer.cell((3, 1)).unwrap().symbol(), "l");
    }
}
