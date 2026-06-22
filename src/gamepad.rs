use std::fmt::Debug;

use tiny_skia::{
    Color, FillRule, Mask, Paint, Path, PathBuilder, Pixmap, PremultipliedColorU8, Rect, Stroke,
    Transform,
};

static FONT_BYTES: &[u8] = include_bytes!("../assets/fonts/Teko-Bold.ttf");
static FONT: std::sync::OnceLock<ab_glyph::FontVec> = std::sync::OnceLock::new();

fn is_arrow(c: char) -> bool {
    matches!(c, '←' | '→' | '↑' | '↓')
}

fn draw_arrow_glyph(img: &mut Pixmap, ch: char, cx: f32, cy: f32, font_size: f32, color: Color) {
    let hs = font_size * 0.28;
    let hw = hs * 0.70;
    let mut pb = PathBuilder::new();
    match ch {
        '→' => { pb.move_to(cx + hs, cy); pb.line_to(cx - hs, cy - hw); pb.line_to(cx - hs, cy + hw); }
        '←' => { pb.move_to(cx - hs, cy); pb.line_to(cx + hs, cy - hw); pb.line_to(cx + hs, cy + hw); }
        '↑' => { pb.move_to(cx, cy - hs); pb.line_to(cx - hw, cy + hs); pb.line_to(cx + hw, cy + hs); }
        '↓' => { pb.move_to(cx, cy + hs); pb.line_to(cx - hw, cy - hs); pb.line_to(cx + hw, cy - hs); }
        _ => return,
    }
    pb.close();
    if let Some(path) = pb.finish() {
        let mut paint = Paint { anti_alias: true, ..Default::default() };
        paint.set_color(color);
        img.fill_path(&path, &paint, FillRule::default(), Transform::identity(), None);
    }
}

fn draw_label(img: &mut Pixmap, text: &str, cx_px: f32, cy_px: f32, radius_px: f32, color: Color) {
    use ab_glyph::{Font, PxScale, ScaleFont};

    let font = FONT.get_or_init(|| {
        ab_glyph::FontVec::try_from_vec(FONT_BYTES.to_vec()).expect("bundled Teko font is valid")
    });

    // Pick a font size that fits within 88% of the button diameter, starting at 60%.
    // Arrow chars (Teko lacks them) use a fixed cell width of 0.7× font_size.
    let diameter = radius_px * 2.0;
    let initial_size = diameter * 0.75;
    let pxscale = PxScale::from(initial_size);
    let scaled = font.as_scaled(pxscale);
    let measured_width: f32 = text
        .chars()
        .map(|c| if is_arrow(c) { initial_size * 0.7 } else { scaled.h_advance(font.glyph_id(c)) })
        .sum();
    let max_width = diameter * 0.92;
    let font_size = if measured_width > max_width {
        initial_size * max_width / measured_width
    } else {
        initial_size
    };

    let pxscale = PxScale::from(font_size);
    let scaled = font.as_scaled(pxscale);
    let ascent = scaled.ascent();
    let descent = scaled.descent(); // negative in ab_glyph convention
    // Center the ascent+descent range at cy_px: baseline = cy_px + (ascent + descent) / 2.
    // Round both to integer pixels so glyphs snap to pixel boundaries and stay crisp.
    let y_baseline = (cy_px + (ascent + descent) / 2.0).round();

    let mut x_cursor = 0.0f32;
    let positions: Vec<(char, f32)> = text
        .chars()
        .map(|c| {
            let x = x_cursor;
            x_cursor += if is_arrow(c) { font_size * 0.7 } else { scaled.h_advance(font.glyph_id(c)) };
            (c, x)
        })
        .collect();
    let x_start = (cx_px - x_cursor / 2.0).round();

    // Pass 1: rasterize Teko glyphs via pixel-level compositing (requires pixels_mut borrow).
    // First render a dark shadow at 4 diagonal offsets, then the main color on top.
    {
        let img_w = img.width();
        let img_h = img.height();
        let pixels = img.pixels_mut();

        let shadow = Color::from_rgba(0.0, 0.0, 0.0, 0.9).unwrap();
        let shadow_offsets: [(f32, f32); 5] =
            [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0), (0.0, 0.0)];
        let pass_colors = [shadow, shadow, shadow, shadow, color];

        for (offset, pass_color) in shadow_offsets.iter().zip(pass_colors.iter()) {
            let cr = pass_color.red();
            let cg = pass_color.green();
            let cb = pass_color.blue();
            let ca = pass_color.alpha();
            for &(c, gx) in &positions {
                if is_arrow(c) {
                    continue;
                }
                let glyph = font.glyph_id(c).with_scale_and_position(
                    pxscale,
                    ab_glyph::point(x_start + gx + offset.0, y_baseline + offset.1),
                );
                let Some(outlined) = font.outline_glyph(glyph) else { continue };
                let bounds = outlined.px_bounds();
                outlined.draw(|x, y, coverage| {
                    let px = bounds.min.x as i32 + x as i32;
                    let py = bounds.min.y as i32 + y as i32;
                    if px < 0 || py < 0 || px >= img_w as i32 || py >= img_h as i32 {
                        return;
                    }
                    let idx = py as usize * img_w as usize + px as usize;
                    let dst = pixels[idx];
                    let src_a = ca * coverage;
                    let inv = 1.0 - src_a;
                    // Premultiplied "over" composite.
                    let out_r = cr * src_a + (dst.red() as f32 / 255.0) * inv;
                    let out_g = cg * src_a + (dst.green() as f32 / 255.0) * inv;
                    let out_b = cb * src_a + (dst.blue() as f32 / 255.0) * inv;
                    let out_a = src_a + (dst.alpha() as f32 / 255.0) * inv;
                    let r = (out_r * 255.0) as u8;
                    let g = (out_g * 255.0) as u8;
                    let b = (out_b * 255.0) as u8;
                    let a = (out_a * 255.0) as u8;
                    pixels[idx] =
                        PremultipliedColorU8::from_rgba(r.min(a), g.min(a), b.min(a), a)
                            .unwrap_or(PremultipliedColorU8::TRANSPARENT);
                });
            }
        }
    }

    // Pass 2: draw arrow chars as geometric paths (requires &mut Pixmap, not pixels).
    for &(c, gx) in &positions {
        if !is_arrow(c) {
            continue;
        }
        let cell_cx = x_start + gx + font_size * 0.35;
        draw_arrow_glyph(img, c, cell_cx, cy_px, font_size, color);
    }
}

