//! A small SVG renderer, enough for icon themes (Breeze, Adwaita, Papirus,
//! Yaru), used when the system has no `rsvg-convert`. It draws paths and
//! the basic shapes, filled and stroked, with solid colours, linear and
//! radial gradients, transforms, opacity, `<use>`, and colours set by
//! class in a `<style>` sheet (`currentColor`, as KDE's icons use it).
//! Masks, clip paths, filters, patterns, text and images are skipped.
//! Coverage is computed on 16 sub-scanlines per pixel row, exactly across.

use std::collections::HashMap;

/// Renders SVG `data` to `size` x `size` BGRA pixels (alpha not
/// premultiplied), fitted to the image as `preserveAspectRatio` does by
/// default. `extra` is CSS applied after the document's own.
pub fn render(data: &[u8], size: usize, extra: &str) -> Option<Vec<[u8; 4]>> {
    let text = std::str::from_utf8(data).ok()?;
    let root = parse(text)?;
    let mut ids = HashMap::new();
    let mut css = String::new();
    collect(&root, &mut ids, &mut css);
    css.push_str(extra);
    let sheet = sheet(&css);
    let num = |k: &str| root.attr(k).and_then(length);
    let view: Vec<f32> = root.attr("viewBox").map(numbers).unwrap_or_default();
    let (vx, vy, vw, vh) = match view[..] {
        [x, y, w, h, ..] if w > 0.0 && h > 0.0 => (x, y, w, h),
        _ => (0.0, 0.0, num("width").unwrap_or(size as f32), num("height").unwrap_or(size as f32)),
    };
    let s = size as f32 / vw.max(vh);
    let m = M([s, 0.0, 0.0, s, (size as f32 - vw * s) / 2.0 - vx * s, (size as f32 - vh * s) / 2.0 - vy * s]);
    let mut cx = Ctx { ids, sheet, size, px: vec![[0.0; 4]; size * size], view: vw, depth: 0 };
    let style = Style::default();
    // The root's own presentation attributes and transform apply too.
    let p = cx.props(&root);
    let style = style.with(&p);
    let m = m.mul(transform(p.get("transform").map_or("", String::as_str)));
    for kid in &root.kids {
        cx.draw(kid, m, &style, opacity(&p));
    }
    Some(
        cx.px
            .iter()
            .map(|&[r, g, b, a]| {
                if a <= 0.0 {
                    return [0; 4];
                }
                let c = |v: f32| ((v / a).clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                [c(b), c(g), c(r), (a.clamp(0.0, 1.0) * 255.0 + 0.5) as u8]
            })
            .collect(),
    )
}

// ---- XML ------------------------------------------------------------------------

struct Node {
    name: String,
    attrs: Vec<(String, String)>,
    kids: Vec<Node>,
    text: String,
}

impl Node {
    fn new(name: &str) -> Node {
        Node { name: name.to_string(), attrs: Vec::new(), kids: Vec::new(), text: String::new() }
    }

    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    fn href(&self) -> Option<&str> {
        self.attr("href").or_else(|| self.attr("xlink:href")).and_then(|h| h.strip_prefix('#'))
    }
}

/// Element names without their namespace prefix ("svg:path" is "path").
fn local(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

/// The `<svg>` element of a document, as a tree.
fn parse(src: &str) -> Option<Node> {
    let mut stack = vec![Node::new("")];
    let mut i = 0;
    while i < src.len() {
        let rest = &src[i..];
        if !rest.starts_with('<') {
            let end = rest.find('<').unwrap_or(rest.len());
            stack.last_mut()?.text.push_str(&rest[..end]);
            i += end;
            continue;
        }
        if rest.starts_with("<!--") {
            i += rest.find("-->")? + 3;
        } else if let Some(cdata) = rest.strip_prefix("<![CDATA[") {
            let end = cdata.find("]]>")?;
            stack.last_mut()?.text.push_str(&cdata[..end]);
            i += 9 + end + 3;
        } else if rest.starts_with("<!") || rest.starts_with("<?") {
            // A DOCTYPE may carry an internal subset in brackets.
            let head = &rest[..rest.find('>')?];
            i += if head.contains('[') { rest.find("]>")? + 2 } else { head.len() + 1 };
        } else if rest.starts_with("</") {
            i += rest.find('>')? + 1;
            if stack.len() > 1 {
                let node = stack.pop()?;
                stack.last_mut()?.kids.push(node);
            }
        } else {
            let b = rest.as_bytes();
            let mut j = 1;
            while j < b.len() && !b[j].is_ascii_whitespace() && b[j] != b'/' && b[j] != b'>' {
                j += 1;
            }
            let mut node = Node::new(&rest[1..j]);
            let closed = loop {
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                match b.get(j)? {
                    b'/' => {
                        j += 1 + rest[j..].find('>')?;
                        break true;
                    }
                    b'>' => {
                        j += 1;
                        break false;
                    }
                    _ => {}
                }
                let start = j;
                while j < b.len() && b[j] != b'=' && !b[j].is_ascii_whitespace() && b[j] != b'>' && b[j] != b'/' {
                    j += 1;
                }
                let key = &rest[start..j];
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                if b.get(j) != Some(&b'=') {
                    if j == start {
                        j += 1; // a stray character
                    }
                    continue;
                }
                j += 1;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                let quote = *b.get(j)?;
                if quote != b'"' && quote != b'\'' {
                    return None;
                }
                let end = j + 1 + rest[j + 1..].find(quote as char)?;
                node.attrs.push((key.to_string(), unescape(&rest[j + 1..end])));
                j = end + 1;
            };
            i += j;
            if closed {
                stack.last_mut()?.kids.push(node);
            } else {
                stack.push(node);
            }
        }
    }
    while stack.len() > 1 {
        let node = stack.pop()?;
        stack.last_mut()?.kids.push(node);
    }
    stack.pop()?.kids.into_iter().find(|n| local(&n.name) == "svg")
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&quot;", "\"").replace("&apos;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

/// Elements by id, and the text of every `<style>`.
fn collect<'a>(node: &'a Node, ids: &mut HashMap<String, &'a Node>, css: &mut String) {
    if let Some(id) = node.attr("id") {
        ids.entry(id.to_string()).or_insert(node);
    }
    if local(&node.name) == "style" {
        css.push_str(&node.text);
        css.push('\n');
    }
    for kid in &node.kids {
        collect(kid, ids, css);
    }
}

// ---- style ----------------------------------------------------------------------

/// CSS rules with one simple selector each: `tag`, `.class`, `#id`,
/// `tag.class` or `*`. Others (descendants, pseudo-classes) are dropped.
struct Rule {
    tag: Option<String>,
    class: Option<String>,
    id: Option<String>,
    decls: Vec<(String, String)>,
}

fn sheet(css: &str) -> Vec<Rule> {
    let mut css = css.to_string();
    while let Some(start) = css.find("/*") {
        let end = css[start..].find("*/").map_or(css.len(), |e| start + e + 2);
        css.replace_range(start..end, "");
    }
    let mut rules = Vec::new();
    for block in css.split('}') {
        let Some((selectors, body)) = block.split_once('{') else { continue };
        let decls = declarations(body);
        for sel in selectors.split(',').map(str::trim) {
            if sel.is_empty() || sel.contains([' ', '>', '+', '~', ':', '[']) {
                continue;
            }
            let (rest, id) = match sel.split_once('#') {
                Some((r, id)) => (r, Some(id.to_string())),
                None => (sel, None),
            };
            let (tag, class) = match rest.split_once('.') {
                Some((t, c)) => (t, Some(c.to_string())),
                None => (rest, None),
            };
            let tag = (!tag.is_empty() && tag != "*").then(|| tag.to_string());
            rules.push(Rule { tag, class, id, decls: decls.clone() });
        }
    }
    rules
}

fn declarations(body: &str) -> Vec<(String, String)> {
    body.split(';')
        .filter_map(|d| d.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().trim_end_matches("!important").trim().to_string()))
        .filter(|(k, v)| !k.is_empty() && !v.is_empty())
        .collect()
}

#[derive(Clone)]
enum Paint {
    None,
    Color([f32; 4]),
    Current,
    Url(String),
}

/// The inherited drawing properties.
#[derive(Clone)]
struct Style {
    fill: Paint,
    fill_opacity: f32,
    evenodd: bool,
    stroke: Paint,
    stroke_opacity: f32,
    stroke_width: f32,
    round_cap: bool,
    color: [f32; 4],
}

impl Default for Style {
    fn default() -> Style {
        Style {
            fill: Paint::Color([0.0, 0.0, 0.0, 1.0]),
            fill_opacity: 1.0,
            evenodd: false,
            stroke: Paint::None,
            stroke_opacity: 1.0,
            stroke_width: 1.0,
            round_cap: false,
            color: [0.0, 0.0, 0.0, 1.0],
        }
    }
}

impl Style {
    fn with(&self, p: &HashMap<String, String>) -> Style {
        let mut s = self.clone();
        let get = |k: &str| p.get(k).map(String::as_str).filter(|v| *v != "inherit");
        if let Some(c) = get("color").and_then(color) {
            s.color = c;
        }
        if let Some(v) = get("fill").and_then(paint) {
            s.fill = v;
        }
        if let Some(v) = get("stroke").and_then(paint) {
            s.stroke = v;
        }
        if let Some(v) = get("fill-opacity").and_then(fraction) {
            s.fill_opacity = v;
        }
        if let Some(v) = get("stroke-opacity").and_then(fraction) {
            s.stroke_opacity = v;
        }
        if let Some(v) = get("stroke-width").and_then(length) {
            s.stroke_width = v;
        }
        if let Some(v) = get("fill-rule") {
            s.evenodd = v == "evenodd";
        }
        if let Some(v) = get("stroke-linecap") {
            s.round_cap = v == "round";
        }
        s
    }
}

fn opacity(p: &HashMap<String, String>) -> f32 {
    p.get("opacity").and_then(|v| fraction(v)).unwrap_or(1.0)
}

/// A number, or a percentage as a fraction, clamped to 0..1.
fn fraction(v: &str) -> Option<f32> {
    let v = v.trim();
    let n = match v.strip_suffix('%') {
        Some(p) => p.trim().parse::<f32>().ok()? / 100.0,
        None => v.parse::<f32>().ok()?,
    };
    Some(n.clamp(0.0, 1.0))
}

/// A length in user units ("3", "3px", "3.5e1").
fn length(v: &str) -> Option<f32> {
    let v = v.trim();
    let v = v.strip_suffix("px").unwrap_or(v);
    v.trim().parse().ok()
}

fn paint(v: &str) -> Option<Paint> {
    let v = v.trim();
    if v == "none" || v == "transparent" {
        return Some(Paint::None);
    }
    if v.eq_ignore_ascii_case("currentColor") {
        return Some(Paint::Current);
    }
    if let Some(url) = v.strip_prefix("url(") {
        let id = url.split(')').next()?.trim().trim_matches(['"', '\'']).trim_start_matches('#');
        return Some(Paint::Url(id.to_string()));
    }
    color(v).map(Paint::Color)
}

fn color(v: &str) -> Option<[f32; 4]> {
    let v = v.trim().to_ascii_lowercase();
    if let Some(hex) = v.strip_prefix('#') {
        let digit = |i: usize| u8::from_str_radix(hex.get(i..i + 1)?, 16).ok();
        let pair = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
        let [r, g, b, a] = match hex.len() {
            3 | 4 => [digit(0)? * 17, digit(1)? * 17, digit(2)? * 17, if hex.len() == 4 { digit(3)? * 17 } else { 255 }],
            6 | 8 => [pair(0)?, pair(2)?, pair(4)?, if hex.len() == 8 { pair(6)? } else { 255 }],
            _ => return None,
        };
        return Some([r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, a as f32 / 255.0]);
    }
    if let Some(args) = v.strip_prefix("rgba(").or_else(|| v.strip_prefix("rgb(")) {
        let parts: Vec<&str> = args.trim_end_matches(')').split([',', ' ', '/']).filter(|p| !p.is_empty()).collect();
        let channel = |p: &str| match p.strip_suffix('%') {
            Some(p) => p.parse::<f32>().ok().map(|n| n / 100.0),
            None => p.parse::<f32>().ok().map(|n| n / 255.0),
        };
        let alpha = parts.get(3).and_then(|a| fraction(a)).unwrap_or(1.0);
        return Some([channel(parts.first()?)?, channel(parts.get(1)?)?, channel(parts.get(2)?)?, alpha]);
    }
    let rgb: u32 = match v.as_str() {
        "black" => 0x000000,
        "white" => 0xffffff,
        "red" => 0xff0000,
        "lime" => 0x00ff00,
        "green" => 0x008000,
        "blue" => 0x0000ff,
        "yellow" => 0xffff00,
        "cyan" | "aqua" => 0x00ffff,
        "magenta" | "fuchsia" => 0xff00ff,
        "gray" | "grey" => 0x808080,
        "silver" => 0xc0c0c0,
        "maroon" => 0x800000,
        "olive" => 0x808000,
        "navy" => 0x000080,
        "purple" => 0x800080,
        "teal" => 0x008080,
        "orange" => 0xffa500,
        _ => return None,
    };
    let c = |s: u32| ((rgb >> s) & 0xff) as f32 / 255.0;
    Some([c(16), c(8), c(0), 1.0])
}

// ---- geometry -------------------------------------------------------------------

/// An affine transform [a b c d e f]: x' = a x + c y + e, y' = b x + d y + f.
#[derive(Clone, Copy)]
struct M([f32; 6]);

impl M {
    const ID: M = M([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

    /// `self` after `o`: o applies first.
    fn mul(self, o: M) -> M {
        let [a, b, c, d, e, f] = self.0;
        let [p, q, r, s, t, u] = o.0;
        M([a * p + c * q, b * p + d * q, a * r + c * s, b * r + d * s, a * t + c * u + e, b * t + d * u + f])
    }

    fn apply(self, (x, y): (f32, f32)) -> (f32, f32) {
        let [a, b, c, d, e, f] = self.0;
        (a * x + c * y + e, b * x + d * y + f)
    }

    fn inverse(self) -> Option<M> {
        let [a, b, c, d, e, f] = self.0;
        let det = a * d - b * c;
        if det.abs() < 1e-12 {
            return None;
        }
        let (ia, ib, ic, id) = (d / det, -b / det, -c / det, a / det);
        Some(M([ia, ib, ic, id, -(ia * e + ic * f), -(ib * e + id * f)]))
    }

    /// How much the transform scales lengths, on average.
    fn scale(self) -> f32 {
        let [a, b, c, d, ..] = self.0;
        (a * d - b * c).abs().sqrt()
    }
}

/// A `transform` attribute: its functions, composed left to right.
fn transform(v: &str) -> M {
    let mut m = M::ID;
    let mut rest = v;
    while let Some(open) = rest.find('(') {
        let name = rest[..open].trim().trim_start_matches(',').trim();
        let Some(close) = rest[open..].find(')') else { break };
        let a = numbers(&rest[open + 1..open + close]);
        let arg = |i: usize, d: f32| a.get(i).copied().unwrap_or(d);
        let t = match name {
            "matrix" if a.len() >= 6 => M([a[0], a[1], a[2], a[3], a[4], a[5]]),
            "translate" => M([1.0, 0.0, 0.0, 1.0, arg(0, 0.0), arg(1, 0.0)]),
            "scale" => M([arg(0, 1.0), 0.0, 0.0, arg(1, arg(0, 1.0)), 0.0, 0.0]),
            "rotate" => {
                let (s, c) = arg(0, 0.0).to_radians().sin_cos();
                let (x, y) = (arg(1, 0.0), arg(2, 0.0));
                M([1.0, 0.0, 0.0, 1.0, x, y]).mul(M([c, s, -s, c, 0.0, 0.0])).mul(M([1.0, 0.0, 0.0, 1.0, -x, -y]))
            }
            "skewX" => M([1.0, 0.0, arg(0, 0.0).to_radians().tan(), 1.0, 0.0, 0.0]),
            "skewY" => M([1.0, arg(0, 0.0).to_radians().tan(), 0.0, 1.0, 0.0, 0.0]),
            _ => M::ID,
        };
        m = m.mul(t);
        rest = &rest[open + close + 1..];
    }
    m
}

/// Numbers in a list ("1,2 -3.5e2.5").
fn numbers(v: &str) -> Vec<f32> {
    let mut lex = Lex { s: v.as_bytes(), i: 0 };
    std::iter::from_fn(|| lex.num()).collect()
}

struct Lex<'a> {
    s: &'a [u8],
    i: usize,
}

impl Lex<'_> {
    fn skip(&mut self) {
        while self.i < self.s.len() && (self.s[self.i].is_ascii_whitespace() || self.s[self.i] == b',') {
            self.i += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip();
        self.s.get(self.i).copied()
    }

    fn num(&mut self) -> Option<f32> {
        self.skip();
        let start = self.i;
        let s = self.s;
        let mut i = self.i;
        if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
            i += 1;
        }
        let digits = |i: &mut usize| {
            let from = *i;
            while *i < s.len() && s[*i].is_ascii_digit() {
                *i += 1;
            }
            *i > from
        };
        let mut any = digits(&mut i);
        if i < s.len() && s[i] == b'.' {
            i += 1;
            any |= digits(&mut i);
        }
        if !any {
            return None;
        }
        if i < s.len() && (s[i] == b'e' || s[i] == b'E') {
            let mut j = i + 1;
            if j < s.len() && (s[j] == b'+' || s[j] == b'-') {
                j += 1;
            }
            if digits(&mut j) {
                i = j;
            }
        }
        self.i = i;
        std::str::from_utf8(&s[start..i]).ok()?.parse().ok()
    }

    /// An arc's flag: a single 0 or 1, which may run into what follows.
    fn flag(&mut self) -> Option<bool> {
        let c = self.peek()?;
        self.i += 1;
        match c {
            b'0' => Some(false),
            b'1' => Some(true),
            _ => None,
        }
    }
}

/// A path in user units: subpaths of lines and cubic curves.
#[derive(Default)]
struct Path {
    subs: Vec<Sub>,
}

#[derive(Default)]
struct Sub {
    start: (f32, f32),
    /// Each a cubic curve: two control points and an end point (a line
    /// has its ends as controls).
    segs: Vec<[(f32, f32); 3]>,
    closed: bool,
}

impl Path {
    fn move_to(&mut self, p: (f32, f32)) {
        self.subs.push(Sub { start: p, ..Sub::default() });
    }

    /// The current point: after a close, where the subpath started (and a
    /// new one starts there).
    fn current(&self) -> (f32, f32) {
        self.subs.last().map_or((0.0, 0.0), |s| if s.closed { s.start } else { s.segs.last().map_or(s.start, |seg| seg[2]) })
    }

    fn line(&mut self, p: (f32, f32)) {
        let from = self.current();
        self.cubic(from, p, p);
    }

    fn cubic(&mut self, c1: (f32, f32), c2: (f32, f32), p: (f32, f32)) {
        if self.subs.last().is_none_or(|s| s.closed) {
            let at = self.current();
            self.move_to(at);
        }
        if let Some(sub) = self.subs.last_mut() {
            sub.segs.push([c1, c2, p]);
        }
    }

    fn close(&mut self) {
        if let Some(sub) = self.subs.last_mut() {
            sub.closed = true;
        }
    }

    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, rx: f32, ry: f32) {
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let (rx, ry) = (rx.min(w / 2.0), ry.min(h / 2.0));
        if rx <= 0.0 || ry <= 0.0 {
            self.move_to((x, y));
            self.line((x + w, y));
            self.line((x + w, y + h));
            self.line((x, y + h));
        } else {
            let (kx, ky) = (rx * KAPPA, ry * KAPPA);
            self.move_to((x + rx, y));
            self.line((x + w - rx, y));
            self.cubic((x + w - rx + kx, y), (x + w, y + ry - ky), (x + w, y + ry));
            self.line((x + w, y + h - ry));
            self.cubic((x + w, y + h - ry + ky), (x + w - rx + kx, y + h), (x + w - rx, y + h));
            self.line((x + rx, y + h));
            self.cubic((x + rx - kx, y + h), (x, y + h - ry + ky), (x, y + h - ry));
            self.line((x, y + ry));
            self.cubic((x, y + ry - ky), (x + rx - kx, y), (x + rx, y));
        }
        self.close();
    }

    fn ellipse(&mut self, cx: f32, cy: f32, rx: f32, ry: f32) {
        if rx <= 0.0 || ry <= 0.0 {
            return;
        }
        let (kx, ky) = (rx * KAPPA, ry * KAPPA);
        self.move_to((cx + rx, cy));
        self.cubic((cx + rx, cy + ky), (cx + kx, cy + ry), (cx, cy + ry));
        self.cubic((cx - kx, cy + ry), (cx - rx, cy + ky), (cx - rx, cy));
        self.cubic((cx - rx, cy - ky), (cx - kx, cy - ry), (cx, cy - ry));
        self.cubic((cx + kx, cy - ry), (cx + rx, cy - ky), (cx + rx, cy));
        self.close();
    }

    /// An elliptical arc to `p`, as cubic curves (SVG 1.1, appendix F.6).
    #[allow(clippy::too_many_arguments)]
    fn arc(&mut self, rx: f32, ry: f32, angle: f32, large: bool, sweep: bool, p: (f32, f32)) {
        let p0 = self.current();
        let (mut rx, mut ry) = (rx.abs(), ry.abs());
        if rx == 0.0 || ry == 0.0 || p0 == p {
            self.line(p);
            return;
        }
        let (sin, cos) = angle.to_radians().sin_cos();
        let (dx, dy) = ((p0.0 - p.0) / 2.0, (p0.1 - p.1) / 2.0);
        let (x1, y1) = (cos * dx + sin * dy, -sin * dx + cos * dy);
        let l = x1 * x1 / (rx * rx) + y1 * y1 / (ry * ry);
        if l > 1.0 {
            rx *= l.sqrt();
            ry *= l.sqrt();
        }
        let num = rx * rx * ry * ry - rx * rx * y1 * y1 - ry * ry * x1 * x1;
        let den = rx * rx * y1 * y1 + ry * ry * x1 * x1;
        let mut k = (num / den).max(0.0).sqrt();
        if large == sweep {
            k = -k;
        }
        let (cx1, cy1) = (k * rx * y1 / ry, -k * ry * x1 / rx);
        let (cx, cy) = (cos * cx1 - sin * cy1 + (p0.0 + p.0) / 2.0, sin * cx1 + cos * cy1 + (p0.1 + p.1) / 2.0);
        let angle_of = |ux: f32, uy: f32| uy.atan2(ux);
        let t1 = angle_of((x1 - cx1) / rx, (y1 - cy1) / ry);
        let mut dt = angle_of((-x1 - cx1) / rx, (-y1 - cy1) / ry) - t1;
        let tau = std::f32::consts::TAU;
        if sweep && dt < 0.0 {
            dt += tau;
        } else if !sweep && dt > 0.0 {
            dt -= tau;
        }
        let n = (dt.abs() / (tau / 4.0)).ceil().max(1.0) as usize;
        let step = dt / n as f32;
        let k = 4.0 / 3.0 * (step / 4.0).tan();
        let point = |t: f32| {
            let (s, c) = t.sin_cos();
            (cx + rx * c * cos - ry * s * sin, cy + rx * c * sin + ry * s * cos)
        };
        let deriv = |t: f32| {
            let (s, c) = t.sin_cos();
            (-rx * s * cos - ry * c * sin, -rx * s * sin + ry * c * cos)
        };
        for i in 0..n {
            let (a, b) = (t1 + step * i as f32, t1 + step * (i + 1) as f32);
            let (pa, pb, da, db) = (point(a), point(b), deriv(a), deriv(b));
            let end = if i + 1 == n { p } else { pb };
            self.cubic((pa.0 + k * da.0, pa.1 + k * da.1), (pb.0 - k * db.0, pb.1 - k * db.1), end);
        }
    }

    /// Path data (the `d` attribute).
    fn data(d: &str) -> Path {
        let mut path = Path::default();
        let mut lex = Lex { s: d.as_bytes(), i: 0 };
        let mut cmd = 0u8;
        // The last control point, for the smooth curves S and T.
        let mut last_cubic: Option<(f32, f32)> = None;
        let mut last_quad: Option<(f32, f32)> = None;
        while let Some(c) = lex.peek() {
            if c.is_ascii_alphabetic() {
                cmd = c;
                lex.i += 1;
            } else if cmd == 0 || cmd.eq_ignore_ascii_case(&b'z') {
                break;
            }
            let rel = cmd.is_ascii_lowercase();
            let cur = path.current();
            let pt = |lex: &mut Lex| -> Option<(f32, f32)> {
                let (x, y) = (lex.num()?, lex.num()?);
                Some(if rel { (cur.0 + x, cur.1 + y) } else { (x, y) })
            };
            let (mut cubic_ctrl, mut quad_ctrl) = (None, None);
            let ok = (|| -> Option<()> {
                match cmd.to_ascii_uppercase() {
                    b'M' => {
                        let p = pt(&mut lex)?;
                        path.move_to(p);
                        // Further pairs are lines.
                        cmd = if rel { b'l' } else { b'L' };
                    }
                    b'L' => path.line(pt(&mut lex)?),
                    b'H' => {
                        let x = lex.num()?;
                        path.line((if rel { cur.0 + x } else { x }, cur.1));
                    }
                    b'V' => {
                        let y = lex.num()?;
                        path.line((cur.0, if rel { cur.1 + y } else { y }));
                    }
                    b'C' => {
                        let (c1, c2, p) = (pt(&mut lex)?, pt(&mut lex)?, pt(&mut lex)?);
                        path.cubic(c1, c2, p);
                        cubic_ctrl = Some(c2);
                    }
                    b'S' => {
                        let c1 = last_cubic.map_or(cur, |c| (2.0 * cur.0 - c.0, 2.0 * cur.1 - c.1));
                        let (c2, p) = (pt(&mut lex)?, pt(&mut lex)?);
                        path.cubic(c1, c2, p);
                        cubic_ctrl = Some(c2);
                    }
                    b'Q' | b'T' => {
                        let q = if cmd.eq_ignore_ascii_case(&b'q') {
                            pt(&mut lex)?
                        } else {
                            last_quad.map_or(cur, |c| (2.0 * cur.0 - c.0, 2.0 * cur.1 - c.1))
                        };
                        let p = pt(&mut lex)?;
                        let c1 = (cur.0 + 2.0 / 3.0 * (q.0 - cur.0), cur.1 + 2.0 / 3.0 * (q.1 - cur.1));
                        let c2 = (p.0 + 2.0 / 3.0 * (q.0 - p.0), p.1 + 2.0 / 3.0 * (q.1 - p.1));
                        path.cubic(c1, c2, p);
                        quad_ctrl = Some(q);
                    }
                    b'A' => {
                        let (rx, ry, angle) = (lex.num()?, lex.num()?, lex.num()?);
                        let (large, sweep) = (lex.flag()?, lex.flag()?);
                        let p = pt(&mut lex)?;
                        path.arc(rx, ry, angle, large, sweep, p);
                    }
                    b'Z' => path.close(),
                    _ => return None,
                }
                Some(())
            })();
            if ok.is_none() {
                break;
            }
            last_cubic = cubic_ctrl;
            last_quad = quad_ctrl;
        }
        path
    }

    /// The box around the points (control points included), for gradients
    /// in `objectBoundingBox` units.
    fn bounds(&self) -> (f32, f32, f32, f32) {
        let mut b = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for sub in &self.subs {
            for p in std::iter::once(sub.start).chain(sub.segs.iter().flatten().copied()) {
                b = (b.0.min(p.0), b.1.min(p.1), b.2.max(p.0), b.3.max(p.1));
            }
        }
        b
    }

    /// The subpaths in device pixels, as polylines with their closed flag.
    fn flatten(&self, m: M) -> Vec<(Vec<(f32, f32)>, bool)> {
        let mut out = Vec::new();
        for sub in &self.subs {
            let mut pts = vec![m.apply(sub.start)];
            for seg in &sub.segs {
                let p0 = *pts.last().unwrap_or(&(0.0, 0.0));
                let [c1, c2, p] = seg.map(|q| m.apply(q));
                let line = |a: (f32, f32), b: (f32, f32)| ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt();
                let len = line(p0, c1) + line(c1, c2) + line(c2, p);
                let straight = line(p0, p);
                let n = if len - straight < 0.01 { 1 } else { ((len / 1.5).ceil() as usize).clamp(2, 64) };
                for i in 1..=n {
                    let t = i as f32 / n as f32;
                    let u = 1.0 - t;
                    let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
                    pts.push((a * p0.0 + b * c1.0 + c * c2.0 + d * p.0, a * p0.1 + b * c1.1 + c * c2.1 + d * p.1));
                }
            }
            if pts.len() > 1 {
                out.push((pts, sub.closed));
            }
        }
        out
    }
}

