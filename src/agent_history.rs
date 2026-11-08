//! The "Agents" tab of the palette's history browser: every agent screenbuddy
//! has run, searchable, with a detail page (brief, plan, steps, result, files)
//! and actions. Drawn with the design tokens: Geist Mono, mono ALL CAPS labels,
//! one accent (SIGNAL), status dots.

use crate::agents::{self, Agent, Status, store};
use crate::ui::tokens::{self, Rgba};
use egui::{Color32, FontFamily, FontId, RichText};
use std::time::{Duration, Instant};

const ROW_H: f32 = 46.0;
/// How often the list is re-read while it is showing (live status).
const REFRESH: Duration = Duration::from_millis(700);

fn col(c: Rgba) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r, c.g, c.b, c.a)
}

/// The one accent.
pub fn accent() -> Color32 {
    col(tokens::SIGNAL)
}

/// Geist Mono (registered by `palette::setup_fonts`).
pub fn mono_family() -> FontFamily {
    FontFamily::Name("geist".into())
}
pub fn mono_medium_family() -> FontFamily {
    FontFamily::Name("geist-medium".into())
}

fn body(text: impl Into<String>) -> RichText {
    RichText::new(text).font(FontId::new(tokens::BODY_PX, mono_family())).color(col(tokens::PAPER))
}

fn strong(text: impl Into<String>) -> RichText {
    RichText::new(text).font(FontId::new(tokens::BODY_PX, mono_medium_family())).color(col(tokens::PAPER))
}

/// 10.5 px ALL CAPS mono label.
pub fn label(text: &str) -> RichText {
    RichText::new(text.to_uppercase()).font(FontId::new(tokens::LABEL_PX, mono_medium_family())).color(col(tokens::MUTE))
}

fn dim(text: impl Into<String>) -> RichText {
    RichText::new(text).font(FontId::new(tokens::BODY_PX, mono_family())).color(col(tokens::MUTE))
}

/// Status dot colour: working = the accent, done = OK, failed = ERR,
/// waiting on the user = ASK, anything else idle.
pub fn dot_color(a: &Agent) -> Color32 {
    if a.ask.is_some() {
        return col(tokens::ASK);
    }
    col(match a.status {
        Status::Running => tokens::SIGNAL,
        Status::Done => tokens::OK,
        Status::Failed => tokens::ERR,
        Status::Pending | Status::Stopped => tokens::MUTE,
    })
}

/// "now", "5m", "3h", "2d", else "Nov 4"-style from days.
pub fn relative(now: u64, then: u64) -> String {
    let d = now.saturating_sub(then);
    match d {
        0..=44 => "now".into(),
        45..=3599 => format!("{}m", (d + 30) / 60),
        3600..=86_399 => format!("{}h", d / 3600),
        86_400..=2_591_999 => format!("{}d", d / 86_400),
        _ => format!("{}mo", d / 2_592_000),
    }
}

/// One line for the list: the reply, else the error, else the latest step,
/// else the brief.
pub fn one_liner(a: &Agent) -> String {
    let pick = if !a.result.trim().is_empty() {
        a.result.as_str()
    } else if let Some(e) = a.error.as_deref().filter(|e| !e.trim().is_empty()) {
        e
    } else if let Some(s) = a.steps.last() {
        s.as_str()
    } else {
        a.brief.as_str()
    };
    let flat = pick.split_whitespace().collect::<Vec<_>>().join(" ");
    // Strip the most common markdown noise so the line reads as text.
    flat.trim_start_matches(['#', '*', '>', '-', ' ']).replace("**", "")
}

fn status_word(a: &Agent) -> &'static str {
    if a.ask.is_some() {
        return "asking";
    }
    match a.status {
        Status::Pending => "starting",
        Status::Running => "working",
        Status::Done => "done",
        Status::Failed => "failed",
        Status::Stopped => "stopped",
    }
}

#[derive(Default)]
pub struct View {
    rows: Vec<Agent>,
    query: String,
    loaded: Option<Instant>,
    pub selected: usize,
    /// The agent whose detail page is open.
    detail: Option<String>,
    follow: String,
    confirm_delete: bool,
}

impl View {
    pub fn in_detail(&self) -> bool {
        self.detail.is_some()
    }

    /// Back from the detail page to the list.
    pub fn back(&mut self) {
        self.detail = None;
        self.follow.clear();
        self.confirm_delete = false;
    }

    fn refresh(&mut self, query: &str) {
        let stale = self.loaded.is_none_or(|t| t.elapsed() >= REFRESH);
        if stale || self.query != query {
            self.rows = store::browse(agents::list(), query);
            self.query = query.to_string();
            self.loaded = Some(Instant::now());
        }
    }