use crate::config::{self, FillDir};

#[derive(Debug)]
pub struct Gamepad<'b> {
    pub backend: Option<Box<dyn Backend + 'b>>,
    pub inputs: Inputs,
    pub input_state: InputState,
    /// Render-resolution multiplier. 1.0 = native layout size; >1 renders the
    /// same layout into a proportionally larger pixmap so it stays crisp when
    /// displayed/scaled up. The OBS plugin leaves this at 1.0.
    pub scale: f32,
    /// Whether to rasterize button labels directly into the pixmap. Set to
    /// `false` in web mode because the `/?labels` HTML overlay handles them.
    pub render_labels: bool,
}

impl Default for Gamepad<'_> {
    fn default() -> Self {
        Self {
            backend: None,
            inputs: Inputs::default(),
            input_state: InputState::default(),
            scale: 1.0,
            render_labels: true,
        }
    }
}

impl<'b> Gamepad<'b> {
    #[allow(dead_code)]
    fn new(config: &config::Gamepad) -> Self {
        let inputs: Inputs = config.into();
        let input_state = (&inputs).into();
        Self { backend: None, inputs, input_state, scale: 1.0, render_labels: true }
    }

    /// Pixel dimensions of the output image at the current `scale`.
    #[allow(dead_code)] // used by the binary (main/web), not the OBS cdylib
    pub fn image_size(&self) -> (u32, u32) {
        let bounds = self.inputs.bounds();
        let w = ((bounds.right() * self.scale).ceil() as u32).max(1);
        let h = ((bounds.bottom() * self.scale).ceil() as u32).max(1);
        (w, h)
    }

    pub fn reload(&mut self, config: &config::Gamepad) {
        self.inputs = config.into();
        self.input_state = (&self.inputs).into();
        self.render_labels = config.show_labels;
        if let Some(b) = &mut self.backend {
            b.reload(&self.inputs)
        }
    }

    #[allow(dead_code)]
    pub fn load<B: Backend + 'b>(
        &mut self,
        config: &config::Gamepad,
        state: B::InitState,
    ) -> Result<(), B::Err> {
        self.inputs = config.into();
        self.input_state = (&self.inputs).into();
        self.backend = Some(Box::new(B::init(state, &self.inputs)?));
        Ok(())
    }

    pub fn poll(&mut self) -> bool {
        self.backend.as_mut().map(|b| b.poll(&mut self.input_state)).unwrap_or_default()
    }
}

#[derive(Debug, Clone, Default)]
pub struct Inputs {
    pub buttons: Vec<Button>,
    pub sticks: Vec<Stick>,
    pub axes: Vec<Axis>,
}

#[derive(Debug, Clone, Default)]
pub struct InputState {
    pub buttons: Vec<bool>,
    pub sticks: Vec<(f32, f32)>,
    pub axes: Vec<f32>,
}

