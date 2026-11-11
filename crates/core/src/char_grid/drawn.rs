//! 字符网格里自己画的字：制表符（U+2500 ~ U+257F）和方块（U+2580 ~ U+259F）。
//!
//! 字体里的这些字不一定铺满格子（行高大于 1 时尤其明显），有的字体里干脆没有，连起来会有缝、接不上。所以都自己画，
//! 和用哪个字体无关：直的线和方块是矩形，对齐物理像素；弧线和斜线是路径，位置、粗细和直的线一样。

use scene::{Color, Quad};

use crate::text::{CellMetrics, TextSystem};
use crate::vector::{LineCap, Stroke, VectorPath};

/// 自己画的字
#[inline]
pub(super) fn is_drawn(ch: char) -> bool {
    matches!(ch as u32, 0x2500..=0x259f)
}

/// 画一个自己画的字（[`is_drawn`]）。`x` 是格子左边（行内坐标），对齐物理像素
pub(super) fn draw(text: &mut TextSystem, ch: char, x: f32, m: &CellMetrics, color: Color, out: &mut Vec<Quad>) {
    let code = ch as u32;
    let scale = text.scale();
    if matches!(code, ARC_FIRST..=DIAGONAL_LAST) {
        let (w, h) = (m.width, m.height);
        let t = line_width(scale);
        let id = curve_id(code, [w, h, t].map(|v| (v * scale).round() as u64));
        if let Some(quad) = text.raster().cell_curve(id, || curve(code, w, h, t, scale), [x, 0.0], color) {
            out.push(quad);
        }
        return;
    }
    let mut pen = Pen { x, w: m.width, h: m.height, scale, t: line_width(scale), color, out };
    match code {
        0x2580..=0x259f => pen.block(code),
        _ => match dashes_of(code) {
            Some(dashes) => pen.dashes(dashes),
            None => pen.lines(LINES[(code - 0x2500) as usize]),
        },
    }
}

/// 细线的粗细：1 逻辑像素，取整到物理像素
fn line_width(scale: f32) -> f32 {
    (scale.round().max(1.0)) / scale
}

/// 制表线一个方向上的样子
#[derive(Clone, Copy, PartialEq)]
enum Arm {
    None,
    Light,
    Heavy,
    Double,
}

