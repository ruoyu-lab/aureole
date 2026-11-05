//! 属性表：每个属性只在这里定义一次（`props!`，宏见 `property/registry.rs`）。
//!
//! 一条写名字、值的类型、默认值、存在哪、改了要重做什么、会不会继承、能不能动画（能不能交给渲染线程）、
//! 适用于哪些元素、绑定的顺序，以及写进节点、从节点读出来的函数。宏生成类型化的键（`property::GAP`）、编号
//! （`PropId::GAP`）、元数据表、`Element` 和样式对象 `Style` 上的链式方法、按名字的解析和格式化。
//!
//! **表按绑定的顺序写**（`order` 从小到大，编译时检查）：挂载时按这个顺序绑定，整体和局部是两个属性的
//! （`padding` 和 `padding_x`、`gap` 和 `gap_x`）局部的排在后面，盖过整体的，不管调用的先后。
//! 一次设几个属性的简写（`size`、`border`、`margin`、`inset`、`position`、`translation`……）手写在
//! `element/property_methods.rs`，只是依次设这几个属性。
//!
//! 加一个属性：在这里加一条（放在绑定顺序里该在的位置），再在 `crates/aureole/tests/fonts/contracts.rs`
//! 的契约表里加一行。

use animation::Curve;
use scene::{Angle, Color};
use taffy::{
    AlignItems, Dimension, Display, FlexWrap, GridPlacement, JustifyContent, LengthPercentage, LengthPercentageAuto, Line, Position,
};

use super::{NodeId, Tree, TreeRef};
use crate::accessibility::{Live, RangeValue, Role, Toggled};
use crate::drag_drop::DragPreview;
use crate::editor::SubmitKey;
use crate::element::{Element, Shader};
use crate::gradient::Fill;
use crate::image::{ImageFit, ImageSource};
use crate::interaction::{Cursor, FocusRing};
use crate::layout::{Align, Direction, Justify, Length, Place, Tracks, align_items, flex_direction, justify_content};
use crate::property::registry::*;
use crate::property::{IntoProp, PropertyValue};
use crate::shadow::Shadows;
use crate::style::Style;
use crate::text::{InlineAlign, TextAlign};
use crate::vector::{VectorPath, Stroke};
use crate::visual_effects::{BlendMode, Filter, Mask};

