// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Scene construction for the dashboard. Pure layout: takes a [`Snapshot`]
//! and produces a Carnelian [`Scene`] plus the hit rectangles of the buttons.

use crate::telemetry::{Battery, Charger, LastBoot, ScanState, Snapshot, Wlan};
use carnelian::color::Color;
use carnelian::drawing::{FontFace, measure_text_width};
use carnelian::scene::facets::{
    RectangleFacet, TextFacetOptions, TextHorizontalAlignment, TextVerticalAlignment,
};
use carnelian::scene::group::GroupMemberData;
use carnelian::scene::layout::Arranger;
use carnelian::scene::scene::{Scene, SceneBuilder};
use carnelian::{Point, Rect, Size};
use euclid::{point2, size2};
use fidl_fuchsia_hardware_power_battery::HealthStatus;
use fidl_fuchsia_hardware_power_charger::{ChargePhase, OperatingMode};

/// On-screen actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Snapshot,
    Fastboot,
    Reboot,
    ExitToVirtcon,
}

impl Action {
    /// The bottom button row, left to right. `Snapshot` has its own row.
    pub const BOTTOM_ROW: [Action; 3] = [Action::Fastboot, Action::Reboot, Action::ExitToVirtcon];

    fn label(self) -> &'static str {
        match self {
            Action::Snapshot => "SAVE SNAPSHOT",
            Action::Fastboot => "FASTBOOT",
            Action::Reboot => "REBOOT",
            Action::ExitToVirtcon => "VIRTCON",
        }
    }

    /// Destructive actions need a second tap.
    pub fn needs_confirmation(self) -> bool {
        matches!(self, Action::Fastboot | Action::Reboot)
    }
}

pub struct Fonts {
    pub regular: FontFace,
    pub mono: FontFace,
}

#[derive(Clone, Debug)]
pub struct Button {
    pub action: Action,
    pub rect: Rect,
}

pub struct Dashboard {
    pub scene: Scene,
    pub buttons: Vec<Button>,
}

pub struct Params<'a> {
    pub size: Size,
    pub snapshot: &'a Snapshot,
    pub fonts: &'a Fonts,
    /// Action awaiting its confirmation tap.
    pub armed: Option<Action>,
    /// Button currently held down.
    pub pressed: Option<Action>,
    /// A snapshot is being collected; the button is shown disabled.
    pub snapshot_busy: bool,
    /// One-line status shown above the buttons (e.g. "Rebooting...").
    pub status: Option<&'a str>,
}

struct Theme {
    background: Color,
    foreground: Color,
    dim: Color,
    rule: Color,
    good: Color,
    warn: Color,
    /// Between `warn` and `bad`; used for the hotter thermal band.
    hot: Color,
    bad: Color,
    button: Color,
    button_pressed: Color,
    button_text: Color,
}

impl Theme {
    fn dark() -> Self {
        let c = |code| Color::from_hash_code(code).expect("valid color");
        Theme {
            background: c("#101418"),
            foreground: c("#F2F4F7"),
            dim: c("#8A94A6"),
            rule: c("#2A3240"),
            good: c("#34C759"),
            warn: c("#FFB020"),
            hot: c("#FF7A1A"),
            bad: c("#FF453A"),
            button: c("#263040"),
            button_pressed: c("#3B4A61"),
            button_text: c("#F2F4F7"),
        }
    }
}

/// Arranger that places every member at the `Point` given as its member data.
#[derive(Debug)]
struct Absolute;

impl Arranger for Absolute {
    fn calculate_size(
        &self,
        group_size: Size,
        _member_sizes: &mut [Size],
        _member_data: &[&Option<GroupMemberData>],
    ) -> Size {
        group_size
    }

    fn arrange(
        &self,
        _group_size: Size,
        _member_sizes: &[Size],
        member_data: &[&Option<GroupMemberData>],
    ) -> Vec<Point> {
        member_data
            .iter()
            .map(|data| {
                data.as_ref()
                    .and_then(|data| data.downcast_ref::<Point>())
                    .copied()
                    .unwrap_or_else(Point::zero)
            })
            .collect()
    }
}

