//! 路径变形：两条路径之间插值（菜单 ↔ 关闭、播放 ↔ 暂停）。
//!
//! 每条路径先规整成三次贝塞尔（直线、二次曲线升成三次，闭合时补一段回到起点），结果缓存在路径上。
//! 开始变形时把两端配成同样的结构：子路径一一配对（每个点平均走的路最短），多出来的子路径从对面最近的
//! 一条长出来（奇偶规则的填充改成从一点长出来，免得重叠的地方被挖空）；段数少的一边把长的段拆开；
//! 闭合的子路径换起点、换方向，让每个点走的路最短。配好的两端也缓存起来，之后每帧只是点的线性插值。
//!
//! 打断（变形途中改目标）时从此刻的形状出发重新配对，此刻每个点的速度经过同样的拆分、换序带到新的配对里，
//! 再投影到新的方向上，弹簧不会突然停下再出发。改回原来的路径时沿原路返回。

use std::sync::Arc;

use zeno::{Command, Point};

/// 一条子路径：起点，之后每段三个点（两个控制点、终点）
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Subpath {
    pub points: Vec<[f32; 2]>,
    pub closed: bool,
}

impl Subpath {
    fn segments(&self) -> usize {
        self.points.len() / 3
    }

    fn segment(&self, i: usize) -> [[f32; 2]; 4] {
        let p = &self.points[i * 3..i * 3 + 4];
        [p[0], p[1], p[2], p[3]]
    }

    fn centroid(&self) -> [f32; 2] {
        let n = self.points.len() as f32;
        let sum = self.points.iter().fold([0.0, 0.0], |s, p| [s[0] + p[0], s[1] + p[1]]);
        [sum[0] / n, sum[1] / n]
    }
}

/// 规整成三次贝塞尔的路径
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Shape {
    pub subpaths: Vec<Subpath>,
}

impl Shape {
    /// 从路径命令规整：直线和二次曲线升成三次，闭合时补一段回到起点；没有任何一段的子路径丢掉
    pub fn from_commands(commands: &[Command]) -> Self {
        let mut subpaths = Vec::new();
        let mut current: Option<Subpath> = None;
        let (mut cursor, mut start) = ([0.0f32; 2], [0.0f32; 2]);
        let finish = |current: &mut Option<Subpath>, subpaths: &mut Vec<Subpath>| {
            if let Some(sub) = current.take()
                && sub.points.len() > 1
            {
                subpaths.push(sub);
            }
        };
        let open = |current: &mut Option<Subpath>, at: [f32; 2]| current.get_or_insert_with(|| Subpath { points: vec![at], closed: false }).points.len();
        for &command in commands {
            match command {
                Command::MoveTo(p) => {
                    finish(&mut current, &mut subpaths);
                    cursor = [p.x, p.y];
                    start = cursor;
                }
                Command::LineTo(p) => {
                    let to = [p.x, p.y];
                    open(&mut current, cursor);
                    current.as_mut().expect("刚建好").points.extend(line(cursor, to));
                    cursor = to;
                }
                Command::QuadTo(c, p) => {
                    let (c, to) = ([c.x, c.y], [p.x, p.y]);
                    open(&mut current, cursor);
                    let c1 = [cursor[0] + (c[0] - cursor[0]) * 2.0 / 3.0, cursor[1] + (c[1] - cursor[1]) * 2.0 / 3.0];
                    let c2 = [to[0] + (c[0] - to[0]) * 2.0 / 3.0, to[1] + (c[1] - to[1]) * 2.0 / 3.0];
                    current.as_mut().expect("刚建好").points.extend([c1, c2, to]);
                    cursor = to;
                }
                Command::CurveTo(c1, c2, p) => {
                    open(&mut current, cursor);
                    current.as_mut().expect("刚建好").points.extend([[c1.x, c1.y], [c2.x, c2.y], [p.x, p.y]]);
                    cursor = [p.x, p.y];
                }
                Command::Close => {
                    if let Some(sub) = current.as_mut() {
                        if cursor != start {
                            sub.points.extend(line(cursor, start));
                        }
                        sub.closed = true;
                    }
                    finish(&mut current, &mut subpaths);
                    cursor = start;
                }
            }
        }
        finish(&mut current, &mut subpaths);
        Self { subpaths }
    }
}

