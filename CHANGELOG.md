# 更新记录

## 未发布

### 新增

- 拖放时跟着指针走的样子能限制位移（见教程第 11 章）：`drag_preview_axis(Direction::Row)` 只横着走、`Direction::Column` 只竖着走，`drag_preview_contained(true)` 碰到父元素的边就停住。横着的标签条、看板的列排序时不再上下飘，也不会被裁剪的父元素切掉一半。影子和抬起都生效，找放置目标照样看指针。
- 单边的内边距：`padding_left`、`padding_right`、`padding_top`、`padding_bottom`，不管调用的先后都盖过 `padding` 和 `padding_x` / `padding_y`。定时器 `after`、`every` 进了前导模块。
- 画布量文字：`Canvas::text_metrics(内容, &样式)` 返回 `CanvasTextMetrics`（宽、高、基线），和 `Canvas::text` 排得一样；自己的元素种类在 `update` 里用新的 `Drawing::record_in(cx, ..)` 画也量得出。
- 显示器列表 `aureole::displays()`（`DisplayInfo`：名字、位置、大小、缩放比例、是不是主显示器，`contains` 判断点在不在上面），和 `WindowHandle::position()` 是同一套坐标；测试工具 `Harness::set_displays`。
- 系统提示音 `aureole::beep()`：macOS、Windows 放用户在系统设置里选的那一个，Linux 返回 `false`；测试工具 `Harness::beep_count()`。平台层 `NotificationService::beep`（有默认实现）。
- 存值 `stored_value(..)` / `StoredValue<T>`：不需要响应的值（`Rc` 包着的服务、配置）换一个 `Copy` 的句柄，和信号一样归作用域，闭包直接捕获，不用先克隆；读写不订阅也不通知（见教程第 3 章）。
- 读已释放的信号、派生值时，panic 的位置是读的那一行，调试构建里信息还写着它是在哪一行创建的。`Signal::try_get`、`try_with`、`Memo::try_get`、`try_with` 在释放后返回 `None`。
- 平台层：`WindowService::run_outside_event_callback`，等这次系统事件处理完、在窗口库的事件回调外面执行（有默认实现：直接执行）。

### 改变

- 样式对象合起来时，后写的状态连同上面的悬停、焦点、按下一起盖过前面的：只改了"勾选"的颜色，勾上以后悬停、按下不再变回默认的；只改了平时的底色，悬停、按下时也是它（默认配着默认底色的悬停色一起让掉）。勾选、选中、展开、禁用这些别的样子照旧留着。以前靠默认悬停效果的，现在要在样式对象里自己写悬停。
- 调试构建默认显示框架的警告：应用没装 `tracing` 的订阅者时，`App::run` 装一个最小的，把框架自己的警告和错误（滚动视口高度是 0、属性设错了元素、快捷键写错、GPU 出错……）打到标准错误。自己装了订阅者的、发布构建不受影响；要自己装的话放在 `App::run` 之前。

### 修正

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