props! {
    // ---- 名字、共享元素 ----

    /// 调试和测试用的名字：测试工具（`Harness::bounds`）按它找元素。可以绑定
    name: String = String::new() => {
        order: 10, storage: Node, dirty: BEHAVIOR, applies: ANY, inherited: false, anim: [],
        method: into, style: false, late: false, bind: typed,
        write: |t, id, v| {
            if let Some(cold) = t.nodes.cold_mut(id) {
                cold.name = (!v.is_empty()).then(|| v.into_boxed_str());
            }
        },
        read: |t, id| t.nodes.cold(id)?.name.as_deref().map(str::to_owned),
        reset: [],
    }

    /// 共享元素：同一个窗口里带着同一个标签的元素是"同一个东西"。新挂载的元素接替旧的那个时，从旧元素在屏幕上的位置和大小
    /// 飞到自己的位置（临界阻尼的弹簧，不回弹），比如列表里的缩略图变成详情页的大图：
    ///
    /// ```
    /// # use aureole::prelude::*;
    /// let open = signal(false);
    /// let photo = |w: f32, h: f32| rect().size(w, h).radius(8).background(Color::hex(0x138a84)).shared("photo-42");
    /// column()
    ///     .on_click(move || open.update(|o| *o = !*o))
    ///     .child_when(move || !open.get(), move || photo(64.0, 64.0))
    ///     .child_when(move || open.get(), move || photo(320.0, 200.0));
    /// ```
    ///
    /// - 同一个标签同时只显示一个元素：最后挂载的那个。旧的还挂着时先藏起来（不画、不接收指针），
    ///   新的离开（卸载、开始退场）后它重新显示，从新的那个此刻的位置飞回来，所以缩略图可以一直留在列表里
    /// - 旧的离开、新的挂载在同一次更新里，或者新的在下一帧之前挂载，都算接上
    /// - 飞行只动合成层的平移和缩放（宽高比不同时内容跟着拉伸），由渲染线程推进，不重新布局；飞的时候画在最上面、
    ///   不受祖先的裁剪（比如原来的滚动视口），也不跟着祖先淡入淡出。飞到一半再换回去时从此刻的位置和速度接着飞
    /// - 系统打开"减少动态效果"时不飞，新的淡入、旧的（还挂着的话）淡出
    /// - 标签只在一个窗口里有效；绑定的标签变了，元素换到新标签下（空字符串是不参加）
    shared: String = String::new() => {
        order: 20, storage: Depth, dirty: COMPOSITOR, applies: ANY, inherited: false, anim: [],
        method: into, style: false, late: false, bind: typed,
        write: |t, id, v| t.set_shared(id, &v),
        read: |t, id| t.nodes.get(id)?.extra.as_deref()?.shared.as_ref().map(|s| s.tag.to_string()),
        reset: [],
    }

    /// 共享元素飞行的曲线（默认是响应 0.4 秒的临界阻尼弹簧），见 [`Element::shared_with`]
    shared_curve: Curve = super::shared::FLIGHT_SPRING.into() => {
        order: 30, storage: Depth, dirty: BEHAVIOR, applies: ANY, inherited: false, anim: [],
        method: manual, style: false, late: false, bind: typed,
        write: |t, id, v| t.set_shared_curve(id, v),
        read: |t, id| t.nodes.get(id)?.extra.as_deref()?.shared.as_ref().map(|s| s.curve),
        reset: [],
    }

    // ---- 无障碍（多行输入框的转给里面的编辑区） ----

    /// 无障碍角色。不设时按元素推断（文字是标签、输入框是文本框、能点的是按钮……），
    /// 推断不出来的容器不出现在无障碍树里（子元素直接归到上一层）
    accessibility_role: Role = Role::GenericContainer => {
        order: 40, storage: Accessibility, dirty: ACCESSIBILITY, applies: ANY, inherited: false, anim: [],
        method: editor, style: false, late: false, bind: typed,
        write: |t, id, v| t.accessibility_set(id, |p| p.role = Some(v)),
        read: |t, id| t.accessibility.props.get(id)?.role,
        reset: [|t, id| t.accessibility_set(id, |p| p.role = None)],
    }

    /// 读屏软件念的名字。按钮、复选框、菜单项不设时用里面的文字；只有图标的按钮一定要设。
    /// 图片的名字就是替代文字：没有名字的图片（和矢量图标）当作装饰，辅助技术跳过
    accessibility_name: String = String::new() => {
        order: 50, storage: Accessibility, dirty: ACCESSIBILITY, applies: ANY, inherited: false, anim: [],
        method: editor_into, style: false, late: false, bind: typed,
        write: |t, id, v| t.accessibility_set(id, |p| p.label = Some(v)),
        read: |t, id| t.accessibility.props.get(id)?.label.clone(),
        reset: [|t, id| t.accessibility_set(id, |p| p.label = None)],
    }

    /// 补充说明，念完名字和状态后念
    accessibility_description: String = String::new() => {
        order: 60, storage: Accessibility, dirty: ACCESSIBILITY, applies: ANY, inherited: false, anim: [],
        method: editor_into, style: false, late: false, bind: typed,
        write: |t, id, v| t.accessibility_set(id, |p| p.description = Some(v)),
        read: |t, id| t.accessibility.props.get(id)?.description.clone(),
        reset: [|t, id| t.accessibility_set(id, |p| p.description = None)],
    }

    /// 对辅助技术隐藏这个元素和它的子孙（纯装饰、和旁边的文字重复的内容）
    accessibility_hidden: bool = false => {
        order: 70, storage: Accessibility, dirty: ACCESSIBILITY, applies: ANY, inherited: false, anim: [],
        method: editor, style: false, late: false, bind: typed,
        write: |t, id, v| t.accessibility_set(id, |p| p.hidden = v),
        read: |t, id| t.accessibility.props.get(id).map(|p| p.hidden),
        reset: [],
    }

    /// 实时区域：内容变化（或出现）时读屏软件主动念出来，提示消息、保存成功之类。
    /// `Live::Polite` 等当前的话念完，`Live::Assertive` 立即打断。容器的名字是里面所有文字连起来；
    /// 里面声明了 `Live::Off` 的子树不算在内，变了也不念（实时区域里一直在变、又不该念的计时、进度数字）
    accessibility_live: Live = Live::Off => {
        order: 80, storage: Accessibility, dirty: ACCESSIBILITY, applies: ANY, inherited: false, anim: [],
        method: editor, style: false, late: false, bind: typed,
        // 声明了 `Live::Off` 也要记下：它把子树从外面的实时区域里排除（见 `subtree_text`），和没声明不一样
        write: |t, id, v| t.accessibility_set(id, |p| p.live = Some(v)),
        read: |t, id| t.accessibility.props.get(id)?.live,
        reset: [],
    }

    /// 勾选状态（复选框、开关、切换按钮），可以绑定信号；`Toggled::Mixed` 是半选
    accessibility_checked: Toggled = Toggled::False => {
        order: 90, storage: Accessibility, dirty: ACCESSIBILITY, applies: ANY, inherited: false, anim: [],
        method: editor_into, style: false, late: false, bind: typed,
        write: |t, id, v| t.accessibility_set(id, |p| p.toggled = Some(v)),
        read: |t, id| t.accessibility.props.get(id)?.toggled,
        reset: [|t, id| t.accessibility_set(id, |p| p.toggled = None)],
    }

    /// 选中状态（标签页、列表里的选项）
    accessibility_selected: bool = false => {
        order: 100, storage: Accessibility, dirty: ACCESSIBILITY, applies: ANY, inherited: false, anim: [],
        method: editor, style: false, late: false, bind: typed,
        write: |t, id, v| t.accessibility_set(id, |p| p.selected = Some(v)),
        read: |t, id| t.accessibility.props.get(id)?.selected,
        reset: [|t, id| t.accessibility_set(id, |p| p.selected = None)],
    }

    /// 展开状态（下拉菜单的按钮、可折叠的分组）
    accessibility_expanded: bool = false => {
        order: 110, storage: Accessibility, dirty: ACCESSIBILITY, applies: ANY, inherited: false, anim: [],
        method: editor, style: false, late: false, bind: typed,
        write: |t, id, v| t.accessibility_set(id, |p| p.expanded = Some(v)),
        read: |t, id| t.accessibility.props.get(id)?.expanded,
        reset: [|t, id| t.accessibility_set(id, |p| p.expanded = None)],
    }

    /// 数值和范围（滑块、进度条）。没设角色时：有 [`Element::on_value_request`] 的是滑块，没有的是进度条
    ///
    /// ```no_run
    /// # use aureole::prelude::*;
    /// # use aureole::accessibility::RangeValue;
    /// let volume = signal(0.4);
    /// rect()
    ///     .accessibility_value(move || RangeValue::new(volume.get(), 0.0, 1.0).step(0.05))
    ///     .accessibility_value_text(move || format!("{:.0}%", volume.get() * 100.0))
    ///     .on_value_request(move |v| volume.set(v))
    ///     .accessibility_name("音量");
    /// ```
    accessibility_value: RangeValue = RangeValue::new(0.0, 0.0, 1.0) => {
        order: 120, storage: Accessibility, dirty: ACCESSIBILITY, applies: ANY, inherited: false, anim: [],
        method: editor, style: false, late: false, bind: typed,
        write: |t, id, v| t.accessibility_set(id, |p| p.value = Some(v)),
        read: |t, id| t.accessibility.props.get(id)?.value,
        reset: [|t, id| t.accessibility_set(id, |p| p.value = None)],
    }

    /// 值的文字（"40%"、"第 3 页，共 10 页"），读屏软件念它而不是数字
    accessibility_value_text: String = String::new() => {
        order: 130, storage: Accessibility, dirty: ACCESSIBILITY, applies: ANY, inherited: false, anim: [],
        method: editor_into, style: false, late: false, bind: typed,
        write: |t, id, v| t.accessibility_set(id, |p| p.value_text = Some(v)),
        read: |t, id| t.accessibility.props.get(id)?.value_text.clone(),
        reset: [|t, id| t.accessibility_set(id, |p| p.value_text = None)],
    }

    // ---- 输入框（多行输入框的转给里面的编辑区） ----

    /// 密码模式（只对单行输入框有效）：每个字显示成一个圆点，不能复制、剪切，不用输入法，不记撤销；
    /// 绑定的信号里照样是真的内容。可以绑定（"显示密码"的开关）。对辅助技术是密码框，只念圆点
    ///
    /// ```no_run
    /// # use aureole::prelude::*;
    /// let (secret, shown) = (signal(String::new()), signal(false));
    /// row().gap(8).child(text_field(secret).password(move || !shown.get())).child(checkbox(shown).label("显示"));
    /// ```
    password: bool = false => {
        order: 140, storage: Editor, dirty: PAINT | ACCESSIBILITY, applies: EDITOR, inherited: false, anim: [],
        method: editor, style: false, late: false, bind: typed,
        write: |t, id, v| t.set_password(id, v),
        read: |t, id| editor(t, id).map(|e| e.password()),
        reset: [],
    }

    /// 输入框为空时显示的提示文字
    placeholder: String = String::new() => {
        order: 150, storage: Editor, dirty: PAINT, applies: EDITOR, inherited: false, anim: [],
        method: editor_into, style: false, late: false, bind: typed,
        write: |t, id, v| t.update_editor(id, |e| e.set_placeholder(v)),
        read: |t, id| editor(t, id).map(|e| e.placeholder_text().to_owned()),
        reset: [],
    }

    /// 多行输入框里提交的键（默认没有，回车只换行）：[`SubmitKey::PrimaryEnter`] 是 Command+回车提交，
    /// [`SubmitKey::Enter`] 是回车提交、Shift+回车换行。单行输入框总是回车提交
    submit_key: SubmitKey = SubmitKey::Enter => {
        order: 160, storage: Editor, dirty: BEHAVIOR, applies: EDITOR, inherited: false, anim: [],
        method: editor, style: false, late: false, bind: typed,
        write: |t, id, v| t.update_editor(id, |e| e.set_submit_key(Some(v))),
        read: |t, id| editor(t, id)?.submit_key(),
        reset: [|t, id| t.update_editor(id, |e| e.set_submit_key(None))],
    }

    /// 输入框里按 Tab 插入制表符（默认 Tab 留给焦点切换）
    accepts_tab: bool = false => {
        order: 170, storage: Editor, dirty: BEHAVIOR, applies: EDITOR, inherited: false, anim: [],
        method: editor, style: false, late: false, bind: typed,
        write: |t, id, v| t.update_editor(id, |e| e.set_accepts_tab(v)),
        read: |t, id| editor(t, id).map(|e| e.accepts_tab()),
        reset: [],
    }

    // ---- 布局 ----

    /// 子元素的排列方向
    direction: Direction = Direction::Row => {
        order: 200, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| s.flex_direction = flex_direction(v)),
        read: |t, id| style(t, id).map(|s| if s.flex_direction == taffy::FlexDirection::Column { Direction::Column } else { Direction::Row }),
        reset: [],
    }

    /// 子元素在交叉轴上的对齐，默认拉伸
    align: Align = Align::Stretch => {
        order: 210, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| s.align_items = align_items(v)),
        read: |t, id| align_of(style(t, id)?.align_items),
        reset: [|t, id| t.update_style(id, |s| s.align_items = AlignItems::NORMAL)],
    }

    /// 子元素在主轴上的分布，默认靠起点
    justify: Justify = Justify::Start => {
        order: 220, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| s.justify_content = justify_content(v)),
        read: |t, id| justify_of(style(t, id)?.justify_content),
        reset: [|t, id| t.update_style(id, |s| s.justify_content = JustifyContent::NORMAL)],
    }

    /// 分配主轴上剩余空间的比例：几个子元素都设了时按比例分
    grow: f32 = 0.0 => {
        order: 230, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| s.flex_grow = v),
        read: |t, id| style(t, id).map(|s| s.flex_grow),
        reset: [],
    }

    /// 宽度：数字是逻辑像素（`.width(200)`），也可以是 [`pct`](crate::pct)`(50)`、[`auto`](crate::auto)`()` 等 [`Length`]。
    /// 像素值可以做动画（[`Property::Width`]）；从百分比、auto 切到像素值时从当前宽度过渡过去。
    /// 和 [`Element::full_width`]、[`Element::size`]、[`Element::full_size`] 设的是同一个宽度，后调用的算数
    /// （组件默认跟着内容宽，调用的地方接着 `.full_width(true)` 或 `.width(..)` 就能改掉）
    width: Length = Length::Auto => {
        order: 240, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [Number, 1e-2],
        method: into, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_size(id, PropId::WIDTH, v),
        read: |t, id| style(t, id).map(|s| length_of(s.size.width)),
        reset: [],
    }

    /// 高度，同 [`Element::width`]；和 [`Element::full_height`] 也是后调用的算数
    height: Length = Length::Auto => {
        order: 250, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [Number, 1e-2],
        method: into, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_size(id, PropId::HEIGHT, v),
        read: |t, id| style(t, id).map(|s| length_of(s.size.height)),
        reset: [],
    }

    /// 四边相同的内边距，可做动画（[`Property::Padding`]）。单边的 [`Element::padding_x`]、[`Element::padding_y`]
    /// 不管先后都盖过它
    padding: f32 = 0.0 => {
        order: 260, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [Number, 1e-2],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_property(id, PropId::PADDING, PropertyValue::Float(v)),
        read: |t, id| style(t, id).map(|s| number_of(s.padding.top)),
        reset: [],
    }

    /// 子元素之间的间距，可做动画（[`Property::Gap`]）。[`Element::gap_x`]、[`Element::gap_y`] 不管先后都盖过它的一个方向
    gap: f32 = 0.0 => {
        order: 270, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [Number, 1e-2],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_property(id, PropId::GAP, PropertyValue::Float(v)),
        read: |t, id| style(t, id).map(|s| number_of(s.gap.width)),
        reset: [],
    }

    /// 左右内边距，盖过 `padding` 的左右两边
    padding_x: f32 = 0.0 => {
        order: 280, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| {
            s.padding.left = LengthPercentage::length(v);
            s.padding.right = LengthPercentage::length(v);
        }),
        read: |t, id| style(t, id).map(|s| number_of(s.padding.left)),
        reset: [],
    }

    /// 上下内边距，盖过 `padding` 的上下两边
    padding_y: f32 = 0.0 => {
        order: 290, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| {
            s.padding.top = LengthPercentage::length(v);
            s.padding.bottom = LengthPercentage::length(v);
        }),
        read: |t, id| style(t, id).map(|s| number_of(s.padding.top)),
        reset: [],
    }

    /// 主轴上放不下时换行；行间距用 `gap` / `gap_y`
    wrap: bool = false => {
        order: 300, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| s.flex_wrap = if v { FlexWrap::Wrap } else { FlexWrap::NoWrap }),
        read: |t, id| style(t, id).map(|s| s.flex_wrap == FlexWrap::Wrap),
        reset: [],
    }

    /// 网格的列，见 [`Tracks`]
    columns: Tracks = Tracks::default() => {
        order: 310, storage: Layout, dirty: LAYOUT, applies: GRID, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| s.grid_template_columns = v.template()),
        read: |_, _| None,
        reset: [],
    }

    /// 网格的行；不指定时按内容自动加行
    rows: Tracks = Tracks::default() => {
        order: 320, storage: Layout, dirty: LAYOUT, applies: GRID, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| s.grid_template_rows = v.template()),
        read: |_, _| None,
        reset: [],
    }

    /// 自动加出来的行的尺寸，默认 auto；给几个就轮流用
    auto_rows: Tracks = Tracks::default() => {
        order: 330, storage: Layout, dirty: LAYOUT, applies: GRID, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| s.grid_auto_rows = v.implicit()),
        read: |_, _| None,
        reset: [],
    }

    /// 自动加出来的列的尺寸
    auto_columns: Tracks = Tracks::default() => {
        order: 340, storage: Layout, dirty: LAYOUT, applies: GRID, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| s.grid_auto_columns = v.implicit()),
        read: |_, _| None,
        reset: [],
    }

    /// 在网格里占哪几列：`.grid_column(1..3)`、`.grid_column(..)`（整行）、`.grid_column(Place::span(2))`，见 [`Place`]
    grid_column: Place = Place::AUTO => {
        order: 350, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| s.grid_column = v.line()),
        read: |t, id| style(t, id).map(|s| place_of(&s.grid_column)),
        reset: [],
    }

    /// 在网格里占哪几行，写法同 [`Element::grid_column`]
    grid_row: Place = Place::AUTO => {
        order: 360, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| s.grid_row = v.line()),
        read: |t, id| style(t, id).map(|s| place_of(&s.grid_row)),
        reset: [],
    }

    /// 网格项目在格子里的横向对齐，默认拉伸
    justify_items: Align = Align::Stretch => {
        order: 370, storage: Layout, dirty: LAYOUT, applies: GRID, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| s.justify_items = align_items(v)),
        read: |t, id| align_of(style(t, id)?.justify_items),
        reset: [|t, id| t.update_style(id, |s| s.justify_items = AlignItems::NORMAL)],
    }

    /// 这个网格项目自己在格子里的横向对齐，覆盖父元素的 `justify_items`
    justify_self: Align = Align::Stretch => {
        order: 380, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| s.justify_self = Some(align_items(v))),
        read: |t, id| style(t, id)?.justify_self.and_then(align_of),
        reset: [|t, id| t.update_style(id, |s| s.justify_self = None)],
    }

    /// 自己在交叉轴上的对齐，覆盖父元素的 `align`
    align_self: Align = Align::Stretch => {
        order: 390, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| s.align_self = Some(align_items(v))),
        read: |t, id| style(t, id)?.align_self.and_then(align_of),
        reset: [|t, id| t.update_style(id, |s| s.align_self = None)],
    }

    /// 多行（换行）或网格轨道在交叉轴上怎么分布，默认拉伸
    align_content: Justify = Justify::Stretch => {
        order: 400, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| s.align_content = justify_content(v)),
        read: |t, id| justify_of(style(t, id)?.align_content),
        reset: [|t, id| t.update_style(id, |s| s.align_content = JustifyContent::NORMAL)],
    }

    /// 主轴空间不够时收缩的比例，默认 1；0 表示不收缩
    shrink: f32 = 1.0 => {
        order: 410, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| s.flex_shrink = v),
        read: |t, id| style(t, id).map(|s| s.flex_shrink),
        reset: [],
    }

    /// 分配剩余空间之前的初始主轴尺寸。`.grow(1).basis(0)` 让兄弟严格按比例分
    basis: Length = Length::Auto => {
        order: 420, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [],
        method: into, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| s.flex_basis = v.dimension()),
        read: |t, id| style(t, id).map(|s| length_of(s.flex_basis)),
        reset: [],
    }

    /// 列之间（横向）的间距，不管先后都盖过 `gap` 的横向
    gap_x: f32 = 0.0 => {
        order: 430, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| s.gap.width = LengthPercentage::length(v)),
        read: |t, id| style(t, id).map(|s| number_of(s.gap.width)),
        reset: [],
    }

    /// 行之间（纵向）的间距，不管先后都盖过 `gap` 的纵向
    gap_y: f32 = 0.0 => {
        order: 440, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| s.gap.height = LengthPercentage::length(v)),
        read: |t, id| style(t, id).map(|s| number_of(s.gap.height)),
        reset: [],
    }

    /// 最小宽度，布局不会把它压得比这更窄
    min_width: Length = Length::Auto => {
        order: 450, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [],
        method: into, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| s.min_size.width = v.lpa()),
        read: |t, id| style(t, id).map(|s| length_of(s.min_size.width)),
        reset: [],
    }

    /// 最小高度
    min_height: Length = Length::Auto => {
        order: 460, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [],
        method: into, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| s.min_size.height = v.lpa()),
        read: |t, id| style(t, id).map(|s| length_of(s.min_size.height)),
        reset: [],
    }

    /// 最大宽度
    max_width: Length = Length::Auto => {
        order: 470, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [],
        method: into, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| s.max_size.width = v.lpa()),
        read: |t, id| style(t, id).map(|s| length_of(s.max_size.width)),
        reset: [],
    }

    /// 最大高度
    max_height: Length = Length::Auto => {
        order: 480, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [],
        method: into, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| s.max_size.height = v.lpa()),
        read: |t, id| style(t, id).map(|s| length_of(s.max_size.height)),
        reset: [],
    }

    /// 宽高比（宽 / 高）：只定了一边时按比例算出另一边，`.full_width(true).aspect_ratio(16.0 / 9.0)`
    aspect_ratio: f32 = 0.0 => {
        order: 490, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| s.aspect_ratio = (v > 0.0).then_some(v)),
        read: |t, id| style(t, id).map(|s| s.aspect_ratio.unwrap_or(0.0)),
        reset: [],
    }

    /// 左外边距。[`auto`](crate::auto) 吃掉剩余空间：`.margin_left(auto())` 把元素推到最右
    margin_left: Length = Length::Px(0.0) => {
        order: 500, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [],
        method: into, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| s.margin.left = v.lpa()),
        read: |t, id| style(t, id).map(|s| length_of(s.margin.left)),
        reset: [],
    }

    /// 右外边距
    margin_right: Length = Length::Px(0.0) => {
        order: 510, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [],
        method: into, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| s.margin.right = v.lpa()),
        read: |t, id| style(t, id).map(|s| length_of(s.margin.right)),
        reset: [],
    }

    /// 上外边距
    margin_top: Length = Length::Px(0.0) => {
        order: 520, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [],
        method: into, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| s.margin.top = v.lpa()),
        read: |t, id| style(t, id).map(|s| length_of(s.margin.top)),
        reset: [],
    }

    /// 下外边距
    margin_bottom: Length = Length::Px(0.0) => {
        order: 530, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [],
        method: into, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| s.margin.bottom = v.lpa()),
        read: |t, id| style(t, id).map(|s| length_of(s.margin.bottom)),
        reset: [],
    }

    /// 左边到父元素左边的距离。设了 `inset_*` 任何一边就是绝对定位：不参与父元素的排列，相对父元素的内边距框放置；
    /// 同时设左右（或上下）时撑开到两边之间
    inset_left: Length = Length::Auto => {
        order: 540, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [],
        method: into, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| absolute(s).left = v.lpa()),
        read: |t, id| style(t, id).map(|s| length_of(s.inset.left)),
        reset: [],
    }

    /// 右边到父元素右边的距离：`.inset_right(16).inset_top(16)` 钉在右上角
    inset_right: Length = Length::Auto => {
        order: 550, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [],
        method: into, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| absolute(s).right = v.lpa()),
        read: |t, id| style(t, id).map(|s| length_of(s.inset.right)),
        reset: [],
    }

    /// 上边到父元素上边的距离（绝对定位）
    inset_top: Length = Length::Auto => {
        order: 560, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [],
        method: into, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| absolute(s).top = v.lpa()),
        read: |t, id| style(t, id).map(|s| length_of(s.inset.top)),
        reset: [],
    }

    /// 下边到父元素下边的距离（绝对定位）
    inset_bottom: Length = Length::Auto => {
        order: 570, storage: Layout, dirty: LAYOUT, applies: ANY, inherited: false, anim: [],
        method: into, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_style(id, |s| absolute(s).bottom = v.lpa()),
        read: |t, id| style(t, id).map(|s| length_of(s.inset.bottom)),
        reset: [],
    }

    /// 布局位置：重新布局把元素挪了地方时（列表插入删除、横排竖排切换、网格换列数），从原来在屏幕上的位置
    /// 滑到新位置。不用绑定值，只用 `.transition(Property::LayoutPosition, 曲线)` 声明。按相对父元素的位置算，
    /// 父元素自己滑动时子元素跟着它走；只是窗口大小变了（拖动窗口边缘）时直接跟上，不滑。
    /// 元素是一个合成层，滑动时只更新层的偏移，不重新布局、不重建图元；大小的变化不做过渡。
    /// 值是布局算出来的位置（相对父元素），不能设
    layout_position: [f32; 2] = [0.0; 2] => {
        order: 580, storage: Layout, dirty: COMPOSITOR, applies: ANY, inherited: false, anim: [Offset, 1e-2],
        method: manual, style: false, late: false, bind: typed,
        write: |_, _, _| {},
        read: |t, id| {
            let node = t.nodes.get(id)?;
            let layout = t.layout.try_get(node.layout)?;
            let at = t.inline_position(id).unwrap_or_default();
            Some([at[0] + layout.location.x, at[1] + layout.location.y])
        },
        reset: [],
    }

    // ---- 绘制 ----

    /// 背景：纯色或渐变（[`crate::Gradient`]），可以绑定，可做动画（[`Property::Background`]，
    /// 纯色和渐变、渐变和渐变之间都能过渡）
    background: Fill = Fill::Color(Color::TRANSPARENT) => {
        order: 600, storage: Paint, dirty: PAINT, applies: ANY, inherited: false, anim: [Fill, 1e-3],
        method: into, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_property(id, PropId::BACKGROUND, PropertyValue::from(v)),
        read: |t, id| fill(animated(t, id, PropId::BACKGROUND)),
        reset: [],
    }

    /// 四个角相同的圆角半径，可做动画（[`Property::Radius`]）
    radius: f32 = 0.0 => {
        order: 610, storage: Paint, dirty: PAINT, applies: ANY, inherited: false, anim: [Number, 1e-2],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_property(id, PropId::RADIUS, PropertyValue::Float(v)),
        read: |t, id| number(animated(t, id, PropId::RADIUS)),
        reset: [],
    }

    /// 边框宽度，逻辑像素，向内画（不占布局空间）。可做动画（[`Property::BorderWidth`]）
    border_width: f32 = 0.0 => {
        order: 620, storage: Paint, dirty: PAINT, applies: ANY, inherited: false, anim: [Number, 1e-2],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_property(id, PropId::BORDER_WIDTH, PropertyValue::Float(v)),
        read: |t, id| number(animated(t, id, PropId::BORDER_WIDTH)),
        reset: [],
    }

    /// 边框颜色：纯色或渐变（[`crate::Gradient`]，铺满元素的框，沿圆角一圈都对），
    /// 可做动画（[`Property::BorderColor`]，纯色和渐变、渐变和渐变之间都能过渡）
    border_color: Fill = Fill::Color(Color::TRANSPARENT) => {
        order: 630, storage: Paint, dirty: PAINT, applies: ANY, inherited: false, anim: [Fill, 1e-3],
        method: into, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_property(id, PropId::BORDER_COLOR, PropertyValue::from(v)),
        read: |t, id| fill(animated(t, id, PropId::BORDER_COLOR)),
        reset: [],
    }

    /// 投影，默认画在形状外面；[`crate::Shadow::inner`] 是画在里面的内阴影。再调用一次是换掉它。
    /// 一层（[`crate::Shadow`]）或者几层（它的数组、`Vec`、[`Shadows`]，或返回它们的闭包），第一层画在最上面：
    /// 比如一层贴近的接触投影加一层大范围的柔和投影，外投影和内阴影可以混在一起。可做动画（[`Property::Shadow`]）：
    /// 同一位置、同一种的两层逐个插值，其余的淡入淡出（按下时外投影收起、内阴影出现）
    ///
    /// ```no_run
    /// # use aureole::prelude::*;
    /// let pressed = signal(false);
    /// let ink = Color::hex(0x0b1e24);
    /// rect()
    ///     .size(160, 48)
    ///     .radius(10)
    ///     .background(Color::WHITE)
    ///     .shadow(move || {
    ///         if pressed.get() {
    ///             Shadows::from(Shadow::inner(0.0, 2.0, 6.0, ink.with_alpha(0.16)))
    ///         } else {
    ///             Shadows::new([Shadow::new(0.0, 1.0, 2.0, ink.with_alpha(0.12)), Shadow::new(0.0, 8.0, 24.0, ink.with_alpha(0.08))])
    ///         }
    ///     })
    ///     .transition(Property::Shadow, Spring::smooth())
    ///     .on_press(move |down| pressed.set(down));
    /// ```
    shadow: Shadows = Shadows::default() => {
        order: 640, storage: Paint, dirty: PAINT, applies: ANY, inherited: false, anim: [Shadow, 1e-3],
        method: into, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_property(id, PropId::SHADOW, PropertyValue::from(v)),
        read: |t, id| animated(t, id, PropId::SHADOW)?.into_shadows(),
        reset: [],
    }

    /// 图片的来源，由 [`crate::image()`] 设（挂载时和淡入、加载状态、播放句柄一起绑定）
    source: ImageSource = ImageSource::rgba(0, 0, Vec::new()) => {
        order: 650, storage: Content, dirty: LAYOUT | PAINT, applies: IMAGE, inherited: false, anim: [],
        method: manual, style: false, late: false, bind: custom(crate::image::bind_source),
        write: |t, id, v| t.request_image(id, &v),
        read: |_, _| None,
        reset: [],
    }

    /// 图片怎么放进内容区，默认 [`ImageFit::Stretch`]。只对 [`crate::image()`] 元素有效
    fit: ImageFit = ImageFit::Stretch => {
        order: 660, storage: Content, dirty: PAINT, applies: IMAGE, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_image(id, |block| block.fit = v),
        read: |t, id| image_block(t, id).map(|b| b.fit),
        reset: [],
    }

    /// 图片在内容区里横向的对齐：0 靠左，0.5 居中（默认），1 靠右。对 `Contain`、`Cover`、`None`、`ScaleDown` 有效
    image_position_x: f32 = 0.5 => {
        order: 670, storage: Content, dirty: PAINT, applies: IMAGE, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_image(id, |block| block.position[0] = v),
        read: |t, id| image_block(t, id).map(|b| b.position[0]),
        reset: [],
    }

    /// 图片在内容区里竖向的对齐：0 靠上，0.5 居中（默认），1 靠下
    image_position_y: f32 = 0.5 => {
        order: 680, storage: Content, dirty: PAINT, applies: IMAGE, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_image(id, |block| block.position[1] = v),
        read: |t, id| image_block(t, id).map(|b| b.position[1]),
        reset: [],
    }

    /// 给图片改色：所有颜色都换成 `tint`，只留下形状（透明度），单色图标用它。颜色自己的透明度再乘上去。
    /// 默认是透明的 [`Color::TRANSPARENT`]，表示不改色。只对 [`crate::image()`] 元素有效
    tint: Color = Color::TRANSPARENT => {
        order: 685, storage: Content, dirty: PAINT, applies: IMAGE, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_image(id, |block| block.tint = v),
        read: |t, id| image_block(t, id).map(|b| b.tint),
        reset: [],
    }

    /// 动图（GIF、动态 WebP）播不播，可以绑定：`.playing(move || hovered.get())` 指针放上去才动。
    /// 停下时停在当前这一帧。不设置时跟着系统：没打开"减少动态效果"时播，打开时停在第一帧。
    /// 不管设成什么，元素滚到视口外、被裁掉、透明、窗口被挡住时都暂时不换帧（不占 CPU 和 GPU）
    playing: bool = true => {
        order: 690, storage: Content, dirty: PAINT, applies: IMAGE, inherited: false, anim: [],
        method: exact, style: false, late: false, bind: typed,
        write: |t, id, v| t.set_playing(id, Some(v)),
        read: |t, id| image_block(t, id)?.intent,
        reset: [|t, id| t.set_playing(id, None)],
    }

    /// 透明度，会乘到所有子元素上，可做动画（[`Property::Opacity`]，合成层，由渲染线程推进）
    opacity: f32 = 1.0 => {
        order: 700, storage: Paint, dirty: COMPOSITOR, applies: ANY, inherited: false, anim: [Number, 1e-3, compositor],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_property(id, PropId::OPACITY, PropertyValue::Float(v)),
        read: |t, id| number(animated(t, id, PropId::OPACITY)),
        reset: [],
    }

    /// 绕中心缩放，连同子元素；不影响布局。可做动画（[`Property::Scale`]，合成层）
    scale: f32 = 1.0 => {
        order: 710, storage: Paint, dirty: COMPOSITOR, applies: ANY, inherited: false, anim: [Number, 1e-4, compositor],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_property(id, PropId::SCALE, PropertyValue::Float(v)),
        read: |t, id| number(animated(t, id, PropId::SCALE)),
        reset: [],
    }

    /// 绕中心旋转（[`Angle`]：`deg(45)`、`rad(..)`、`turns(..)`），正值顺时针，连同子元素；不影响布局。
    /// 可做动画（[`Property::Rotation`]，合成层）
    rotation: Angle = Angle::ZERO => {
        order: 720, storage: Paint, dirty: COMPOSITOR, applies: ANY, inherited: false, anim: [Number, 1e-4, compositor],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_property(id, PropId::ROTATION, PropertyValue::from(v)),
        read: |t, id| number(animated(t, id, PropId::ROTATION)).map(scene::rad),
        reset: [],
    }

    /// 水平平移，逻辑像素，连同子元素；不影响布局。可做动画（[`Property::TranslationX`]，合成层）
    translation_x: f32 = 0.0 => {
        order: 730, storage: Paint, dirty: COMPOSITOR, applies: ANY, inherited: false, anim: [Number, 1e-2, compositor],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_property(id, PropId::TRANSLATION_X, PropertyValue::Float(v)),
        read: |t, id| number(animated(t, id, PropId::TRANSLATION_X)),
        reset: [],
    }

    /// 竖直平移，逻辑像素。可做动画（[`Property::TranslationY`]，合成层）
    translation_y: f32 = 0.0 => {
        order: 740, storage: Paint, dirty: COMPOSITOR, applies: ANY, inherited: false, anim: [Number, 1e-2, compositor],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_property(id, PropId::TRANSLATION_Y, PropertyValue::Float(v)),
        read: |t, id| number(animated(t, id, PropId::TRANSLATION_Y)),
        reset: [],
    }

    /// 背朝屏幕时不画（连同子元素，也不接收指针）：转过 90 度以后看到的是背面。
    /// 两个面叠在一起、背面先转 180 度，就是能翻过来的卡片（见 [`Element::rotation_y`]）
    backface_hidden: bool = false => {
        order: 750, storage: Depth, dirty: PAINT, applies: ANY, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_extra(id, |e| e.backface_hidden = v),
        read: |t, id| Some(t.nodes.get(id)?.extra.as_deref().is_some_and(|e| e.backface_hidden)),
        reset: [],
    }

    /// 绕 x 轴（水平的轴）旋转（[`Angle`]），上边转向远处为正；连同子元素，不影响布局。
    ///
    /// 和 [`Element::rotation_y`] 一样，转过的元素按透视画（近大远小），见 [`Element::perspective`]。
    /// 用 `.transition(Property::RotationX, ..)` 声明过渡时由渲染线程推进
    rotation_x: Angle = Angle::ZERO => {
        order: 760, storage: Depth, dirty: COMPOSITOR, applies: ANY, inherited: false, anim: [Number, 1e-4, depth],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_property(id, PropId::ROTATION_X, PropertyValue::from(v)),
        read: |t, id| number(animated(t, id, PropId::ROTATION_X)).map(scene::rad),
        reset: [],
    }

    /// 绕 y 轴（竖直的轴）旋转，右边转向远处为正；连同子元素，不影响布局。翻转卡片：
    ///
    /// ```
    /// # use aureole::prelude::*;
    /// let turned = signal(false);
    /// let face = |label: &str, color: Color| rect().size(160, 100).radius(10).background(color).backface_hidden(true).child(text(label));
    /// column()
    ///     .rotation_y(move || if turned.get() { deg(180) } else { deg(0) })
    ///     .transition(Property::RotationY, Spring::new(ms(500), 1.0))
    ///     .on_click(move || turned.update(|f| *f = !*f))
    ///     .child(face("正面", Color::hex(0x138a84)))
    ///     .child(face("背面", Color::hex(0x4a6fa5)).rotation_y(deg(180)).position(0, 0));
    /// ```
    rotation_y: Angle = Angle::ZERO => {
        order: 770, storage: Depth, dirty: COMPOSITOR, applies: ANY, inherited: false, anim: [Number, 1e-4, depth],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_property(id, PropId::ROTATION_Y, PropertyValue::from(v)),
        read: |t, id| number(animated(t, id, PropId::ROTATION_Y)).map(scene::rad),
        reset: [],
    }

    /// 自己绕 x、y 轴旋转时的观察距离，逻辑像素：越小透视越强（近处更大、远处更小）。
    /// 视点在变换中心（[`Element::transform_origin`]）的正前方。`f32::INFINITY` 是没有透视。
    /// 没设时用父元素的 [`Element::child_perspective`]，父元素也没设时是 1000。可以做动画（[`Property::Perspective`]）
    perspective: f32 = 0.0 => {
        order: 780, storage: Depth, dirty: COMPOSITOR, applies: ANY, inherited: false, anim: [Number, 1e-2, depth],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_property(id, PropId::PERSPECTIVE, PropertyValue::Float(v)),
        read: |t, id| number(animated(t, id, PropId::PERSPECTIVE)),
        reset: [],
    }

    /// 给子元素的透视：没设 [`Element::perspective`] 的子元素绕 x、y 轴旋转时，都从这个元素中心的正前方、
    /// 这么远（逻辑像素）的地方看过去，一排卡片共用一个消失点，离中心越远的看起来越斜
    child_perspective: f32 = 0.0 => {
        order: 790, storage: Depth, dirty: PAINT, applies: ANY, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_extra(id, |e| e.child_perspective = v.max(0.0)),
        read: |t, id| Some(t.nodes.get(id)?.extra.as_deref().map_or(0.0, |e| e.child_perspective)),
        reset: [],
    }

    /// 缩放和各个方向的旋转绕哪一点，横向：占元素宽度的比例，默认 0.5 是中间，0 是左边。改它只重画，不重新布局
    transform_origin_x: f32 = 0.5 => {
        order: 800, storage: Depth, dirty: PAINT, applies: ANY, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_extra(id, |e| e.origin[0] = v),
        read: |t, id| Some(t.nodes.get(id)?.extra.as_deref().map_or(0.5, |e| e.origin[0])),
        reset: [],
    }

    /// 缩放和旋转绕哪一点，竖向：占元素高度的比例，默认 0.5 是中间，0 是上边
    transform_origin_y: f32 = 0.5 => {
        order: 810, storage: Depth, dirty: PAINT, applies: ANY, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_extra(id, |e| e.origin[1] = v),
        read: |t, id| Some(t.nodes.get(id)?.extra.as_deref().map_or(0.5, |e| e.origin[1])),
        reset: [],
    }

    /// 把子元素（和自己的文字）裁剪在自己的圆角矩形里，超出的部分不画。
    /// 裁剪区域跟着元素的变换和动画走，边缘抗锯齿
    clip: bool = false => {
        order: 820, storage: Paint, dirty: PAINT, applies: ANY, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_paint(id, |p| p.clip = v),
        read: |t, id| Some(t.nodes.get(id)?.paint.clip),
        reset: [],
    }

    /// 把自己连同子元素模糊，标准差是 `sigma` 逻辑像素，可做动画（[`Property::Blur`]）
    blur: f32 = 0.0 => {
        order: 830, storage: Paint, dirty: PAINT, applies: ANY, inherited: false, anim: [Number, 1e-2],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_property(id, PropId::BLUR, PropertyValue::Float(v)),
        read: |t, id| number(animated(t, id, PropId::BLUR)),
        reset: [],
    }

    /// 背景模糊（毛玻璃）：自己的形状里显示后面内容模糊后的样子，标准差 `sigma` 逻辑像素。
    /// 再配一个半透明的背景色当作玻璃的颜色。可做动画（[`Property::BackdropBlur`]）
    backdrop_blur: f32 = 0.0 => {
        order: 840, storage: Paint, dirty: PAINT, applies: ANY, inherited: false, anim: [Number, 1e-2],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_property(id, PropId::BACKDROP_BLUR, PropertyValue::Float(v)),
        read: |t, id| number(animated(t, id, PropId::BACKDROP_BLUR)),
        reset: [],
    }

    /// 背景模糊的饱和度，1 不变；系统的毛玻璃通常在 1.5 ~ 1.8 之间，颜色更鲜艳
    backdrop_saturation: f32 = 1.0 => {
        order: 850, storage: Paint, dirty: PAINT, applies: ANY, inherited: false, anim: [Number, 1e-3],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_property(id, PropId::BACKDROP_SATURATION, PropertyValue::Float(v)),
        read: |t, id| number(animated(t, id, PropId::BACKDROP_SATURATION)),
        reset: [],
    }

    /// 路径的形状，由 [`crate::path()`] 设：绑定的路径换了时从旧的形状变成新的（菜单 ↔ 关闭、播放 ↔ 暂停，
    /// 用 `.transition(Property::Path, 曲线)` 声明）。两条路径的段数、子路径数可以不同；中途再换时从此刻的样子接着变。
    /// 不能做退场值和关键帧
    path: VectorPath = VectorPath::svg("") => {
        order: 860, storage: Content, dirty: LAYOUT | PAINT, applies: PATH, inherited: false, anim: [Path, 1e-3],
        method: manual, style: false, late: false, bind: typed,
        write: |t, id, v| t.set_path(id, v),
        read: |t, id| path_block(t, id)?.path.clone(),
        reset: [],
    }

    /// 路径改成描边（只对 [`crate::path()`] 元素有效），颜色仍然是文字颜色
    stroke: Stroke = Stroke::new(1.0) => {
        order: 870, storage: Content, dirty: PAINT, applies: PATH, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_path(id, |b| b.stroke = Some(v)),
        read: |t, id| path_block(t, id)?.stroke,
        reset: [|t, id| t.update_path(id, |b| b.stroke = None)],
    }

    /// 描边从哪里开始画（按每条子路径的长度比例，0..1，默认 0），可做动画（[`Property::StrokeStart`]）
    stroke_start: f32 = 0.0 => {
        order: 880, storage: Paint, dirty: PAINT, applies: PATH, inherited: false, anim: [Number, 1e-3],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_property(id, PropId::STROKE_START, PropertyValue::Float(v)),
        read: |t, id| number(animated(t, id, PropId::STROKE_START)),
        reset: [],
    }

    /// 描边画到哪里（0..1，默认 1）。对它做动画（[`Property::StrokeEnd`]）就是线条被"描出来"的效果
    stroke_end: f32 = 1.0 => {
        order: 890, storage: Paint, dirty: PAINT, applies: PATH, inherited: false, anim: [Number, 1e-3],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_property(id, PropId::STROKE_END, PropertyValue::Float(v)),
        read: |t, id| number(animated(t, id, PropId::STROKE_END)),
        reset: [],
    }

    /// 用自定义着色器填充背景，见 [`Element::shader`]
    shader: Shader = Shader::new("") => {
        order: 900, storage: Paint, dirty: PAINT, applies: ANY, inherited: false, anim: [],
        method: manual, style: true, late: false, bind: typed,
        write: |t, id, v| {
            let index = t.register_shader(&v);
            let index = u16::try_from(index).expect("自定义着色器太多");
            t.update_paint(id, |p| p.shader = index);
        },
        read: |t, id| t.shader_of(id),
        reset: [|t, id| t.update_paint(id, |p| p.shader = 0)],
    }

    /// 传给自定义着色器的 4 个数（`in.params`），可以绑定信号
    shader_params: [f32; 4] = [0.0; 4] => {
        order: 910, storage: Paint, dirty: PAINT, applies: ANY, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_paint(id, |p| p.shader_params = v),
        read: |t, id| Some(t.nodes.get(id)?.paint.shader_params),
        reset: [],
    }

    /// 滤镜：亮度、对比度、灰度、饱和度、色相、反相、褐色调、不透明度、模糊的组合（[`Filter`]），作用于自己连同子元素。
    /// 可以绑定，`.transition(Property::Filter, ..)` 让变化时过渡（悬停时从灰到彩色）。
    ///
    /// 只有一个图元（一张图片、一块颜色）时直接在那个图元上算，不离屏；否则先画进离屏纹理，合成时算。
    /// 滤镜参数在动、内容没变时不重画内容，直接用缓存的纹理
    filter: Filter = Filter::NONE => {
        order: 920, storage: VisualEffects, dirty: PAINT, applies: ANY, inherited: false, anim: [Filter, 1e-3],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_property(id, PropId::FILTER, PropertyValue::Filter(v)),
        read: |t, id| match animated(t, id, PropId::FILTER)? {
            PropertyValue::Filter(f) => Some(f),
            _ => None,
        },
        reset: [],
    }

    /// 和后面已经画好的内容怎么混合（[`BlendMode`]：正片叠底、滤色、叠加……），作用于自己连同子元素。
    /// 在线性、预乘的颜色上算；后面的内容只到最近的离屏祖先（半透明、有效果的祖先）为止
    blend: BlendMode = BlendMode::Normal => {
        order: 930, storage: VisualEffects, dirty: PAINT, applies: ANY, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_effects(id, |e| e.blend = v),
        read: |t, id| Some(t.nodes.get(id)?.paint.effects.as_ref().map_or(BlendMode::Normal, |e| e.blend)),
        reset: [],
    }

    /// 把自己连同子元素画成一张纹理缓存起来：内容不变时，平移、缩放、旋转、倾斜、透明度、滤镜的动画只合成这张纹理，
    /// 不重画里面的内容。适合里面有模糊、投影、很多文字，又要整体动的元素。
    ///
    /// 半透明、有模糊和其他效果的元素本来就画进离屏纹理，自动缓存，不用设这一项。代价是一张纹理的显存，
    /// 动的时候纹理按此刻的变换贴上去（停下以后重画一次，回到逐像素清晰）
    group_cache: bool = false => {
        order: 940, storage: VisualEffects, dirty: PAINT, applies: ANY, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_effects(id, |e| e.cache = v),
        read: |t, id| Some(t.nodes.get(id)?.paint.effects.as_ref().is_some_and(|e| e.cache)),
        reset: [],
    }

    /// 遮罩：自己连同子元素只在遮罩不透明的地方显示（[`Mask`]：渐变、图片、路径或任意元素）。
    /// 可以绑定：换成另一个遮罩时卸载旧的遮罩元素、挂上新的
    mask: Mask = Mask::none() => {
        order: 950, storage: VisualEffects, dirty: PAINT, applies: ANY, inherited: false, anim: [],
        method: exact, style: false, late: false, bind: custom(crate::visual_effects::bind_mask),
        write: |_, _, _| {},
        read: |_, _| None,
        reset: [],
    }

    // ---- 文字 ----
    // 字号、字重、行高、颜色这些可以设在任何元素上，子孙里的文字继承，自己设的优先

    /// 文字的内容，由 [`crate::text()`] 设
    text: String = String::new() => {
        order: 960, storage: Content, dirty: LAYOUT | TEXT, applies: TEXT, inherited: false, anim: [],
        method: manual, style: false, late: false, bind: typed,
        write: |t, id, v| t.update_content(id, v),
        read: |t, id| text_block(t, id).map(|b| b.content().to_owned()),
        reset: [],
    }

    /// 字号，逻辑像素，默认 14；可以继承
    font_size: f32 = 14.0 => {
        order: 970, storage: Text, dirty: TEXT, applies: ANY, inherited: true, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_text_override(id, |o| o.size = Some(v)),
        read: |t, id| t.nodes.get(id)?.text.size,
        reset: [|t, id| t.set_text_override(id, |o| o.size = None)],
    }

    /// 字重，100～900，默认 400；600 为半粗；可以继承
    font_weight: f32 = 400.0 => {
        order: 980, storage: Text, dirty: TEXT, applies: ANY, inherited: true, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_text_override(id, |o| o.weight = Some(v)),
        read: |t, id| t.nodes.get(id)?.text.weight,
        reset: [|t, id| t.set_text_override(id, |o| o.weight = None)],
    }

    /// 行高，字号的倍数，默认 1.4；可以继承
    line_height: f32 = 1.4 => {
        order: 990, storage: Text, dirty: TEXT, applies: ANY, inherited: true, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_text_override(id, |o| o.line_height = Some(v)),
        read: |t, id| t.nodes.get(id)?.text.line_height,
        reset: [|t, id| t.set_text_override(id, |o| o.line_height = None)],
    }

    /// 下划线，不继承
    underline: bool = false => {
        order: 1000, storage: Text, dirty: TEXT, applies: ANY, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_text_override(id, |o| o.underline = v),
        read: |t, id| Some(t.nodes.get(id)?.text.underline),
        reset: [],
    }

    /// 删除线，不继承
    strikethrough: bool = false => {
        order: 1010, storage: Text, dirty: TEXT, applies: ANY, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_text_override(id, |o| o.strikethrough = v),
        read: |t, id| Some(t.nodes.get(id)?.text.strikethrough),
        reset: [],
    }

    /// 文字颜色，也是路径的填充或描边色，可以继承，可做动画（[`Property::Color`]）。
    /// 也可以是渐变：铺满元素自己的框，文字和路径露出框里对应位置的颜色；子孙继承时各自铺满自己的框
    color: Fill = Fill::Color(crate::defaults::TEXT) => {
        order: 1020, storage: Text, dirty: PAINT, applies: ANY, inherited: true, anim: [Fill, 1e-3],
        method: into, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_property(id, PropId::COLOR, PropertyValue::from(v)),
        read: |t, id| t.nodes.get(id)?.text.color.clone(),
        reset: [|t, id| t.clear_color(id)],
    }

    /// 字体族名，可以继承。系统里装的字体直接用名字；自己带的字体先用 [`crate::font::load`] 加载。
    /// 这个字体里没有的字（比如英文字体里的中文）从系统字体回退
    font_family: String = String::new() => {
        order: 1030, storage: Text, dirty: TEXT, applies: ANY, inherited: true, anim: [],
        method: into, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_text_override(id, |o| o.family = Some(crate::font::family_id(&v))),
        read: |t, id| t.nodes.get(id)?.text.family.and_then(crate::font::family_name).map(|n| n.to_string()),
        reset: [|t, id| t.set_text_override(id, |o| o.family = None)],
    }

    /// 字距（每个字后面多出的距离），逻辑像素，可以是负数；可以继承
    letter_spacing: f32 = 0.0 => {
        order: 1040, storage: Text, dirty: TEXT, applies: ANY, inherited: true, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_text_override(id, |o| o.letter_spacing = Some(v)),
        read: |t, id| t.nodes.get(id)?.text.letter_spacing,
        reset: [|t, id| t.set_text_override(id, |o| o.letter_spacing = None)],
    }

    /// 文字在行内的对齐，可以继承。对齐的范围是文字元素的宽度，见 [`TextAlign`]
    text_align: TextAlign = TextAlign::Start => {
        order: 1050, storage: Text, dirty: TEXT, applies: ANY, inherited: true, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_text_override(id, |o| o.align = Some(v)),
        read: |t, id| t.nodes.get(id)?.text.align,
        reset: [|t, id| t.set_text_override(id, |o| o.align = None)],
    }

    /// 选区的颜色（可选中的文字和输入框），可以继承；默认是半透明的蓝色
    selection_color: Color = crate::defaults::SELECTION => {
        order: 1060, storage: Text, dirty: PAINT, applies: ANY, inherited: true, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_text_override(id, |o| o.selection = Some(v)),
        read: |t, id| t.nodes.get(id)?.text.selection,
        reset: [|t, id| t.set_text_override(id, |o| o.selection = None)],
    }

    /// 选择根：里面的文字（子孙里的 [`crate::text()`]、[`crate::rich_text`]，不用逐个 `.selectable(true)`）连成一串，
    /// 拖动可以从一段选到另一段（一列段落、一串消息、几段富文本），Shift+点击扩展，⌘A 选中选择根里的全部，
    /// ⌘C 复制连起来的文字：上下排的段之间换行，并排的用制表符隔开。双击选词、三击选段照常，之后拖动按词、按段跨段扩展。
    /// 点过、选过之后 Shift+方向键从活动端扩展、收缩（按字、按词、行首行尾、整个选择根的开头结尾，修饰键按平台惯例），
    /// 活动端滚到看得见。失去焦点后选区还在，画成不活跃的颜色。
    ///
    /// 可点击（`on_click`）、能获得焦点的元素（按钮这类）里面的字和输入框不参加；选择根里再套一个选择根时，里面的自成一组。
    /// 没有选择根时每段文字各选各的（[`Element::selectable`]）。虚拟列表里滚出去的行卸载后不在选择根里，
    /// 选区一端所在的行卸载时整个选区清空
    ///
    /// ```
    /// # use aureole::prelude::*;
    /// column()
    ///     .selection_root(true)
    ///     .gap(12)
    ///     .child(text("第一段"))
    ///     .child(rich_text().span("第二段，").span(span("加粗").font_weight(600)))
    ///     .child(row().gap(8).child(text("梨茶")).child(text("并排的两段，复制时用制表符隔开")));
    /// ```
    selection_root: bool = false => {
        order: 1070, storage: Content, dirty: BEHAVIOR, applies: ANY, inherited: false, anim: [],
        method: exact, style: false, late: false, bind: typed,
        write: |t, id, v| t.selection_roots.set(id, v),
        read: |t, id| Some(t.selection_roots.contains(id)),
        reset: [],
    }

    /// 斜体，可以继承。字体有斜体就用斜体，没有（比如中文字体）时把字形倾斜画出来
    italic: bool = false => {
        order: 1080, storage: Text, dirty: TEXT, applies: ANY, inherited: true, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_text_override(id, |o| o.italic = v),
        read: |t, id| Some(t.nodes.get(id)?.text.italic),
        reset: [],
    }

    /// 等宽数字：每个数字一样宽，跳动的计数、计时、表格里的数字不会左右晃；可以继承
    tabular_numbers: bool = false => {
        order: 1090, storage: Text, dirty: TEXT, applies: ANY, inherited: true, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_text_override(id, |o| o.tabular = v),
        read: |t, id| Some(t.nodes.get(id)?.text.tabular),
        reset: [],
    }

    /// 最多显示几行，超出的部分截掉、最后一行末尾加省略号（中文按字截断）；0 是不限。不继承
    max_lines: usize = 0 => {
        order: 1100, storage: Content, dirty: LAYOUT, applies: TEXT, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_text_block(id, |block| block.set_max_lines(v)),
        read: |t, id| text_block(t, id).map(|b| b.max_lines()),
        reset: [],
    }

    /// 文字可以用指针选中：拖动选择、双击选词（中文、日文按词典分词）、三击选整段，Command+C 复制，
    /// Command+A 选中这段文字，Shift+方向键扩展、收缩（和 [`Element::selection_root`] 同一套）。点别处时选区还在，画成不活跃的颜色（淡灰），
    /// 再点这段文字时重新选。只对 [`crate::text()`] 和 [`crate::rich_text`] 有效
    selectable: bool = false => {
        order: 1110, storage: Content, dirty: BEHAVIOR, applies: TEXT, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.update_text_block(id, |block| {
            block.selectable = v;
            false
        }),
        read: |t, id| text_block(t, id).map(|b| b.selectable),
        reset: [],
    }

    /// 元素放在富文本里（[`crate::Span::inline`]）时怎么在行里对齐，默认基线对齐，见 [`InlineAlign`]。
    /// 用闭包换内容时设在闭包返回的元素上。不放在富文本里时不起作用。不继承
    inline_align: InlineAlign = InlineAlign::Baseline => {
        order: 1115, storage: Content, dirty: LAYOUT, applies: ANY, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_inline_align(id, v),
        read: |t, id| t.inline_align(id),
        reset: [],
    }

    /// 字符网格（`char_grid`）用字体的连字（编程字体的 `->`、`!=`）：同样式连续的格子一起整形（遇到空格分开），
    /// 字形仍然放在各自的格子上，插入点、选区、复制都还是一格一格的。默认关：终端要的是输出什么画什么，
    /// 而且开着时改一行要多整形这一行没见过的词；代码编辑器按需要打开。只对 `char_grid` 有效
    ligated: bool = false => {
        order: 1118, storage: Content, dirty: PAINT, applies: CHAR_GRID, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| set_grid_ligated(t, id, v),
        read: |t, id| grid_ligated(t, id),
        reset: [],
    }

    // ---- 交互 ----

    /// 能获得键盘焦点：Tab / Shift+Tab 按树的顺序走到它，指针按下也让它获得焦点。
    /// 有焦点时回车、空格触发它的 `on_click`；`on_key_down` 从它开始往外冒泡。输入框天生能获得焦点
    focusable: bool = false => {
        order: 1200, storage: Interaction, dirty: BEHAVIOR, applies: ANY, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| {
            t.with_input(id, |i| i.focusable = v);
        },
        read: |t, id| Some(t.node_input(id).is_some_and(|i| i.focusable)),
        reset: [],
    }

    /// 方向键在子孙之间移动焦点：`Direction::Column` 用 ↑ ↓，`Direction::Row` 用 ← →（菜单、工具栏、列表）。
    /// 子孙里的 `on_key_down` 没停止传播时才生效
    focus_arrows: Direction = Direction::Column => {
        order: 1210, storage: Interaction, dirty: BEHAVIOR, applies: ANY, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| {
            t.with_input(id, |i| i.arrows = Some(v));
        },
        read: |t, id| t.node_input(id)?.arrows,
        reset: [|t, id| {
            t.with_input(id, |i| i.arrows = None);
        }],
    }

    /// 用键盘得到焦点时画的焦点环；[`FocusRing::NONE`] 不画
    focus_ring: FocusRing = FocusRing::NONE => {
        order: 1220, storage: Interaction, dirty: PAINT, applies: ANY, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| {
            t.with_input(id, |i| i.ring = Some(v));
            t.paint.invalidate();
        },
        read: |t, id| t.node_input(id)?.ring,
        reset: [|t, id| {
            t.with_input(id, |i| i.ring = None);
            t.paint.invalidate();
        }],
    }

    /// 兄弟之间的叠放次序（默认 0）：大的画在上面，指针也先点到它。相同的保持声明顺序。
    /// 只在兄弟之间比较；要盖住整个窗口用 [`Element::overlay`]
    stack_order: i32 = 0 => {
        order: 1230, storage: Interaction, dirty: PAINT, applies: ANY, inherited: false, anim: [],
        method: manual, style: true, late: false, bind: typed,
        write: |t, id, v| t.set_z(id, v),
        read: |t, id| Some(t.node_input(id).map_or(0, |i| i.z)),
        reset: [],
    }

    /// 指针在元素上（含子元素，除非子元素自己设了）时的指针形状，可以绑定信号（比如拖动时换成 `Grabbing`）
    cursor: Cursor = Cursor::Default => {
        order: 1240, storage: Interaction, dirty: BEHAVIOR, applies: ANY, inherited: false, anim: [],
        method: exact, style: true, late: false, bind: typed,
        write: |t, id, v| {
            t.with_input(id, |i| i.cursor = Some(v));
            t.router.mark_cursor();
        },
        read: |t, id| t.node_input(id)?.cursor,
        reset: [|t, id| {
            t.with_input(id, |i| i.cursor = None);
            t.router.mark_cursor();
        }],
    }

    /// 窗口的拖动区域：按住这里拖动窗口，双击按系统设置缩放或最小化窗口。自己画标题栏（`TitleBar::Hidden`）时设在标题栏上。
    ///
    /// 里面能接按下的元素（按钮、输入框、有 `on_click` 的元素……）照常响应，按在它们上面不拖窗口；
    /// 子元素设 `false` 在区域里挖一块。处理器调用 `prevent_builtin(Builtin::WINDOW_DRAG)` 取消
    window_drag_area: bool = false => {
        order: 1242, storage: Interaction, dirty: BEHAVIOR, applies: ANY, inherited: false, anim: [],
        method: exact, style: false, late: false, bind: typed,
        write: |t, id, v| t.set_window_drag_area(id, Some(v)),
        read: |t, id| Some(t.node_input(id).is_some_and(|i| i.window_drag_area == Some(true))),
        reset: [|t, id| t.set_window_drag_area(id, None)],
    }

    /// 应用自己画的最大化按钮：系统按最大化按钮对待这块地方（Windows 11 上悬停时弹出贴靠布局）。
    /// 点击、悬停照常交给元素，最大化由 `on_click` 做（`WindowHandle::set_maximized`）。
    /// 没有这回事的平台上不起作用。一个窗口一个，设了几个时后设的算数
    window_maximize_button: bool = false => {
        order: 1244, storage: Interaction, dirty: BEHAVIOR, applies: ANY, inherited: false, anim: [],
        method: exact, style: false, late: false, bind: typed,
        write: |t, id, v| t.set_window_maximize_button(id, v),
        read: |t, id| Some(t.is_window_maximize_button(id)),
        reset: [],
    }

    /// 禁用：元素和子孙不响应点击、拖动、右键，不能获得焦点，Tab 跳过它们。外观由应用自己表现（比如降低透明度）
    disabled: bool = false => {
        order: 1250, storage: Interaction, dirty: BEHAVIOR | ACCESSIBILITY, applies: ANY, inherited: false, anim: [],
        method: exact, style: false, late: false, bind: typed,
        write: |t, id, v| {
            t.with_input(id, |i| i.disabled = v);
            super::accessibility::disabled_changed(t, id);
            // 正在里面组字：放弃
            if v {
                t.composition_disabled(id);
            }
        },
        read: |t, id| Some(t.node_input(id).is_some_and(|i| i.disabled)),
        reset: [],
    }

    /// 拖动时跟着指针走的样子，默认 [`DragPreview::Copy`]
    drag_preview: DragPreview = DragPreview::Copy => {
        order: 1260, storage: Interaction, dirty: BEHAVIOR, applies: ANY, inherited: false, anim: [],
        method: exact, style: false, late: false, bind: typed,
        write: |t, id, v| {
            t.with_input(id, |i| i.drag_preview = v);
        },
        read: |t, id| Some(t.node_input(id)?.drag_preview),
        reset: [],
    }

    // ---- 挂载子元素之后才绑定 ----

    /// 绕排的元素和旁边的字之间的距离，默认 12；同一边上下相邻的两个绕排元素之间也隔这么远。只对 [`crate::flow()`] 有效
    flow_gap: f32 = crate::flow::GAP => {
        order: 1300, storage: Flow, dirty: LAYOUT, applies: FLOW, inherited: false, anim: [],
        method: exact, style: false, late: true, bind: typed,
        write: |t, id, v| t.set_flow_gap(id, v),
        read: |t, id| t.flow_gap(id),
        reset: [],
    }

    /// 滚动条的颜色（默认是半透明的灰）。只对滚动视口有效
    scrollbar_color: Color = crate::defaults::SCROLLBAR => {
        order: 1310, storage: Scroll, dirty: PAINT, applies: SCROLL, inherited: false, anim: [],
        method: exact, style: false, late: true, bind: typed,
        write: |t, id, v| t.set_scrollbar_color(id, Some(v)),
        read: |t, id| t.nodes.get(id)?.scroll.as_ref()?.bar,
        reset: [|t, id| t.set_scrollbar_color(id, None)],
    }
}

