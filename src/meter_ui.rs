//! The DPS meter's egui rendering — host-agnostic. The Wayland layer-shell
//! host and the X11 fallback viewport both call `draw()` with the current
//! `CombatState`; window concerns (positioning, click-through, hiding) stay
//! in the host. Visual layout mirrors the Windows client's meter: title bar
//! with tabs and chrome icons, ranked rows in class colors, cumulative
//! footer that expands into the mob picker.

use egui::{Color32, RichText};
use froklog::state::CombatState;

use crate::meter_core::{
    build_mob_picker_entries, build_summary_line, compute_snapshot, fmt_duration, fmt_k,
    resolve_view_mob_id, MeterTab,
};

/// UI state that survives across frames but is not persisted config.
pub struct MeterView {
    pub tab: MeterTab,
    pub pinned: Option<u64>,
    pub picker_open: bool,
}

impl Default for MeterView {
    fn default() -> Self {
        Self {
            tab: MeterTab::Dps,
            pinned: None,
            picker_open: false,
        }
    }
}

/// What the host must act on after a frame.
pub enum MeterAction {
    /// Title bar dragged by this much — move the window.
    Drag(egui::Vec2),
    /// Copy this summary line to the clipboard.
    Copy(String),
    /// Clear combat totals (the parser reset flag).
    Reset,
    /// Open the main window's meter settings.
    OpenSettings,
}

/// Snapshot of the persisted meter settings the frame needs.
#[derive(Clone, Copy)]
pub struct MeterStyle {
    pub max_rows: usize,
    pub font_size: f32,
}

fn dim(c: (u8, u8, u8)) -> Color32 {
    Color32::from_rgb(c.0, c.1, c.2)
}

/// The group-share bar: fill = this row's fraction of the cumulative total,
/// painted with the web viewer's class-gradient recipe — solid for one
/// class, blended stops for 2–3 (`classGradient` in stream.html).
fn paint_share_bar(
    ui: &mut egui::Ui,
    size: egui::Vec2,
    pct: f32,
    colors: &[(u8, u8, u8)],
    label_size: f32,
) {
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 2.0, Color32::from_rgba_unmultiplied(255, 255, 255, 12));
    let w = rect.width() * pct.clamp(0.0, 1.0);
    if w >= 1.0 {
        let c = |i: usize| {
            let (r, g, b) = colors[i.min(colors.len() - 1)];
            Color32::from_rgba_unmultiplied(r, g, b, 200)
        };
        // Same stop positions as the web bars.
        let stops: Vec<(f32, Color32)> = match colors.len() {
            0 | 1 => vec![(0.0, c(0)), (1.0, c(0))],
            2 => vec![(0.0, c(0)), (0.35, c(0)), (0.65, c(1)), (1.0, c(1))],
            _ => vec![
                (0.0, c(0)),
                (0.25, c(0)),
                (0.40, c(1)),
                (0.60, c(1)),
                (0.75, c(2)),
                (1.0, c(2)),
            ],
        };
        let mut mesh = egui::Mesh::default();
        let (top, bot) = (rect.top(), rect.bottom());
        for pair in stops.windows(2) {
            let (t0, c0) = pair[0];
            let (t1, c1) = pair[1];
            let (x0, x1) = (rect.left() + w * t0, rect.left() + w * t1);
            let base = mesh.vertices.len() as u32;
            mesh.colored_vertex(egui::pos2(x0, top), c0);
            mesh.colored_vertex(egui::pos2(x1, top), c1);
            mesh.colored_vertex(egui::pos2(x1, bot), c1);
            mesh.colored_vertex(egui::pos2(x0, bot), c0);
            mesh.add_triangle(base, base + 1, base + 2);
            mesh.add_triangle(base, base + 2, base + 3);
        }
        p.add(egui::Shape::mesh(mesh));
    }
    p.text(
        egui::pos2(rect.right() - 3.0, rect.center().y),
        egui::Align2::RIGHT_CENTER,
        format!("{:.0}%", pct * 100.0),
        egui::FontId::proportional(label_size),
        Color32::from_rgb(235, 235, 240),
    );
}