/// Thin helper around `SceneBuilder` for pixel-positioned primitives.
struct Canvas<'a> {
    builder: &'a mut SceneBuilder,
}

impl Canvas<'_> {
    fn at(point: Point) -> Option<GroupMemberData> {
        Some(Box::new(point))
    }

    fn text(&mut self, face: &FontFace, text: &str, size: f32, origin: Point, color: Color) {
        let options = TextFacetOptions {
            color,
            horizontal_alignment: TextHorizontalAlignment::Left,
            vertical_alignment: TextVerticalAlignment::Top,
            ..TextFacetOptions::default()
        };
        self.builder.text_with_data(
            face.clone(),
            text,
            size,
            Point::zero(),
            options,
            Self::at(origin),
        );
    }

    fn text_right(
        &mut self,
        face: &FontFace,
        text: &str,
        size: f32,
        right: f32,
        y: f32,
        color: Color,
    ) {
        let width = measure_text_width(face, size, text);
        self.text(face, text, size, point2(right - width, y), color);
    }

    fn text_center(&mut self, face: &FontFace, text: &str, size: f32, rect: Rect, color: Color) {
        let width = measure_text_width(face, size, text);
        let ascent = face.ascent(size);
        let descent = face.descent(size);
        let height = ascent - descent;
        let x = rect.origin.x + (rect.size.width - width) / 2.0;
        let y = rect.origin.y + (rect.size.height - height) / 2.0;
        self.text(face, text, size, point2(x, y), color);
    }

    fn rect(&mut self, rect: Rect, color: Color, radius: Option<f32>) {
        let facet = match radius {
            Some(radius) => RectangleFacet::new_rounded(rect.size, radius, color),
            None => RectangleFacet::new(rect.size, color),
        };
        self.builder.facet_with_data(facet, Self::at(rect.origin));
    }
}

fn charge_label(battery: &Battery, charger: Option<&Charger>) -> &'static str {
    if !battery.present {
        return "No battery";
    }
    if let Some(charger) = charger {
        match (charger.charge_phase, charger.operating_mode) {
            (Some(ChargePhase::Done), _) => return "Full",
            (
                Some(
                    ChargePhase::Trickle
                    | ChargePhase::Fast
                    | ChargePhase::Taper
                    | ChargePhase::TopOff,
                ),
                _,
            ) => return "Charging",
            (_, Some(OperatingMode::Passthrough)) | (Some(ChargePhase::None), _) => {
                return if charger.online == Some(false) { "Discharging" } else { "Not charging" };
            }
            (_, Some(OperatingMode::Charging)) => return "Charging",
            (_, Some(OperatingMode::Discharging | OperatingMode::Otg)) => return "Discharging",
            _ if charger.online == Some(false) => return "Discharging",
            _ => {}
        }
    }
    match battery.current_ua {
        Some(c) if c > 0 => "Charging",
        Some(c) if c < 0 => "Discharging",
        _ => "Unknown",
    }
}

fn health_label(health: Option<HealthStatus>) -> Option<&'static str> {
    Some(match health? {
        HealthStatus::Good => "good",
        HealthStatus::Cold => "cold",
        HealthStatus::Cool => "cool",
        HealthStatus::Warm => "warm",
        HealthStatus::Hot => "hot",
        HealthStatus::Dead => "dead",
        HealthStatus::OverVoltage => "over-voltage",
        _ => "unknown",
    })
}

fn format_uptime(uptime: zx::MonotonicDuration) -> String {
    let total = uptime.into_seconds().max(0);
    let (d, h, m, s) = (total / 86_400, (total / 3_600) % 24, (total / 60) % 60, total % 60);
    if d > 0 { format!("{d}d {h:02}h {m:02}m") } else { format!("{h:02}h {m:02}m {s:02}s") }
}

fn format_gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

/// `(text, is_graceful)` for the last-boot row.
fn format_last_boot(last: &LastBoot) -> (String, bool) {
    let reason = match last.reason {
        Some(reason) if !reason.is_unknown() => format!("{reason:?}"),
        _ => String::from("Unknown"),
    };
    let graceful = last.graceful.unwrap_or(true);
    let mut text = if graceful { reason } else { format!("{reason} (ungraceful)") };
    if let Some(up) = last.uptime {
        text.push_str(&format!(" · up {}", format_uptime(up)));
    }
    (text, graceful)
}

