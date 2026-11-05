//! 按行、按字素簇的文字片段（无障碍的 `TextRun`）：输入框按排出来的每一行给一个，字长按字素簇（组合的表情符号、
//! 组合附加符号是一个字），读屏软件按字走、读"当前行"都和屏幕上一样。

use std::hash::{Hash, Hasher};

use accesskit::{Node, Role};

use crate::segmentation::{graphemes, is_ideographic};

/// 一行文字的片段节点：值（`line_break` 时末尾加一个换行符，算一个字）、按字素簇的字长、词的开头。
/// `positions` 给出时（每个字素簇的起点和宽度，元素坐标里相对这一行的左边）也带上字的位置，放大镜能跟着插入点
pub(crate) fn text_run(text: &str, line_break: bool, positions: Option<(&[f32], &[f32])>) -> Node {
    let mut node = Node::new(Role::TextRun);
    let mut lengths = Vec::with_capacity(text.len());
    let mut words = Vec::new();
    let mut previous_space = true;
    for (start, end) in clusters(text) {
        let cluster = &text[start..end];
        let first = cluster.chars().next().unwrap_or(' ');
        let space = first.is_whitespace();
        // 词从非空白开始（前面是空白），汉字、假名一个字一个词；行首的空白也自成一词
        if (!space && (previous_space || is_ideographic(first))) || (space && lengths.is_empty()) {
            words.push(lengths.len());
        }
        previous_space = space;
        // 一个字素簇超过 255 个字节（很少见）：在字和字之间拆开，每段都不超过
        if cluster.len() <= u8::MAX as usize {
            lengths.push(cluster.len() as u8);
        } else {
            let mut piece = 0;
            for c in cluster.chars() {
                if piece + c.len_utf8() > u8::MAX as usize {
                    lengths.push(piece as u8);
                    piece = 0;
                }
                piece += c.len_utf8();
            }
            lengths.push(piece as u8);
        }
    }
    let mut value = String::with_capacity(text.len() + 1);
    value.push_str(text);
    if line_break {
        value.push('\n');
        lengths.push(1);
    }
    if let Some((x, w)) = positions
        && x.len() == lengths.len()
    {
        node.set_character_positions(x.to_vec());
        node.set_character_widths(w.to_vec());
    }
    node.set_value(value);
    let count = lengths.len();
    node.set_character_lengths(lengths);
    // 词的开头是 `u8`：一行超过 255 个字时不给（可选的信息）
    if count <= u8::MAX as usize {
        node.set_word_starts(words.into_iter().map(|w| w as u8).collect::<Vec<u8>>());
    }
    node
}

/// 字素簇：(起点, 终点) 字节偏移
pub(crate) fn clusters(text: &str) -> impl Iterator<Item = (usize, usize)> + '_ {
    let mut bounds = graphemes(text);
    let mut last = bounds.next().unwrap_or(0);
    std::iter::from_fn(move || {
        let next = bounds.next()?;
        let cluster = (last, next);
        last = next;
        Some(cluster)
    })
}

/// 字节偏移 `offset` 之前有几个字素簇（落在字素簇中间时算到它前面）
pub(crate) fn cluster_index(text: &str, offset: usize) -> usize {
    clusters(text).take_while(|&(_, end)| end <= offset).count()
}

/// 第 `index` 个字素簇的起点（字节偏移）；超出时是末尾
pub(crate) fn cluster_offset(text: &str, index: usize) -> usize {
    clusters(text).nth(index).map_or(text.len(), |(start, _)| start)
}

/// 片段版本用的哈希：一次吃 8 个字节、乘法混合。只用来看变没变，不用防碰撞攻击；
/// 默认的 SipHash 在长文的输入框里每打一个字都要把全文过一遍，慢好几倍
#[derive(Default)]
struct StampHasher(u64);

impl StampHasher {
    fn mix(&mut self, n: u64) {
        self.0 = (self.0.rotate_left(5) ^ n).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}

impl Hasher for StampHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        let (words, rest) = bytes.as_chunks::<8>();
        for &word in words {
            self.mix(u64::from_le_bytes(word));
        }
        let mut word = [0u8; 8];
        word[..rest.len()].copy_from_slice(rest);
        // 尾巴的长度也混进去：末尾补的 0 和真的 0 分得开
        self.mix(u64::from_le_bytes(word) ^ (rest.len() as u64) << 56);
    }

    fn write_u64(&mut self, n: u64) {
        self.mix(n);
    }
}

/// 一行片段的版本：文字、换行、位置变了就变
pub(crate) fn stamp(text: &str, line_break: bool, rect: [f32; 4]) -> u64 {
    let mut hasher = StampHasher::default();
    text.hash(&mut hasher);
    line_break.hash(&mut hasher);
    for v in rect {
        // 取到 1/64 像素，浮点数的微小抖动不算变
        ((v * 64.0).round() as i64).hash(&mut hasher);
    }
    hasher.finish()
}
