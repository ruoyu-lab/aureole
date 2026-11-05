//! 测试里离不开核心的那部分：元素树的快照（测试工具的 `element_tree`、`element_info` 读的）、
//! 测试工具的事件来源、生成动图的编码器。测试工具本身（`Harness`、回放、截图）在 `aureole-testing` 里，
//! 门面的 `aureole::testing` 把两边合在一起。

use std::time::Duration;

use crate::input::{Buttons, GestureInput, InputKind, Key, KeyInput, Modifiers, MouseButton, PointerId, PointerInput, GesturePhase, WheelDelta, WheelInput};

mod elements;

pub use elements::{AnimationInfo, Edges, ElementFlags, ElementInfo, ElementNode, ElementTree, PaintInfo};

/// 测试工具的事件来源：记着指针的位置、按着的键、修饰键，按真实窗口的样子生成输入事件
/// （测试工具在 `aureole-testing` 里）
#[doc(hidden)]
#[derive(Default)]
pub struct Source {
    pointer: Option<[f32; 2]>,
    buttons: Buttons,
    modifiers: Modifiers,
    /// 屏幕上的手指和它们最后的位置
    fingers: Vec<(PointerId, [f32; 2])>,
}

impl Source {
    /// 指针此刻的状态（位置、按着的键、修饰键），`button` 是这次按下、松开的键
    pub fn pointer_input(&self, button: Option<MouseButton>) -> PointerInput {
        PointerInput { position: self.pointer, button, buttons: self.buttons, modifiers: self.modifiers, ..PointerInput::mouse(None) }
    }

    /// 指针移到 `point`；`None` 是离开窗口
    pub fn moved(&mut self, point: Option<[f32; 2]>) -> InputKind {
        self.pointer = point;
        match point {
            Some(_) => InputKind::PointerMove(self.pointer_input(None)),
            None => InputKind::PointerLeave(self.pointer_input(None)),
        }
    }

    /// 在指针此刻的位置按下、松开
    pub fn button(&mut self, button: MouseButton, pressed: bool) -> InputKind {
        self.buttons.set(Buttons::from_button(button), pressed);
        let input = self.pointer_input(Some(button));
        if pressed { InputKind::PointerDown(input) } else { InputKind::PointerUp(input) }
    }

    /// 修饰键变了
    pub fn modifiers(&mut self, modifiers: Modifiers) -> InputKind {
        self.modifiers = modifiers;
        InputKind::Modifiers(modifiers)
    }

    /// 在指针此刻的位置滚动
    pub fn wheel(&self, delta: WheelDelta, phase: GesturePhase) -> InputKind {
        InputKind::Wheel(WheelInput { delta, phase, position: self.pointer, modifiers: self.modifiers })
    }

    /// 在指针此刻的位置捏合、旋转
    pub fn gesture(&self, delta: f32, phase: GesturePhase) -> GestureInput {
        GestureInput { delta, phase, position: self.pointer }
    }

    fn finger(&self, finger: PointerId, point: [f32; 2]) -> PointerInput {
        PointerInput { modifiers: self.modifiers, ..PointerInput::touch(finger, point) }
    }

    /// 手指放上 `point`
    pub fn touch_down(&mut self, finger: PointerId, point: [f32; 2]) -> InputKind {
        self.fingers.retain(|(id, _)| *id != finger);
        self.fingers.push((finger, point));
        InputKind::PointerDown(PointerInput { button: Some(MouseButton::Left), ..self.finger(finger, point) })
    }

    /// 手指移到 `point`
    pub fn touch_move(&mut self, finger: PointerId, point: [f32; 2]) -> InputKind {
        if let Some((_, at)) = self.fingers.iter_mut().find(|(id, _)| *id == finger) {
            *at = point;
        }
        InputKind::PointerMove(self.finger(finger, point))
    }

    fn lift(&mut self, finger: PointerId) -> PointerInput {
        let point = self.fingers.iter().find(|(id, _)| *id == finger).map_or([0.0; 2], |(_, at)| *at);
        self.fingers.retain(|(id, _)| *id != finger);
        PointerInput { buttons: Buttons::empty(), ..self.finger(finger, point) }
    }

    /// 手指抬起（在它最后的位置）
    pub fn touch_up(&mut self, finger: PointerId) -> InputKind {
        InputKind::PointerUp(PointerInput { button: Some(MouseButton::Left), ..self.lift(finger) })
    }

    /// 系统收走了这根手指
    pub fn touch_cancel(&mut self, finger: PointerId) -> InputKind {
        InputKind::PointerCancel(self.lift(finger))
    }
}

/// 键入一个字符：按下（带上输入的文字）再松开，不经过输入法
#[doc(hidden)]
pub fn typed(ch: char) -> [InputKind; 2] {
    let text = ch.to_string();
    let key = Key::Character(text.as_str().into());
    [
        InputKind::KeyDown(KeyInput { text: Some(text.as_str().into()), ..KeyInput::new(key.clone(), Modifiers::empty()) }),
        InputKind::KeyUp(KeyInput::new(key, Modifiers::empty())),
    ]
}