// ---- 读写用的小函数 ----

fn style(t: &Tree, id: NodeId) -> Option<&taffy::Style> {
    let node = t.nodes.get(id)?;
    Some(t.layout.style(node.layout))
}

fn animated(t: &Tree, id: NodeId, property: PropId) -> Option<PropertyValue> {
    t.nodes.contains_key(id).then(|| t.current_value(id, property))
}

fn number(value: Option<PropertyValue>) -> Option<f32> {
    match value? {
        PropertyValue::Float(v) => Some(v),
        _ => None,
    }
}

fn fill(value: Option<PropertyValue>) -> Option<Fill> {
    value?.into_fill()
}

fn text_block(t: &Tree, id: NodeId) -> Option<&crate::text::TextBlock> {
    t.nodes.kind(id)
}

#[cfg(feature = "char-grid")]
fn set_grid_ligated(t: &mut Tree, id: NodeId, ligated: bool) {
    if let Some(block) = t.nodes.kind_mut::<crate::char_grid::CharGridBlock>(id)
        && block.ligated != ligated
    {
        block.ligated = ligated;
        t.paint.invalidate();
    }
}

#[cfg(not(feature = "char-grid"))]
fn set_grid_ligated(_: &mut Tree, _: NodeId, _: bool) {}

#[cfg(feature = "char-grid")]
fn grid_ligated(t: &Tree, id: NodeId) -> Option<bool> {
    t.nodes.kind::<crate::char_grid::CharGridBlock>(id).map(|block| block.ligated)
}