pub trait Backend: Debug {
    type InitState
    where
        Self: Sized;
    type Err: Debug + Send + Sync + 'static
    where
        Self: Sized;

    fn init(state: Self::InitState, inputs: &Inputs) -> Result<Self, Self::Err>
    where
        Self: Sized;

    fn poll(&mut self, state: &mut InputState) -> bool;

    fn reload(&mut self, inputs: &Inputs);
}

impl Inputs {
    pub fn minimize(&mut self) {
        let bounds = self.bounds();
        let t = Transform::from_translate(-bounds.left(), -bounds.top());
        for b in &mut self.buttons {
            b.path = b.path.clone().transform(t).unwrap();
        }
        for s in &mut self.sticks {
            s.path = s.path.clone().transform(t).unwrap();
            if let Some((path, _, _)) = &mut s.gate {
                *path = path.clone().transform(t).unwrap();
            }
        }
        for a in &mut self.axes {
            a.path = a.path.clone().transform(t).unwrap();
        }
    }

    pub fn bounds(&self) -> Rect {
        self.buttons
            .iter()
            .map(Button::bounds)
            .chain(self.sticks.iter().map(Stick::bounds))
            .chain(self.axes.iter().map(Axis::bounds))
            .reduce(combine)
            .unwrap_or_else(|| Rect::from_ltrb(0.0, 0.0, 100.0, 100.0).unwrap())
    }
}

#[derive(Clone, Debug)]
pub struct Button {
    pub id: u8,
    pub path: Path,
    pub fill: ColorPair,
    pub outline: Option<(ColorPair, f32)>,
    pub label: Option<String>,
    /// Label text color pair (inactive/active). `None` = use the CSS default (#fff).
    pub label_color: Option<ColorPair>,
    pub skin: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Axis {
    pub axis: RawAxis,
    pub path: Path,
    pub direction: FillDir,
    pub fill: ColorPair,
    pub outline: Option<(Color, f32)>,
}

#[derive(Clone, Debug)]
pub struct Stick {
    pub x: RawAxis,
    pub y: RawAxis,
    pub deadzone: f32,
    pub path: Path,
    pub displacement: f32,
    pub fill: ColorPair,
    pub outline: Option<(ColorPair, f32)>,
    pub gate: Option<(Path, ColorPair, f32)>,
    pub skin: Option<String>,
}

// TODO
#[allow(unused)]
#[derive(Clone, Debug)]
pub struct Dpad {
    pub x: RawAxis,
    pub y: RawAxis,
    pub up: Path,
    pub down: Path,
    pub left: Path,
    pub right: Path,
    pub fill: ColorPair,
    pub outline: Option<(Path, Color, f32)>,
}

#[derive(Clone, Debug, Default)]
pub struct RawAxis {
    pub id: u8,
    pub invert: bool,
}

impl From<&config::Gamepad> for Inputs {
    fn from(config: &config::Gamepad) -> Self {
        let mut temp = Self {
            buttons: config.buttons.iter().map(|b| b.load(config)).collect(),
            axes: config.axes.iter().map(|b| b.load(config)).collect(),
            sticks: config.sticks.iter().map(|b| b.load(config)).collect(),
        };
        temp.minimize();
        temp
    }
}

impl Gamepad<'_> {
    pub fn render(&self, img: &mut Pixmap) {
        let mut stroke = Stroke::default();
        let mut paint = Paint { anti_alias: true, ..Default::default() };
        let f = FillRule::default();
        // Everything is drawn through this transform, so bumping `scale` renders
        // the whole layout (paths, strokes, displacements) at a higher resolution.
        let t = Transform::from_scale(self.scale, self.scale);
        img.fill(Color::TRANSPARENT);

        for (button, &pressed) in self.inputs.buttons.iter().zip(&self.input_state.buttons)
        {
            paint.set_color(button.fill.get(pressed));
            img.fill_path(&button.path, &paint, f, t, None);

            if let Some((colors, weight)) = &button.outline {
                paint.set_color(colors.get(pressed));
                stroke.width = *weight;
                img.stroke_path(&button.path, &paint, &stroke, t, None);
            }

            if self.render_labels {
                if let Some(text) = &button.label {
                    let pb = button.path.bounds();
                    let cx_px = (pb.left() + pb.right()) / 2.0 * self.scale;
                    let cy_px = (pb.top() + pb.bottom()) / 2.0 * self.scale;
                    let radius_px = pb.width() / 2.0 * self.scale;
                    let color = button
                        .label_color
                        .as_ref()
                        .map(|p| p.get(pressed))
                        .unwrap_or(Color::WHITE);
                    draw_label(img, text, cx_px, cy_px, radius_px, color);
                }
            }
        }

        let mut mask = Mask::new(img.width(), img.height()).unwrap();
        for (axis, &percent) in self.inputs.axes.iter().zip(&self.input_state.axes) {
            // background
            let rect = axis.path.bounds();
            paint.set_color(axis.fill.inactive);
            img.fill_path(&axis.path, &paint, f, t, None);

            let percent = if axis.axis.invert { 1.0 - percent } else { percent };
            // active fill
            use FillDir::*;
            let mut left = rect.left();
            let mut top = rect.top();
            let mut right = rect.right();
            let mut bottom = rect.bottom();
            match axis.direction {
                TopToBottom => bottom -= rect.height() * percent,
                LeftToRight => right -= rect.width() * percent,
                BottomToTop => top += rect.height() * (1.0 - percent),
                RightToLeft => left += rect.width() * (1.0 - percent),
            };
            if let Some(rect) = Rect::from_ltrb(left, top, right, bottom)
                && rect.width() > 0.05
                && rect.height() > 0.05
            {
                mask.clear();
                mask.fill_path(&axis.path, tiny_skia::FillRule::Winding, true, t);

                let active_path = PathBuilder::from_rect(rect);
                paint.set_color(axis.fill.active);
                // img.fill_path(&active_path, &paint, f, t, None);
                img.fill_path(&active_path, &paint, f, t, Some(&mask));
            }

            // border
            if let Some((color, weight)) = axis.outline {
                stroke.width = weight;
                paint.set_color(color);
                img.stroke_path(&axis.path, &paint, &stroke, t, None);
            }
        }

        for (stick, &(x, y)) in self.inputs.sticks.iter().zip(&self.input_state.sticks) {
            let deadzone = stick.deadzone;
            let is_active =
                !(-deadzone < x && x < deadzone && -deadzone < y && y < deadzone);
            let x = if stick.x.invert { -x } else { x };
            let y = if stick.x.invert { -y } else { y };
            let cx = stick.displacement * x * (1.0 - y * y / 2.0).sqrt();
            let cy = stick.displacement * y * (1.0 - x * x / 2.0).sqrt();

            if let Some((path, color, weight)) = &stick.gate {
                paint.set_color(color.get(is_active));
                stroke.width = *weight;
                img.stroke_path(path, &paint, &stroke, t, None);
            }

            let trans = t.pre_translate(cx, cy);
            paint.set_color(stick.fill.get(is_active));
            img.fill_path(&stick.path, &paint, f, trans, None);

            if let Some((colors, weight)) = &stick.outline {
                paint.set_color(colors.get(is_active));
                stroke.width = *weight;
                img.stroke_path(&stick.path, &paint, &stroke, trans, None);
            }
        }
    }
}