    /// Draws the list, or the detail page. `keys` = (up, down, enter) from the
    /// palette's key handling; `md` renders the result.
    pub fn show(&mut self, ui: &mut egui::Ui, query: &str, keys: (bool, bool, bool), md: &mut egui_commonmark::CommonMarkCache) {
        self.refresh(query);
        if self.rows.iter().any(|a| a.status.is_active()) || self.detail.is_some() {
            ui.ctx().request_repaint_after(REFRESH);
        }
        if let Some(id) = self.detail.clone() {
            self.detail_page(ui, &id, keys.2, md);
        } else {
            self.list(ui, keys);
        }
    }

    fn list(&mut self, ui: &mut egui::Ui, keys: (bool, bool, bool)) {
        let (up, down, enter) = keys;
        if down && self.selected + 1 < self.rows.len() {
            self.selected += 1;
        }
        if up {
            self.selected = self.selected.saturating_sub(1);
        }
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        if self.rows.is_empty() {
            let msg = if self.query.trim().is_empty() { "No agents yet." } else { "No agents match." };
            ui.label(dim(msg));
            return;
        }
        let now = store::now();
        let mut open = None;
        let mut unarchive = None;
        if enter && let Some(a) = self.rows.get(self.selected) {
            open = Some(a.id.clone());
        }
        let selected = self.selected;
        for (i, a) in self.rows.iter().enumerate() {
            let (rect, resp) = {
                let w = ui.available_width();
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, ROW_H), egui::Sense::click());
                (rect, resp)
            };
            let sel = i == selected;
            if sel || resp.hovered() {
                ui.painter().rect_filled(rect, tokens::RADIUS_CHIP, Color32::from_white_alpha(if sel { 16 } else { 9 }));
            }
            if sel {
                let bar = egui::Rect::from_min_size(rect.left_top() + egui::vec2(0.0, 9.0), egui::vec2(2.0, ROW_H - 18.0));
                ui.painter().rect_filled(bar, 1.0, col(tokens::SIGNAL));
            }
            if sel && (up || down) {
                resp.scroll_to_me(None);
            }
            ui.painter().circle_filled(rect.left_top() + egui::vec2(16.0, 17.0), 3.5, dot_color(a));