fn line(a: [f32; 2], b: [f32; 2]) -> [[f32; 2]; 3] {
    let d = [b[0] - a[0], b[1] - a[1]];
    [[a[0] + d[0] / 3.0, a[1] + d[1] / 3.0], [a[0] + d[0] * 2.0 / 3.0, a[1] + d[1] * 2.0 / 3.0], b]
}

/// 配好对的两端：点数和结构一样，每帧按进度线性插值
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MorphPair {
    pub from: Vec<[f32; 2]>,
    pub to: Vec<[f32; 2]>,
    /// 每条子路径：起点在 `from` / `to` 里的下标、段数、是否闭合
    pub layout: Vec<(u32, u32, bool)>,
    /// 两端的画布（同 SVG 的 viewBox），按进度插值
    pub from_canvas: Option<[f32; 4]>,
    pub to_canvas: Option<[f32; 4]>,
}

impl MorphPair {
    /// 进度 `t`（弹簧过冲时可以超出 0..=1）的点
    pub fn points(&self, t: f32) -> Vec<[f32; 2]> {
        // 写成两端的加权和：t 为 0、1 时正好是两端
        let s = 1.0 - t;
        self.from.iter().zip(&self.to).map(|(a, b)| [a[0] * s + b[0] * t, a[1] * s + b[1] * t]).collect()
    }

