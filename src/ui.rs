use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{channel::mpsc, StreamExt};
use gpui::{
    div, prelude::*, px, rgb, App, AsyncApp, Context, Hsla, IntoElement,
    MouseButton, Render, SharedString, WeakEntity, Window,
};

use crate::export;
use crate::charts::{self, donut, nice_max, ChartSpec, DonutRow, LegendItem, LineChart, LineSeries, Slice};
use crate::telemetry::{self, Hub, Selection, Status, Telemetry, HISTORY};
use crate::theme::Theme;

type SpecStore = Rc<RefCell<HashMap<String, ChartSpec>>>;

pub struct Monitor {
    theme: Theme,
    tel: Telemetry,
    sel: Arc<Mutex<Selection>>,
    dropdown_open: bool,
    /// Latest chart data per card title, refreshed every render (used by PNG export).
    specs: SpecStore,
    handle: WeakEntity<Monitor>,
    toast: Option<(String, bool)>,
}

/// Right-click on a card: ask where to save, then render the chart off-screen to PNG.
fn export_card(title: String, specs: SpecStore, theme: Theme, handle: WeakEntity<Monitor>, cx: &mut App) {
    let Some(spec) = specs.borrow().get(&title).cloned() else { return };
    let say = move |msg: String, ok: bool, cx: &mut AsyncApp| {
        let _ = handle.update(cx, |m, cx| {
            m.toast = Some((msg, ok));
            cx.notify();
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_secs(5)).await;
                let _ = this.update(cx, |m, cx| {
                    m.toast = None;
                    cx.notify();
                });
            })
            .detach();
        });
    };
    let rx = cx.prompt_for_new_path(&export::default_dir(), Some(&export::suggested_name(spec.title())));
    cx.spawn(async move |cx| {
        let Ok(Ok(Some(mut path))) = rx.await else { return };
        if path.extension().is_none() {
            path.set_extension("png");
        }
        let p = path.clone();
        let result = cx.background_executor().spawn(async move { export::save_png(&spec, &theme, &p) }).await;
        match result {
            Ok(()) => say(format!("Saved {}", path.display()), true, cx),
            Err(e) => say(format!("Save failed: {e}"), false, cx),
        }
    })
    .detach();
}

// -------------------------------------------------------------- formatting --

fn fmt_kb(kb: u64) -> String {
    let b = kb as f64 * 1024.0;
    if b >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} GB", b / 1073741824.0)
    } else if b >= 1024.0 * 1024.0 {
        format!("{:.0} MB", b / 1048576.0)
    } else {
        format!("{kb} KB")
    }
}

pub fn fmt_rate(bps: f32) -> String {
    if bps >= 1024.0 * 1024.0 {
        let v = bps / 1048576.0;
        if v >= 100.0 { format!("{v:.0} MB/s") } else { format!("{v:.1} MB/s") }
    } else if bps >= 1024.0 {
        let v = bps / 1024.0;
        if v >= 100.0 { format!("{v:.0} KB/s") } else { format!("{v:.1} KB/s") }
    } else {
        format!("{bps:.0} B/s")
    }
}

fn fmt_pct(v: f32) -> String {
    format!("{v:.0}%")
}

fn fmt_uptime(s: u64) -> String {
    let (d, h, m) = (s / 86400, s % 86400 / 3600, s % 3600 / 60);
    if d > 0 { format!("{d}d {h}h {m}m") } else { format!("{h}h {m}m") }
}

fn load_color(t: &Theme, pct: f32) -> Hsla {
    if pct < 60.0 { t.good } else if pct < 85.0 { t.warn } else { t.bad }
}

// ------------------------------------------------------------------- view --

impl Monitor {
    pub fn new(hub: Hub, cx: &mut Context<Self>) -> Self {
        let (tx, mut rx) = mpsc::unbounded();
        let sel = hub.sel.clone();
        telemetry::spawn_worker(tx, sel.clone());
        let shared = hub.tel.clone();

        cx.spawn(async move |this, cx| {
            while let Some(msg) = rx.next().await {
                shared.lock().unwrap_or_else(|e| e.into_inner()).apply(msg.clone());
                let ok = this.update(cx, |m, cx| {
                    m.tel.apply(msg);
                    cx.notify();
                });
                if ok.is_err() {
                    break;
                }
            }
        })
        .detach();

        Self {
            theme: Theme::dark(),
            tel: Telemetry::new(),
            sel,
            dropdown_open: false,
            specs: Default::default(),
            handle: cx.weak_entity(),
            toast: None,
        }
    }

