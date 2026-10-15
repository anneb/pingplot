#![windows_subsystem = "windows"]

use eframe::egui::{self, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use egui_plot::{HoverPosition, Line, Plot, PlotPoints, Points, VLine};
use pingplot::engine::{Hop, Stats, Trace};
use std::time::Duration;

const INTERVALS: [(u64, &str); 6] =
    [(500, "0.5 s"), (1000, "1 s"), (2500, "2.5 s"), (5000, "5 s"), (10000, "10 s"), (30000, "30 s")];
const FOCUS: [(f64, &str); 7] = [
    (60.0, "1 min"),
    (300.0, "5 min"),
    (600.0, "10 min"),
    (1800.0, "30 min"),
    (3600.0, "1 hour"),
    (6.0 * 3600.0, "6 hours"),
    (f64::INFINITY, "all"),
];

const GOOD: Color32 = Color32::from_rgb(60, 180, 75);
const WARN: Color32 = Color32::from_rgb(230, 190, 30);
const BAD: Color32 = Color32::from_rgb(220, 50, 50);

fn latency_color(ms: f64) -> Color32 {
    if ms < 100.0 {
        GOOD
    } else if ms < 200.0 {
        WARN
    } else {
        BAD
    }
}

fn loss_color(loss: f64) -> Color32 {
    if loss <= 0.0 {
        GOOD
    } else if loss < 10.0 {
        WARN
    } else {
        BAD
    }
}

/// Round up to a "nice" axis maximum.
fn nice_max(v: f64) -> f64 {
    for m in [10.0, 20.0, 40.0, 100.0, 200.0, 400.0, 1000.0, 2000.0] {
        if v <= m {
            return m;
        }
    }
    (v / 1000.0).ceil() * 1000.0
}

fn fmt_ago(secs: f64) -> String {
    let s = (-secs).max(0.0).round() as i64;
    if s == 0 {
        "now".into()
    } else if s < 60 {
        format!("-{s}s")
    } else if s < 3600 {
        format!("-{}:{:02}", s / 60, s % 60)
    } else {
        format!("-{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    }
}

struct App {
    host: String,
    interval_ms: u64,
    focus: f64,
    trace: Option<Trace>,
    error: Option<String>,
    /// Selected hop index for the timeline; None = destination.
    selected: Option<usize>,
}

impl App {
    fn start(&mut self) {
        if let Some(t) = &self.trace {
            t.stop();
        }
        self.selected = None;
        match Trace::start(self.host.trim(), self.interval_ms) {
            Ok(t) => {
                self.trace = Some(t);
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
    }

    fn report(&self) -> String {
        let Some(t) = &self.trace else { return String::new() };
        let hops = t.hops.lock().unwrap();
        let since = t.elapsed() - self.focus;
        let mut s = format!("PingPlot trace to {} ({})\n", t.host, t.target);
        s += &format!(
            "{:>3}  {:<55} {:>6} {:>5} {:>5} {:>7} {:>7} {:>7} {:>7}\n",
            "Hop", "Host", "Loss%", "Sent", "Recv", "Best", "Avg", "Worst", "Last"
        );
        for (i, h) in hops[..t.hop_count(&hops)].iter().enumerate() {
            let st = h.stats(since);
            s += &format!(
                "{:>3}  {:<55} {:>6.1} {:>5} {:>5} {:>7.1} {:>7.1} {:>7.1} {:>7}\n",
                i + 1,
                h.label(),
                st.loss,
                st.sent,
                st.recv,
                st.best,
                st.avg,
                st.worst,
                st.last.map_or("*".into(), |v| format!("{v:.1}"))
            );
        }
        s
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = &ui.ctx().clone();
        ctx.request_repaint_after(Duration::from_millis(250));

        egui::Panel::top("controls").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("Target:");
                let edit = ui.add(egui::TextEdit::singleline(&mut self.host).desired_width(220.0).hint_text("host or IP"));
                let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let running = self.trace.as_ref().is_some_and(|t| t.running());
                if running {
                    if ui.button("⏹ Stop").clicked() {
                        self.trace.as_ref().unwrap().stop();
                    }
                } else if ui.button("▶ Start").clicked() || enter {
                    self.start();
                }
                if running && enter {
                    self.start();
                }

                ui.separator();
                ui.label("Interval:");
                egui::ComboBox::from_id_salt("interval")
                    .selected_text(INTERVALS.iter().find(|i| i.0 == self.interval_ms).map_or("?", |i| i.1))
                    .show_ui(ui, |ui| {
                        for (ms, name) in INTERVALS {
                            ui.selectable_value(&mut self.interval_ms, ms, name);
                        }
                    });
                if let Some(t) = &self.trace {
                    t.interval_ms.store(self.interval_ms, std::sync::atomic::Ordering::Relaxed);
                }

                ui.label("Focus:");
                egui::ComboBox::from_id_salt("focus")
                    .selected_text(FOCUS.iter().find(|f| f.0 == self.focus).map_or("?", |f| f.1))
                    .show_ui(ui, |ui| {
                        for (secs, name) in FOCUS {
                            ui.selectable_value(&mut self.focus, secs, name);
                        }
                    });

                ui.separator();
                if ui.add_enabled(self.trace.is_some(), egui::Button::new("📋 Copy")).clicked() {
                    ctx.copy_text(self.report());
                }
            });
            if let Some(e) = &self.error {
                ui.colored_label(BAD, e);
            } else if let Some(t) = &self.trace {
                let state = if t.running() { "Tracing" } else { "Stopped" };
                ui.label(format!("{state} {} ({}) — {}", t.host, t.target, fmt_ago(-t.elapsed()).trim_start_matches('-')));
            }
            ui.add_space(2.0);
        });

        let Some(trace) = &self.trace else {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.centered_and_justified(|ui| ui.label("Enter a host name or IP address and press Start."));
            });
            return;
        };

        let now = trace.elapsed();
        let since = now - self.focus;
        let (hops, stats): (Vec<Hop>, Vec<Stats>) = {
            let all = trace.hops.lock().unwrap();
            let n = trace.hop_count(&all);
            let hops: Vec<Hop> = all[..n].to_vec();
            let stats = hops.iter().map(|h| h.stats(since)).collect();
            (hops, stats)
        };
        let dest = hops.len().checked_sub(1);
        let sel = self.selected.filter(|&i| i < hops.len()).or(dest);

        egui::Panel::bottom("timeline")
            .resizable(true)
            .default_size(260.0)
            .min_size(120.0)
            .show(ui, |ui| {
                let Some(i) = sel else { return };
                let h = &hops[i];
                ui.horizontal(|ui| {
                    ui.strong(format!("Hop {}: {}", i + 1, h.label()));
                    if self.selected.is_some() && ui.small_button("show destination").clicked() {
                        self.selected = None;
                    }
                });
                let window = self.focus.min(now.max(60.0));
                let mut ok: Vec<[f64; 2]> = Vec::new();
                let mut lost: Vec<f64> = Vec::new();
                for s in h.samples.iter().filter(|s| s.t >= now - window) {
                    match s.rtt {
                        Some(r) => ok.push([s.t - now, r]),
                        None => lost.push(s.t - now),
                    }
                }
                let ymax = nice_max(ok.iter().map(|p| p[1]).fold(0.0, f64::max) * 1.1);
                Plot::new("timeline_plot")
                    .allow_drag(false)
                    .allow_zoom(false)
                    .allow_scroll(false)
                    .allow_boxed_zoom(false)
                    .include_x(-window)
                    .include_x(0.0)
                    .include_y(0.0)
                    .include_y(ymax)
                    .y_axis_label("ms")
                    .x_axis_formatter(|m, _| fmt_ago(m.value))
                    .label_formatter(|pos| {
                        let p = match pos {
                            HoverPosition::NearDataPoint { position, .. } | HoverPosition::Elsewhere { position } => position,
                        };
                        Some(format!("{}\n{:.1} ms", fmt_ago(p.x), p.y))
                    })
                    .show(ui, |pu| {
                        for x in &lost {
                            pu.vline(VLine::new("lost", *x).color(BAD.gamma_multiply(0.6)).width(2.0));
                        }
                        pu.line(Line::new("latency", PlotPoints::from(ok.clone())).color(Color32::from_rgb(70, 130, 220)).width(1.5));
                        pu.points(Points::new("samples", PlotPoints::from(ok)).color(Color32::from_rgb(70, 130, 220)).radius(1.5));
                    });
            });

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                if let Some(clicked) = hop_table(ui, &hops, &stats, sel) {
                    self.selected = Some(clicked);
                }
            });
        });
    }
}