            let top = egui::Rect::from_min_size(rect.left_top() + egui::vec2(30.0, 5.0), egui::vec2(rect.width() - 40.0, 24.0));
            let mut row = ui.new_child(egui::UiBuilder::new().max_rect(top).layout(egui::Layout::right_to_left(egui::Align::Center)));
            if a.archived && row.add(egui::Button::new(label("Unarchive")).small()).clicked() {
                unarchive = Some(a.id.clone());
            }
            if a.archived {
                row.label(label("Archived").color(col(tokens::ASK)));
            }
            row.label(label(&relative(now, a.ended.or(a.started).unwrap_or(a.created))));
            row.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add(egui::Label::new(strong(&a.name)).truncate());
            });

            let line = egui::Rect::from_min_size(rect.left_top() + egui::vec2(30.0, 27.0), egui::vec2(rect.width() - 40.0, 16.0));
            let mut sub = ui.new_child(egui::UiBuilder::new().max_rect(line));
            sub.add(egui::Label::new(dim(one_liner(a)).size(11.5)).truncate());

            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if resp.clicked() && unarchive.is_none() {
                open = Some(a.id.clone());
            }
        }
        if let Some(id) = unarchive {
            agents::archive(&id, false);
            self.loaded = None;
            open = None;
        }
        if let Some(id) = open {
            self.detail = Some(id);
            self.follow.clear();
            self.confirm_delete = false;
        }
    }

    fn detail_page(&mut self, ui: &mut egui::Ui, id: &str, enter: bool, md: &mut egui_commonmark::CommonMarkCache) {
        let Some(a) = agents::get(id) else {
            // Deleted underneath us.
            self.back();
            return;
        };
        let active = a.status.is_active();
        let mut go_back = false;
        ui.horizontal(|ui| {
            if ui.add(egui::Button::new(label("\u{2039} Back")).frame(false)).clicked() {
                go_back = true;
            }
            ui.label(dim("\u{b7}"));
            let (r, _) = ui.allocate_exact_size(egui::vec2(8.0, 14.0), egui::Sense::hover());
            ui.painter().circle_filled(r.center(), 3.5, dot_color(&a));
            ui.label(label(status_word(&a)));
            if a.archived {
                ui.label(label("Archived").color(col(tokens::ASK)));
            }
        });
        ui.add(egui::Label::new(strong(&a.name).size(15.0)).wrap());
        ui.add_space(2.0);

        section(ui, "Brief");
        ui.add(egui::Label::new(body(&a.brief)).wrap());

        if !a.plan.is_empty() {
            section(ui, "Plan");
            for (i, s) in a.plan.iter().enumerate() {
                ui.add(egui::Label::new(body(format!("{:02}  {s}", i + 1))).wrap());
            }
        }
        if !a.steps.is_empty() {
            section(ui, "Steps");
            let skip = a.steps.len().saturating_sub(12);
            if skip > 0 {
                ui.label(dim(format!("\u{2026} {skip} earlier")).size(11.0));
            }
            for s in a.steps.iter().skip(skip) {
                ui.add(egui::Label::new(dim(s).size(11.5)).wrap());
            }
        }
        if let Some(e) = a.error.as_deref().filter(|e| !e.trim().is_empty()) {
            section(ui, "Error");
            ui.add(egui::Label::new(body(e).color(col(tokens::ERR))).wrap());
        }
        if !a.result.trim().is_empty() {
            section(ui, "Result");
            egui_commonmark::CommonMarkViewer::new().show(ui, md, &crate::math::normalize(&a.result));
        }
        if !a.files.is_empty() {
            section(ui, "Files");
            ui.horizontal_wrapped(|ui| {
                for f in &a.files {
                    let name = f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    let chip = egui::Button::new(body(name).size(11.5))
                        .fill(Color32::from_white_alpha(12))
                        .corner_radius(tokens::RADIUS_CHIP);
                    let r = ui.add(chip).on_hover_text(f.display().to_string());
                    if r.clicked() {
                        crate::win::shell_open(f);
                    }
                    if ui.add(egui::Button::new(label("Reveal")).frame(false)).clicked() {
                        reveal(f);
                    }
                }
            });
        }

        section(ui, "Follow up");
        let mut send = false;
        ui.horizontal(|ui| {
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.follow)
                    .id_salt("agent-follow")
                    .hint_text(dim("Ask it to do more\u{2026}"))
                    .font(FontId::new(tokens::BODY_PX, mono_family()))
                    .desired_width(ui.available_width() - 64.0),
            );
            // The palette consumes Enter before the field sees it.
            send = enter && r.has_focus();
            send |= ui.button(label("Send")).clicked();
        });
        if send && !self.follow.trim().is_empty() && agents::follow_up(id, &self.follow) {
            self.follow.clear();
        }

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if active {
                if ui.button(label("Stop")).clicked() {
                    agents::stop(id);
                }
            } else if ui.button(label("Retry")).clicked() {
                agents::retry(id);
            }
            if ui.button(label(if a.archived { "Unarchive" } else { "Archive" })).clicked() {
                agents::archive(id, !a.archived);
                self.loaded = None;
            }
            if !active {
                if self.confirm_delete {
                    ui.label(label("Delete for good?").color(col(tokens::ERR)));
                    if ui.button(label("Delete").color(col(tokens::ERR))).clicked() {
                        agents::delete(id);
                        self.loaded = None;
                        go_back = true;
                    }
                    if ui.button(label("Cancel")).clicked() {
                        self.confirm_delete = false;
                    }
                } else if ui.button(label("Delete")).clicked() {
                    self.confirm_delete = true;
                }
            }
        });
        ui.add_space(4.0);
        if go_back {
            self.back();
        }
    }
}

fn section(ui: &mut egui::Ui, name: &str) {
    ui.add_space(8.0);
    ui.label(label(name));
}

/// Opens Explorer with the file selected.
fn reveal(path: &std::path::Path) {
    let _ = std::process::Command::new("explorer.exe").arg(format!("/select,{}", path.display())).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_times() {
        assert_eq!(relative(1000, 990), "now");
        assert_eq!(relative(1000, 1000 - 300), "5m");
        assert_eq!(relative(100_000, 100_000 - 7300), "2h");
        assert_eq!(relative(1_000_000, 1_000_000 - 3 * 86_400), "3d");
        assert_eq!(relative(10, 50), "now");
    }

    #[test]
    fn one_liner_prefers_result_then_error_then_step() {
        let mut a = Agent { brief: "do it".into(), ..Default::default() };
        assert_eq!(one_liner(&a), "do it");
        a.steps = vec!["reading".into()];
        assert_eq!(one_liner(&a), "reading");
        a.error = Some("boom".into());
        assert_eq!(one_liner(&a), "boom");
        a.result = "## Found  **3** files\n\nall good".into();
        assert_eq!(one_liner(&a), "Found 3 files all good");
    }
}