/// Control distance of a quarter circle drawn as one cubic curve.
const KAPPA: f32 = 0.552_284_8;

// ---- drawing --------------------------------------------------------------------

struct Ctx<'a> {
    ids: HashMap<String, &'a Node>,
    sheet: Vec<Rule>,
    size: usize,
    /// Premultiplied RGBA.
    px: Vec<[f32; 4]>,
    /// The viewBox width, for percentages in user units.
    view: f32,
    depth: usize,
}

/// Elements never drawn where they stand.
const SKIP: &[&str] = &[
    "defs", "clipPath", "mask", "symbol", "linearGradient", "radialGradient", "pattern", "filter", "style", "metadata", "title",
    "desc", "text", "image", "marker", "script", "namedview",
];

impl Ctx<'_> {
    /// An element's properties: presentation attributes, then style sheet
    /// rules, then its `style` attribute.
    fn props(&self, node: &Node) -> HashMap<String, String> {
        let mut p: HashMap<String, String> = node.attrs.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let classes: Vec<&str> = node.attr("class").map(|c| c.split_whitespace().collect()).unwrap_or_default();
        let tag = local(&node.name);
        for rule in &self.sheet {
            let matches = rule.tag.as_deref().is_none_or(|t| t == tag)
                && rule.class.as_deref().is_none_or(|c| classes.contains(&c))
                && rule.id.as_deref().is_none_or(|i| node.attr("id") == Some(i));
            if matches {
                p.extend(rule.decls.iter().cloned());
            }
        }
        if let Some(style) = node.attr("style") {
            p.extend(declarations(style));
        }
        p
    }

    fn draw(&mut self, node: &Node, m: M, parent: &Style, alpha: f32) {
        let tag = local(&node.name);
        if SKIP.contains(&tag) || self.depth > 24 {
            return;
        }
        let p = self.props(node);
        if p.get("display").is_some_and(|d| d == "none") || p.get("visibility").is_some_and(|v| v == "hidden" || v == "collapse") {
            return;
        }
        let style = parent.with(&p);
        let alpha = alpha * opacity(&p);
        let m = m.mul(transform(p.get("transform").map_or("", String::as_str)));
        let num = |k: &str| p.get(k).and_then(|v| self.length(v)).unwrap_or(0.0);
        let mut path = Path::default();
        match tag {
            "svg" | "g" | "a" | "switch" => {
                let m = if tag == "svg" { m.mul(M([1.0, 0.0, 0.0, 1.0, num("x"), num("y")])) } else { m };
                self.depth += 1;
                for kid in &node.kids {
                    self.draw(kid, m, &style, alpha);
                }
                self.depth -= 1;
                return;
            }
            "use" => {
                let Some(target) = node.href().and_then(|id| self.ids.get(id).copied()) else { return };
                let m = m.mul(M([1.0, 0.0, 0.0, 1.0, num("x"), num("y")]));
                self.depth += 1;
                if local(&target.name) == "symbol" {
                    for kid in &target.kids {
                        self.draw(kid, m, &style, alpha);
                    }
                } else {
                    self.draw(target, m, &style, alpha);
                }
                self.depth -= 1;
                return;
            }
            "path" => path = Path::data(p.get("d").map_or("", String::as_str)),
            "rect" => {
                let (rx, ry) = (p.get("rx").and_then(|v| self.length(v)), p.get("ry").and_then(|v| self.length(v)));
                let (rx, ry) = (rx.or(ry).unwrap_or(0.0), ry.or(rx).unwrap_or(0.0));
                path.rect(num("x"), num("y"), num("width"), num("height"), rx, ry);
            }
            "circle" => path.ellipse(num("cx"), num("cy"), num("r"), num("r")),
            "ellipse" => path.ellipse(num("cx"), num("cy"), num("rx"), num("ry")),
            "line" => {
                path.move_to((num("x1"), num("y1")));
                path.line((num("x2"), num("y2")));
            }
            "polyline" | "polygon" => {
                let pts = numbers(p.get("points").map_or("", String::as_str));
                for (i, xy) in pts.chunks_exact(2).enumerate() {
                    if i == 0 {
                        path.move_to((xy[0], xy[1]));
                    } else {
                        path.line((xy[0], xy[1]));
                    }
                }
                if tag == "polygon" {
                    path.close();
                }
            }
            _ => return,
        }
        if path.subs.is_empty() {
            return;
        }
        let lines = path.flatten(m);
        if tag != "line" && !matches!(style.fill, Paint::None) {
            let polys: Vec<Vec<(f32, f32)>> = lines.iter().map(|(pts, _)| pts.clone()).collect();
            let cov = self.coverage(&polys, style.evenodd);
            self.fill(&cov, &style.fill, &style, style.fill_opacity * alpha, &path, m);
        }
        if !matches!(style.stroke, Paint::None) && style.stroke_width > 0.0 {
            let polys = stroke(&lines, style.stroke_width * m.scale() / 2.0, style.round_cap);
            let cov = self.coverage(&polys, false);
            self.fill(&cov, &style.stroke, &style, style.stroke_opacity * alpha, &path, m);
        }
    }

    fn length(&self, v: &str) -> Option<f32> {
        match v.trim().strip_suffix('%') {
            Some(p) => p.trim().parse::<f32>().ok().map(|n| n / 100.0 * self.view),
            None => length(v),
        }
    }

    /// How much of each pixel the polygons cover, by the fill rule.
    fn coverage(&self, polys: &[Vec<(f32, f32)>], evenodd: bool) -> Vec<f32> {
        const SUB: usize = 16;
        let n = self.size;
        let mut cov = vec![0.0f32; n * n];
        let mut edges = Vec::new();
        for poly in polys {
            for (i, &a) in poly.iter().enumerate() {
                let b = poly[(i + 1) % poly.len()];
                if a.1 != b.1 {
                    edges.push((a, b));
                }
            }
        }
        if edges.is_empty() {
            return cov;
        }
        let top = edges.iter().map(|(a, b)| a.1.min(b.1)).fold(f32::MAX, f32::min).max(0.0) as usize;
        let bottom = (edges.iter().map(|(a, b)| a.1.max(b.1)).fold(f32::MIN, f32::max).ceil().max(0.0) as usize).min(n);
        let mut hits: Vec<(f32, i32)> = Vec::new();
        for y in top..bottom {
            let row = &mut cov[y * n..(y + 1) * n];
            for s in 0..SUB {
                let sy = y as f32 + (s as f32 + 0.5) / SUB as f32;
                hits.clear();
                for &(a, b) in &edges {
                    let (lo, hi) = if a.1 < b.1 { (a, b) } else { (b, a) };
                    if sy >= lo.1 && sy < hi.1 {
                        let x = lo.0 + (sy - lo.1) / (hi.1 - lo.1) * (hi.0 - lo.0);
                        hits.push((x, if a.1 < b.1 { 1 } else { -1 }));
                    }
                }
                hits.sort_by(|a, b| a.0.total_cmp(&b.0));
                let mut winding = 0;
                for pair in hits.windows(2) {
                    winding += pair[0].1;
                    let inside = if evenodd { winding % 2 != 0 } else { winding != 0 };
                    if inside {
                        span(row, pair[0].0, pair[1].0, 1.0 / SUB as f32);
                    }
                }
            }
        }
        cov
    }

    /// Composites `paint` over the image where `cov` says.
    #[allow(clippy::too_many_arguments)]
    fn fill(&mut self, cov: &[f32], paint: &Paint, style: &Style, alpha: f32, path: &Path, m: M) {
        let shade: Box<dyn Fn(f32, f32) -> [f32; 4]> = match paint {
            Paint::None => return,
            Paint::Color(c) => {
                let c = *c;
                Box::new(move |_, _| c)
            }
            Paint::Current => {
                let c = style.color;
                Box::new(move |_, _| c)
            }
            Paint::Url(id) => match self.gradient(id, path, m) {
                Some(g) => Box::new(move |x, y| g.at(x, y)),
                None => return,
            },
        };
        let n = self.size;
        for (i, &c) in cov.iter().enumerate() {
            if c <= 0.0 {
                continue;
            }
            let [r, g, b, a] = shade((i % n) as f32 + 0.5, (i / n) as f32 + 0.5);
            let a = c.min(1.0) * a * alpha;
            let d = &mut self.px[i];
            *d = [r * a + d[0] * (1.0 - a), g * a + d[1] * (1.0 - a), b * a + d[2] * (1.0 - a), a + d[3] * (1.0 - a)];
        }
    }

    /// A gradient by id, its attributes and stops followed through `href`.
    fn gradient(&self, id: &str, path: &Path, m: M) -> Option<Gradient> {
        let mut chain = Vec::new();
        let mut at = self.ids.get(id).copied();
        while let Some(node) = at {
            if chain.len() > 8 {
                break;
            }
            chain.push(node);
            at = node.href().and_then(|h| self.ids.get(h).copied());
        }
        let first = chain.first()?;
        let kind = local(&first.name);
        if kind != "linearGradient" && kind != "radialGradient" {
            return None;
        }
        let attr = |k: &str| chain.iter().find_map(|n| n.attr(k));
        let user = attr("gradientUnits") == Some("userSpaceOnUse");
        let dim = if user { self.view } else { 1.0 };
        let val = |k: &str, d: f32| {
            attr(k)
                .and_then(|v| match v.trim().strip_suffix('%') {
                    Some(p) => p.trim().parse::<f32>().ok().map(|n| n / 100.0 * dim),
                    None => length(v),
                })
                .unwrap_or(d)
        };
        let mut stops = Vec::new();
        if let Some(holder) = chain.iter().find(|n| n.kids.iter().any(|k| local(&k.name) == "stop")) {
            for stop in holder.kids.iter().filter(|k| local(&k.name) == "stop") {
                let p = self.props(stop);
                let offset = p.get("offset").and_then(|v| fraction(v)).unwrap_or(0.0);
                let mut c = p.get("stop-color").and_then(|v| color(v)).unwrap_or([0.0, 0.0, 0.0, 1.0]);
                c[3] *= p.get("stop-opacity").and_then(|v| fraction(v)).unwrap_or(1.0);
                let offset = stops.last().map_or(offset, |&(o, _): &(f32, [f32; 4])| offset.max(o));
                stops.push((offset, c));
            }
        }
        if stops.is_empty() {
            return None;
        }
        let units = if user {
            M::ID
        } else {
            let (x0, y0, x1, y1) = path.bounds();
            M([x1 - x0, 0.0, 0.0, y1 - y0, x0, y0])
        };
        let inv = m.mul(units).mul(transform(attr("gradientTransform").unwrap_or(""))).inverse()?;
        let shape = if kind == "linearGradient" {
            Shape::Linear(val("x1", 0.0), val("y1", 0.0), val("x2", if user { self.view } else { 1.0 }), val("y2", 0.0))
        } else {
            let half = if user { self.view / 2.0 } else { 0.5 };
            Shape::Radial(val("cx", half), val("cy", half), val("r", half))
        };
        let spread = match attr("spreadMethod") {
            Some("reflect") => 1,
            Some("repeat") => 2,
            _ => 0,
        };
        Some(Gradient { stops, inv, shape, spread })
    }
}

