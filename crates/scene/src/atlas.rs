//! 字形和矢量路径的图集。CPU 侧负责装箱、保留像素副本、记录待上传的更新；
//! GPU 侧（渲染器的 `Renderer::apply_atlas`）按更新维护纹理。
//! 两者之间只传 [`AtlasUpdate`]，所以栅格化可以在任何线程做。

/// 图集种类：单色字形存覆盖率（1 字节），彩色字形（emoji）存 sRGB RGBA，
/// 矢量路径存覆盖率（1 字节）。路径单独一页：会变的路径（动画、图表）装满时只清它这一页，
/// 不影响字形
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AtlasKind {
    /// 单色字形，覆盖率
    Mask,
    /// 彩色字形，sRGB RGBA
    Color,
    /// 矢量路径，覆盖率
    Path,
}

impl AtlasKind {
    /// 每个像素的字节数：覆盖率 1，RGBA 4
    pub fn bytes_per_pixel(self) -> u32 {
        match self {
            Self::Mask | Self::Path => 1,
            Self::Color => 4,
        }
    }
}

/// CPU 侧图集交给 GPU 侧的一条更新，按顺序应用（渲染器的 `Renderer::apply_atlas`）
#[derive(Clone, Debug)]
pub enum AtlasUpdate {
    /// 重建该图集的纹理并清空，边长 `size`
    Reset {
        /// 哪个图集
        kind: AtlasKind,
        /// 新的边长，像素
        size: u32,
    },
    /// 把一块像素（逐行紧密排列）写到图集的 (x, y)
    Upload {
        /// 哪个图集
        kind: AtlasKind,
        /// 左上角横坐标，像素
        x: u32,
        /// 左上角纵坐标，像素
        y: u32,
        /// 宽，像素
        width: u32,
        /// 高，像素
        height: u32,
        /// 像素数据，每像素字节数见 [`AtlasKind::bytes_per_pixel`]
        pixels: Vec<u8>,
    },
    /// 图片图集和大图纹理（见 [`crate::ImageUpdate`]），和字形一起按顺序交给渲染线程
    Image(crate::ImageUpdate),
}

/// 图集的最大边长（设备的纹理边长上限更小时按设备的，见 [`max_atlas_size`]）
pub const MAX_ATLAS_SIZE: u32 = 4096;

/// 这台设备上图集的最大边长：[`MAX_ATLAS_SIZE`] 和设备的纹理边长上限（[`crate::max_texture_size`]）取小的
pub fn max_atlas_size() -> u32 {
    MAX_ATLAS_SIZE.min(crate::max_texture_size())
}
/// 字形之间留空，避免线性采样时串色
const PADDING: u32 = 1;

/// 字形和路径图集的 CPU 侧：装箱、保留像素副本、攒下待上传的 [`AtlasUpdate`]
pub struct GlyphAtlas {
    mask: Page,
    color: Page,
    path: Page,
    updates: Vec<AtlasUpdate>,
}

impl Default for GlyphAtlas {
    fn default() -> Self {
        Self::new()
    }
}

impl GlyphAtlas {
    /// 三页空图集：单色 512、彩色 256、路径 512 像素见方
    pub fn new() -> Self {
        let mask = Page::new(AtlasKind::Mask, 512);
        let color = Page::new(AtlasKind::Color, 256);
        let path = Page::new(AtlasKind::Path, 512);
        let updates = vec![mask.reset_update(), color.reset_update(), path.reset_update()];
        Self { mask, color, path, updates }
    }

    fn page(&mut self, kind: AtlasKind) -> &mut Page {
        match kind {
            AtlasKind::Mask => &mut self.mask,
            AtlasKind::Color => &mut self.color,
            AtlasKind::Path => &mut self.path,
        }
    }

    /// 放入一张位图（逐行紧密排列），返回归一化的 UV `[u0, v0, u1, v1]`；放不下返回 `None`
    pub fn insert(&mut self, kind: AtlasKind, width: u32, height: u32, pixels: &[u8]) -> Option<[f32; 4]> {
        debug_assert_eq!(pixels.len(), (width * height * kind.bytes_per_pixel()) as usize);
        let page = self.page(kind);
        let [x, y] = page.allocate(width + PADDING, height + PADDING)?;
        page.blit(x, y, width, height, pixels);
        let size = page.size as f32;
        let uv = [x as f32 / size, y as f32 / size, (x + width) as f32 / size, (y + height) as f32 / size];
        self.updates.push(AtlasUpdate::Upload { kind, x, y, width, height, pixels: pixels.to_vec() });
        Some(uv)
    }