#[cfg(not(feature = "char-grid"))]
fn grid_ligated(_: &Tree, _: NodeId) -> Option<bool> {
    None
}

fn editor(t: &Tree, id: NodeId) -> Option<&crate::editor::EditorBlock> {
    t.nodes.kind(id)
}

fn path_block(t: &Tree, id: NodeId) -> Option<&crate::vector::PathBlock> {
    t.nodes.kind(id)
}

fn image_block(t: &Tree, id: NodeId) -> Option<&crate::image::ImageBlock> {
    t.nodes.kind(id)
}

/// 设了 `inset_*` 任何一边就是绝对定位
fn absolute(style: &mut taffy::Style) -> &mut taffy::Rect<LengthPercentageAuto> {
    style.position = Position::Absolute;
    &mut style.inset
}

fn length_of(value: impl Into<Expanded>) -> Length {
    match value.into() {
        Expanded::Px(v) => Length::Px(v),
        Expanded::Pct(v) => Length::Pct(v * 100.0),
        Expanded::Auto => Length::Auto,
        Expanded::MinContent => Length::MinContent,
        Expanded::MaxContent => Length::MaxContent,
    }
}

fn number_of(value: LengthPercentage) -> f32 {
    match value.expand() {
        taffy::style::ExpandedLengthPercentage::Length(v) => v,
        _ => 0.0,
    }
}

