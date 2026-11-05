/// 渲染目标的物理像素尺寸和缩放系数
#[derive(Clone, Copy, Debug)]
pub struct Viewport {
    /// 宽，物理像素
    pub width: u32,
    /// 高，物理像素
    pub height: u32,
    /// 每逻辑像素对应的物理像素数（显示器的缩放比例）
    pub scale_factor: f32,
}