    /// 进度 `t` 的画布
    pub fn canvas(&self, t: f32) -> Option<[f32; 4]> {
        match (self.from_canvas, self.to_canvas) {
            (Some(a), Some(b)) => Some(std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t)),
            (a, b) => b.or(a),
        }
    }

    /// 进度 `t` 的路径命令（三次贝塞尔）
    pub fn commands(&self, t: f32) -> Vec<Command> {
        let points = self.points(t);
        let mut commands = Vec::with_capacity(points.len() / 3 + self.layout.len() * 2);
        let point = |p: [f32; 2]| Point::new(p[0], p[1]);
        for &(start, segments, closed) in &self.layout {
            let (start, segments) = (start as usize, segments as usize);
            commands.push(Command::MoveTo(point(points[start])));
            for s in 0..segments {
                let i = start + 1 + s * 3;
                commands.push(Command::CurveTo(point(points[i]), point(points[i + 1]), point(points[i + 2])));
            }
            if closed {
                commands.push(Command::Close);
            }
        }
        commands
    }

    /// 按布局拆回子路径（`t` 时刻的样子），给打断时重新配对用
    fn subpaths(&self, points: &[[f32; 2]]) -> Vec<Subpath> {
        self.layout
            .iter()
            .map(|&(start, segments, closed)| Subpath {
                points: points[start as usize..start as usize + 1 + segments as usize * 3].to_vec(),
                closed,
            })
            .collect()
    }

    /// 两端所有点之间的距离平方和（速度投影的分母）
    fn spread(&self) -> f32 {
        self.from.iter().zip(&self.to).map(|(a, b)| (b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sum()
    }
}

/// 从 `from` 变到 `to` 的配对。`even_odd`：按奇偶规则填充（多出来的子路径从一点长出来，不和别的重叠）
pub(crate) fn pair(from: &Shape, from_canvas: Option<[f32; 4]>, to: &Shape, to_canvas: Option<[f32; 4]>, even_odd: bool) -> MorphPair {
    let (pairs, _) = match_shapes(&from.subpaths, None, &to.subpaths, even_odd);
    assemble(pairs, from_canvas, to_canvas)
}

/// 变形途中改目标：从进度 `t` 的样子（进度速度 `speed`，每秒）出发变到 `to`，返回新的配对和新进度的初始速度
pub(crate) fn retarget(
    current: &MorphPair,
    t: f32,
    speed: f32,
    to: &Shape,
    to_canvas: Option<[f32; 4]>,
    even_odd: bool,
) -> (MorphPair, f32) {
    let points = current.points(t);
    // 此刻每个点的速度：进度速度乘上两端的差
    let velocity: Vec<[f32; 2]> = current.from.iter().zip(&current.to).map(|(a, b)| [(b[0] - a[0]) * speed, (b[1] - a[1]) * speed]).collect();
    let (from, velocity) = (current.subpaths(&points), current.subpaths(&velocity));
    let (pairs, velocity) = match_shapes(&from, Some(&velocity), &to.subpaths, even_odd);
    let pair = assemble(pairs, current.canvas(t), to_canvas);
    let velocity: Vec<[f32; 2]> = velocity.into_iter().flat_map(|sub| sub.points).collect();
    (pair.clone(), project(&pair, &velocity))
}

/// 变形途中改回原来的路径：从进度 `t` 的样子沿原路返回
pub(crate) fn reverse(current: &MorphPair, t: f32, speed: f32) -> (MorphPair, f32) {
    let points = current.points(t);
    let velocity: Vec<[f32; 2]> = current.from.iter().zip(&current.to).map(|(a, b)| [(b[0] - a[0]) * speed, (b[1] - a[1]) * speed]).collect();
    let pair = MorphPair {
        from: points,
        to: current.from.clone(),
        layout: current.layout.clone(),
        from_canvas: current.canvas(t),
        to_canvas: current.from_canvas,
    };
    let speed = project(&pair, &velocity);
    (pair, speed)
}

/// 点的速度投影到新配对的方向上，得到新进度的速度
fn project(pair: &MorphPair, velocity: &[[f32; 2]]) -> f32 {
    let spread = pair.spread();
    if spread <= 1e-9 {
        return 0.0;
    }
    let along: f32 = pair.from.iter().zip(&pair.to).zip(velocity).map(|((a, b), v)| (b[0] - a[0]) * v[0] + (b[1] - a[1]) * v[1]).sum();
    (along / spread).clamp(-100.0, 100.0)
}

fn assemble(pairs: Vec<(Subpath, Subpath)>, from_canvas: Option<[f32; 4]>, to_canvas: Option<[f32; 4]>) -> MorphPair {
    let mut pair = MorphPair { from: Vec::new(), to: Vec::new(), layout: Vec::new(), from_canvas, to_canvas };
    for (a, b) in pairs {
        debug_assert_eq!(a.points.len(), b.points.len());
        pair.layout.push((pair.from.len() as u32, a.segments() as u32, a.closed && b.closed));
        pair.from.extend(a.points);
        pair.to.extend(b.points);
    }
    pair
}

/// 子路径配对：`from` 的顺序、方向、起点保持不变（只会拆段），`to` 换序、换方向、换起点去配它。
/// `velocity` 是 `from` 每个点的速度，跟着做同样的拆分，返回配对和拆好的速度
fn match_shapes(
    from: &[Subpath],
    velocity: Option<&[Subpath]>,
    to: &[Subpath],
    even_odd: bool,
) -> (Vec<(Subpath, Subpath)>, Vec<Subpath>) {
    let costs: Vec<Vec<f32>> = from.iter().map(|a| to.iter().map(|b| align(a, b).2).collect()).collect();
    let assignment = assign(&costs, from.len(), to.len());
    let zero = |sub: &Subpath| Subpath { points: vec![[0.0; 2]; sub.points.len()], closed: sub.closed };
    let mut pairs = Vec::new();
    let mut velocities = Vec::new();
    let mut used = vec![false; to.len()];
    for (i, a) in from.iter().enumerate() {
        let v = velocity.map_or_else(|| zero(a), |v| v[i].clone());
        let target = match assignment[i] {
            Some(j) => {
                used[j] = true;
                to[j].clone()
            }
            // 多出来的：变成对面最近的那条（奇偶规则时收成自己中心的一点）
            None if even_odd || to.is_empty() => collapsed(a, a.centroid()),
            None => {
                let j = nearest(&costs[i]).expect("对面不是空的");
                to[j].clone()
            }
        };
        let (a, b, v) = fit(a, &target, &v);
        pairs.push((a, b));
        velocities.push(v);
    }
    // 对面多出来的：从这边最近的那条长出来（奇偶规则时从它自己中心的一点长出来）
    for (j, b) in to.iter().enumerate() {
        if used[j] {
            continue;
        }
        let column: Vec<f32> = costs.iter().map(|row| row[j]).collect();
        let (source, v) = match nearest(&column) {
            Some(i) if !even_odd => (from[i].clone(), velocity.map_or_else(|| zero(&from[i]), |v| v[i].clone())),
            _ => {
                let source = collapsed(b, b.centroid());
                let v = zero(&source);
                (source, v)
            }
        };
        let (a, b, v) = fit(&source, b, &v);
        pairs.push((a, b));
        velocities.push(v);
    }
    (pairs, velocities)
}

/// 一条子路径收成一点：和 `like` 一样的结构，所有点都在 `at`
fn collapsed(like: &Subpath, at: [f32; 2]) -> Subpath {
    Subpath { points: vec![at; like.points.len()], closed: like.closed }
}

fn nearest(costs: &[f32]) -> Option<usize> {
    costs.iter().enumerate().min_by(|a, b| a.1.total_cmp(b.1)).map(|(i, _)| i)
}

/// 每条 `from` 配哪条 `to`（`None` 是多出来的）：总代价最小。数量少时穷举，多时贪心
fn assign(costs: &[Vec<f32>], n: usize, m: usize) -> Vec<Option<usize>> {
    let mut result = vec![None; n];
    if n == 0 || m == 0 {
        return result;
    }
    let small = n.min(m);
    if n.max(m) <= 8 && small <= 6 {
        // 穷举：较少的一边每条依次挑较多一边里还没用过的
        let cost = |s: usize, l: usize| if n <= m { costs[s][l] } else { costs[l][s] };
        let mut search = Search { small, large: n.max(m), cost: &cost, used: vec![false; n.max(m)], picks: Vec::new(), best: (f32::INFINITY, Vec::new()) };
        search.run(0, 0.0);
        for (s, &l) in search.best.1.iter().enumerate() {
            if n <= m {
                result[s] = Some(l);
            } else {
                result[l] = Some(s);
            }
        }
        return result;
    }
    let mut candidates: Vec<(f32, usize, usize)> = (0..n).flat_map(|i| (0..m).map(move |j| (costs[i][j], i, j))).collect();
    candidates.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut taken = vec![false; m];
    for (_, i, j) in candidates {
        if result[i].is_none() && !taken[j] {
            result[i] = Some(j);
            taken[j] = true;
        }
    }
    result
}

/// 穷举配对的状态
struct Search<'a> {
    small: usize,
    large: usize,
    cost: &'a dyn Fn(usize, usize) -> f32,
    used: Vec<bool>,
    picks: Vec<usize>,
    best: (f32, Vec<usize>),
}