fn format_charger(charger: &Charger) -> (String, String) {
    let source = match charger.source_type {
        Some(fidl_fuchsia_hardware_power_charger::SourceType::Ac) => "AC",
        Some(fidl_fuchsia_hardware_power_charger::SourceType::Usb) => "USB",
        Some(fidl_fuchsia_hardware_power_charger::SourceType::Wireless) => "Wireless",
        _ => "Source?",
    };
    let state = match charger.online {
        Some(false) => String::from("offline"),
        _ => {
            let phase = charger.charge_phase.map(|p| format!("{p:?}").to_lowercase());
            let mode = charger.operating_mode.map(|m| format!("{m:?}").to_lowercase());
            match (phase, mode) {
                (Some(phase), _) if phase != "none" => format!("{source} · {phase}"),
                (_, Some(mode)) => format!("{source} · {mode}"),
                _ => source.to_string(),
            }
        }
    };
    let mut input = Vec::new();
    if let Some(uv) = charger.input_voltage_uv {
        input.push(format!("{:.2}V", uv as f32 / 1e6));
    }
    if let Some(ua) = charger.input_current_ua {
        input.push(format!("{:.2}A", ua as f32 / 1e6));
    }
    if let Some(uv) = charger.float_voltage_uv {
        input.push(format!("float {:.2}V", uv as f32 / 1e6));
    }
    (state, input.join(" "))
}

fn format_wlan(wlan: &Wlan) -> (String, String, bool) {
    let device = match (wlan.phys, wlan.ifaces, &wlan.mac) {
        (0, _, _) => String::from("no phy"),
        (_, 0, _) => format!("{} phy, no iface", wlan.phys),
        (_, _, Some(mac)) => mac.clone(),
        (p, i, None) => format!("{p} phy, {i} iface"),
    };
    let (scan, ok) = match &wlan.scan {
        ScanState::Unavailable => (String::from("scan unavailable"), false),
        ScanState::Scanning => (String::from("scanning…"), true),
        ScanState::Done { networks, at } => {
            let age = (zx::MonotonicInstant::get() - *at).into_seconds().max(0);
            (format!("{networks} networks · {age}s ago"), true)
        }
        ScanState::Failed(e) => (format!("scan failed: {e}"), false),
    };
    (device, scan, ok)
}