    /// 预留一块区域（内容是空的），返回左上角的像素位置；放不下返回 `None`。
    /// 之后用 [`Self::write`] 反复改写里面的内容（每帧都变的东西，比如变形中的路径，不用每帧占一块新的）
    pub fn reserve(&mut self, kind: AtlasKind, width: u32, height: u32) -> Option<[u32; 2]> {
        let page = self.page(kind);
        let [x, y] = page.allocate(width + PADDING, height + PADDING)?;
        let blank = vec![0; (width * height * kind.bytes_per_pixel()) as usize];
        page.blit(x, y, width, height, &blank);
        Some([x, y])
    }

    /// 把一块像素写到 (x, y)（[`Self::reserve`] 预留的区域里），并记下要上传的更新
    pub fn write(&mut self, kind: AtlasKind, x: u32, y: u32, width: u32, height: u32, pixels: &[u8]) {
        debug_assert_eq!(pixels.len(), (width * height * kind.bytes_per_pixel()) as usize);
        let page = self.page(kind);
        if x + width > page.size || y + height > page.size {
            return;
        }
        page.blit(x, y, width, height, pixels);
        self.updates.push(AtlasUpdate::Upload { kind, x, y, width, height, pixels: pixels.to_vec() });
    }

    /// 图集满了：边长翻倍（最大 [`max_atlas_size`]）并清空。调用方要丢掉自己缓存的 UV
    pub fn grow(&mut self, kind: AtlasKind) {
        let page = self.page(kind);
        let size = (page.size * 2).min(max_atlas_size());
        *page = Page::new(kind, size);
        let update = page.reset_update();
        self.updates.push(update);
    }

    /// 某一页的边长和用量：装箱已经用到的高度占边长的比例（0 到 1）
    pub fn usage(&self, kind: AtlasKind) -> (u32, f32) {
        let page = match kind {
            AtlasKind::Mask => &self.mask,
            AtlasKind::Color => &self.color,
            AtlasKind::Path => &self.path,
        };
        (page.size, page.next_y as f32 / page.size as f32)
    }

    /// 取走自上次以来的更新，交给 GPU 侧
    pub fn take_updates(&mut self) -> Vec<AtlasUpdate> {
        std::mem::take(&mut self.updates)
    }

    /// 完整内容，用于给新的 GPU 侧（例如离屏渲染）建立图集
    pub fn snapshot(&self) -> Vec<AtlasUpdate> {
        [&self.mask, &self.color, &self.path]
            .into_iter()
            .flat_map(|page| {
                [
                    page.reset_update(),
                    AtlasUpdate::Upload {
                        kind: page.kind,
                        x: 0,
                        y: 0,
                        width: page.size,
                        height: page.size,
                        pixels: page.pixels.clone(),
                    },
                ]
            })
            .collect()
    }
}

/// 一页图集，按"货架"装箱：同一行放高度相近的字形
struct Page {
    kind: AtlasKind,
    size: u32,
    pixels: Vec<u8>,
    shelves: Vec<Shelf>,
    next_y: u32,
}

struct Shelf {
    y: u32,
    height: u32,
    next_x: u32,
}

impl Page {
    fn new(kind: AtlasKind, size: u32) -> Self {
        let len = (size * size * kind.bytes_per_pixel()) as usize;
        Self { kind, size, pixels: vec![0; len], shelves: Vec::new(), next_y: 0 }
    }

    fn reset_update(&self) -> AtlasUpdate {
        AtlasUpdate::Reset { kind: self.kind, size: self.size }
    }

    fn allocate(&mut self, width: u32, height: u32) -> Option<[u32; 2]> {
        if width > self.size || height > self.size {
            return None;
        }
        // 找一个高度合适（不浪费超过一半）且还放得下的货架
        let fits = |shelf: &&mut Shelf| {
            shelf.height >= height && shelf.height <= height + height / 2 + 2 && shelf.next_x + width <= self.size
        };
        if let Some(shelf) = self.shelves.iter_mut().find(fits) {
            let x = shelf.next_x;
            shelf.next_x += width;
            return Some([x, shelf.y]);
        }
        if self.next_y + height > self.size {
            return None;
        }
        let y = self.next_y;
        self.next_y += height;
        self.shelves.push(Shelf { y, height, next_x: width });
        Some([0, y])
    }

    fn blit(&mut self, x: u32, y: u32, width: u32, height: u32, pixels: &[u8]) {
        let bpp = self.kind.bytes_per_pixel() as usize;
        let row = width as usize * bpp;
        for r in 0..height as usize {
            let dst = ((y as usize + r) * self.size as usize + x as usize) * bpp;
            self.pixels[dst..dst + row].copy_from_slice(&pixels[r * row..(r + 1) * row]);
        }
    }
}
