//! macOS 的打印：`NSPrintPanel` 选打印机和纸张，`NSPrintOperation` 印一个只在打印时用的视图，它一页一页画交来的位图。

use std::cell::RefCell;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::{AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSImage, NSPrintInfo, NSPrintOperation, NSPrintPanel, NSPrintPanelOptions, NSPrintPanelResult, NSResponder, NSView,
};
use objc2_foundation::{NSInteger, NSObject, NSPoint, NSRange, NSRect, NSSize, NSString};
use raw_window_handle::HasWindowHandle;

use super::{PrintDialogDone, PrintDone, PrintSetup};
use crate::{PlatformError, RgbaImage};

/// 打印时画成的分辨率：系统按点交给打印机，不给打印机的点阵，按常见的激光打印机
const DPI: f32 = 300.0;

thread_local! {
    /// 对话框选好的、还没印的：编号和纸张设置
    static INFOS: RefCell<Vec<(u64, Retained<NSPrintInfo>)>> = const { RefCell::new(Vec::new()) };
}

/// 弹出打印面板：有父窗口时是它的表单，马上返回；没有时是独立的面板，关掉才返回。在主线程调用 `done`
pub fn print_dialog(parent: Option<&dyn HasWindowHandle>, done: PrintDialogDone) {
    let Some(mtm) = MainThreadMarker::new() else {
        return done(Err(PlatformError::System("打印面板只能在主线程打开".into())));
    };
    // 共用的设置：上次选的打印机、纸张，和别的 macOS 应用一样记着
    let info = NSPrintInfo::sharedPrintInfo();
    let panel = NSPrintPanel::printPanel(mtm);
    panel.setOptions(NSPrintPanelOptions::ShowsCopies | NSPrintPanelOptions::ShowsPaperSize | NSPrintPanelOptions::ShowsOrientation);
    let finish = {
        let info = info.clone();
        move |printed: bool| {
            if !printed {
                return done(Ok(None));
            }
            let setup = setup_of(&info);
            INFOS.with(|infos| infos.borrow_mut().push((setup.id, info)));
            done(Ok(Some(setup)));
        }
    };
    match parent.and_then(crate::system::macos::ns_window) {
        Some(window) => {
            let finish = RefCell::new(Some(finish));
            let handler = RcBlock::new(move |result: NSPrintPanelResult| {
                if let Some(finish) = finish.borrow_mut().take() {
                    finish(result == NSPrintPanelResult::Printed);
                }
            });
            panel.beginSheetUsingPrintInfo_onWindow_completionHandler(&info, &window, Some(&handler));
        }
        // 模态的面板转自己的循环：在窗口库的事件回调外面开（不然这期间窗口库把事件都攒着，画面停住）。NSModalResponseOK 是 1
        None => crate::macos::run_outside_event_callback(Box::new(move || finish(panel.runModalWithPrintInfo(&info) == 1))),
    }
}

/// 纸张设置换成点（左上角为原点）
fn setup_of(info: &NSPrintInfo) -> PrintSetup {
    let paper = info.paperSize();
    let bounds = info.imageablePageBounds();
    // 能印到的范围是左下角为原点的
    let top = paper.height - bounds.origin.y - bounds.size.height;
    PrintSetup {
        paper: [paper.width as f32, paper.height as f32],
        printable: [bounds.origin.x as f32, top as f32, bounds.size.width as f32, bounds.size.height as f32],
        dpi: DPI,
        id: super::next_id(),
    }
}

