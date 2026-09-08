//! The Unicode logo from `examples/ascii_logo.py`, with a breathing gradient stop.
use std::sync::OnceLock;
use std::time::Instant;

use gpui::{
    Context, IntoElement, Render, SharedString, TextRun, Window, canvas, point, prelude::*, px,
};

const TEXTURE_SIZE: usize = 306;
const COLUMNS: usize = 48;
const ROWS: usize = 24;
const BREATH_PERIOD: f64 = 8.;
const GLYPHS: [char; 5] = [' ', '·', '∙', '•', '●'];

/// A separate view keeps the decorative animation from rerendering the picker.
pub(super) struct AsciiLogo {
    started: Instant,
}

impl AsciiLogo {
    pub(super) fn new() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

impl Render for AsciiLogo {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let seconds = if crate::motion::enabled(cx) {
            window.request_animation_frame();
            self.started.elapsed().as_secs_f64()
        } else {
            0.0
        };
        let phase = (seconds / BREATH_PERIOD).fract();
        let t = (2. * phase - 1.).abs();
        // Smootherstep settles both velocity and acceleration at each turnaround.
        let eased = t.powi(3) * (t * (6. * t - 15.) + 10.);
        let stop_opacity = 0.3 * eased;
        let cells = render(COLUMNS, ROWS, stop_opacity);
        let theme = crate::theme::theme(cx);
        canvas(
            |_, _, _| {},
            move |bounds, _, window, cx| {
                let cell_width = (bounds.size.width / COLUMNS as f32)
                    .min(bounds.size.height / (ROWS * 2) as f32);
                if cell_width <= px(0.) {
                    return;
                }
                let line_height = cell_width * 2.;
                let font_size = cell_width / 0.6;
                let origin = bounds.center()
                    - point(
                        cell_width * COLUMNS as f32 / 2.,
                        line_height * ROWS as f32 / 2.,
                    );
                let font = gpui::font(crate::theme::BUNDLED_FAMILY);
                let mut glyphs = std::collections::HashMap::new();
                for (index, value) in cells.iter().copied().enumerate() {
                    let (level, shade) = quantize(value);
                    if level == 0 {
                        continue;
                    }
                    let shaped = glyphs.entry((level, shade)).or_insert_with(|| {
                        let text = SharedString::from(GLYPHS[level as usize].to_string());
                        let run = TextRun {
                            len: text.len(),
                            font: font.clone(),
                            color: theme.logo_color(shade as f32 / 23.),
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        };
                        window
                            .text_system()
                            .shape_line(text, font_size, &[run], None)
                    });
                    // Position each glyph on the terminal grid even if font fallback
                    // gives one of the Unicode circles a different advance width.
                    let position = origin
                        + point(
                            cell_width * (index % COLUMNS) as f32
                                + (cell_width - shaped.width) / 2.,
                            line_height * (index / COLUMNS) as f32,
                        );
                    let _ = shaped.paint(position, line_height, window, cx);
                }
            },
        )
        .size_full()
    }
}

fn quantize(value: f64) -> (u8, u8) {
    (
        (value * 4.).round_ties_even().clamp(0., 4.) as u8,
        (value * 23.).round_ties_even().clamp(0., 23.) as u8,
    )
}

// Each covered pixel retains the two gradient contributions independently.
// Recompose the strokes before sampling so their overlaps keep their brightness.
struct GradientSample {
    index: usize,
    fixed_alpha: f64,
    stop_weight: f64,
}

fn samples() -> &'static [GradientSample] {
    static SAMPLES: OnceLock<Vec<GradientSample>> = OnceLock::new();
    SAMPLES.get_or_init(rasterize)
}

fn render(width: usize, height: usize, stop_opacity: f64) -> Vec<f64> {
    let mut cells = vec![0.; width * height];
    let scale = ((width as f64 - 4.) / 4.4).min((height as f64 - 2.) / 2.2);
    if scale <= 0. {
        return cells;
    }
    let mut texture = vec![0.; TEXTURE_SIZE * TEXTURE_SIZE];
    for sample in samples() {
        let alpha = sample.fixed_alpha + sample.stop_weight * stop_opacity;
        texture[sample.index] += (1. - texture[sample.index]) * alpha;
    }
    for row in 0..height {
        for col in 0..width {
            let mut total = 0.;
            for oy in [-0.375, -0.125, 0.125, 0.375] {
                let y = (row as f64 + oy - (height as f64 - 1.) / 2.) / scale;
                for ox in [-0.25, 0.25] {
                    let x = (col as f64 + ox - (width as f64 - 1.) / 2.) / (scale * 2.);
                    let u = (x + 1.) * TEXTURE_SIZE as f64 / 2. - 0.5;
                    let v = (y + 1.) * TEXTURE_SIZE as f64 / 2. - 0.5;
                    let ix = u.floor();
                    let iy = v.floor();
                    if ix >= 0.
                        && iy >= 0.
                        && ix < (TEXTURE_SIZE - 1) as f64
                        && iy < (TEXTURE_SIZE - 1) as f64
                    {
                        let fx = u - ix;
                        let fy = v - iy;
                        let i = iy as usize * TEXTURE_SIZE + ix as usize;
                        total += ((1. - fx) * texture[i] + fx * texture[i + 1]) * (1. - fy);
                        total += ((1. - fx) * texture[i + TEXTURE_SIZE]
                            + fx * texture[i + TEXTURE_SIZE + 1])
                            * fy;
                    }
                }
            }
            cells[row * width + col] = total / 8.;
        }
    }
    cells
}

