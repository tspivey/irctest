#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn insertion_preserves_unicode_anchor_and_trimming() {
        let existing = vec!["😀 é".into(); 10_000];
        let anchor = ViewAnchor {
            cursor: TextPoint {
                line: 40,
                column: 3,
            },
            selection: Some((
                TextPoint {
                    line: 42,
                    column: 1,
                },
                TextPoint {
                    line: 39,
                    column: 2,
                },
            )),
        };
        let edit = history_edit(&existing, vec!["older".into(); 3], 20, anchor);
        assert_eq!(edit.trim_prefix, 3);
        assert_eq!(
            edit.anchor.cursor,
            TextPoint {
                line: 40,
                column: 3
            }
        );
        let e = history_edit(
            &["live 😀".into()],
            vec!["old".into()],
            0,
            ViewAnchor {
                cursor: TextPoint { line: 0, column: 6 },
                selection: None,
            },
        );
        assert_eq!(e.anchor.cursor, TextPoint { line: 1, column: 6 });
        let p = delivery_policy(Some(HistoryReason::Older), false);
        assert!(!p.speak && !p.membership && !p.log);
    }
    #[test]
    fn history_into_an_empty_view_keeps_the_anchor_on_a_valid_line() {
        let edit = history_edit(&[], vec!["old 😀 é".into()], 0, ViewAnchor::default());
        assert_eq!(edit.anchor.cursor, TextPoint::default());
    }
    #[test]
    fn reversed_selection_and_live_append_keep_the_same_original_lines() {
        let existing = vec!["😀 é before".into(), "live one".into(), "live two".into()];
        let edit = history_edit(
            &existing,
            vec!["older one".into(), "older two".into()],
            1,
            ViewAnchor {
                cursor: TextPoint { line: 2, column: 4 },
                selection: Some((
                    TextPoint { line: 2, column: 4 },
                    TextPoint { line: 0, column: 2 },
                )),
            },
        );
        let mut rendered = existing.clone();
        rendered.splice(edit.insert_at..edit.insert_at, edit.inserted.clone());
        rendered.drain(..edit.trim_prefix);
        assert_eq!(rendered[edit.anchor.cursor.line], existing[2]);
        let (end, start) = edit.anchor.selection.unwrap();
        assert_eq!(end, TextPoint { line: 4, column: 4 });
        assert_eq!(start, TextPoint { line: 0, column: 2 });
        assert_eq!(rendered[start.line], existing[0]);
    }
    #[test]
    fn live_initial_and_catchup_policies_follow_context_logging_rules() {
        let live = delivery_policy(None, true);
        assert!(live.speak && live.membership && live.log);
        for reason in [
            HistoryReason::Initial,
            HistoryReason::CatchUp,
            HistoryReason::Older,
        ] {
            for cold in [true, false] {
                let policy = delivery_policy(Some(reason), cold);
                assert!(!policy.speak && !policy.membership);
                assert_eq!(policy.log, reason == HistoryReason::CatchUp && !cold);
            }
        }
    }
}

use super::HistoryReason;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TextPoint {
    pub line: usize,
    pub column: usize,
}
#[derive(Debug, Clone, Default)]
pub struct ViewAnchor {
    pub cursor: TextPoint,
    pub selection: Option<(TextPoint, TextPoint)>,
}
pub struct TimelineEdit {
    pub insert_at: usize,
    pub inserted: Vec<String>,
    pub trim_prefix: usize,
    pub anchor: ViewAnchor,
}
pub fn history_edit(
    existing: &[String],
    older: Vec<String>,
    insert_at: usize,
    mut anchor: ViewAnchor,
) -> TimelineEdit {
    let at = insert_at.min(existing.len());
    let count = older.len();
    let trim = (existing.len() + count).saturating_sub(10_000);
    let shift = |p: &mut TextPoint| {
        if existing.is_empty() {
            *p = TextPoint::default();
            return;
        }
        if p.line >= at {
            p.line += count;
        }
        if p.line < trim {
            p.line = 0;
            p.column = 0;
        } else {
            p.line -= trim;
        }
    };
    shift(&mut anchor.cursor);
    if let Some((a, b)) = &mut anchor.selection {
        shift(a);
        shift(b);
    }
    TimelineEdit {
        insert_at: at,
        inserted: older,
        trim_prefix: trim,
        anchor,
    }
}
pub struct DeliveryPolicy {
    pub speak: bool,
    pub membership: bool,
    pub log: bool,
}
pub fn delivery_policy(reason: Option<HistoryReason>, cold_start: bool) -> DeliveryPolicy {
    DeliveryPolicy {
        speak: reason.is_none(),
        membership: reason.is_none(),
        log: reason.is_none() || (reason == Some(HistoryReason::CatchUp) && !cold_start),
    }
}