/// taffy 的几种长度展开以后，读回 [`Length`]
enum Expanded {
    Px(f32),
    Pct(f32),
    Auto,
    MinContent,
    MaxContent,
}

impl From<Dimension> for Expanded {
    fn from(d: Dimension) -> Self {
        use taffy::style::ExpandedDimension as E;
        match d.expand() {
            E::Length(v) => Self::Px(v),
            E::Percent(v) => Self::Pct(v),
            E::MinContent => Self::MinContent,
            E::MaxContent => Self::MaxContent,
            _ => Self::Auto,
        }
    }
}

impl From<LengthPercentageAuto> for Expanded {
    fn from(d: LengthPercentageAuto) -> Self {
        use taffy::style::ExpandedLengthPercentageAuto as E;
        match d.expand() {
            E::Length(v) => Self::Px(v),
            E::Percent(v) => Self::Pct(v),
            _ => Self::Auto,
        }
    }
}

/// 布局样式里的对齐读回 [`Align`]（没设、是默认的 `NORMAL` 时是 `None`）
fn align_of(a: AlignItems) -> Option<Align> {
    let known = [
        (AlignItems::START, Align::Start),
        (AlignItems::FLEX_START, Align::Start),
        (AlignItems::CENTER, Align::Center),
        (AlignItems::END, Align::End),
        (AlignItems::FLEX_END, Align::End),
        (AlignItems::BASELINE, Align::Baseline),
        (AlignItems::STRETCH, Align::Stretch),
    ];
    known.iter().find(|(k, _)| *k == a).map(|(_, v)| *v)
}

