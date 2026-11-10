//! Software renderer for the on-screen overlay (frame counter + app name).
//! Draws glyph outlines with tiny-skia so we get proper anti-aliased
//! black outlines, and hands back a premultiplied RGBA pixmap.

use tiny_skia::{
    Color, FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke, Transform,
};
use ttf_parser::{Face, OutlineBuilder};

const WHITE: (u8, u8, u8) = (255, 255, 255);
const RED: (u8, u8, u8) = (255, 38, 38);

pub struct Style<'a> {
    /// Measured frame rate (rounded).
    pub fps: u32,
    /// Seconds recorded so far; shown only while recording.
    pub elapsed: u64,
    pub recording: bool,
    /// User font-size multipliers (1.0 = default).
    pub counter_scale: f32,
    pub label_scale: f32,
    /// Idle-only legend lines, e.g. "F9: Toggle Rec".
    pub hints: &'a [String],
    pub name: &'a str,
    /// Seconds, drives the pulsing dot.
    pub time: f32,
}

pub struct Renderer {
    font: Vec<u8>,
}

impl Renderer {
    pub fn new() -> Result<Self, String> {
        let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
        for f in ["impact.ttf", "arialbd.ttf", "segoeuib.ttf", "verdanab.ttf"] {
            if let Ok(data) = std::fs::read(format!("{windir}\\Fonts\\{f}")) {
                if Face::parse(&data, 0).is_ok() {
                    return Ok(Self { font: data });
                }
            }
        }
        Err("no usable bold font found in the Windows Fonts folder".into())
    }

    /// Canvas size in pixels for a given window scale (1.0 == 1080p window) and
    /// the largest user font multiplier.
    pub fn canvas(scale: f32, user: f32) -> (u32, u32) {
        let m = scale * user.max(1.0);
        ((660.0 * m) as u32, (310.0 * m) as u32)
    }

    pub fn render(&self, st: &Style, scale: f32) -> Pixmap {
        let (w, h) = Self::canvas(scale, st.counter_scale.max(st.label_scale));
        let mut pm = Pixmap::new(w, h).unwrap();
        let face = Face::parse(&self.font, 0).unwrap();

        let big = 84.0 * scale * st.counter_scale;
        let small = 34.0 * scale * st.label_scale;
        let pad = 14.0 * scale;
        let color = if st.recording { RED } else { WHITE };

        // --- LIVE dot -------------------------------------------------
        let dot_r = big * 0.2;
        let dot_cx = pad + dot_r * 1.6;
        let dot_cy = pad + big * 0.52;
        if st.recording {
            let k = 0.5 + 0.5 * (st.time * std::f32::consts::TAU * 1.1).sin();
            // soft halo
            let mut halo = Paint::default();
            halo.set_color(Color::from_rgba8(RED.0, RED.1, RED.2, (70.0 * (1.0 - k)) as u8 + 10));
            halo.anti_alias = true;
            if let Some(p) = PathBuilder::from_circle(dot_cx, dot_cy, dot_r * (1.15 + 0.55 * k)) {
                pm.fill_path(&p, &halo, FillRule::Winding, Transform::identity(), None);
            }
            // core with black outline
            let r = dot_r * (0.82 + 0.18 * k);
            if let Some(p) = PathBuilder::from_circle(dot_cx, dot_cy, r) {
                let mut black = Paint::default();
                black.set_color(Color::BLACK);
                black.anti_alias = true;
                let stroke = Stroke {
                    width: big * 0.09,
                    line_join: LineJoin::Round,
                    ..Stroke::default()
                };
                pm.stroke_path(&p, &black, &stroke, Transform::identity(), None);
                let mut fill = Paint::default();
                fill.set_color(Color::from_rgba8(RED.0, RED.1, RED.2, (170.0 + 85.0 * k) as u8));
                fill.anti_alias = true;
                pm.fill_path(&p, &fill, FillRule::Winding, Transform::identity(), None);
            }
        }

        // --- FPS counter ---------------------------------------------
        let text_x = pad + dot_r * 3.6;
        let baseline = pad + big * 0.84;
        let digit = measure(&face, "0", big, true);
        let fps = st.fps.to_string();
        // right-align inside a 3-digit cell so the "FPS" label doesn't jitter
        let cell = digit * 3.0;
        let used = digit * fps.len() as f32;
        let nx = text_x + (cell - used).max(0.0);
        draw_text(&mut pm, &face, &fps, big, nx, baseline, true, color, big * 0.13);
        let label = big * 0.4;
        draw_text(&mut pm, &face, "FPS", label, text_x + cell + big * 0.12, baseline, false, color, label * 0.16);

        // --- app name -------------------------------------------------
        let max_w = w as f32 - text_x - pad;
        let name = fit(&face, st.name, small, max_w);
        draw_text(
            &mut pm,
            &face,
            &name,
            small,
            text_x,
            baseline + small * 1.25,
            false,
            WHITE,
            small * 0.14,
        );

        // --- recording clock, or the hotkey legend when idle -----------
        let line3 = baseline + small * 1.25;
        if st.recording {
            let (h, m, s) = (st.elapsed / 3600, st.elapsed / 60 % 60, st.elapsed % 60);
            let t = if h > 0 { format!("REC {h}:{m:02}:{s:02}") } else { format!("REC {m:02}:{s:02}") };
            let sz = small * 1.15;
            draw_text(&mut pm, &face, &t, sz, text_x, line3 + sz * 1.15, true, RED, sz * 0.14);
        } else {
            let sz = small * 0.78;
            for (i, l) in st.hints.iter().enumerate() {
                draw_text(&mut pm, &face, l, sz, text_x, line3 + sz * (1.45 + 1.3 * i as f32), false, WHITE, sz * 0.15);
            }
        }
        pm
    }
}

