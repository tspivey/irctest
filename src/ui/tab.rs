use std::cell::Cell;
use std::rc::Rc;

use irctest::playback::{TextPoint, ViewAnchor, history_edit};
use wxdragon::prelude::*;

use super::richedit::disable_beep;

/// One notebook page: a read-only output box above a single-line input box.
pub struct Tab {
    pub panel: Panel,
    pub output: TextCtrl,
    pub input: TextCtrl,
    output_reviewed: Rc<Cell<bool>>,
}

impl Tab {
    /// Creates the page's widgets. The caller adds `panel` to the notebook.
    pub fn new(notebook: &Notebook) -> Self {
        let panel = Panel::builder(notebook).build();

        // A static text just before each control gives it its accessible name on Windows.
        // Tab order follows creation order: Message first, then Output (the layout below
        // still puts Output on top).
        let input_label = StaticText::builder(&panel).with_label("Message").build();
        let input = TextCtrl::builder(&panel)
            .with_style(TextCtrlStyle::ProcessEnter | TextCtrlStyle::ProcessTab)
            .build();
        let output_label = StaticText::builder(&panel).with_label("Output").build();
        let output = TextCtrl::builder(&panel)
            .with_style(
                TextCtrlStyle::MultiLine
                    | TextCtrlStyle::ReadOnly
                    | TextCtrlStyle::Rich2
                    | TextCtrlStyle::DontWrap,
            )
            .build();
        disable_beep(&output);
        let output_reviewed = Rc::new(Cell::new(false));
        let reviewed = output_reviewed.clone();
        output.on_set_focus(move |event| {
            reviewed.set(true);
            event.skip(true);
        });

        let sizer = BoxSizer::builder(Orientation::Vertical).build();
        sizer.add(
            &output_label,
            0,
            SizerFlag::Left | SizerFlag::Top | SizerFlag::Right,
            4,
        );
        sizer.add(&output, 1, SizerFlag::Expand | SizerFlag::All, 4);
        let input_row = BoxSizer::builder(Orientation::Horizontal).build();
        input_row.add(
            &input_label,
            0,
            SizerFlag::AlignCenterVertical | SizerFlag::Right,
            4,
        );
        input_row.add(&input, 1, SizerFlag::Expand, 0);
        sizer.add_sizer(&input_row, 0, SizerFlag::Expand | SizerFlag::All, 4);
        panel.set_sizer(sizer, true);

        Self {
            panel,
            output,
            input,
            output_reviewed,
        }
    }

    /// Starts at the end until Output is first focused, then preserves reading position.
    /// Beyond `MAX_LINES`, the oldest lines are removed.
    pub fn append_line(&self, line: &str) -> usize {
        let (sel_from, sel_to) = self.output.get_selection();
        let cursor = self.output.get_insertion_point();

        // Newline before rather than after, so there's no empty last line to arrow onto.
        if self.output.get_last_position() > 0 {
            self.output.append_text(&format!("\n{line}"));
        } else {
            self.output.append_text(line);
        }

        // Positions after the removed text shift back by its length.
        let untrimmed = self.line_count();
        let removed = self.trim_to_max_lines();
        let shift = |pos: i64| (pos - removed).max(0);

        // Keep new output at the bottom until the user starts reviewing it.
        if sel_from != sel_to {
            self.output.set_selection(shift(sel_from), shift(sel_to));
        } else if !self.output_reviewed.get() {
            self.output
                .set_insertion_point(self.output.get_last_position());
        } else {
            self.output.set_insertion_point(shift(cursor));
        }
        untrimmed.saturating_sub(self.line_count())
    }

    pub fn line_count(&self) -> usize {
        if self.output.get_last_position() == 0 {
            0
        } else {
            self.output.get_number_of_lines().max(0) as usize
        }
    }
    /// Inserts without replacing the native control contents. Coordinate conversions
    /// deliberately stay in wx units (UTF-16 on Windows), never Rust byte offsets.
    pub fn insert_history(&self, lines: &[String], before_line: usize) -> HistoryInsertion {
        let output = self.output;
        let count = self.line_count();
        let point = |pos| {
            let (column, line) = output.position_to_xy(pos).unwrap_or((0, 0));
            TextPoint {
                line: line.max(0) as usize,
                column: column.max(0) as usize,
            }
        };
        let (from, to) = output.get_selection();
        let anchor = ViewAnchor {
            cursor: point(output.get_insertion_point()),
            selection: (from != to).then(|| (point(from), point(to))),
        };
        let existing: Vec<_> = (0..count)
            .map(|line| output.get_line_text(line as i64))
            .collect();
        let edit = history_edit(&existing, lines.to_vec(), before_line, anchor);
        if !edit.inserted.is_empty() {
            let position = if edit.insert_at == count {
                output.get_last_position()
            } else {
                output.xy_to_position(0, edit.insert_at as i64)
            };
            output.set_insertion_point(position);
            let text = edit.inserted.join("\n");
            let text = if count == 0 {
                text
            } else if edit.insert_at == count {
                format!("\n{text}")
            } else {
                format!("{text}\n")
            };
            output.write_text(&text);
            self.trim_to_max_lines();
            let restore =
                |p: TextPoint| output.xy_to_position(p.column as i64, p.line as i64).max(0);
            if let Some((from, to)) = edit.anchor.selection {
                output.set_selection(restore(from), restore(to));
            } else if !self.output_reviewed.get() {
                output.set_insertion_point(output.get_last_position());
            } else {
                output.set_insertion_point(restore(edit.anchor.cursor));
            }
        }
        HistoryInsertion {
            at: edit.insert_at,
            inserted: edit.inserted.len(),
            trimmed: edit.trim_prefix,
        }
    }

    /// Removes the oldest lines beyond `MAX_LINES`; returns how many characters went.
    fn trim_to_max_lines(&self) -> i64 {
        let excess = self.output.get_number_of_lines() as i64 - MAX_LINES;
        if excess <= 0 {
            return 0;
        }
        // Start of the first line to keep.
        let end = self.output.xy_to_position(0, excess);
        if end <= 0 {
            return 0;
        }
        self.output.remove(0, end);
        end
    }
}

#[derive(Debug, Clone, Copy)]
pub struct HistoryInsertion {
    pub at: usize,
    pub inserted: usize,
    pub trimmed: usize,
}

/// Lines kept in each tab's output.
const MAX_LINES: i64 = 10_000;

/// Sets a notebook tab's label. wxDragon's `Notebook` has no binding for this, so it calls
/// wxDragon's `wxd_Treebook_SetPageText`, which does `((wxTreebook*)self)->SetPageText(...)`:
/// a virtual call on `wxBookCtrlBase`, so it reaches `wxNotebook::SetPageText`. Technically
/// undefined behaviour (the object isn't a wxTreebook); switch to a `wxd_Notebook_SetPageText`
/// binding if wxDragon gains one.
pub fn set_page_text(notebook: &Notebook, page: usize, text: &str) {
    let Ok(text) = std::ffi::CString::new(text) else {
        return;
    };
    let ptr = notebook.handle_ptr() as *mut wxdragon::ffi::wxd_Treebook_t;
    unsafe { wxdragon::ffi::wxd_Treebook_SetPageText(ptr, page, text.as_ptr()) };
}