/// 布局样式里的分布读回 [`Justify`]
fn justify_of(j: JustifyContent) -> Option<Justify> {
    let known = [
        (JustifyContent::START, Justify::Start),
        (JustifyContent::FLEX_START, Justify::Start),
        (JustifyContent::CENTER, Justify::Center),
        (JustifyContent::END, Justify::End),
        (JustifyContent::FLEX_END, Justify::End),
        (JustifyContent::SPACE_BETWEEN, Justify::SpaceBetween),
        (JustifyContent::SPACE_AROUND, Justify::SpaceAround),
        (JustifyContent::SPACE_EVENLY, Justify::SpaceEvenly),
        (JustifyContent::STRETCH, Justify::Stretch),
    ];
    known.iter().find(|(k, _)| *k == j).map(|(_, v)| *v)
}

fn place_of(line: &Line<GridPlacement<String>>) -> Place {
    let edge = |p: &GridPlacement<String>| match p {
        GridPlacement::Line(n) => crate::layout::Edge::Line(n.as_i16()),
        GridPlacement::Span(n) => crate::layout::Edge::Span(*n),
        _ => crate::layout::Edge::Auto,
    };
    Place::new(edge(&line.start), edge(&line.end))
}

/// 网格容器（[`crate::grid`] 设的）：用 [`Display::Grid`] 布局子元素
pub(crate) fn mark_grid(tree: &TreeRef, id: NodeId) {
    tree.borrow_mut().update_style(id, |s| s.display = Display::Grid);
}