fn combine(a: Rect, b: Rect) -> Rect {
    Rect::from_ltrb(
        a.left().min(b.left()),
        a.top().min(b.top()),
        a.right().max(b.right()),
        a.bottom().max(b.bottom()),
    )
    .unwrap()
}

fn expand(r: Rect, f: f32) -> Rect {
    Rect::from_ltrb(r.left() - f, r.top() - f, r.right() + f, r.bottom() + f).unwrap()
}

#[derive(Clone, Debug)]
pub struct ColorPair {
    pub active: Color,
    pub inactive: Color,
}

impl ColorPair {
    pub fn new(active: Color, inactive: Color) -> Self {
        Self { active, inactive }
    }

    pub fn get(&self, active: bool) -> Color {
        if active { self.active } else { self.inactive }
    }
}

impl Button {
    pub fn bounds(&self) -> Rect {
        if let Some((_, width)) = self.outline {
            expand(self.path.bounds(), width)
        } else {
            self.path.bounds()
        }
    }
}

impl Stick {
    pub fn bounds(&self) -> Rect {
        let mut bounds = expand(self.path.bounds(), self.displacement);
        if let Some((_, width)) = self.outline {
            bounds = expand(bounds, width)
        }
        if let Some((path, _, width)) = &self.gate {
            bounds = combine(bounds, expand(path.bounds(), *width))
        }
        bounds
    }
}

impl Axis {
    pub fn bounds(&self) -> Rect {
        if let Some((_, width)) = &self.outline {
            expand(self.path.bounds(), *width)
        } else {
            self.path.bounds()
        }
    }
}

impl From<&Inputs> for InputState {
    fn from(inputs: &Inputs) -> Self {
        Self {
            buttons: vec![false; inputs.buttons.len()],
            axes: vec![0.0; inputs.axes.len()],
            sticks: vec![Default::default(); inputs.sticks.len()],
        }
    }
}