/// Render one meter frame into `ui`. Returns actions for the host plus
/// whether anything is worth showing at all (no engaged mob = hide).
///
/// `preview` forces the chrome to render with a placeholder when there is no
/// combat — the Windows client's "Show All Windows" idea: while the user is
/// on the Meter settings tab, an idle meter must stay visible and draggable
/// or a fresh install looks exactly like a crash.
pub fn draw(
    ui: &mut egui::Ui,
    view: &mut MeterView,
    cs: &CombatState,
    style: MeterStyle,
    locked: bool,
    preview: bool,
) -> (Vec<MeterAction>, bool) {
    let mut actions = Vec::new();

    let resolved = resolve_view_mob_id(cs, view.pinned);
    if resolved.is_none() && !preview {
        return (actions, false);
    }
    if resolved.is_none() {
        // Preview placeholder: title bar (draggable) + a hint, no data rows.
        let fs = style.font_size;
        let title = ui
            .horizontal(|ui| {
                for tab in MeterTab::ALL.iter() {
                    let _ = ui.selectable_label(view.tab == *tab, egui::RichText::new(tab.label()).size(fs - 1.0));
                }
            })
            .response;
        let drag = ui.interact(
            title.rect,
            ui.id().with("meter-drag-preview"),
            egui::Sense::drag(),
        );
        if drag.dragged() && !locked {
            let delta = drag.drag_delta();
            if delta != egui::Vec2::ZERO {
                actions.push(MeterAction::Drag(delta));
            }
        }
        ui.separator();
        ui.label(
            egui::RichText::new("waiting for combat — drag this bar to position the meter")
                .size(fs - 1.0)
                .color(Color32::from_rgb(150, 150, 160)),
        );
        return (actions, true);
    }
    let mob_id = resolved.unwrap();
    // A pin that fell off the mob list silently reverts to auto, like Windows.
    if view.pinned.is_some() && !cs.mob_list.iter().any(|m| Some(m.id) == view.pinned) {
        view.pinned = None;
    }

    let snap = compute_snapshot(cs, mob_id, view.tab, style.max_rows);
    let fs = style.font_size;
    let text_col = Color32::from_rgb(228, 228, 234);
    let dim_col = Color32::from_rgb(150, 150, 160);

    // ── Title bar: tabs left, chrome icons right — like the Windows meter,
    // the mob name lives in the footer, never up here. Locked keeps the
    // chrome drawn (Windows parity) but inert and dimmed.
    let title_resp = ui
        .horizontal(|ui| {
            for tab in MeterTab::ALL.iter() {
                let selected = view.tab == *tab;
                let label = RichText::new(tab.label()).size(fs - 1.0);
                if ui
                    .add_enabled(!locked, egui::SelectableLabel::new(selected, label))
                    .clicked()
                {
                    view.tab = *tab;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(!locked, egui::Button::new(RichText::new("⚙").size(fs)).small())
                    .on_hover_text("Meter settings")
                    .clicked()
                {
                    actions.push(MeterAction::OpenSettings);
                }
                if ui
                    .add_enabled(!locked, egui::Button::new(RichText::new("🗑").size(fs)).small())
                    .on_hover_text("Clear combat totals")
                    .clicked()
                {
                    view.pinned = None;
                    actions.push(MeterAction::Reset);
                }
                if ui
                    .add_enabled(!locked, egui::Button::new(RichText::new("📋").size(fs)).small())
                    .on_hover_text("Copy summary for chat")
                    .clicked()
                {
                    actions.push(MeterAction::Copy(build_summary_line(&snap, view.tab)));
                }
            });
        })
        .response;

    // The whole title bar doubles as the drag handle, like the Windows
    // meter's caption area. Sense the rect explicitly: labels eat hover
    // but not drag.
    let drag = ui.interact(
        title_resp.rect,
        ui.id().with("meter-drag"),
        egui::Sense::drag(),
    );
    if drag.dragged() && !locked {
        let delta = drag.drag_delta();
        if delta != egui::Vec2::ZERO {
            actions.push(MeterAction::Drag(delta));
        }
    }

    ui.separator();

    // ── Column headers ──
    let rank_w = fs * 1.2;
    let num_w = fs * 3.2;
    let bar_w = fs * 5.0;
    egui::Grid::new("meter-rows")
        .num_columns(5)
        .min_col_width(rank_w)
        .spacing([fs * 0.5, fs * 0.25])
        .show(ui, |ui| {
            ui.label(RichText::new("#").size(fs - 2.0).color(dim_col));
            ui.label(RichText::new("Name").size(fs - 2.0).color(dim_col));
            for header in [view.tab.amount_col_label(), view.tab.rate_col_label()] {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.set_min_width(num_w);
                    ui.label(RichText::new(header).size(fs - 2.0).color(dim_col));
                });
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.set_min_width(bar_w);
                ui.label(RichText::new("%").size(fs - 2.0).color(dim_col));
            });
            ui.end_row();

            // ── Ranked rows: top row gold, names in class colors ──
            for (i, row) in snap.rows.iter().enumerate() {
                let name_col = if i == 0 {
                    Color32::from_rgb(230, 195, 90)
                } else {
                    dim(row.color)
                };
                ui.label(
                    RichText::new(format!("{}", i + 1))
                        .size(fs - 1.0)
                        .color(dim_col),
                );
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = fs * 0.3;
                    ui.label(RichText::new(&row.name).size(fs).color(name_col));
                    if let Some(owner) = &row.owner {
                        // Pet row: name its owner, like the web viewer's
                        // "Pet · Owner" chip.
                        ui.label(
                            RichText::new(format!("({owner})"))
                                .size(fs - 3.0)
                                .color(dim_col),
                        );
                    }
                });
                for val in [fmt_k(row.total), fmt_k(row.rate)] {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.set_min_width(num_w);
                        ui.label(RichText::new(val).size(fs).color(text_col));
                    });
                }
                // Group-share bar: this row's slice of the cumulative total,
                // class-gradient filled like the web viewer's row bars. Fills
                // the rest of the row so its right edge lines up with the
                // "%" header.
                let pct = if snap.footer_total > 0 {
                    row.total as f32 / snap.footer_total as f32
                } else {
                    0.0
                };
                let w = ui.available_width().max(bar_w);
                paint_share_bar(ui, egui::vec2(w, fs * 0.8), pct, &row.colors, fs - 4.0);
                ui.end_row();
            }
        });

    ui.separator();

    // ── Footer: mob name + caret (click = picker) left, totals + timer right ──
    let footer = ui
        .horizontal(|ui| {
            let pin_marker = if view.pinned.is_some() { "📌 " } else { "" };
            let caret = if view.picker_open { "▴" } else { "▾" };
            ui.label(
                RichText::new(format!("{pin_marker}{} {caret}", snap.mob_name))
                    .size(fs - 1.0)
                    .strong()
                    .color(Color32::from_rgb(200, 210, 230)),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(fmt_duration(snap.elapsed_secs))
                        .size(fs - 1.0)
                        .color(dim_col),
                );
                ui.label(
                    RichText::new(format!(
                        "{} ({}/s)",
                        fmt_k(snap.footer_total),
                        fmt_k(snap.footer_rate)
                    ))
                    .size(fs - 1.0)
                    .strong()
                    .color(text_col),
                );
            });
        })
        .response;
    let footer_click = ui.interact(
        footer.rect,
        ui.id().with("meter-picker-toggle"),
        egui::Sense::click(),
    );
    if footer_click.clicked() && !locked {
        view.picker_open = !view.picker_open;
    }

    // ── Mob picker: Auto + recent confirmed mobs with activity dots ──
    if view.picker_open && !locked {
        ui.separator();
        for entry in build_mob_picker_entries(cs) {
            ui.horizontal(|ui| {
                ui.label(RichText::new("●").size(fs - 3.0).color(dim(entry.dot)));
                let selected = entry.id == view.pinned && entry.id.is_some();
                if ui
                    .selectable_label(
                        selected,
                        RichText::new(&entry.label).size(fs - 1.0).color(text_col),
                    )
                    .clicked()
                {
                    view.pinned = entry.id;
                    view.picker_open = false;
                }
            });
        }
    }

    (actions, true)
}