/// Adds `weight` times the part of each pixel between `x0` and `x1`.
fn span(row: &mut [f32], x0: f32, x1: f32, weight: f32) {
    let w = row.len() as f32;
    let (x0, x1) = (x0.clamp(0.0, w), x1.clamp(0.0, w));
    if x1 <= x0 {
        return;
    }
    let (i0, i1) = (x0 as usize, x1 as usize);
    if i0 == i1 {
        row[i0.min(row.len() - 1)] += (x1 - x0) * weight;
        return;
    }
    row[i0] += (i0 as f32 + 1.0 - x0) * weight;
    for v in &mut row[i0 + 1..i1] {
        *v += weight;
    }
    if i1 < row.len() {
        row[i1] += (x1 - i1 as f32) * weight;
    }
}

/// The outline of stroked polylines, `half` the width each side: a
/// quadrilateral per segment and a disc at each joint (and round caps),
/// all turning the same way so they add up as one shape.
fn stroke(lines: &[(Vec<(f32, f32)>, bool)], half: f32, round_cap: bool) -> Vec<Vec<(f32, f32)>> {
    let mut out = Vec::new();
    let disc = |c: (f32, f32)| -> Vec<(f32, f32)> {
        (0..12).map(|i| (i as f32 * std::f32::consts::TAU / 12.0).sin_cos()).map(|(s, k)| (c.0 + k * half, c.1 + s * half)).collect()
    };
    for (pts, closed) in lines {
        let mut pts = pts.clone();
        if *closed && pts.first() != pts.last() {
            pts.push(pts[0]);
        }
        for pair in pts.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            let len = (dx * dx + dy * dy).sqrt();
            if len < 1e-6 {
                continue;
            }
            let (nx, ny) = (-dy / len * half, dx / len * half);
            out.push(vec![(a.0 + nx, a.1 + ny), (b.0 + nx, b.1 + ny), (b.0 - nx, b.1 - ny), (a.0 - nx, a.1 - ny)]);
        }
        let last = pts.len() - 1;
        for (i, &p) in pts.iter().enumerate() {
            if (i > 0 && i < last) || *closed || round_cap {
                out.push(disc(p));
            }
        }
    }
    // One orientation for all, so overlaps do not cancel out.
    for poly in &mut out {
        let area: f32 = poly.iter().zip(poly.iter().cycle().skip(1)).map(|(a, b)| a.0 * b.1 - b.0 * a.1).sum();
        if area < 0.0 {
            poly.reverse();
        }
    }
    out
}