// Like the Python prototype, this reads only the bundled logo's absolute
// M/C/Z paths and two-stop gradients. It is not a general SVG loader.
fn rasterize() -> Vec<GradientSample> {
    let svg = include_str!("../../logo.svg");
    let number = regex::Regex::new(r"[-+]?(?:\d*\.\d+|\d+)").unwrap();
    let numbers = |s: &str| -> Vec<f64> {
        number
            .find_iter(s)
            .map(|n| n.as_str().parse().unwrap())
            .collect()
    };
    let attribute = |tag: &str, name: &str| -> String {
        tag.split_once(&format!("{name}=\""))
            .unwrap()
            .1
            .split('"')
            .next()
            .unwrap()
            .to_owned()
    };
    let viewbox = numbers(&attribute(svg, "viewBox"));
    let scale = TEXTURE_SIZE as f64 / viewbox[2];
    let mut samples = Vec::new();
    for path in svg.split("<path ").skip(1) {
        let path = path.split_once("/>").unwrap().0;
        let data = attribute(path, "d");
        let mut segments = data
            .trim_start_matches('M')
            .trim_end_matches('Z')
            .split('C');
        let start = numbers(segments.next().unwrap());
        let mut points = vec![[start[0], start[1]]];
        for segment in segments {
            let values = numbers(segment);
            let p0 = *points.last().unwrap();
            for step in 1..=12 {
                let t = step as f64 / 12.;
                let s = 1. - t;
                points.push(std::array::from_fn(|k| {
                    s.powi(3) * p0[k]
                        + 3. * s * s * t * values[k]
                        + 3. * s * t * t * values[k + 2]
                        + t.powi(3) * values[k + 4]
                }));
            }
        }
        points.push(points[0]);
        for p in &mut points {
            p[0] = (p[0] - viewbox[0]) * scale;
            p[1] = (p[1] - viewbox[1]) * scale;
        }
        let radius = attribute(path, "stroke-width").parse::<f64>().unwrap() * scale / 2.;
        let mut coverage = vec![0_f64; TEXTURE_SIZE * TEXTURE_SIZE];
        for segment in points.windows(2) {
            let [ax, ay] = segment[0];
            let [bx, by] = segment[1];
            let dx = bx - ax;
            let dy = by - ay;
            let length2 = dx * dx + dy * dy;
            if length2 < 1e-12 {
                continue;
            }
            let left = (ax.min(bx) - radius - 1.).max(0.) as usize;
            let right = (ax.max(bx) + radius + 1.).ceil().min(TEXTURE_SIZE as f64) as usize;
            let top = (ay.min(by) - radius - 1.).max(0.) as usize;
            let bottom = (ay.max(by) + radius + 1.).ceil().min(TEXTURE_SIZE as f64) as usize;
            for y in top..bottom {
                for x in left..right {
                    let t = (((x as f64 + 0.5 - ax) * dx + (y as f64 + 0.5 - ay) * dy) / length2)
                        .clamp(0., 1.);
                    let distance =
                        (x as f64 + 0.5 - ax - t * dx).hypot(y as f64 + 0.5 - ay - t * dy);
                    let i = y * TEXTURE_SIZE + x;
                    coverage[i] = coverage[i].max((radius + 0.5 - distance).min(1.));
                }
            }
        }
        let stroke = attribute(path, "stroke");
        let id = stroke
            .strip_prefix("url(#")
            .unwrap()
            .strip_suffix(')')
            .unwrap();
        let gradient = svg
            .split_once(&format!("<linearGradient id=\"{id}\""))
            .unwrap()
            .1
            .split_once("</linearGradient>")
            .unwrap()
            .0;
        let coords: [f64; 4] =
            ["x1", "y1", "x2", "y2"].map(|key| attribute(gradient, key).parse().unwrap());
        let dx = coords[2] - coords[0];
        let dy = coords[3] - coords[1];
        let stops: Vec<_> = gradient.split("<stop ").skip(1).collect();
        let opacity = |stop: &str| -> f64 {
            if stop.contains("stop-opacity=") {
                attribute(stop, "stop-opacity").parse().unwrap()
            } else {
                1.
            }
        };
        let a = opacity(stops[0]);
        let end = attribute(stops[1], "offset").parse::<f64>().unwrap();
        for (i, ink) in coverage.into_iter().enumerate() {
            if ink <= 0. {
                continue;
            }
            let x = (i % TEXTURE_SIZE) as f64 + 0.5;
            let y = (i / TEXTURE_SIZE) as f64 + 0.5;
            let t = (((x / scale + viewbox[0] - coords[0]) * dx
                + (y / scale + viewbox[1] - coords[1]) * dy)
                / ((dx * dx + dy * dy) * end))
                .clamp(0., 1.);
            samples.push(GradientSample {
                index: i,
                fixed_alpha: ink * a * (1. - t),
                stop_weight: ink * t,
            });
        }
    }
    samples
}
