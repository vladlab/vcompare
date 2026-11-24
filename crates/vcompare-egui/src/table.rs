//! The Frames tab: every frame's metrics, virtual-scrolled, sortable,
//! synced with the cursor.

use crate::data::FrameRows;
use crate::theme;
use eframe::egui::{self, Align, RichText, Sense, Ui};
use egui_extras::{Column, TableBuilder};

#[derive(Clone, Copy, PartialEq)]
enum SortKey {
    Frame,
    Metric(usize),
}

pub struct FrameTable {
    sort: SortKey,
    ascending: bool,
    /// Row order: indices into `FrameRows` for the current sort.
    order: Vec<u32>,
    /// Position in `order` of each row (inverse of `order`).
    position: Vec<u32>,
    last_cursor: i64,
    scroll_to: Option<usize>,
}

impl FrameTable {
    pub fn new(rows: &FrameRows) -> Self {
        let mut t = Self {
            sort: SortKey::Frame,
            ascending: true,
            order: vec![],
            position: vec![],
            last_cursor: i64::MIN,
            scroll_to: None,
        };
        t.resort(rows);
        t
    }

    fn resort(&mut self, rows: &FrameRows) {
        let n = rows.frames.len();
        let mut order: Vec<u32> = (0..n as u32).collect();
        if let SortKey::Metric(c) = self.sort {
            let col = &rows.columns[c].1;
            // NaN (no value) last either way.
            order.sort_by(|&a, &b| {
                let (x, y) = (col[a as usize], col[b as usize]);
                match (x.is_nan(), y.is_nan()) {
                    (true, true) => std::cmp::Ordering::Equal,
                    (true, false) => std::cmp::Ordering::Greater,
                    (false, true) => std::cmp::Ordering::Less,
                    _ if self.ascending => x.partial_cmp(&y).unwrap(),
                    _ => y.partial_cmp(&x).unwrap(),
                }
            });
        } else if !self.ascending {
            order.reverse();
        }
        let mut position = vec![0u32; n];
        for (p, &i) in order.iter().enumerate() {
            position[i as usize] = p as u32;
        }
        self.order = order;
        self.position = position;
    }

    /// Row frame `steps` rows away from `cursor` in the current order.
    pub fn step(&self, rows: &FrameRows, cursor: i64, steps: i64) -> Option<i64> {
        let i = rows.index_of(cursor)?;
        let p = self.position[i] as i64 + steps;
        let p = p.clamp(0, self.order.len() as i64 - 1) as usize;
        Some(rows.frames[self.order[p] as usize])
    }

    pub fn show(&mut self, ui: &mut Ui, rows: &FrameRows, cursor: i64) -> Option<i64> {
        if cursor != self.last_cursor {
            self.last_cursor = cursor;
            self.scroll_to = rows.index_of(cursor).map(|i| self.position[i] as usize);
        }
        let mut clicked = None;
        let mut resort = None;
        let row_h = 22.0;
        let mono = egui::FontId::monospace(12.0);

        let mut tb = TableBuilder::new(ui)
            .striped(true)
            // Jump, don't glide: a jump across a feature would take seconds.
            .animate_scrolling(false)
            .sense(Sense::click())
            .resizable(false)
            .cell_layout(egui::Layout::right_to_left(Align::Center))
            .column(Column::exact(80.0))
            .column(Column::exact(110.0));
        for _ in &rows.columns {
            tb = tb.column(Column::exact(82.0));
        }
        tb = tb.column(Column::remainder());
        if let Some(p) = self.scroll_to.take() {
            tb = tb.scroll_to_row(p, Some(Align::Center));
        }

        let header = |ui: &mut Ui, label: &str, key: SortKey, me: &Self| -> bool {
            // Right-to-left cell: the sort arrow first, then the label.
            if me.sort == key {
                let (r, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), Sense::hover());
                let c = r.center();
                let (a, b) = if me.ascending { (-3.0, 3.0) } else { (3.0, -3.0) };
                let pts = vec![c + egui::vec2(-4.0, b), c + egui::vec2(4.0, b), c + egui::vec2(0.0, a)];
                ui.painter().add(egui::Shape::convex_polygon(pts, theme::TEXT_DIM, egui::Stroke::NONE));
            }
            let r = ui.add(egui::Label::new(RichText::new(label).color(theme::TEXT_DIM).size(12.0)).sense(Sense::click()));
            r.clicked()
        };

        tb.header(24.0, |mut h| {
            h.col(|ui| {
                if header(ui, "Frame", SortKey::Frame, self) {
                    resort = Some(SortKey::Frame);
                }
            });
            h.col(|ui| {
                ui.label(RichText::new("TC").color(theme::TEXT_DIM).size(12.0));
            });
            for (c, (m, _)) in rows.columns.iter().enumerate() {
                h.col(|ui| {
                    if header(ui, theme::column_style(m).0, SortKey::Metric(c), self) {
                        resort = Some(SortKey::Metric(c));
                    }
                });
            }
            h.col(|_| {});
        })
        .body(|body| {
            body.rows(row_h, self.order.len(), |mut row| {
                let i = self.order[row.index()] as usize;
                let f = rows.frames[i];
                row.set_selected(f == cursor);
                row.col(|ui| {
                    ui.label(RichText::new(f.to_string()).font(mono.clone()));
                });
                row.col(|ui| {
                    ui.label(RichText::new(&rows.tcs[i]).font(mono.clone()).color(theme::TEXT_DIM));
                });
                for (m, col) in &rows.columns {
                    row.col(|ui| {
                        let v = col[i];
                        let text = if v.is_nan() { "—".to_string() } else { format!("{v:.*}", theme::column_style(m).1) };
                        ui.label(RichText::new(text).font(mono.clone()));
                    });
                }
                row.col(|_| {});
                if row.response().clicked() {
                    clicked = Some(f);
                }
            });
        });

        if let Some(k) = resort {
            if self.sort == k {
                self.ascending = !self.ascending;
            } else {
                self.sort = k;
                // Metrics: biggest first, the usual question.
                self.ascending = k == SortKey::Frame;
            }
            self.resort(rows);
            self.scroll_to = rows.index_of(cursor).map(|i| self.position[i] as usize);
        }
        if let Some(f) = clicked {
            // The row is already on screen: don't scroll to it.
            self.last_cursor = f;
        }
        clicked
    }
}