enum Shape {
    Linear(f32, f32, f32, f32),
    Radial(f32, f32, f32),
}

struct Gradient {
    stops: Vec<(f32, [f32; 4])>,
    /// From device pixels to the gradient's own space.
    inv: M,
    shape: Shape,
    /// Pad, reflect or repeat.
    spread: u8,
}

impl Gradient {
    fn at(&self, x: f32, y: f32) -> [f32; 4] {
        let (gx, gy) = self.inv.apply((x, y));
        let t = match self.shape {
            Shape::Linear(x1, y1, x2, y2) => {
                let (dx, dy) = (x2 - x1, y2 - y1);
                let len = dx * dx + dy * dy;
                if len == 0.0 { 1.0 } else { ((gx - x1) * dx + (gy - y1) * dy) / len }
            }
            Shape::Radial(cx, cy, r) => {
                if r <= 0.0 {
                    1.0
                } else {
                    ((gx - cx).powi(2) + (gy - cy).powi(2)).sqrt() / r
                }
            }
        };
        let t = match self.spread {
            1 => {
                let t = t.rem_euclid(2.0);
                if t > 1.0 { 2.0 - t } else { t }
            }
            2 => t.rem_euclid(1.0),
            _ => t.clamp(0.0, 1.0),
        };
        let stops = &self.stops;
        if t <= stops[0].0 {
            return stops[0].1;
        }
        for pair in stops.windows(2) {
            let ((o0, c0), (o1, c1)) = (pair[0], pair[1]);
            if t <= o1 {
                let f = if o1 > o0 { (t - o0) / (o1 - o0) } else { 1.0 };
                return std::array::from_fn(|k| c0[k] + (c1[k] - c0[k]) * f);
            }
        }
        stops[stops.len() - 1].1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(img: &[[u8; 4]], size: usize, x: usize, y: usize) -> [u8; 4] {
        img[y * size + x]
    }

    #[test]
    fn shapes_and_colours() {
        let svg = br##"<svg viewBox="0 0 16 16"><rect width="16" height="8" fill="#ff0000"/>
            <circle cx="8" cy="12" r="3" style="fill:rgb(0,0,255)"/></svg>"##;
        let img = render(svg, 16, "").unwrap();
        assert_eq!(px(&img, 16, 2, 2), [0, 0, 255, 255]);
        assert_eq!(px(&img, 16, 8, 12), [255, 0, 0, 255]);
        assert_eq!(px(&img, 16, 1, 14)[3], 0);
    }

    /// KDE's icons colour parts by class, through `currentColor`.
    #[test]
    fn class_colours_and_paths() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><defs><style type="text/css">
            .ColorScheme-Accent { color:#3daee9; }</style></defs>
            <path style="fill:currentColor" d="M 2 3 L 2 10 L 1 10 L 1 29 L 31 29 L 31 8 L 30 8 L 30 5 L 16 5 L 14 3 z" class="ColorScheme-Accent"/>
            <path style="fill-opacity:0.33;fill-rule:evenodd" d="m 2,3 0,7 9,0 L 13,8 30,8 30,5 16,5 14,3 2,3 Z"/></svg>"##;
        let img = render(svg, 32, "").unwrap();
        assert_eq!(px(&img, 32, 16, 20), [0xe9, 0xae, 0x3d, 255]);
        // Darkened by the second path, a third black.
        let top = px(&img, 32, 5, 6);
        assert!(top[0] < 0xe9 && top[0] > 0x80 && top[3] == 255, "{top:?}");
        assert_eq!(px(&img, 32, 0, 0)[3], 0);
    }

    #[test]
    fn gradients_transforms_and_arcs() {
        let svg = br##"<svg width="20" height="20"><linearGradient id="g" x2="0" y2="1"><stop offset="0" stop-color="#000"/>
            <stop offset="1" stop-color="#fff"/></linearGradient><linearGradient id="h" xlink:href="#g"/>
            <g transform="translate(10 0)"><rect width="10" height="20" fill="url(#h)"/></g>
            <path d="M0 10a5 5 0 1 0 10 0a5 5 0 1 0-10 0z" fill="lime"/></svg>"##;
        let img = render(svg, 20, "").unwrap();
        let (top, bottom) = (px(&img, 20, 15, 0), px(&img, 20, 15, 19));
        assert!(top[0] < 30 && bottom[0] > 225, "{top:?} {bottom:?}");
        assert_eq!(px(&img, 20, 5, 10), [0, 255, 0, 255]);
        assert_eq!(px(&img, 20, 5, 2)[3], 0);
    }

    #[test]
    fn strokes() {
        let svg = br##"<svg viewBox="0 0 10 10"><path d="M1 5H9" stroke="#000" stroke-width="2" fill="none"/></svg>"##;
        let img = render(svg, 10, "").unwrap();
        assert_eq!(px(&img, 10, 5, 4)[3], 255);
        assert_eq!(px(&img, 10, 5, 7)[3], 0);
    }

    /// Every SVG icon installed renders, and draws something.
    #[test]
    #[cfg(target_os = "linux")]
    fn renders_installed_icons() {
        let Ok(themes) = std::fs::read_dir("/usr/share/icons") else { return };
        let (mut drawn, mut tried) = (0, 0);
        for theme in themes.flatten() {
            for sub in ["mimetypes/32", "places/32", "scalable/mimetypes", "scalable/places"] {
                let Ok(files) = std::fs::read_dir(theme.path().join(sub)) else { continue };
                for file in files.flatten().take(60) {
                    if file.path().extension().is_none_or(|e| e != "svg") {
                        continue;
                    }
                    let Ok(data) = std::fs::read(file.path()) else { continue };
                    tried += 1;
                    let img = render(&data, 24, "").unwrap_or_else(|| panic!("{}", file.path().display()));
                    drawn += img.iter().any(|p| p[3] > 0) as usize;
                }
            }
        }
        assert!(drawn * 10 >= tried * 9, "{drawn} of {tried} drawn");
    }
}

/// A contact sheet of the icons in FF_SVG_SHEET (a folder), next to how
/// rsvg-convert draws them when FF_SVG_REF holds its PNGs: for looking at
/// by eye (`cargo test -p ff-gui contact_sheet -- --ignored`).
#[cfg(test)]
#[test]
#[ignore]
fn contact_sheet() {
    let dir = std::env::var("FF_SVG_SHEET").unwrap();
    let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "svg")).collect();
    files.sort();
    files.truncate(64);
    let (cell, cols) = (40, 8);
    let rows = files.len().div_ceil(cols);
    let side = cell * cols.max(rows);
    let mut out = vec![[0xf0, 0xf0, 0xf0, 255u8]; side * side];
    for (i, f) in files.iter().enumerate() {
        let Some(img) = render(&std::fs::read(f).unwrap(), 32, "") else { continue };
        let (ox, oy) = ((i % cols) * cell + 4, (i / cols) * cell + 4);
        for y in 0..32 {
            for x in 0..32 {
                let p = img[y * 32 + x];
                let d = &mut out[(oy + y) * side + ox + x];
                let a = p[3] as u32;
                for k in 0..3 {
                    d[k] = ((p[k] as u32 * a + d[k] as u32 * (255 - a)) / 255) as u8;
                }
            }
        }
    }
    std::fs::write(format!("{dir}/../sheet.png"), crate::app_icon::png(side, &out)).unwrap();
}