impl Search<'_> {
    /// 较少一边的第 `s` 条往后挑，已经花了 `total`；不比已知最好的少时剪掉
    fn run(&mut self, s: usize, total: f32) {
        if total >= self.best.0 {
            return;
        }
        if s == self.small {
            self.best = (total, self.picks.clone());
            return;
        }
        for l in 0..self.large {
            if !self.used[l] {
                self.used[l] = true;
                self.picks.push(l);
                let cost = (self.cost)(s, l);
                self.run(s + 1, total + cost);
                self.picks.pop();
                self.used[l] = false;
            }
        }
    }
}

/// 把 `a`、`b` 配成同样的段数，`b` 换起点、换方向，返回拆好的 `a`、对齐的 `b`、拆好的速度
fn fit(a: &Subpath, b: &Subpath, velocity: &Subpath) -> (Subpath, Subpath, Subpath) {
    let n = a.segments().max(b.segments()).max(1);
    let pieces = pieces(a, n);
    let (a, velocity) = (split(a, &pieces), split(velocity, &pieces));
    let (_, b, _) = align(&a, b);
    (a, b, velocity)
}

/// `a` 不动，把 `b` 配成和它一样的段数并对齐起点和方向；返回（拆好的 a，对齐的 b，平均每个点移动距离的平方）
fn align(a: &Subpath, b: &Subpath) -> (Subpath, Subpath, f32) {
    let n = a.segments().max(b.segments()).max(1);
    let a = split(a, &pieces(a, n));
    let b = split(b, &pieces(b, n));
    let closed = a.closed && b.closed;
    let mut best = (f32::INFINITY, b.clone());
    for reversed in [false, true] {
        let oriented = if reversed { reverse_subpath(&b) } else { b.clone() };
        // 闭合的子路径换起点；开放的只换方向
        let rotations = if closed { n } else { 1 };
        for r in 0..rotations {
            let candidate = if r == 0 { oriented.clone() } else { rotate(&oriented, r) };
            let cost: f32 = a.points.iter().zip(&candidate.points).map(|(p, q)| (p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2)).sum();
            if cost < best.0 {
                best = (cost, candidate);
            }
        }
    }
    let mean = best.0 / a.points.len() as f32;
    (a, best.1, mean)
}

