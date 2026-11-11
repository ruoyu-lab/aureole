# 更新记录

## 0.0.1-beta.3

第三个测试版：多了试验性的 CEF 视图（新的 crate `aureole-webview`），框架这边补上了外部纹理、打印、表格、跨窗口拖放、
自定义的剪贴板和拖放格式；macOS 的低电量模式下只出 60 帧，修了 Windows 上改窗口大小、安装程序和字符网格的问题。

### 新增

- **CEF 视图**（新的 crate `aureole-webview`，试验性）：内嵌 CEF 内核，画面离屏画进外部纹理，当成普通元素合成，圆角、透明度、变换、动画对它都有效。
  第一个视图挂载时才加载内核。页面要应用出面的事（对话框、选文件、下载、权限、新窗口、右键菜单、认证、证书错误）是控制器上的钩子，
  页面的状态是信号，配置（`WebProfile`）管 Cookie、缓存、代理、请求拦截、用户脚本，`window.aureole` 互发消息，`app://` 放应用自带的页面。
  macOS、Windows 上能用、能用 `cargo bundle` 打包（macOS 默认开沙箱，Windows 可以开）；Linux 上能编译，没在真机上跑过。
  编译时下载内核、要 cmake 和 Ninja，不在框架的工作区里，单独依赖，见[它的说明](https://github.com/ruoyu-lab/aureole/blob/master/crates/webview/README.md)。
- `is_low_power_mode()`：系统开没开低电量模式（macOS）、节电模式（Windows）。
- `WindowHandle::refresh_rate()`：窗口所在显示器的刷新率（每次调用问一次系统）。
- **外部纹理**：框架以外的代码往 GPU 纹理里画、交给渲染器合成。`ExternalTexture::new()` 拿编号，`set(gpu, 纹理)`、`changed()`；元素种类用 `PaintContext::external_texture(编号)` 铺满内容区，圆角、透明度、变换、裁剪和图片一样；主线程用 `shared_gpu()` 拿窗口共用的设备。
- **运行中挂退出钩子**：`at_exit(闭包)`，排在 `App::on_exit` 挂的后面。
- **打印**：`PrintJob::new(|page: &PageSetup| 每一页的元素).title(..).print().await` 弹出系统的打印对话框，用户选好打印机和纸张以后按能印到的范围排版，每页画成位图（按浅色外观，最多 300 点每英寸）交给打印机，结果是 `PrintOutcome`。macOS、Windows 有（`Capabilities::supports_print`）。
- **元素之间的拖放跨窗口**：拖着元素到了同一个应用的另一个窗口时交给系统的拖放，数据留在进程里，那个窗口的元素照常进入、经过、离开、`on_drop`；结果回到源元素的 `on_drag_out_end`。窗口宿主多了 `HostWindow::app_window_at`、`WindowTree::element_drag_over` / `element_drag_leave` / `element_drop`，测试工具有 `set_app_windows`、`take_drag_out_request`。macOS、Windows 有。
- **拖进来读带格式的文字和应用自己定的格式**：`ExternalDropEvent::rich_text`、`ExternalDropEvent::custom(格式名)`，自己定的格式要先 `aureole::accept_drop_format(格式名)` 登记（在 `App::run` 之前）；`ExternalDataKinds` 多了 `RICH_TEXT`、`CUSTOM`。macOS、Windows 有。
- **拖到应用外面带更多格式**：`DragOut::image`、`DragOut::rich_text`、`DragOut::custom`，`with_text`、`with_image`、`with_rich_text`、`with_custom`、`with_files` 一起拖几种，和剪贴板一样接收的程序挑自己认的；只拖图片时拖动的图像是这张图。macOS、Windows 有。
- **剪贴板放应用自己定的格式**：`ClipboardContent::custom(格式名, 字节)`、`clipboard::read_custom(格式名)`，字节原样放、原样读，可以和标准格式一起放；能力是 `supports_clipboard_custom`（macOS、Windows 有，Linux 没有）。
- **把元素滚进视口**：`ElementHandle`（`.element_handle(h)` 挂到任何元素上），`scroll_into_view()` 平滑、`jump_into_view()` 立即，由内到外每一层视口都滚到正好露出它。请求在下一次布局之后执行，刚加进来的元素同一刻就能要求滚过去。
- **表格**：`table(数据, key)` 加 `TableColumn`，十万行也只挂载看得见的部分。固定表头、横向滚动、单选和多选（`Signal<Option<K>>`、`Signal<Vec<K>>`）、键盘（方向键、翻页、Home、End，Shift 选一段，主修饰键 + A 全选）、拖动调列宽、点表头改排序状态（数据由应用排）、单元格自己搭、`.row_element` 给行加东西，每个部位都收样式对象，读屏软件能读出行和列头；能选中时每一格带着这一行的选中状态，Windows 的读屏软件也知道哪些行选中了。
- 无障碍：`accessibility_active` 标出有焦点的容器里的当前项，读屏软件的焦点跟着它走。
- `ListSource::reader`、`ListReader`：在虚拟列表外面按引用读同一份数据。
- `font_family` 可以写逗号隔开的几个字体：`.font_family("Monaco, Menlo")`，前面的优先。以前整串被当成一个名字，找不到时用系统的比例字体，字符网格的排版会错乱。

### 改变

- **点击按画出来的形状判断**：圆角外面、路径没画到的地方（填充按填充规则，描边看离线条够不够近）不再算点中，点击落到下面的元素上；以前按布局矩形，圆角按钮的角外面、图标笔画之间的空隙也算。角上有子元素时：元素没裁剪子元素照样点得到，`clip(true)` 的连子元素一起点不到。直接把点击挂在路径图标上的，点空隙不再触发：把点击放到外面的容器上。

### 修正

- **macOS 开着低电量模式时框架白画一半的帧**：系统只显示每秒 60 帧，显示链接照样按 120Hz 走。现在帧时钟降到每秒 60 帧，开了、关了时跟着换。
- **卸载程序没卸完时安装目录里的 `uninstall.exe` 也被删掉**（Windows）：卸载窗口直接关掉、点取消、卸到一半取消或失败（已经撤销）、静默卸载失败，退出时都会删掉原来的卸载程序，之后"应用和功能"里再卸载报找不到文件。现在只有卸载完成了才删原来的 `uninstall.exe` 和空了的安装目录，别的情况只删拷到临时目录的那份。
- **默认的安装向导在深色外观下字看不清**：页面写死白底，标题、复选框的文字跟着外观变浅。现在底色、文字、说明、错误的颜色都跟着外观。
- **默认向导左边一栏是名字的第一个字**：没给图片时显示应用图标（安装时从安装包里取，卸载时读安装目录里的那份；.ico 里取最大的一张），没有图标才用第一个字。
- **预发布的版本号在安装程序里被截掉后缀**：`0.2.1-beta` 在向导、"应用和功能"的显示版本、安装程序的文件名里照原样；只在系统只认数字的地方（应用包的版本、文件版本）写成 `0.2.1`。
- Windows 上 125%、150%、175% 缩放时，下拉选择弹出的列表、右键菜单、对话框这些浮层里的字发虚：浮层只取整到逻辑像素，150% 下逻辑坐标是奇数时整层落在半个物理像素上，合成时按半个像素采样。现在按窗口的缩放对齐到物理像素（100%、200% 不受影响，所以 Mac 的视网膜屏上看不出来）。
- 字体里没有的符号（⏺ ⎿ ⟳，Monaco、Courier New 里的 ❯ ✻ ✔）显示成缺字符号：系统的回退按文字种类找，问不到这些字。现在每个平台有一份符号字体接在回退的最后，只有原来缺字的字才用到它们，普通文字和字符网格都一样。Windows、Linux 的列表还没在真实桌面上验证过。
- 字符网格：四分之一方块（▖ ▗ ▙ ▛ 等）、粗线、双线、圆角、虚线、斜线的制表符用的是字体里的字，行高大于 1 时接不上。现在 U+2500 ~ U+259F 全部由框架自己画，铺满格子，和字体无关。
- 字符网格：回退来的字比格子宽很多时盖到旁边的格子上，现在缩小到放得下。
- Windows 上拖动窗口边缘改大小时画面几乎不动，松手后要过将近一秒才跟上，新扩出来的地方先是黑的：鼠标拖动每秒几百次改大小，渲染线程每一次都重建交换链（一次十几毫秒），一帧都画不出来。现在积压的几次改大小只按最后一次重建。RTX 4090 上实测：拖动中从 0 帧到每秒约 40 帧，松手后画出最终大小从 832 毫秒到 12 毫秒。
- Windows、Linux 上一个小窗口也占两三百 MB：wgpu 的显存分配器（Vulkan、DX12）按"性能优先"一开始就向驱动要 128MB、64MB 起的整块，核显上这些都算进进程的内存。改成按小块要，同一个窗口在核显上 283MB → 104MB，在 RTX 4090 上独占显存 254MB → 74MB；框架自己的图集、纹理、缓冲没变。macOS 不受影响。

## 0.0.1-beta.2

第二个测试版：框架自带的样子有了深色，默认跟着系统；补上了写应用时常用到的几项能力，修了 macOS 上窗口位置和缩放的问题。有破坏性改动，见"改变"。

### 新增

- **深色**：应用的外观 `AppearanceMode`（`System` 跟着系统，默认；`Light`；`Dark`），`App::appearance_mode(..)`、`set_appearance_mode(..)`、`appearance_mode()`，生效的外观 `appearance()`（响应式）。框架自带的样子有浅色、深色两套调色板（`defaults::Palette::LIGHT`、`Palette::DARK`），`defaults::palette()` 按生效的外观取，自己的颜色要跟着深浅变时在闭包里读它。
- 拖放时跟着指针走的样子能限制位移（见教程第 11 章）：`drag_preview_axis(Direction::Row)` 只横着走、`Direction::Column` 只竖着走，`drag_preview_contained(true)` 碰到父元素的边就停住。横着的标签条、看板的列排序时不再上下飘，也不会被裁剪的父元素切掉一半。影子和抬起都生效，找放置目标照样看指针。
- 单边的内边距：`padding_left`、`padding_right`、`padding_top`、`padding_bottom`，不管调用的先后都盖过 `padding` 和 `padding_x` / `padding_y`。定时器 `after`、`every` 进了前导模块。
- 画布量文字：`Canvas::text_metrics(内容, &样式)` 返回 `CanvasTextMetrics`（宽、高、基线），和 `Canvas::text` 排得一样；自己的元素种类在 `update` 里用新的 `Drawing::record_in(cx, ..)` 画也量得出。
- 显示器列表 `aureole::displays()`（`DisplayInfo`：名字、位置、大小、缩放比例、是不是主显示器，`contains` 判断点在不在上面），和 `WindowHandle::position()` 是同一套坐标；测试工具 `Harness::set_displays`。
- 系统提示音 `aureole::beep()`：macOS、Windows 放用户在系统设置里选的那一个，Linux 返回 `false`；测试工具 `Harness::beep_count()`。平台层 `NotificationService::beep`（有默认实现）。
- 存值 `stored_value(..)` / `StoredValue<T>`：不需要响应的值（`Rc` 包着的服务、配置）换一个 `Copy` 的句柄，和信号一样归作用域，闭包直接捕获，不用先克隆；读写不订阅也不通知（见教程第 3 章）。
- 读已释放的信号、派生值时，panic 的位置是读的那一行，调试构建里信息还写着它是在哪一行创建的。`Signal::try_get`、`try_with`、`Memo::try_get`、`try_with` 在释放后返回 `None`。
- 平台层：`WindowService::run_outside_event_callback`，等这次系统事件处理完、在窗口库的事件回调外面执行（有默认实现：直接执行）。

### 改变

- **框架自带的样子默认跟着系统的深浅色**：窗口底色、没设颜色的文字、选区、滚动条、基础控件、输入框、提示、窗口的标题栏和材质在深色系统上换成深色那套（只换中性色，强调色不变）；以前一直是浅色。自己写死了浅色背景、又没设文字颜色的地方会变成浅底浅字：颜色写成读 `palette()` 的闭包，或者 `App::appearance_mode(AppearanceMode::Light)` 固定成浅色。`aureole::defaults` 的颜色常量（`BACKGROUND`、`SURFACE`、`TEXT`、`ACCENT`……）换成了调色板的字段：`Palette::LIGHT.text`、`palette().text`。
- 样式对象合起来时，后写的状态连同上面的悬停、焦点、按下一起盖过前面的：只改了"勾选"的颜色，勾上以后悬停、按下不再变回默认的；只改了平时的底色，悬停、按下时也是它（默认配着默认底色的悬停色一起让掉）。勾选、选中、展开、禁用这些别的样子照旧留着。以前靠默认悬停效果的，现在要在样式对象里自己写悬停。
- 调试构建默认显示框架的警告：应用没装 `tracing` 的订阅者时，`App::run` 装一个最小的，把框架自己的警告和错误（滚动视口高度是 0、属性设错了元素、快捷键写错、GPU 出错……）打到标准错误。自己装了订阅者的、发布构建不受影响；要自己装的话放在 `App::run` 之前。

### 修正

- Windows 上应用是深色时，窗口材质（Mica）仍然按浅色画：现在在窗口上写深色标记（设材质之前），深浅变了再设一次材质类型。Windows 是实验性的，这一条还没在真机上确认过。
- macOS 上 `Window::position(x, y)` 放的是内容区的左上角，`WindowHandle::position()` 读的却是整个窗口的，差一个标题栏的高度：存下位置、下次启动再放回去时，窗口每次往上漂一截。现在三者都是整个窗口连同标题栏的左上角。
- macOS 上双击拖动区域、`WindowHandle::set_maximized` 缩放窗口时，动画的约 0.35 秒里内容停着不动，结束后才一下跳到新的大小。现在缩放动画的每一步都按新的大小出一帧，`is_maximized` 在缩放中保持设的值。

## 0.0.1-beta.1

第一个公开的测试版。API 还会改，第一个正式版之前任何版本都可能有破坏性改动。

有这些：

- 窗口和事件循环、跟着显示器刷新的帧调度、输入法、渲染（wgpu）
- 细粒度信号、链式 API、样式对象
- 布局、文字和富文本、图片和矢量图标、画布、虚拟列表、滚动
- 动画：过渡、弹簧、关键帧、时间线、共享元素，合成层的动画在渲染线程上推进
- 基础控件：按钮、输入框、复选框、开关、单选组、下拉选择、滑块、进度条、数字输入、日期、时间、颜色、文件选择
- 拖放、快捷键、菜单栏、托盘、通知、无障碍
- 不开窗口的测试工具（`testing` 特性）、文件热加载和重新编译后原地重启（`devtools` 特性）
- 打包工具 `cargo bundle` 和用 Aureole 写的安装程序

平台：主要支持 macOS；Windows、Linux 是实验性的，还没在真实桌面上验证过。
