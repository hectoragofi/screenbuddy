//! LaTeX math in answers: KaTeX layout (katex-native) rasterized to egui textures.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use egui::{Color32, ColorImage, TextureHandle, TextureOptions, Ui, Vec2};
use katex_native::katex::KatexContext;
use katex_native::raster::Raster;

/// Rendered formulas, keyed by TeX, mode, pixel size and color. Owned by the
/// palette so textures die with its egui context.
#[derive(Default)]
pub struct MathCache {
    tex: HashMap<(String, bool, u32, [u8; 4]), Option<(TextureHandle, f32)>>,
}

impl MathCache {
    pub fn clear(&mut self) {
        self.tex.clear();
    }
}

/// Draws `tex` into `ui`; falls back to the raw source when KaTeX rejects it.
pub fn show(ui: &mut Ui, cache: &RefCell<MathCache>, tex: &str, inline: bool) {
    let tex = tex.trim();
    let ppp = ui.ctx().pixels_per_point();
    let body = ui.style().text_styles[&egui::TextStyle::Body].size;
    let pt = if inline { body * 1.1 } else { body * 1.25 };
    let color = ui.visuals().text_color();
    let px = (pt * ppp).round().max(6.0) as u32;
    let key = (tex.to_string(), inline, px, color.to_array());

    let rendered = {
        let mut c = cache.borrow_mut();
        if c.tex.len() > 512 {
            c.tex.clear();
        }
        if !c.tex.contains_key(&key) {
            let r = render(&mut c, ui.ctx(), tex, inline, px, color);
            c.tex.insert(key.clone(), r);
        }
        c.tex[&key].clone()
    };

    let Some((handle, baseline)) = rendered else {
        let src = if inline { format!("${tex}$") } else { format!("$${tex}$$") };
        ui.label(egui::RichText::new(src).monospace());
        return;
    };

    let native = handle.size_vec2() / ppp;
    // Shrink wide formulas to the available width rather than clipping them.
    let max_w = ui.max_rect().width().max(40.0);
    let k = (max_w / native.x).min(1.0);
    let size = native * k;
    let img = egui::Image::new(&handle).fit_to_exact_size(size);
    if inline {
        // Rows centre their items; move the formula so its math axis
        // (about 0.25em above the baseline) lines up with the text's middle.
        let base = baseline / ppp * k;
        let centre_above_base = base - size.y / 2.0;
        let shift = (centre_above_base - 0.25 * pt * k).clamp(-body, body);
        let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::hover());
        img.paint_at(ui, rect.translate(Vec2::new(0.0, shift)));
        resp.on_hover_text(tex);
    } else {
        ui.end_row();
        let w = ui.max_rect().width();
        ui.allocate_ui_with_layout(
            Vec2::new(w, size.y + 8.0),
            egui::Layout::top_down(egui::Align::Center),
            |ui| {
                ui.add_space(4.0);
                ui.add(img).on_hover_text(tex);
            },
        );
        ui.end_row();
    }
}

fn render(
    c: &mut MathCache,
    ctx: &egui::Context,
    tex: &str,
    inline: bool,
    px: u32,
    color: Color32,
) -> Option<(TextureHandle, f32)> {
    let _ = c;
    let [r, g, b, a] = color.to_srgba_unmultiplied();
    let raster = rasterize(tex, !inline, px as f32, [r, g, b, a])?;
    let img = ColorImage::from_rgba_unmultiplied(
        [raster.width as usize, raster.height as usize],
        &raster.rgba,
    );
    let handle = ctx.load_texture(format!("math:{tex}"), img, TextureOptions::LINEAR);
    Some((handle, raster.baseline as f32))
}

thread_local! {
    static KATEX: KatexContext = KatexContext::default();
    static INK: RefCell<HashMap<(String, bool, u32, [u8; 4]), Option<Arc<Raster>>>> =
        RefCell::new(HashMap::new());
}

/// KaTeX layout + raster at `px` pixels per em; `None` if the TeX is invalid.
fn rasterize(tex: &str, display: bool, px: f32, rgba: [u8; 4]) -> Option<Raster> {
    let layout = KATEX.with(|k| katex_native::layout(k, tex, display)).ok()?;
    let [r, g, b, a] = rgba;
    let raster = katex_native::raster::rasterize(&layout, f64::from(px), katex_native::Color { r, g, b, a })?;
    (raster.width > 0 && raster.height > 0).then_some(raster)
}

/// Cached formula bitmap (straight RGBA) for painting outside egui.
pub fn ink(tex: &str, display: bool, px: f32, rgb: [u8; 3]) -> Option<Arc<Raster>> {
    let key = (tex.to_string(), display, px.to_bits(), [rgb[0], rgb[1], rgb[2], 255]);
    INK.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() > 256 {
            c.clear();
        }
        c.entry(key)
            .or_insert_with(|| rasterize(tex, display, px, [rgb[0], rgb[1], rgb[2], 255]).map(Arc::new))
            .clone()
    })
}

/// A run of plain text or a formula (`display` for `$$..$$` / `\[..\]`).
#[derive(Debug, PartialEq)]
pub enum Piece {
    Text(String),
    Math { tex: String, display: bool },
}