fn fit(face: &Face, s: &str, size: f32, max_w: f32) -> String {
    let mut s: Vec<char> = s.chars().collect();
    let mut truncated = false;
    while !s.is_empty() && measure(face, &s.iter().collect::<String>(), size, false) > max_w {
        s.pop();
        truncated = true;
    }
    let mut out: String = s.into_iter().collect();
    if truncated {
        out.push('…');
    }
    out
}

fn advance(face: &Face, c: char, scale: f32) -> f32 {
    face.glyph_index(c)
        .and_then(|g| face.glyph_hor_advance(g))
        .unwrap_or(0) as f32
        * scale
}

fn measure(face: &Face, s: &str, size: f32, tabular: bool) -> f32 {
    let scale = size / face.units_per_em() as f32;
    let digit = ('0'..='9').map(|c| advance(face, c, scale)).fold(0.0, f32::max);
    s.chars()
        .map(|c| if tabular && c.is_ascii_digit() { digit } else { advance(face, c, scale) })
        .sum()
}

struct Builder<'a> {
    pb: &'a mut PathBuilder,
    ox: f32,
    oy: f32,
    s: f32,
}

impl OutlineBuilder for Builder<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.pb.move_to(self.ox + x * self.s, self.oy - y * self.s);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.pb.line_to(self.ox + x * self.s, self.oy - y * self.s);
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.pb.quad_to(
            self.ox + x1 * self.s,
            self.oy - y1 * self.s,
            self.ox + x * self.s,
            self.oy - y * self.s,
        );
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.pb.cubic_to(
            self.ox + x1 * self.s,
            self.oy - y1 * self.s,
            self.ox + x2 * self.s,
            self.oy - y2 * self.s,
            self.ox + x * self.s,
            self.oy - y * self.s,
        );
    }
    fn close(&mut self) {
        self.pb.close();
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_text(
    pm: &mut Pixmap,
    face: &Face,
    s: &str,
    size: f32,
    x: f32,
    baseline: f32,
    tabular: bool,
    rgb: (u8, u8, u8),
    outline: f32,
) {
    let scale = size / face.units_per_em() as f32;
    let digit = ('0'..='9').map(|c| advance(face, c, scale)).fold(0.0, f32::max);
    let mut pb = PathBuilder::new();
    let mut pen = x;
    for c in s.chars() {
        if let Some(g) = face.glyph_index(c) {
            let adv = if tabular && c.is_ascii_digit() { digit } else { advance(face, c, scale) };
            // centre digits inside their fixed cell
            let natural = advance(face, c, scale);
            let off = if tabular && c.is_ascii_digit() { (adv - natural) / 2.0 } else { 0.0 };
            face.outline_glyph(g, &mut Builder { pb: &mut pb, ox: pen + off, oy: baseline, s: scale });
            pen += adv;
        }
    }
    let Some(path) = pb.finish() else { return };

    let mut black = Paint::default();
    black.set_color(Color::BLACK);
    black.anti_alias = true;
    let stroke = Stroke {
        width: outline,
        line_join: LineJoin::Round,
        line_cap: LineCap::Round,
        ..Stroke::default()
    };
    pm.stroke_path(&path, &black, &stroke, Transform::identity(), None);

    let mut fill = Paint::default();
    fill.set_color(Color::from_rgba8(rgb.0, rgb.1, rgb.2, 255));
    fill.anti_alias = true;
    pm.fill_path(&path, &fill, FillRule::Winding, Transform::identity(), None);
}