/// U+2500 ~ U+257F 每个字从格子中间往左、右、上、下伸出去的线。虚线、弧线、斜线不在这张表里（全是 `O`）
const LINES: [[Arm; 4]; 0x80] = {
    use Arm::{Double as D, Heavy as H, Light as L, None as O};
    [
        [L, L, O, O], [H, H, O, O], [O, O, L, L], [O, O, H, H], // ─ ━ │ ┃
        [O, O, O, O], [O, O, O, O], [O, O, O, O], [O, O, O, O], // ┄ ┅ ┆ ┇
        [O, O, O, O], [O, O, O, O], [O, O, O, O], [O, O, O, O], // ┈ ┉ ┊ ┋
        [O, L, O, L], [O, H, O, L], [O, L, O, H], [O, H, O, H], // ┌ ┍ ┎ ┏
        [L, O, O, L], [H, O, O, L], [L, O, O, H], [H, O, O, H], // ┐ ┑ ┒ ┓
        [O, L, L, O], [O, H, L, O], [O, L, H, O], [O, H, H, O], // └ ┕ ┖ ┗
        [L, O, L, O], [H, O, L, O], [L, O, H, O], [H, O, H, O], // ┘ ┙ ┚ ┛
        [O, L, L, L], [O, H, L, L], [O, L, H, L], [O, L, L, H], // ├ ┝ ┞ ┟
        [O, L, H, H], [O, H, H, L], [O, H, L, H], [O, H, H, H], // ┠ ┡ ┢ ┣
        [L, O, L, L], [H, O, L, L], [L, O, H, L], [L, O, L, H], // ┤ ┥ ┦ ┧
        [L, O, H, H], [H, O, H, L], [H, O, L, H], [H, O, H, H], // ┨ ┩ ┪ ┫
        [L, L, O, L], [H, L, O, L], [L, H, O, L], [H, H, O, L], // ┬ ┭ ┮ ┯
        [L, L, O, H], [H, L, O, H], [L, H, O, H], [H, H, O, H], // ┰ ┱ ┲ ┳
        [L, L, L, O], [H, L, L, O], [L, H, L, O], [H, H, L, O], // ┴ ┵ ┶ ┷
        [L, L, H, O], [H, L, H, O], [L, H, H, O], [H, H, H, O], // ┸ ┹ ┺ ┻
        [L, L, L, L], [H, L, L, L], [L, H, L, L], [H, H, L, L], // ┼ ┽ ┾ ┿
        [L, L, H, L], [L, L, L, H], [L, L, H, H], [H, L, H, L], // ╀ ╁ ╂ ╃
        [L, H, H, L], [H, L, L, H], [L, H, L, H], [H, H, H, L], // ╄ ╅ ╆ ╇
        [H, H, L, H], [H, L, H, H], [L, H, H, H], [H, H, H, H], // ╈ ╉ ╊ ╋
        [O, O, O, O], [O, O, O, O], [O, O, O, O], [O, O, O, O], // ╌ ╍ ╎ ╏
        [D, D, O, O], [O, O, D, D], [O, D, O, L], [O, L, O, D], // ═ ║ ╒ ╓
        [O, D, O, D], [D, O, O, L], [L, O, O, D], [D, O, O, D], // ╔ ╕ ╖ ╗
        [O, D, L, O], [O, L, D, O], [O, D, D, O], [D, O, L, O], // ╘ ╙ ╚ ╛
        [L, O, D, O], [D, O, D, O], [O, D, L, L], [O, L, D, D], // ╜ ╝ ╞ ╟
        [O, D, D, D], [D, O, L, L], [L, O, D, D], [D, O, D, D], // ╠ ╡ ╢ ╣
        [D, D, O, L], [L, L, O, D], [D, D, O, D], [D, D, L, O], // ╤ ╥ ╦ ╧
        [L, L, D, O], [D, D, D, O], [D, D, L, L], [L, L, D, D], // ╨ ╩ ╪ ╫
        [D, D, D, D], [O, O, O, O], [O, O, O, O], [O, O, O, O], // ╬ ╭ ╮ ╯
        [O, O, O, O], [O, O, O, O], [O, O, O, O], [O, O, O, O], // ╰ ╱ ╲ ╳
        [L, O, O, O], [O, O, L, O], [O, L, O, O], [O, O, O, L], // ╴ ╵ ╶ ╷
        [H, O, O, O], [O, O, H, O], [O, H, O, O], [O, O, O, H], // ╸ ╹ ╺ ╻
        [L, H, O, O], [O, O, L, H], [H, L, O, O], [O, O, H, L], // ╼ ╽ ╾ ╿
    ]
};

/// ╭ ╮ ╯ ╰ ╱ ╲ ╳：画成路径的字
const ARC_FIRST: u32 = 0x256d;
const ARC_LAST: u32 = 0x2570;
const DIAGONAL_LAST: u32 = 0x2573;

/// ▖ ▗ ▘ ▙ ▚ ▛ ▜ ▝ ▞ ▟（U+2596 ~ U+259F）占哪几块：四位分别是左上、右上、左下、右下
const QUADRANTS: [u8; 10] = [0b0100, 0b1000, 0b0001, 0b1101, 0b1001, 0b0111, 0b1011, 0b0010, 0b0110, 0b1110];

/// 虚线：几段、横的还是竖的、是不是粗线
#[derive(Clone, Copy)]
struct Dashes {
    count: u32,
    horizontal: bool,
    heavy: bool,
}

/// 这个字是虚线时的样子：┄ ┅ ┆ ┇ 三段，┈ ┉ ┊ ┋ 四段，╌ ╍ ╎ ╏ 两段
fn dashes_of(code: u32) -> Option<Dashes> {
    let (count, i) = match code {
        0x2504..=0x2507 => (3, code - 0x2504),
        0x2508..=0x250b => (4, code - 0x2508),
        0x254c..=0x254f => (2, code - 0x254c),
        _ => return None,
    };
    Some(Dashes { count, horizontal: i < 2, heavy: i % 2 == 1 })
}