/// Splits a short label into text and math runs. A single `$` only opens
/// math when the next char isn't a space and a closing `$` follows that isn't
/// preceded by one, so "$5 and $10" stays text.
pub fn split(s: &str) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut rest = s;
    'outer: while !rest.is_empty() {
        for (open, close, display) in [("$$", "$$", true), (r"\[", r"\]", true), (r"\(", r"\)", false), ("$", "$", false)] {
            if let Some(after) = rest.strip_prefix(open) {
                if let Some(end) = after.find(close) {
                    let tex = &after[..end];
                    let ok = !tex.trim().is_empty()
                        && (open != "$" || (!tex.starts_with(' ') && !tex.ends_with(' ')));
                    if ok {
                        if !text.is_empty() {
                            out.push(Piece::Text(std::mem::take(&mut text)));
                        }
                        out.push(Piece::Math { tex: tex.trim().to_string(), display });
                        rest = &after[end + close.len()..];
                        continue 'outer;
                    }
                }
            }
        }
        let ch = rest.chars().next().unwrap();
        text.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    if !text.is_empty() {
        out.push(Piece::Text(text));
    }
    out
}

/// Rewrites `\(..\)` and `\[..\]` to the `$..$` / `$$..$$` the Markdown
/// parser understands, leaving code spans and fenced blocks alone.
pub fn normalize(md: &str) -> String {
    if !md.contains("\\(") && !md.contains("\\[") {
        return md.to_string();
    }
    let mut out = String::with_capacity(md.len());
    let mut fenced = false;
    for line in md.split_inclusive('\n') {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            fenced = !fenced;
            out.push_str(line);
            continue;
        }
        if fenced {
            out.push_str(line);
            continue;
        }
        let mut in_code = false;
        let mut chars = line.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '`' {
                in_code = !in_code;
                out.push(ch);
                continue;
            }
            if ch == '\\' && !in_code {
                match chars.peek() {
                    Some('(') | Some(')') => {
                        chars.next();
                        out.push('$');
                        continue;
                    }
                    Some('[') | Some(']') => {
                        chars.next();
                        out.push_str("$$");
                        continue;
                    }
                    _ => {}
                }
            }
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_delimiters_outside_code() {
        assert_eq!(normalize(r"so \(x^2\) and \[y\]"), r"so $x^2$ and $$y$$");
        assert_eq!(normalize(r"`\(a\)` \(b\)"), r"`\(a\)` $b$");
        assert_eq!(normalize("```\n\\(a\\)\n```\n"), "```\n\\(a\\)\n```\n");
    }

    #[test]
    fn splits_labels() {
        assert_eq!(split("$5 and $10"), vec![Piece::Text("$5 and $10".into())]);
        assert_eq!(
            split(r"x = $\frac{1}{2}$!"),
            vec![
                Piece::Text("x = ".into()),
                Piece::Math { tex: r"\frac{1}{2}".into(), display: false },
                Piece::Text("!".into()),
            ]
        );
        assert_eq!(split(r"$$e^{i\pi}$$"), vec![Piece::Math { tex: r"e^{i\pi}".into(), display: true }]);
        assert!(ink(r"\sqrt{x}", false, 20.0, [255, 255, 255]).is_some());
        assert!(ink(r"\frac{", false, 20.0, [255, 255, 255]).is_none());
    }

    #[test]
    fn katex_lays_out_common_math() {
        let ctx = KatexContext::default();
        for t in [r"\frac{a}{b}", r"\int_0^1 x^2\,dx", r"\sum_{n=1}^\infty \frac1{n^2}=\frac{\pi^2}6", r"\begin{pmatrix}1&0\\0&1\end{pmatrix}"] {
            let l = katex_native::layout(&ctx, t, true).unwrap();
            let r = katex_native::raster::rasterize(&l, 24.0, katex_native::Color { r: 0, g: 0, b: 0, a: 255 });
            assert!(r.is_some_and(|r| r.width > 0), "{t}");
        }
    }

    #[test]
    fn markdown_math_renders_in_egui() {
        let ctx = egui::Context::default();
        let cache = std::rc::Rc::new(RefCell::new(MathCache::default()));
        let mut md = egui_commonmark::CommonMarkCache::default();
        let text = normalize(concat!(r"Roots: \(x = 2\) or $x=3$.", "\n\n", r"$$x=\frac{-b\pm\sqrt{b^2-4ac}}{2a}$$", "\n\nDone, costs $5."));
        for _ in 0..2 {
            let mut out = ctx.run_ui(Default::default(), |ui| {
                let c = cache.clone();
                let f = move |ui: &mut Ui, tex: &str, inline: bool| show(ui, &c, tex, inline);
                egui_commonmark::CommonMarkViewer::new().render_math_fn(Some(&f)).show(ui, &mut md, &text);
            });
            out.textures_delta.clear();
        }
        let c = cache.borrow();
        assert_eq!(c.tex.len(), 3);
        assert!(c.tex.values().all(|v| v.is_some()));
    }
}