    fn card(&self, title: &str, subtitle: Option<String>, body: impl IntoElement) -> gpui::Div {
        let t = self.theme;
        let (specs, handle, key) = (self.specs.clone(), self.handle.clone(), title.to_string());
        div()
            .on_mouse_down(MouseButton::Right, move |_, _, cx| {
                export_card(key.clone(), specs.clone(), t, handle.clone(), cx)
            })
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .bg(t.surface)
            .border_1()
            .border_color(t.border)
            .rounded_xl()
            .shadow_sm()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .items_baseline()
                    .child(div().text_sm().font_weight(gpui::FontWeight::SEMIBOLD).text_color(t.text).child(title.to_string()))
                    .children(subtitle.map(|s| div().text_xs().text_color(t.text_muted).child(s))),
            )
            .child(body)
    }

    fn tile(&self, label: &str, value: String, sub: String, accent: Hsla) -> gpui::Div {
        let t = self.theme;
        div()
            .flex_1()
            .min_w(px(170.0))
            .flex()
            .flex_col()
            .gap_1()
            .px_4()
            .py_3()
            .bg(t.surface)
            .border_1()
            .border_color(t.border)
            .rounded_xl()
            .child(div().flex().flex_row().items_center().gap_2()
                .child(div().size(px(8.0)).rounded_full().bg(accent))
                .child(div().text_xs().text_color(t.text_muted).child(label.to_string())))
            .child(div().text_2xl().font_weight(gpui::FontWeight::BOLD).text_color(t.text).child(value))
            .child(div().text_xs().text_color(t.text_muted).child(sub))
    }

    fn legend_row(&self, color: Hsla, name: &str, value: String, pct: f32) -> gpui::Div {
        let t = self.theme;
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(div().size(px(10.0)).rounded_sm().bg(color).flex_none())
            .child(div().flex_1().text_sm().text_color(t.text).child(name.to_string()))
            .child(div().text_sm().text_color(t.text_muted).child(value))
            .child(div().w(px(44.0)).text_sm().text_right().font_weight(gpui::FontWeight::SEMIBOLD).text_color(t.text).child(fmt_pct(pct)))
    }

    fn donut_card(
        &self,
        title: &str,
        subtitle: Option<String>,
        center_big: String,
        center_small: &str,
        parts: Vec<(&str, u64, Hsla)>,
    ) -> gpui::Div {
        let t = self.theme;
        let total: u64 = parts.iter().map(|p| p.1).sum::<u64>().max(1);
        let slices = parts.iter().map(|p| Slice { value: p.1 as f32, color: p.2 }).collect();
        self.specs.borrow_mut().insert(
            title.to_string(),
            ChartSpec::Donut {
                title: title.to_string(),
                subtitle: subtitle.clone(),
                center_big: center_big.clone(),
                center_small: center_small.to_string(),
                rows: parts
                    .iter()
                    .map(|(n, v, c)| DonutRow {
                        name: n.to_string(),
                        value: fmt_kb(*v),
                        pct: *v as f32 / total as f32 * 100.0,
                        amount: *v as f32,
                        color: *c,
                    })
                    .collect(),
            },
        );
        let size = 176.0;
        let ring = div()
            .size(px(size))
            .flex_none()
            .relative()
            .child(donut(slices, size, 22.0, t.surface_alt))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .child(div().text_2xl().font_weight(gpui::FontWeight::BOLD).text_color(t.text).child(center_big))
                    .child(div().text_xs().text_color(t.text_muted).child(center_small.to_string())),
            );
        let legend = div().flex_1().min_w(px(180.0)).flex().flex_col().gap_2().justify_center().children(
            parts.iter().map(|(n, v, c)| self.legend_row(*c, n, fmt_kb(*v), *v as f32 / total as f32 * 100.0)),
        );
        self.card(
            title,
            subtitle,
            div().flex().flex_row().flex_wrap().items_center().gap_5().child(ring).child(legend),
        )
    }

    fn line_card(
        &self,
        title: &str,
        live: Vec<(String, Hsla, String)>,
        series: Vec<LineSeries>,
        y_max: f32,
        fmt: fn(f32) -> String,
    ) -> gpui::Div {
        let t = self.theme;
        self.specs.borrow_mut().insert(
            title.to_string(),
            ChartSpec::Line {
                title: title.to_string(),
                legend: live.iter().map(|(n, c, v)| LegendItem { name: n.clone(), color: *c, value: v.clone() }).collect(),
                series: series.clone(),
                y_max,
                fmt,
                window_len: HISTORY,
            },
        );
        let readout = div().flex().flex_row().flex_wrap().gap_x_4().gap_y_1().children(live.into_iter().map(|(name, c, v)| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .child(div().w(px(10.0)).h(px(3.0)).rounded_full().bg(c))
                .child(div().text_xs().text_color(t.text_muted).child(name))
                .child(
                    div()
                        .w(px(if v.ends_with('%') { 40.0 } else { 78.0 }))
                        .text_sm()
                        .text_right()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(t.text)
                        .child(v),
                )
        }));
        let chart = LineChart {
            series,
            y_max,
            window_len: HISTORY,
            last_arrival: self.tel.last_arrival,
            animate: self.tel.status == Status::Connected,
            height: 170.0,
            fmt,
            theme: t,
        }
        .render();
        self.card(title, None, div().flex().flex_col().gap_3().child(readout).child(chart))
    }

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let connected = self.tel.status == Status::Connected;
        let dev_label: SharedString = match &self.tel.selected {
            Some(sn) => {
                let name = format!("{} {}", self.tel.info.manufacturer, self.tel.info.model);
                let name = name.trim();
                let dev = self.tel.devices.iter().find(|d| &d.serial == sn);
                let name = if name.is_empty() { dev.map(|d| d.model.clone()).unwrap_or_default() } else { name.to_string() };
                let ver = if self.tel.info.android.is_empty() { String::new() } else { format!(" (Android {}, API {})", self.tel.info.android, self.tel.info.sdk) };
                format!("{}{ver}  ·  {sn}", if name.is_empty() { "Android device" } else { &name }).into()
            }
            None => "No device".into(),
        };
        let pulse = if connected && self.tel.ticks % 2 == 1 { 1.0 } else { 0.45 };
        let dot = if connected { t.good } else { t.warn };

        let pill = div()
            .id("device-pill")
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .py_1p5()
            .rounded_full()
            .bg(t.surface_alt)
            .border_1()
            .border_color(t.border)
            .cursor_pointer()
            .hover(|s| s.border_color(t.accent))
            .on_click(cx.listener(|this, _, _, cx| {
                this.dropdown_open = !this.dropdown_open;
                cx.notify();
            }))
            .child(div().size(px(9.0)).rounded_full().bg(dot.opacity(pulse)))
            .child(div().text_sm().text_color(t.text).child(dev_label))
            .child(div().text_xs().text_color(t.text_muted).child(if connected {
                format!("{} ms", self.tel.latency.as_millis())
            } else {
                "idle".to_string()
            }))
            .child(div().text_xs().text_color(t.text_muted).child(if self.dropdown_open { "▲" } else { "▼" }));

        let menu = self.dropdown_open.then(|| {
            div()
                .absolute()
                .top(px(40.0))
                .left_0()
                .min_w(px(320.0))
                .flex()
                .flex_col()
                .p_1()
                .bg(t.surface)
                .border_1()
                .border_color(t.border)
                .rounded_lg()
                .shadow_lg()
                .occlude()
                .when(self.tel.devices.is_empty(), |d| {
                    d.child(div().px_3().py_2().text_sm().text_color(t.text_muted).child("No devices found (adb devices)"))
                })
                .children(self.tel.devices.iter().enumerate().map(|(i, d)| {
                    let serial = d.serial.clone();
                    let active = self.tel.selected.as_ref() == Some(&d.serial);
                    div()
                        .id(("dev", i))
                        .flex()
                        .flex_row()
                        .justify_between()
                        .gap_4()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .cursor_pointer()
                        .when(active, |s| s.bg(t.surface_alt))
                        .hover(|s| s.bg(t.surface_alt))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Ok(mut s) = this.sel.lock() {
                                s.serial = Some(serial.clone());
                                s.pinned = true;
                            }
                            this.dropdown_open = false;
                            cx.notify();
                        }))
                        .child(div().text_sm().text_color(t.text).child(format!("{}  {}", d.model, d.serial)))
                        .child(div().text_xs().text_color(if d.ready() { t.good } else { t.warn }).child(d.state.clone()))
                }))
        });

        let toggle = div()
            .id("theme-toggle")
            .size(px(38.0))
            .flex_none()
            .p(px(8.0))
            .rounded_full()
            .bg(t.surface_alt)
            .border_1()
            .border_color(t.border)
            .cursor_pointer()
            .hover(|s| s.border_color(t.accent))
            .on_click(cx.listener(|this, _, _, cx| {
                this.theme = this.theme.toggled();
                cx.notify();
            }))
            .child(charts::theme_icon(t.mode, t.accent, t.surface_alt));

        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px_6()
            .py_3()
            .bg(t.surface)
            .border_b_1()
            .border_color(t.border)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_4()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(div().size(px(22.0)).rounded_md().bg(rgb(0x3ddc84)))
                            .child(div().text_lg().font_weight(gpui::FontWeight::BOLD).text_color(t.text).child("Android Monitor")),
                    )
                    .child(
                    div()
                        .relative()
                        .child(pill)
                        .children(menu.map(|m| gpui::deferred(m).with_priority(10))),
                ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_4()
                    .child(div().text_xs().text_color(t.text_muted).child("Right-click a chart to save it as PNG"))
                    .child(toggle),
            )
    }

    fn waiting(&self, msg: &str) -> gpui::Div {
        let t = self.theme;
        div().flex_1().flex().items_center().justify_center().child(
            div()
                .max_w(px(520.0))
                .flex()
                .flex_col()
                .items_center()
                .gap_3()
                .p_8()
                .bg(t.surface)
                .border_1()
                .border_color(t.border)
                .rounded_xl()
                .child(div().size(px(14.0)).rounded_full().bg(t.warn))
                .child(div().text_xl().font_weight(gpui::FontWeight::SEMIBOLD).text_color(t.text).child("Waiting for ADB device"))
                .child(div().text_sm().text_center().text_color(t.text_muted).child(msg.to_string()))
                .child(div().text_xs().text_center().text_color(t.text_muted).child(
                    "Settings → About phone → tap Build number 7× → Developer options → USB debugging. Verify with `adb devices`. Polling resumes automatically.",
                )),
        )
    }

    fn dashboard(&self) -> gpui::Div {
        let t = self.theme;
        let tel = &self.tel;
        let m = &tel.mem;

        // ---- memory breakdown
        let cache_kb = m.available_kb.saturating_sub(m.free_kb);
        let used_kb = m.total_kb.saturating_sub(m.free_kb + cache_kb);
        let mem_pct = tel.mem_used_pct();

        // ---- tiles
        let cpu = tel.cpu_now();
        let battery = tel.slow.battery.clone();
        let tiles = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_4()
            .child(self.tile("CPU", fmt_pct(cpu), format!("{} cores", tel.cores.len()), load_color(&t, cpu)))
            .child(self.tile("Memory", fmt_pct(mem_pct), format!("{} of {}", fmt_kb(m.total_kb.saturating_sub(m.available_kb)), fmt_kb(m.total_kb)), load_color(&t, mem_pct)))
            .child(self.tile("Download", fmt_rate(tel.rx_now()), tel.slow.ip.as_ref().map(|(i, ip)| format!("{i} · {ip}")).unwrap_or_else(|| "Rx".into()), t.series[0]))
            .child(self.tile("Upload", fmt_rate(tel.tx_now()), "Tx".into(), t.series[1]))
            .child(self.tile(
                "Battery",
                battery.as_ref().and_then(|b| b.level).map(|l| format!("{l}%")).unwrap_or_else(|| "—".into()),
                battery
                    .as_ref()
                    .map(|b| format!("{}{}", b.status, b.temp_c.map(|c| format!(" · {c:.1}°C")).unwrap_or_default()))
                    .unwrap_or_else(|| "unavailable".into()),
                battery.as_ref().and_then(|b| b.level).map(|l| if l > 30 { t.good } else if l > 15 { t.warn } else { t.bad }).unwrap_or(t.text_muted),
            ))
            .child(self.tile(
                "Load avg",
                tel.load.map(|l| format!("{:.2}", l.0)).unwrap_or_else(|| "—".into()),
                format!("up {}", tel.uptime.map(fmt_uptime).unwrap_or_else(|| "—".into())),
                t.series[3],
            ));

        // ---- memory donut
        let mem_card = self
            .donut_card(
                "Memory allocation",
                Some(format!("swap {} / {}", fmt_kb(m.swap_total_kb.saturating_sub(m.swap_free_kb)), fmt_kb(m.swap_total_kb)))
                    .filter(|_| m.swap_total_kb > 0),
                fmt_pct(mem_pct),
                "used",
                vec![("Used", used_kb, t.series[1]), ("Cached / reclaimable", cache_kb, t.series[0]), ("Free", m.free_kb, t.series[2])],
            )
            .flex_1()
            .min_w(px(420.0));

        // ---- storage donut
        let storage_card = match &tel.slow.storage {
            Some(s) => self.donut_card(
                "Storage (/data)",
                Some(format!("{} total", fmt_kb(s.total_kb))),
                fmt_pct(s.used_kb as f32 / s.total_kb.max(1) as f32 * 100.0),
                "used",
                vec![("Used", s.used_kb, t.series[4]), ("Available", s.avail_kb, t.series[2])],
            ),
            None => self.card("Storage (/data)", None, div().text_sm().text_color(t.text_muted).child("Storage info unavailable")),
        }
        .flex_1()
        .min_w(px(420.0));

        // ---- CPU line
        let cpu_card = self
            .line_card(
                "CPU utilisation",
                vec![("total".into(), t.series[0], fmt_pct(cpu))],
                vec![LineSeries { data: tel.cpu_hist.iter().copied().collect(), color: t.series[0], fill: true }],
                100.0,
                fmt_pct,
            )
            .flex_1()
            .min_w(px(460.0));

        // ---- Network line
        let peak = tel.rx_hist.iter().chain(tel.tx_hist.iter()).copied().fold(0.0, f32::max);
        let net_card = self
            .line_card(
                "Network throughput",
                vec![("Rx".into(), t.series[0], fmt_rate(tel.rx_now())), ("Tx".into(), t.series[1], fmt_rate(tel.tx_now()))],
                vec![
                    LineSeries { data: tel.rx_hist.iter().copied().collect(), color: t.series[0], fill: true },
                    LineSeries { data: tel.tx_hist.iter().copied().collect(), color: t.series[1], fill: false },
                ],
                nice_max(peak * 1.1, 10.0 * 1024.0),
                fmt_rate,
            )
            .flex_1()
            .min_w(px(460.0));

        // ---- Memory trend
        let mem_line = self
            .line_card(
                "Memory usage trend",
                vec![("used".into(), t.series[3], fmt_pct(mem_pct))],
                vec![LineSeries { data: tel.mem_hist.iter().copied().collect(), color: t.series[3], fill: true }],
                100.0,
                fmt_pct,
            )
            .flex_1()
            .min_w(px(460.0));

        // ---- per-core line chart (one colour per core)
        let cores = self
            .line_card(
                "Per-core CPU",
                tel.cores.iter().enumerate().map(|(i, v)| (format!("C{i}"), t.core_color(i), fmt_pct(*v))).collect(),
                tel.core_hist
                    .iter()
                    .enumerate()
                    .map(|(i, h)| LineSeries { data: h.iter().copied().collect(), color: t.core_color(i), fill: false })
                    .collect(),
                100.0,
                fmt_pct,
            )
            .flex_1()
            .min_w(px(460.0));
        let warn = (!tel.warnings.is_empty()).then(|| {
            div().px_4().py_2().rounded_lg().bg(t.warn.opacity(0.15)).border_1().border_color(t.warn).text_sm().text_color(t.text).child(tel.warnings.join(" · "))
        });

        div()
            .flex()
            .flex_col()
            .gap_4()
            .p_6()
            .children(warn)
            .child(tiles)
            .child(div().flex().flex_row().flex_wrap().gap_4().child(mem_card).child(cpu_card))
            .child(div().flex().flex_row().flex_wrap().gap_4().child(net_card).child(storage_card))
            .child(div().flex().flex_row().flex_wrap().gap_4().child(mem_line).child(cores))
    }
}

impl Render for Monitor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let body = match &self.tel.status {
            Status::Connected => div().id("scroll").flex_1().overflow_y_scroll().child(self.dashboard()),
            Status::Starting => div().id("scroll").flex_1().flex().child(self.waiting("Looking for devices…")),
            Status::NoDevice(msg) => div().id("scroll").flex_1().flex().child(self.waiting(msg)),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.bg)
            .text_color(t.text)
            .child(self.header(cx))
            .child(body)
            .children(self.toast.as_ref().map(|(msg, ok)| {
                div().absolute().bottom_4().w_full().flex().justify_center().child(
                    div()
                        .max_w(px(900.0))
                        .px_4()
                        .py_2()
                        .rounded_lg()
                        .bg(t.surface)
                        .border_1()
                        .border_color(if *ok { t.good } else { t.bad })
                        .shadow_lg()
                        .text_sm()
                        .text_color(t.text)
                        .child(msg.clone()),
                )
            }))
    }
}