/// Trims `text` from the front (keeping the most specific tail, e.g. the hash
/// of a version string) so it fits in `max_width` at `size`.
fn fit_tail(face: &FontFace, size: f32, text: &str, max_width: f32) -> String {
    if measure_text_width(face, size, text) <= max_width {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut start = 0;
    while start < chars.len() {
        let candidate: String =
            std::iter::once('…').chain(chars[start..].iter().copied()).collect();
        if measure_text_width(face, size, &candidate) <= max_width {
            return candidate;
        }
        start += 1;
    }
    "…".to_string()
}

pub fn build(params: Params<'_>) -> Dashboard {
    let Params { size, snapshot, fonts, armed, pressed, snapshot_busy, status } = params;
    let theme = Theme::dark();
    // Scene must stay mutable: `Scene::layout()` repositions group members via
    // `set_facet_size`/`set_facet_location`, which assert on `mutable`.
    let mut builder = SceneBuilder::new().background_color(theme.background);
    builder.start_group("root", Box::new(Absolute));
    let mut canvas = Canvas { builder: &mut builder };

    let width = size.width;
    let height = size.height;
    // Base scale: the panel is ~500 ppi, so be generous (≈71 px on a
    // 1280 px wide display). Rows that don't fit are truncated.
    let unit = (width.min(height) / 18.0).clamp(12.0, 100.0);
    let margin = unit;
    let content_left = margin;
    let content_right = width - margin;
    let content_width = content_right - content_left;

    let title_size = unit * 1.3;
    let big_size = unit * 3.0;
    let body_size = unit * 0.8;
    let small_size = unit * 0.6;
    let line = body_size * 1.5;

    let regular = &fonts.regular;
    let mono = &fonts.mono;
    let mut y = margin;

    // Temperature bands shared by the battery pack and the SoC sensors.
    // The pack reports its own health; sensors only have a number.
    let temp_color = |celsius: f32| match celsius {
        t if t >= 75.0 => theme.bad,
        t if t >= 60.0 => theme.hot,
        t if t >= 45.0 => theme.warn,
        _ => theme.good,
    };
    let health_color = |health: Option<HealthStatus>| match health {
        Some(HealthStatus::Good) => theme.good,
        Some(HealthStatus::Cool) | Some(HealthStatus::Warm) => theme.warn,
        Some(HealthStatus::Hot) | Some(HealthStatus::Cold) => theme.bad,
        Some(HealthStatus::Dead) | Some(HealthStatus::OverVoltage) => theme.bad,
        _ => theme.dim,
    };

    // Header -----------------------------------------------------------
    canvas.text(regular, "Fuchsia", title_size, point2(content_left, y), theme.foreground);
    let product_board = match (&snapshot.product, &snapshot.board) {
        (Some(p), Some(b)) => format!("{p}.{b}"),
        (Some(p), None) => p.clone(),
        _ => String::from("(no build info yet)"),
    };
    canvas.text_right(
        mono,
        &product_board,
        body_size,
        content_right,
        y + title_size - body_size,
        theme.dim,
    );
    y += title_size * 1.4;
    canvas.rect(Rect::new(point2(content_left, y), size2(content_width, 2.0)), theme.rule, None);
    y += unit * 0.6;

    // Battery ----------------------------------------------------------
    match &snapshot.battery {
        Some(battery) => {
            let level = battery.level_percent;
            let level_text = match level {
                Some(level) => format!("{:.0}%", level.clamp(0.0, 100.0)),
                None => String::from("--%"),
            };
            let level_color = match level {
                Some(l) if l <= 15.0 => theme.bad,
                Some(l) if l <= 35.0 => theme.warn,
                _ => theme.good,
            };
            canvas.text(regular, &level_text, big_size, point2(content_left, y), level_color);
            let level_width = measure_text_width(regular, big_size, &level_text);
            let detail_x = content_left + level_width + unit * 0.6;
            let mut detail_y = y + big_size * 0.15;
            canvas.text(
                regular,
                charge_label(battery, snapshot.charger.as_ref()),
                body_size,
                point2(detail_x, detail_y),
                theme.foreground,
            );
            detail_y += line;
            // Volts and smoothed power in milliwatts (+ charging / − discharging).
            let volts = battery.voltage_uv.map(|uv| uv as f32 / 1e6);
            let amps = battery.current_avg_ua.map(|ua| ua / 1e6);
            let mut electrical = Vec::new();
            if let Some(v) = volts {
                electrical.push(format!("{v:.3} V"));
            }
            if let (Some(v), Some(a)) = (volts, amps) {
                electrical.push(format!("{:+.0} mW", v * a * 1e3));
            }
            if !electrical.is_empty() {
                canvas.text(
                    mono,
                    &electrical.join("  "),
                    small_size,
                    point2(detail_x, detail_y),
                    theme.dim,
                );
                detail_y += small_size * 1.5;
            }
            if let Some(t) = battery.temp_celsius {
                let mut text = format!("{t:.1} °C");
                if let Some(health) = health_label(battery.health) {
                    text.push_str(&format!("  {health}"));
                }
                let color = match battery.health {
                    Some(_) => health_color(battery.health),
                    None => temp_color(t),
                };
                canvas.text(mono, &text, small_size, point2(detail_x, detail_y), color);
            }
            y += big_size * 1.15;
            // Level bar.
            let bar_height = unit * 0.5;
            let bar = Rect::new(point2(content_left, y), size2(content_width, bar_height));
            // Fill first, then track: earlier facets render on top.
            if let Some(level) = level {
                let fill_width =
                    (content_width * (level.clamp(0.0, 100.0) / 100.0)).max(bar_height);
                let fill = Rect::new(bar.origin, size2(fill_width, bar_height));
                canvas.rect(fill, level_color, Some(bar_height / 2.0));
            }
            canvas.rect(bar, theme.rule, Some(bar_height / 2.0));
            y += bar_height + unit * 0.6;
        }
        None => {
            canvas.text(regular, "Battery", body_size, point2(content_left, y), theme.dim);
            canvas.text_right(mono, "waiting for driver…", body_size, content_right, y, theme.dim);
            y += line + unit * 0.3;
        }
    }
    canvas.rect(Rect::new(point2(content_left, y), size2(content_width, 2.0)), theme.rule, None);
    y += unit * 0.5;

    // Key / value rows -------------------------------------------------
    let label_width = unit * 3.6;
    let value_max = content_width - label_width;
    let row_colored =
        |canvas: &mut Canvas<'_>, y: &mut f32, label: &str, value: Option<(String, Color)>| {
            canvas.text(regular, label, body_size, point2(content_left, *y), theme.dim);
            let (text, color) = match value {
                Some((v, color)) => (fit_tail(mono, body_size, &v, value_max), color),
                None => (String::from("N/A"), theme.rule),
            };
            canvas.text_right(mono, &text, body_size, content_right, *y, color);
            *y += line;
        };
    let row = |canvas: &mut Canvas<'_>, y: &mut f32, label: &str, value: Option<String>| {
        row_colored(canvas, y, label, value.map(|v| (v, theme.foreground)));
    };

    row(&mut canvas, &mut y, "Device", snapshot.device_name.clone());
    row(&mut canvas, &mut y, "Serial", snapshot.serial.clone());
    row(&mut canvas, &mut y, "Build", snapshot.version.clone());
    row(&mut canvas, &mut y, "Uptime", Some(format_uptime(snapshot.uptime)));
    match &snapshot.last_boot {
        Some(last) => {
            let (text, graceful) = format_last_boot(last);
            let color = if graceful { theme.foreground } else { theme.bad };
            row_colored(&mut canvas, &mut y, "Last boot", Some((text, color)));
        }
        None => row(&mut canvas, &mut y, "Last boot", None),
    }
    let memory = match (snapshot.mem_free_bytes, snapshot.mem_total_bytes) {
        (Some(free), Some(total)) => {
            Some(format!("{} free / {}", format_gib(free), format_gib(total)))
        }
        _ => None,
    };
    row(&mut canvas, &mut y, "Memory", memory);
    let cpu = snapshot.cpu_load.as_ref().filter(|l| !l.is_empty()).map(|loads| {
        let avg = loads.iter().sum::<f32>() / loads.len() as f32;
        let max = loads.iter().cloned().fold(0.0_f32, f32::max);
        format!("{avg:.0}% avg  {max:.0}% max  ({} cpus)", loads.len())
    });
    row(&mut canvas, &mut y, "CPU", cpu);
    // One entry per frequency domain (little → big cores on Tensor).
    let freq = (!snapshot.cpu_freq_hz.is_empty()).then(|| {
        snapshot
            .cpu_freq_hz
            .iter()
            .map(|(_, hz)| format!("{:.2}", *hz as f64 / 1e9))
            .collect::<Vec<_>>()
            .join("  ")
    });
    row(&mut canvas, &mut y, "CPU GHz", freq);

    match &snapshot.charger {
        Some(charger) => {
            let (state, input) = format_charger(charger);
            let color = match charger.health {
                Some(fidl_fuchsia_hardware_power_charger::Health::Good) | None => theme.foreground,
                Some(_) => theme.warn,
            };
            row_colored(&mut canvas, &mut y, "Charger", Some((state, color)));
            if !input.is_empty() {
                row(&mut canvas, &mut y, "Charger in", Some(input));
            }
        }
        None => row(&mut canvas, &mut y, "Charger", None),
    }

    match &snapshot.wlan {
        Some(wlan) => {
            let (device, scan, ok) = format_wlan(wlan);
            row(&mut canvas, &mut y, "WLAN", Some(device));
            let color = if ok { theme.foreground } else { theme.dim };
            row_colored(&mut canvas, &mut y, "WLAN scan", Some((scan, color)));
        }
        None => row(&mut canvas, &mut y, "WLAN", None),
    }

    if snapshot.addresses.is_empty() {
        row(&mut canvas, &mut y, "Network", None);
    } else {
        for (iface, addr) in snapshot.addresses.iter().take(4) {
            row(&mut canvas, &mut y, &format!("IP {iface}"), Some(addr.clone()));
        }
    }

    // Buttons occupy the bottom; temperatures fill whatever is left.
    let button_height = unit * 2.0;
    let gap = unit * 0.5;
    let buttons_top = height - margin - button_height;
    let snapshot_top = buttons_top - gap - button_height;
    let status_y = snapshot_top - small_size * 1.8;

    if snapshot.temps.is_empty() {
        row(&mut canvas, &mut y, "Thermal", None);
    } else {
        // Only the hottest few sensors are interesting at a glance.
        const HOTTEST: usize = 4;
        let mut temps: Vec<&(String, f32)> = snapshot.temps.iter().collect();
        temps.sort_by(|a, b| b.1.total_cmp(&a.1));
        let shown = temps.len().min(HOTTEST);
        canvas.text(
            regular,
            &format!("Thermal · hottest {shown} of {}", temps.len()),
            small_size,
            point2(content_left, y),
            theme.dim,
        );
        y += small_size * 1.6;
        for (name, temp) in temps.into_iter().take(HOTTEST) {
            if y + line > status_y - unit * 0.4 {
                break;
            }
            let value = (format!("{temp:.1} °C"), temp_color(*temp));
            row_colored(&mut canvas, &mut y, name, Some(value));
        }
    }

    // Status line + buttons -------------------------------------------
    let (status_text, status_color) = match (armed, status) {
        (Some(action), _) => (format!("Tap {} again to confirm", action.label()), theme.warn),
        (None, Some(status)) => {
            let color = if status.starts_with("Failed:") { theme.bad } else { theme.dim };
            (status.to_string(), color)
        }
        (None, None) => {
            (String::from("devscreen prototype · VIRTCON hands the panel to virtcon"), theme.dim)
        }
    };
    let status_width = measure_text_width(regular, small_size, &status_text);
    let status_size = if status_width > content_width {
        (small_size * content_width / status_width).max(unit * 0.35)
    } else {
        small_size
    };
    canvas.text(
        regular,
        &status_text,
        status_size,
        point2(content_left, status_y + (small_size - status_size)),
        status_color,
    );

    let mut buttons = Vec::new();
    let mut button = |canvas: &mut Canvas<'_>, action: Action, rect: Rect, enabled: bool| {
        let is_armed = armed == Some(action);
        let is_pressed = pressed == Some(action);
        let color = match (is_armed, is_pressed) {
            (true, _) => theme.bad,
            (false, true) => theme.button_pressed,
            (false, false) => theme.button,
        };
        // Carnelian paints earlier facets on top of later ones, so the label
        // must be added before the button background.
        let label = match (is_armed, enabled) {
            (true, _) => "CONFIRM",
            (false, false) => "COLLECTING SNAPSHOT…",
            (false, true) => action.label(),
        };
        let mut label_size = body_size;
        if measure_text_width(regular, label_size, label) > rect.size.width - unit * 0.6 {
            label_size = small_size;
        }
        let text_color = if enabled { theme.button_text } else { theme.dim };
        canvas.text_center(regular, label, label_size, rect, text_color);
        canvas.rect(rect, color, Some(unit * 0.3));
        if enabled {
            buttons.push(Button { action, rect });
        }
    };

    let snapshot_rect =
        Rect::new(point2(content_left, snapshot_top), size2(content_width, button_height));
    button(&mut canvas, Action::Snapshot, snapshot_rect, !snapshot_busy);

    let button_count = Action::BOTTOM_ROW.len() as f32;
    let button_width = (content_width - gap * (button_count - 1.0)) / button_count;
    for (index, action) in Action::BOTTOM_ROW.iter().copied().enumerate() {
        let x = content_left + index as f32 * (button_width + gap);
        let rect = Rect::new(point2(x, buttons_top), size2(button_width, button_height));
        button(&mut canvas, action, rect, true);
    }

    builder.end_group();
    Dashboard { scene: builder.build(), buttons }
}