/// 一个格子里的画笔：矩形都对齐物理像素，同一个位置算出来的边都一样，所以相邻的块、相邻的格子之间没有缝
struct Pen<'a> {
    x: f32,
    w: f32,
    h: f32,
    scale: f32,
    /// 细线的粗细
    t: f32,
    color: Color,
    out: &'a mut Vec<Quad>,
}

impl Pen<'_> {
    fn snap(&self, v: f32) -> f32 {
        (v * self.scale).round() / self.scale
    }

    fn rect(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, opacity: f32) {
        let (x0, y0, x1, y1) = (self.snap(self.x + x0), self.snap(y0), self.snap(self.x + x1), self.snap(y1));
        if x1 > x0 && y1 > y0 {
            self.out.push(Quad::new(x0, y0, x1 - x0, y1 - y0).fill(self.color).opacity(opacity));
        }
    }

    fn fill(&mut self, x0: f32, y0: f32, x1: f32, y1: f32) {
        self.rect(x0, y0, x1, y1, 1.0);
    }

    /// 粗细是 `thickness` 的线放在 `length` 正中时的起点
    fn middle(&self, length: f32, thickness: f32) -> f32 {
        self.snap(length * 0.5 - thickness * 0.5)
    }

    fn block(&mut self, code: u32) {
        let (w, h) = (self.w, self.h);
        match code {
            // ▀ 上半，▁ ~ ▇ 下面 1/8 ~ 7/8，█ 整格，▉ ~ ▏ 左边 7/8 ~ 1/8，▐ 右半
            0x2580 => self.fill(0.0, 0.0, w, h * 0.5),
            0x2581..=0x2588 => self.fill(0.0, h * (1.0 - (code - 0x2580) as f32 / 8.0), w, h),
            0x2589..=0x258f => self.fill(0.0, 0.0, w * (0x2590 - code) as f32 / 8.0, h),
            0x2590 => self.fill(w * 0.5, 0.0, w, h),
            // ░ ▒ ▓：整格，按浓淡减淡
            0x2591..=0x2593 => self.rect(0.0, 0.0, w, h, (code - 0x2590) as f32 * 0.25),
            // ▔ 上面 1/8，▕ 右边 1/8
            0x2594 => self.fill(0.0, 0.0, w, h / 8.0),
            0x2595 => self.fill(w * 7.0 / 8.0, 0.0, w, h),
            _ => {
                let quadrants = QUADRANTS[(code - 0x2596) as usize];
                let (mx, my) = (w * 0.5, h * 0.5);
                for (bit, [x0, y0, x1, y1]) in [[0.0, 0.0, mx, my], [mx, 0.0, w, my], [0.0, my, mx, h], [mx, my, w, h]].into_iter().enumerate() {
                    if quadrants & (1 << bit) != 0 {
                        self.fill(x0, y0, x1, y1);
                    }
                }
            }
        }
    }

    /// 虚线：每段占一份的正中 60%，格子之间的空隙和段之间的一样
    fn dashes(&mut self, dashes: Dashes) {
        let thickness = if dashes.heavy { self.t * 2.0 } else { self.t };
        let (length, across) = if dashes.horizontal { (self.w, self.h) } else { (self.h, self.w) };
        let start = self.middle(across, thickness);
        let share = length / dashes.count as f32;
        for i in 0..dashes.count {
            let (from, to) = ((i as f32 + 0.2) * share, (i as f32 + 0.8) * share);
            if dashes.horizontal {
                self.fill(from, start, to, start + thickness);
            } else {
                self.fill(start, from, start + thickness, to);
            }
        }
    }

    fn lines(&mut self, arms: [Arm; 4]) {
        let [left, right, up, down] = arms;
        let across_double = left == Arm::Double || right == Arm::Double;
        let along_double = up == Arm::Double || down == Arm::Double;
        match (across_double, along_double) {
            (false, false) => self.single_lines(arms),
            (true, false) => self.double_across(arms),
            (false, true) => self.double_along(arms),
            (true, true) => self.double_both(arms),
        }
    }

    /// 只有细线和粗线：每条线画到中间交汇的那一块的另一边，转角是方的。对着的两条一样时画成一条
    fn single_lines(&mut self, [left, right, up, down]: [Arm; 4]) {
        let (w, h, t) = (self.w, self.h, self.t);
        let thick = |arm: Arm| match arm {
            Arm::Light => t,
            Arm::Heavy => t * 2.0,
            _ => 0.0,
        };
        let (across, along) = (thick(left).max(thick(right)), thick(up).max(thick(down)));
        // 交汇的一块：宽是竖线的粗细，高是横线的粗细；只有一个方向的线时是方的
        let (hub_w, hub_h) = (if along > 0.0 { along } else { across }, if across > 0.0 { across } else { along });
        let (hub_x, hub_y) = (self.middle(w, hub_w), self.middle(h, hub_h));
        let horizontal = |pen: &mut Self, arm: Arm, from: f32, to: f32| {
            if arm != Arm::None {
                let y = pen.middle(h, thick(arm));
                pen.fill(from, y, to, y + thick(arm));
            }
        };
        if left == right {
            horizontal(self, left, 0.0, w);
        } else {
            horizontal(self, left, 0.0, hub_x + hub_w);
            horizontal(self, right, hub_x, w);
        }
        let vertical = |pen: &mut Self, arm: Arm, from: f32, to: f32| {
            if arm != Arm::None {
                let x = pen.middle(w, thick(arm));
                pen.fill(x, from, x + thick(arm), to);
            }
        };
        if up == down {
            vertical(self, up, 0.0, h);
        } else {
            vertical(self, up, 0.0, hub_y + hub_h);
            vertical(self, down, hub_y, h);
        }
    }

    /// 横的是双线，竖的是细线或者没有（═ ╒ ╕ ╘ ╛ ╞ ╡ ╤ ╧ ╪）。横线穿过去时，竖线只接到靠近它的那一条上
    fn double_across(&mut self, [left, right, up, down]: [Arm; 4]) {
        let (w, h, t) = (self.w, self.h, self.t);
        let (x, y) = (self.middle(w, t), self.middle(h, t * 3.0));
        let (from, to) = (if left != Arm::None { 0.0 } else { x }, if right != Arm::None { w } else { x + t });
        self.fill(from, y, to, y + t);
        self.fill(from, y + t * 2.0, to, y + t * 3.0);
        let through = left != Arm::None && right != Arm::None;
        if up != Arm::None || down != Arm::None {
            let top = if up != Arm::None { 0.0 } else if through { y + t * 2.0 } else { y };
            let bottom = if down != Arm::None { h } else if through { y + t } else { y + t * 3.0 };
            self.fill(x, top, x + t, bottom);
        }
    }

    /// 竖的是双线，横的是细线或者没有（║ ╓ ╖ ╙ ╜ ╟ ╢ ╥ ╨ ╫）。竖线穿过去时，横线只接到靠近它的那一条上
    fn double_along(&mut self, [left, right, up, down]: [Arm; 4]) {
        let (w, h, t) = (self.w, self.h, self.t);
        let (x, y) = (self.middle(w, t * 3.0), self.middle(h, t));
        let (from, to) = (if up != Arm::None { 0.0 } else { y }, if down != Arm::None { h } else { y + t });
        self.fill(x, from, x + t, to);
        self.fill(x + t * 2.0, from, x + t * 3.0, to);
        let through = up != Arm::None && down != Arm::None;
        if left != Arm::None || right != Arm::None {
            let start = if left != Arm::None { 0.0 } else if through { x + t * 2.0 } else { x };
            let end = if right != Arm::None { w } else if through { x + t } else { x + t * 3.0 };
            self.fill(start, y, end, y + t);
        }
    }

    /// 横竖都是双线（╔ ╗ ╚ ╝ ╠ ╣ ╦ ╩ ╬）：四条线各分两段。旁边有线的一段停在里面的角上，
    /// 没有时绕到外面的角上，对面还有线时穿过去
    fn double_both(&mut self, [left, right, up, down]: [Arm; 4]) {
        let (w, h, t) = (self.w, self.h, self.t);
        let (x, y) = (self.middle(w, t * 3.0), self.middle(h, t * 3.0));
        let (l, r, u, d) = (left != Arm::None, right != Arm::None, up != Arm::None, down != Arm::None);
        // 上面一条横线的旁边是往上的线，下面一条的旁边是往下的线；左边一条竖线的旁边是往左的线，右边一条的旁边是往右的线
        for (row, beside) in [(y, u), (y + t * 2.0, d)] {
            if l {
                let end = if beside { x + t } else if r { w } else { x + t * 3.0 };
                self.fill(0.0, row, end, row + t);
            }
            if r {
                let start = if beside { x + t * 2.0 } else if l { 0.0 } else { x };
                self.fill(start, row, w, row + t);
            }
        }
        for (column, beside) in [(x, l), (x + t * 2.0, r)] {
            if u {
                let end = if beside { y + t } else if d { h } else { y + t * 3.0 };
                self.fill(column, 0.0, column + t, end);
            }
            if d {
                let start = if beside { y + t * 2.0 } else if u { 0.0 } else { y };
                self.fill(column, start, column + t, h);
            }
        }
    }
}