#[allow(dead_code, reason = "属性系统按名字读写、覆盖、查来源的底层能力，现在只有测试用")]
impl Tree {
    /// 节点上一个属性自己设的值，按注册表里这个属性的读函数读。
    /// 没设的继承属性、写进布局样式以后读不回来的（网格轨道）、遮罩是 `None`
    pub(crate) fn prop_value(&self, id: NodeId, prop: PropId) -> Option<AnyValue> {
        (prop.metadata().read)(self, id)
    }

    /// 按注册表写一个属性的值，和这个属性的链式方法设静态值一样（声明了过渡的照样过渡）。值的类型不对时什么都不做，返回 `false`。
    /// 属性被覆盖着（[`Tree::override_prop`]）时不写，记成撤掉覆盖后回到的值
    pub(crate) fn set_prop(&mut self, id: NodeId, prop: PropId, value: &AnyValue) -> bool {
        let meta = prop.metadata();
        if self.is_overridden(id, prop) {
            if !meta.accepts(value) {
                return false;
            }
            self.overrides.insert((id, prop), Some(value.clone()));
            return true;
        }
        (meta.write)(self, id, value)
    }

    /// 回到没设的样子：多数是写默认值，继承的属性是去掉自己设的值
    pub(crate) fn reset_prop(&mut self, id: NodeId, prop: PropId) {
        (prop.metadata().reset)(self, id)
    }
}