/// 动图的格式，见 [`encode_animation`]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AnimationFormat {
    /// GIF：每帧最多 256 色，延迟按 10 毫秒取整
    Gif,
    /// 动态 WebP（每帧无损）
    WebP,
}

/// 测试和演示用：把几帧像素编码成动图，不用往仓库里放二进制文件。`frames` 每帧是 RGBA8（直通 alpha，
/// 逐行紧密排列，都是 `width` x `height`），`delays` 是每帧的毫秒数，`loops` 是一共播几遍（0 表示一直循环）。
///
/// ```
/// use aureole::testing::{AnimationFormat, encode_animation};
/// let red = [255, 0, 0, 255].repeat(16 * 16);
/// let blue = [0, 0, 255, 255].repeat(16 * 16);
/// let gif = encode_animation(AnimationFormat::Gif, 16, 16, &[red, blue], &[aureole::ms(100), aureole::ms(200)], 0);
/// assert_eq!(&gif[..3], b"GIF");
/// ```
pub fn encode_animation(format: AnimationFormat, width: u32, height: u32, frames: &[Vec<u8>], delays: &[Duration], loops: u32) -> Vec<u8> {
    assert_eq!(frames.len(), delays.len(), "每帧要有一个延迟");
    let mut out = Vec::new();
    match format {
        AnimationFormat::Gif => {
            use image::codecs::gif::{GifEncoder, Repeat};
            let mut encoder = GifEncoder::new_with_speed(&mut out, 10);
            let repeat = if loops == 0 { Repeat::Infinite } else { Repeat::Finite(loops.min(u16::MAX as u32) as u16) };
            encoder.set_repeat(repeat).expect("写 GIF 失败");
            let frames = frames.iter().zip(delays).map(|(pixels, delay)| {
                let buffer = image::RgbaImage::from_raw(width, height, pixels.clone()).expect("像素数和大小不符");
                image::Frame::from_parts(buffer, 0, 0, image::Delay::from_numer_denom_ms(delay.as_millis() as u32, 1))
            });
            encoder.encode_frames(frames).expect("写 GIF 失败");
        }
        AnimationFormat::WebP => {
            // RIFF 容器：VP8X（有动画、有 alpha）+ ANIM（背景色、循环次数）+ 每帧一个 ANMF（位置、大小、时长、无损的 VP8L 数据）
            let chunk = |out: &mut Vec<u8>, tag: &[u8; 4], payload: &[u8]| {
                out.extend_from_slice(tag);
                out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                out.extend_from_slice(payload);
                if payload.len() % 2 == 1 {
                    out.push(0);
                }
            };
            let u24 = |v: u32| [v as u8, (v >> 8) as u8, (v >> 16) as u8];
            let mut body = b"WEBP".to_vec();
            let mut vp8x = vec![0x12, 0, 0, 0];
            vp8x.extend(u24(width - 1));
            vp8x.extend(u24(height - 1));
            chunk(&mut body, b"VP8X", &vp8x);
            let mut anim = vec![0, 0, 0, 0];
            anim.extend((loops.min(u16::MAX as u32) as u16).to_le_bytes());
            chunk(&mut body, b"ANIM", &anim);
            for (pixels, delay) in frames.iter().zip(delays) {
                let mut still = Vec::new();
                image::codecs::webp::WebPEncoder::new_lossless(&mut still)
                    .encode(pixels, width, height, image::ExtendedColorType::Rgba8)
                    .expect("写 WebP 失败");
                let mut frame = Vec::new();
                frame.extend(u24(0));
                frame.extend(u24(0));
                frame.extend(u24(width - 1));
                frame.extend(u24(height - 1));
                frame.extend(u24(delay.as_millis() as u32));
                // 不和前一帧混合、不清除：每帧都是完整的画面
                frame.push(0b10);
                frame.extend_from_slice(webp_image_chunks(&still));
                chunk(&mut body, b"ANMF", &frame);
            }
            out.extend_from_slice(b"RIFF");
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend(body);
        }
    }
    out
}

/// 单张 WebP 文件里的图像数据块（跳过文件头和 VP8X 之类的元数据块）
fn webp_image_chunks(file: &[u8]) -> &[u8] {
    let mut at = 12;
    while at + 8 <= file.len() {
        let size = u32::from_le_bytes(file[at + 4..at + 8].try_into().expect("4 字节")) as usize;
        let tag = &file[at..at + 4];
        if tag == b"VP8L" || tag == b"VP8 " || tag == b"ALPH" {
            return &file[at..];
        }
        at += 8 + size + size % 2;
    }
    panic!("WebP 里没有图像数据")
}