/// 印：每页铺满能印到的范围，显示系统的进度面板。在窗口库的事件回调外面印（打印操作转自己的模态循环），印完在主线程调用 `done`
pub fn print(setup: PrintSetup, title: &str, pages: Vec<RgbaImage>, done: PrintDone) {
    let Some(mtm) = MainThreadMarker::new() else {
        return done(Err(PlatformError::System("只能在主线程打印".into())));
    };
    let info = INFOS.with(|infos| {
        let mut infos = infos.borrow_mut();
        infos.iter().position(|(id, _)| *id == setup.id).map(|index| infos.remove(index).1)
    });
    let Some(info) = info else {
        return done(Err(PlatformError::System("这次打印已经印过了，或者不是对话框给的".into())));
    };
    if let Err(error) = super::check_pages(&pages) {
        return done(Err(error));
    }
    if pages.is_empty() {
        return done(Ok(true));
    }
    // 页边距就是打印机印不到的地方：视图的一页正好是能印到的范围
    let [left, top, width, height] = setup.printable.map(f64::from);
    let [paper_width, paper_height] = setup.paper.map(f64::from);
    info.setLeftMargin(left);
    info.setTopMargin(top);
    info.setRightMargin((paper_width - left - width).max(0.0));
    info.setBottomMargin((paper_height - top - height).max(0.0));
    let mut images = Vec::with_capacity(pages.len());
    for (index, page) in pages.iter().enumerate() {
        let Some(rep) = crate::clipboard::macos::bitmap_of(page) else {
            return done(Err(PlatformError::System(format!("第 {} 页转不成位图", index + 1).into())));
        };
        let image = NSImage::initWithSize(NSImage::alloc(), NSSize::new(width, height));
        image.addRepresentation(&rep);
        images.push(image);
    }
    let view = PrintView::new(mtm, images, NSSize::new(width, height));
    let operation = NSPrintOperation::printOperationWithView_printInfo(&view, &info);
    operation.setShowsPrintPanel(false);
    operation.setShowsProgressPanel(true);
    operation.setJobTitle(Some(&NSString::from_str(title)));
    // 没印完：用户在进度面板上取消了（出错时系统自己提示）。视图要活到印完
    crate::macos::run_outside_event_callback(Box::new(move || {
        let printed = operation.runOperation();
        drop(view);
        done(Ok(printed));
    }));
}

struct Ivars {
    /// 每一页，第一页在最上面
    pages: Vec<Retained<NSImage>>,
    /// 一页的大小（点）
    page: NSSize,
}

define_class!(
    // SAFETY: NSView 的子类不需要额外的约定；PrintView 没有实现 Drop。下面的方法和 NSView 声明的签名一致
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "AureolePrintView"]
    #[ivars = Ivars]
    struct PrintView;

    impl PrintView {
        /// 自己分页：一页一张图
        #[unsafe(method(knowsPageRange:))]
        fn knows_page_range(&self, range: *mut NSRange) -> bool {
            if !range.is_null() {
                // SAFETY: 系统给的指针不为空时指向一个能写的范围
                unsafe { *range = NSRange::new(1, self.ivars().pages.len()) };
            }
            true
        }

        /// 第 `page` 页（从 1 数）在视图里的范围：视图不翻转，第一页在最上面
        #[unsafe(method(rectForPage:))]
        fn rect_for_page(&self, page: NSInteger) -> NSRect {
            self.page_rect(page.max(1) as usize - 1)
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, dirty: NSRect) {
            for (index, image) in self.ivars().pages.iter().enumerate() {
                let rect = self.page_rect(index);
                let below = rect.origin.y + rect.size.height <= dirty.origin.y;
                let above = rect.origin.y >= dirty.origin.y + dirty.size.height;
                if !below && !above {
                    image.drawInRect(rect);
                }
            }
        }
    }
);

impl PrintView {
    fn new(mtm: MainThreadMarker, pages: Vec<Retained<NSImage>>, page: NSSize) -> Retained<Self> {
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(page.width, page.height * pages.len() as f64));
        let this = Self::alloc(mtm).set_ivars(Ivars { pages, page });
        // SAFETY: initWithFrame: 是 NSView 的初始化方法，返回初始化好的同一个对象；实例变量在上一行已经设好
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn page_rect(&self, index: usize) -> NSRect {
        let Ivars { pages, page } = self.ivars();
        let y = page.height * (pages.len().saturating_sub(index + 1)) as f64;
        NSRect::new(NSPoint::new(0.0, y), *page)
    }
}