/// PingPlotter-style hop table with loss bar and latency graph columns.
/// Returns the index of a clicked row.
fn hop_table(ui: &mut egui::Ui, hops: &[Hop], stats: &[Stats], sel: Option<usize>) -> Option<usize> {
    let mut clicked = None;
    let graph_w = (ui.available_width() - 760.0).max(200.0);
    let xmax = nice_max(stats.iter().map(|s| s.worst).fold(0.0, f64::max));
    let mut avg_points: Vec<Pos2> = Vec::new();
    let mut graph_x = (0.0, 0.0);

    egui::Grid::new("hops").striped(true).spacing([10.0, 4.0]).show(ui, |ui| {
        for h in ["Hop", "Loss %", "", "Sent", "Best", "Avg", "Worst", "Last", "Host"] {
            ui.strong(h);
        }
        ui.horizontal(|ui| {
            ui.strong("Latency (ms)");
            ui.add_space(graph_w - 140.0);
            ui.label(format!("0 – {xmax:.0}"));
        });
        ui.end_row();

        for (i, (h, s)) in hops.iter().zip(stats).enumerate() {
            let selected = sel == Some(i);
            if ui.selectable_label(selected, format!("{:>2}", i + 1)).clicked() {
                clicked = Some(i);
            }
            let has_data = s.sent > 0;
            let lc = if has_data && h.addr.is_some() { loss_color(s.loss) } else { Color32::GRAY };
            ui.colored_label(lc, format!("{:.1}", s.loss));

            // Loss bar (0-100%).
            let (r, _) = ui.allocate_exact_size(Vec2::new(60.0, 14.0), Sense::hover());
            ui.painter().rect_filled(r, 2.0, ui.visuals().extreme_bg_color);
            if s.loss > 0.0 {
                let w = r.width() * (s.loss / 100.0) as f32;
                // Hops that never answer are usually routers ignoring ICMP, not real loss.
                let c = if h.addr.is_some() { BAD } else { Color32::GRAY.gamma_multiply(0.5) };
                ui.painter().rect_filled(Rect::from_min_size(r.min, Vec2::new(w, r.height())), 2.0, c);
            }

            let f = |v: f64| if s.recv > 0 { format!("{v:.1}") } else { "-".into() };
            ui.label(s.sent.to_string());
            ui.label(f(s.best));
            ui.label(f(s.avg));
            ui.label(f(s.worst));
            ui.label(s.last.map_or("*".into(), |v| format!("{v:.1}")));
            let host = egui::Label::new(h.label()).sense(Sense::click()).truncate();
            let resp = ui.allocate_ui_with_layout(
                Vec2::new(320.0, 18.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.set_min_width(320.0);
                    ui.add(host)
                },
            );
            if resp.inner.clicked() {
                clicked = Some(i);
            }

            // Latency graph: best–worst range, avg marker.
            let (r, resp) = ui.allocate_exact_size(Vec2::new(graph_w, 18.0), Sense::click());
            if resp.clicked() {
                clicked = Some(i);
            }
            graph_x = (r.left(), r.right());
            let p = ui.painter();
            if selected {
                p.rect_filled(r, 0.0, ui.visuals().selection.bg_fill.gamma_multiply(0.4));
            }
            let x = |ms: f64| r.left() + (r.width() * (ms / xmax).min(1.0) as f32);
            if s.recv > 0 {
                let y = r.center().y;
                p.rect_filled(
                    Rect::from_min_max(Pos2::new(x(s.best), y - 3.0), Pos2::new(x(s.worst).max(x(s.best) + 1.0), y + 3.0)),
                    2.0,
                    latency_color(s.avg).gamma_multiply(0.35),
                );
                let c = Pos2::new(x(s.avg), y);
                p.circle_filled(c, 4.5, latency_color(s.avg));
                avg_points.push(c);
            }
            if let Some(last) = s.last {
                p.line_segment(
                    [Pos2::new(x(last), r.top() + 2.0), Pos2::new(x(last), r.bottom() - 2.0)],
                    Stroke::new(1.5, ui.visuals().text_color()),
                );
            }
            ui.end_row();
        }
    });

    // Connect the average markers across hops, like PingPlotter's red line.
    if avg_points.len() > 1 {
        ui.painter().add(egui::Shape::line(avg_points, Stroke::new(1.5, BAD.gamma_multiply(0.8))));
    }
    // Grid lines at 25% steps below the graph.
    if graph_x.1 > graph_x.0 {
        let y = ui.cursor().top() + 2.0;
        for q in 0..=4 {
            let gx = graph_x.0 + (graph_x.1 - graph_x.0) * q as f32 / 4.0;
            ui.painter().text(
                Pos2::new(gx, y),
                egui::Align2::CENTER_TOP,
                format!("{:.0}", xmax * q as f64 / 4.0),
                FontId::proportional(11.0),
                ui.visuals().weak_text_color(),
            );
        }
        ui.add_space(18.0);
    }
    clicked
}

fn main() -> eframe::Result {
    let host = std::env::args().nth(1).unwrap_or_default();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("PingPlot").with_inner_size([1250.0, 720.0]),
        ..Default::default()
    };
    eframe::run_native(
        "PingPlot",
        options,
        Box::new(move |_cc| {
            let mut app = App { host, interval_ms: 1000, focus: 600.0, trace: None, error: None, selected: None };
            if !app.host.is_empty() {
                app.start();
            }
            Ok(Box::new(app))
        }),
    )
}