/// 路径图集里的缓存键：哪个字，格子的宽、高和线宽（物理像素）。普通路径的标识从 1 往上数，碰不到最高位
fn curve_id(code: u32, [w, h, t]: [u64; 3]) -> u64 {
    (1 << 63) | (((code - 0x2500) as u64) << 48) | ((w & 0xffff) << 32) | ((h & 0xffff) << 16) | (t & 0xffff)
}

/// 弧线和斜线的路径（原点是格子左上角）和描边。线的中心和直的线一样，弧线的两头在格子边上正好接上旁边的直线
fn curve(code: u32, w: f32, h: f32, t: f32, scale: f32) -> (VectorPath, Stroke) {
    if code > ARC_LAST {
        // 方头超出端点半个线宽：斜着连起来的两格在角上没有缺口
        let stroke = Stroke { cap: LineCap::Square, ..Stroke::new(t) };
        let path = VectorPath::builder();
        let path = if code != 0x2572 { path.move_to(w, 0.0).line_to(0.0, h) } else { path };
        let path = if code != 0x2571 { path.move_to(0.0, 0.0).line_to(w, h) } else { path };
        return (path.build(), stroke);
    }
    let snap = |v: f32| (v * scale).round() / scale;
    let (cx, cy) = (snap(w * 0.5 - t * 0.5) + t * 0.5, snap(h * 0.5 - t * 0.5) + t * 0.5);
    // ╭ ╮ ╯ ╰：往右还是往左，往下还是往上
    let (right, down) = (matches!(code, 0x256d | 0x2570), matches!(code, 0x256d | 0x256e));
    let (end_x, end_y) = (if right { w } else { 0.0 }, if down { h } else { 0.0 });
    // 两头都留一小段直的：弧线不在格子边上开始弯，和旁边的直线接得严
    let radius = ((end_x - cx).abs().min((end_y - cy).abs()) - t).max(t);
    let (sx, sy) = (if right { radius } else { -radius }, if down { radius } else { -radius });
    // 四分之一圆的三次贝塞尔：控制点离端点 0.5523 个半径
    const ROUND: f32 = 1.0 - 0.5523;
    let path = VectorPath::builder()
        .move_to(end_x, cy)
        .line_to(cx + sx, cy)
        .curve_to(cx + sx * ROUND, cy, cx, cy + sy * ROUND, cx, cy + sy)
        .line_to(cx, end_y)
        .build();
    (path, Stroke::new(t))
}