/// 段数从现在的拆到 `n`：多出来的份数按长度分给各段（每次给"平均每份最长"的那段）
fn pieces(sub: &Subpath, n: usize) -> Vec<usize> {
    let count = sub.segments();
    let mut pieces = vec![1usize; count];
    if count == 0 {
        return pieces;
    }
    let lengths: Vec<f32> = (0..count)
        .map(|i| {
            let s = sub.segment(i);
            s.windows(2).map(|w| ((w[1][0] - w[0][0]).powi(2) + (w[1][1] - w[0][1]).powi(2)).sqrt()).sum::<f32>()
        })
        .collect();
    for _ in count..n {
        let i = (0..count).max_by(|&x, &y| (lengths[x] / pieces[x] as f32).total_cmp(&(lengths[y] / pieces[y] as f32))).expect("至少一段");
        pieces[i] += 1;
    }
    pieces
}

/// 按份数把每段拆成参数等分的几段（de Casteljau）
fn split(sub: &Subpath, pieces: &[usize]) -> Subpath {
    if sub.segments() == 0 {
        // 收成一点的子路径没有段：按份数补上原地不动的段
        let n: usize = pieces.iter().sum::<usize>().max(1);
        return Subpath { points: vec![sub.points.first().copied().unwrap_or_default(); 1 + n * 3], closed: sub.closed };
    }
    if pieces.iter().all(|&k| k == 1) {
        return sub.clone();
    }
    let mut points = vec![sub.points[0]];
    for (i, &k) in pieces.iter().enumerate() {
        let mut rest = sub.segment(i);
        for j in 0..k {
            let remaining = k - j;
            if remaining == 1 {
                points.extend_from_slice(&rest[1..]);
                break;
            }
            let (head, tail) = subdivide(rest, 1.0 / remaining as f32);
            points.extend_from_slice(&head[1..]);
            rest = tail;
        }
    }
    Subpath { points, closed: sub.closed }
}

fn subdivide(s: [[f32; 2]; 4], t: f32) -> ([[f32; 2]; 4], [[f32; 2]; 4]) {
    let lerp = |a: [f32; 2], b: [f32; 2]| [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
    let (ab, bc, cd) = (lerp(s[0], s[1]), lerp(s[1], s[2]), lerp(s[2], s[3]));
    let (abc, bcd) = (lerp(ab, bc), lerp(bc, cd));
    let mid = lerp(abc, bcd);
    ([s[0], ab, abc, mid], [mid, bcd, cd, s[3]])
}

fn reverse_subpath(sub: &Subpath) -> Subpath {
    Subpath { points: sub.points.iter().rev().copied().collect(), closed: sub.closed }
}

/// 闭合的子路径从第 `r` 段开始（终点和起点重合，点序列整体轮换）
fn rotate(sub: &Subpath, r: usize) -> Subpath {
    let n = sub.segments();
    let mut points = Vec::with_capacity(sub.points.len());
    for s in 0..n {
        let seg = sub.segment((s + r) % n);
        if s == 0 {
            points.push(seg[0]);
        }
        points.extend_from_slice(&seg[1..]);
    }
    Subpath { points, closed: sub.closed }
}

/// 配对的缓存：同一对路径（按标识）只配一次。最多存这么多对，满了清空
pub(crate) const PAIR_CACHE: usize = 64;

/// 变形的状态，挂在路径元素上
pub(crate) struct Morph {
    pub pair: Arc<MorphPair>,
    /// 0..1（弹簧过冲时可以超出）
    pub progress: f32,
    /// 从哪条路径变过来（中途改回它时沿原路返回）；打断后从中间的样子出发时为 `None`
    pub source: Option<crate::vector::VectorPath>,
    /// 图集里预留的一块，每帧改写（见 `vector.rs` 的 `Raster`）
    pub slot: Option<crate::vector::Slot>,
    /// 按奇偶规则填充（跟目标路径）
    pub even_odd: bool,
}
