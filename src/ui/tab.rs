use wxdragon::prelude::*;

/// One notebook page: a read-only output box above a single-line input box.
pub struct Tab {
    pub panel: Panel,
    pub output: TextCtrl,
    pub input: TextCtrl,
}

impl Tab {
    /// Creates the page's widgets. The caller adds `panel` to the notebook.
    pub fn new(notebook: &Notebook) -> Self {
        let panel = Panel::builder(notebook).build();

        // A static text just before each control gives it its accessible name on Windows.
        // Tab order follows creation order: Message first, then Output (the layout below
        // still puts Output on top).
        let input_label = StaticText::builder(&panel).with_label("Message").build();
        let input = TextCtrl::builder(&panel).with_style(TextCtrlStyle::ProcessEnter).build();
        let output_label = StaticText::builder(&panel).with_label("Output").build();
        let output = TextCtrl::builder(&panel)
            .with_style(TextCtrlStyle::MultiLine | TextCtrlStyle::ReadOnly | TextCtrlStyle::Rich2)
            .build();

        let sizer = BoxSizer::builder(Orientation::Vertical).build();
        sizer.add(&output_label, 0, SizerFlag::Left | SizerFlag::Top | SizerFlag::Right, 4);
        sizer.add(&output, 1, SizerFlag::Expand | SizerFlag::All, 4);
        let input_row = BoxSizer::builder(Orientation::Horizontal).build();
        input_row.add(&input_label, 0, SizerFlag::AlignCenterVertical | SizerFlag::Right, 4);
        input_row.add(&input, 1, SizerFlag::Expand, 0);
        sizer.add_sizer(&input_row, 0, SizerFlag::Expand | SizerFlag::All, 4);
        panel.set_sizer(sizer, true);

        Self { panel, output, input }
    }

    /// Adds a line at the end of the output without moving the cursor or selection.
    pub fn append_line(&self, line: &str) {
        let (sel_from, sel_to) = self.output.get_selection();
        let cursor = self.output.get_insertion_point();

        // Newline before rather than after, so there's no empty last line to arrow onto.
        if self.output.get_last_position() > 0 {
            self.output.append_text(&format!("\n{line}"));
        } else {
            self.output.append_text(line);
        }

        // append_text moves the cursor to the end; put it back.
        if sel_from != sel_to {
            self.output.set_selection(sel_from, sel_to);
        } else {
            self.output.set_insertion_point(cursor);
        }
    }
}
