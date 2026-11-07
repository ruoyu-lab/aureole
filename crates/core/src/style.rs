//! 样式对象：一组元素属性（布局、绘制、文字、过渡），可以带"在某个状态下"的值（悬停、按下、焦点、禁用，
//! 控件的部位还有选中、勾选、打开）。定义一次，到处用：放进函数、自己的结构体，用 `+` 合起来，
//! `.style(&s)` 用到任何元素上，基础控件的每个部位也收样式对象（见 `aureole-controls`）。
//!
//! 用到元素上就是按顺序调用对应的链式方法：静态值还是静态值，绑定还是绑定；只有在某个状态下有值的属性
//! 才多一个绑定（读状态，选出当前的值）。没有状态的样式对象不记任何状态，每帧也没有额外的开销。

use std::any::Any;
use std::ops::Add;
use std::rc::Rc;

use animation::Curve;
use reactive::{Signal, signal};
use smallvec::SmallVec;

use crate::element::Element;
use crate::gradient::Fill;
use crate::layout::{Length, pct};
use crate::property::registry::{Eval, FromPropValue, PropKey, PropId, PropSet, PropType, Props, Slot, Stored, eval, split};
use crate::property::{IntoProp, Prop, Property};

// ---- 状态 ----

/// 样式对象按哪种状态换值。几个状态同时成立时，**排在后面的优先**：
/// 悬停 < 焦点 < 按下 < 选中 < 勾选 < 打开 < 禁用。
/// 嵌套写的（同时满足几个状态，比如"勾选并且悬停"）先比其中最靠后的那个，一样时再比下一个，所以
/// "勾选并且悬停"优先于只写了"勾选"的，"勾选并且按下"又优先于"勾选并且悬停"。禁用时不算悬停和按下。
/// 这是一个样式对象里的；后面再用的样式对象写了"勾选"时，前面的"勾选并且悬停"让给它（见 [`Style::extend`]）
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum State {
    /// 指针在上面（含子元素）
    Hovered,
    /// 显示焦点的时候：用键盘（Tab、方向键）得到焦点、画焦点环时；输入框只要有焦点就算（插入点在里面）
    Focused,
    /// 指针按着
    Pressed,
    /// 选中（下拉列表里当前的值）
    Selected,
    /// 勾选、打开（复选框、开关、单选的一项）
    Checked,
    /// 展开（下拉框打开着）
    Open,
    /// 禁用
    Disabled,
}

impl State {
    /// 全部状态，按优先级从低到高
    pub const ALL: [State; 7] =
        [State::Hovered, State::Focused, State::Pressed, State::Selected, State::Checked, State::Open, State::Disabled];

    /// 状态在掩码里的位：位越高优先级越高，所以掩码按数值比较就是按优先级比较
    pub(crate) const fn bit(self) -> u16 {
        1 << self as u16
    }
}

/// 悬停的位
pub const HOVERED: u16 = State::Hovered.bit();
pub(crate) const FOCUSED: u16 = State::Focused.bit();
/// 按下的位
pub const PRESSED: u16 = State::Pressed.bit();
pub(crate) const DISABLED: u16 = State::Disabled.bit();
/// 元素自己能记的状态：悬停、焦点、按下、禁用
pub const TRACKABLE: u16 = HOVERED | FOCUSED | PRESSED | DISABLED;
/// 交互的反馈：悬停、焦点、按下。它们是某种样子上的变化，不是另一种样子（[`Style::extend`] 按它让）
const FEEDBACK: u16 = HOVERED | FOCUSED | PRESSED;

/// 样式对象按哪些状态切换：每种状态一个响应式的读取（信号、派生值、闭包），或者固定的 `true` / `false`。
/// 克隆只加引用计数。
///
/// 用 [`Element::style_with`] 交给样式对象；这里没给的悬停、焦点、按下、禁用由元素自己记。
/// 自己做的列表行、标签这类有"选中"的东西用它：
///
/// ```
/// # use aureole::prelude::*;
/// let picked = signal(false);
/// let row_style = Style::new()
///     .padding_x(8)
///     .hovered(|s| s.background(Color::hex(0xf0f2f5)))
///     .selected(|s| s.background(Color::hex(0xe7effd)).color(Color::hex(0x2563eb)));
/// row().style_with(&row_style, &States::new().selected(picked)).child(text("收件箱"));
/// ```
#[derive(Clone, Default)]
pub struct States(Option<Rc<StatesInner>>);

#[derive(Clone, Default)]
struct StatesInner {
    /// 元素或控件记下的悬停、焦点、按下、禁用，`from_bits` 里的状态读它
    bits: Option<Signal<u16>>,
    from_bits: u16,
    /// 给出了的状态（包括固定为假的）
    provided: u16,
    /// 一直成立的
    always: u16,
    /// 一般只有悬停、按下、焦点、勾选这几个：放在里面，建状态、给每个部位复制一份时不另外分配
    readers: SmallVec<[Reader; 4]>,
}

/// 一种状态的读取函数：状态的位，什么时候成立
type Reader = (u16, Rc<dyn Fn() -> bool>);

impl States {
    /// 空的：所有状态都由元素自己记
    pub fn new() -> Self {
        Self(None)
    }

    fn inner(&mut self) -> &mut StatesInner {
        Rc::make_mut(self.0.get_or_insert_with(Default::default))
    }

    /// 某个状态什么时候成立：`true` / `false`、信号、派生值、闭包
    pub fn when(mut self, state: State, value: impl IntoProp<bool>) -> Self {
        let bit = state.bit();
        let inner = self.inner();
        inner.from_bits &= !bit;
        inner.always &= !bit;
        inner.readers.retain(|(b, _)| *b != bit);
        inner.provided |= bit;
        match value.into_prop() {
            Prop::Static(true) => inner.always |= bit,
            Prop::Static(false) => {}
            Prop::Dynamic(f) => inner.readers.push((bit, Rc::from(f))),
        }
        self
    }

    /// 悬停
    pub fn hovered(self, value: impl IntoProp<bool>) -> Self {
        self.when(State::Hovered, value)
    }

    /// 显示焦点
    pub fn focused(self, value: impl IntoProp<bool>) -> Self {
        self.when(State::Focused, value)
    }

    /// 按下
    pub fn pressed(self, value: impl IntoProp<bool>) -> Self {
        self.when(State::Pressed, value)
    }

    /// 选中
    pub fn selected(self, value: impl IntoProp<bool>) -> Self {
        self.when(State::Selected, value)
    }

    /// 勾选、打开
    pub fn checked(self, value: impl IntoProp<bool>) -> Self {
        self.when(State::Checked, value)
    }

    /// 展开
    pub fn open(self, value: impl IntoProp<bool>) -> Self {
        self.when(State::Open, value)
    }

    /// 禁用
    pub fn disabled(self, value: impl IntoProp<bool>) -> Self {
        self.when(State::Disabled, value)
    }

    /// 某个状态此刻是否成立（响应式）；没给出的是 `false`
    pub fn contains(&self, state: State) -> bool {
        self.current(state.bit()) != 0
    }

    /// 悬停、焦点、按下、禁用里 `mask` 的这几种读这个信号里的位，覆盖前面给的
    pub(crate) fn with_bits(mut self, bits: Signal<u16>, mask: u16) -> Self {
        let inner = self.inner();
        // 已经有另一个信号（很少见）：原来从它读、这次不由新信号接管的状态改成读取函数
        if let Some(old) = inner.bits.take().filter(|old| *old != bits) {
            for state in State::ALL {
                let bit = state.bit();
                if inner.from_bits & bit != 0 && mask & bit == 0 {
                    inner.readers.push((bit, Rc::new(move || old.get() & bit != 0)));
                }
            }
            inner.from_bits &= mask;
        }
        inner.bits = Some(bits);
        inner.from_bits |= mask;
        inner.provided |= mask;
        inner.always &= !mask;
        inner.readers.retain(|(b, _)| *b & mask == 0);
        self
    }

    /// 两组状态合起来：`top` 给出了的盖过这里的
    fn overlay(&self, top: &States) -> States {
        let Some(t) = &top.0 else { return self.clone() };
        if self.0.is_none() {
            return top.clone();
        }
        let mut out = self.clone();
        if let Some(bits) = t.bits {
            out = out.with_bits(bits, t.from_bits);
        }
        let inner = out.inner();
        let given = t.provided & !t.from_bits;
        inner.from_bits &= !given;
        inner.always = (inner.always & !given) | t.always;
        inner.readers.retain(|(b, _)| *b & given == 0);
        inner.readers.extend(t.readers.iter().cloned());
        inner.provided |= t.provided;
        out
    }

    /// 给出了的状态
    fn provided(&self) -> u16 {
        self.0.as_ref().map_or(0, |s| s.provided)
    }

    /// 可能成立的状态
    fn live(&self) -> u16 {
        self.0.as_ref().map_or(0, |s| s.always | s.from_bits | s.readers.iter().fold(0, |m, (b, _)| m | b))
    }

    fn always(&self) -> u16 {
        self.0.as_ref().map_or(0, |s| s.always)
    }

    /// 此刻成立的状态（只读 `needed` 里的，响应式）。悬停、按下在禁用时不算
    fn current(&self, needed: u16) -> u16 {
        let Some(s) = &self.0 else { return 0 };
        let feedback = needed & (HOVERED | PRESSED) != 0;
        let needed = if feedback { needed | (DISABLED & s.provided) } else { needed };
        let mut now = s.always & needed;
        let mut blocked = false;
        if let Some(bits) = s.bits {
            let wanted = needed & s.from_bits;
            if wanted != 0 || (feedback && s.from_bits & (HOVERED | PRESSED) != 0) {
                let raw = bits.get();
                now |= raw & wanted;
                // 记下的悬停、按下：元素禁用时都不算（不管样式用不用禁用状态）
                blocked = feedback && raw & DISABLED != 0;
            }
        }
        for (bit, read) in &s.readers {
            if needed & bit != 0 && read() {
                now |= bit;
            }
        }
        if blocked || now & DISABLED != 0 { now & !(HOVERED | PRESSED) } else { now }
    }
}

// ---- 属性值 ----

/// 样式对象里一个属性的值：静态值，或者共用的读取函数（一个样式对象用到几个元素上，每个元素都用这一个闭包，不再分配）
#[derive(Clone)]
enum Value {
    Static(Stored),
    Dynamic(Rc<dyn Eval>),
}

impl Value {
    fn from_slot(slot: Slot) -> Self {
        match slot {
            Slot::Static(s) => Self::Static(s),
            Slot::Bound(f) => Self::Dynamic(Rc::from(f)),
            Slot::Shared(f) => Self::Dynamic(f),
        }
    }

    /// 设到元素上：静态值复制一份，绑定共用闭包
    fn slot(&self) -> Slot {
        match self {
            Self::Static(s) => Slot::Static(s.clone()),
            Self::Dynamic(f) => Slot::Shared(f.clone()),
        }
    }

    /// 求值，写进 `out`（`Option<属性的类型>`）
    #[inline]
    fn eval(&self, id: PropId, out: &mut dyn Any) {
        match self {
            Self::Static(s) => (id.metadata().put)(s, out),
            Self::Dynamic(f) => f.eval(out),
        }
    }
}

/// 一个属性和它的值（用注册表的键，不另写属性清单）
#[derive(Clone)]
struct Decl {
    id: PropId,
    value: Value,
}

/// 有状态的属性：一个绑定，读状态，选出第一个成立的状态下的值，都不成立时是平时的值
struct Layered {
    id: PropId,
    base: Value,
    layers: Box<[(u16, Value)]>,
    states: States,
    needed: u16,
}

impl Eval for Layered {
    fn eval(&self, out: &mut dyn Any) {
        let now = self.states.current(self.needed);
        for (mask, value) in self.layers.iter() {
            if now & mask == *mask {
                return value.eval(self.id, out);
            }
        }
        self.base.eval(self.id, out)
    }
}

/// 有状态的属性设到元素上：平时的值是样式对象给的，没给时是元素上原来的，元素上也没有时是注册表里的默认值。
/// 返回用的平时的值和建出来的绑定的身份
fn combine(id: PropId, base: Option<&Decl>, layers: &[(u16, &Decl)], el: &mut Element, states: &States) -> (Decl, Option<*const ()>) {
    let base = match base {
        Some(d) => d.value.clone(),
        None => match el.props.take(id) {
            Some(entry) => Value::from_slot(entry.slot),
            None => Value::Static((id.metadata().default_stored)()),
        },
    };
    let layers: Box<[(u16, Value)]> = layers.iter().map(|(m, d)| (*m, d.value.clone())).collect();
    let slot = bind_layers(id, base.clone(), layers, states);
    let identity = slot.identity();
    el.props.put(id, slot, true);
    (Decl { id, value: base }, identity)
}

/// 按状态选值的绑定。最高优先级的那层一直成立时就是那一层的值
fn bind_layers(id: PropId, base: Value, layers: Box<[(u16, Value)]>, states: &States) -> Slot {
    let always = states.always();
    if let Some((mask, value)) = layers.first()
        && mask & !always == 0
    {
        return value.slot();
    }
    let needed = layers.iter().fold(0, |m, (mask, _)| m | mask);
    Slot::Bound(Box::new(Layered { id, base, layers, states: states.clone(), needed }))
}

/// 元素上这个属性此刻还是 `identity` 那个绑定（上一个样式对象建的，没被后面的链式方法换掉）
fn holds(el: &Element, id: PropId, identity: *const ()) -> bool {
    el.props.get(id).and_then(|e| e.slot.identity()) == Some(identity)
}

// ---- 样式对象 ----

/// 样式对象：一组元素属性，可以带"在某个状态下"的值。定义一次，到处用；克隆只加引用计数。
///
/// - 能放进去的：布局（尺寸、内边距、外边距、间距、方向、对齐、伸缩）、绘制（背景和渐变、边框和渐变边框、圆角、投影、
///   透明度、滤镜、平移缩放旋转）、文字（字号、字重、行高、颜色、字距、对齐、下划线……，照常继承）、指针形状、过渡
///   （`.transition(..)`）；其他元素方法用 [`Style::chain`]。值和元素的链式方法一样：静态值、信号、派生值、闭包。
/// - 状态：`.hovered(|s| ..)`、`.pressed(..)`、`.focused(..)`、`.disabled(..)`，控件的部位还有 `.checked(..)`、
///   `.selected(..)`、`.open(..)`；可以嵌套（`.checked(|s| s.hovered(..))` 是"勾选并且悬停"）。
///   同时成立时谁优先见 [`State`]。状态一变属性立即换成新值；元素（或样式对象）声明了过渡（`.transition(..)`）时按过渡变过去。
/// - 合起来：`a + &b`（或 `a.extend(&b)`）按属性、按状态逐个合：`b` 在某个状态下设了的值，盖过 `a` 在这个状态、
///   以及这个状态再加上悬停、焦点、按下时的值（`b` 写了"勾选"，`a` 的"勾选并且悬停"也让给它；`b` 只写了平时的值，
///   `a` 悬停、按下时的也让掉）；`a` 在勾选、选中、展开、禁用这些别的样子下的值留着。`a` 不变。
/// - 用到元素上：[`Element::style`]，见那里的先后规则。
///
/// 只在某个状态下设、平时没设的属性，平时用元素上原来的值；元素上也没有时用默认值
/// （文字颜色、字号这些继承的属性也是默认值，不是继承来的，要继承时平时的值也设上）。
///
/// ```
/// # use aureole::prelude::*;
/// fn chip() -> Style {
///     Style::new()
///         .padding_x(10)
///         .height(28)
///         .radius(14)
///         .background(Color::WHITE)
///         .border(1, Color::hex(0xc4cad2))
///         .hovered(|s| s.background(Color::hex(0xf0f2f5)))
///         .pressed(|s| s.background(Color::hex(0xe3e7ec)))
/// }
/// let chip = chip();
/// let active = chip.clone() + &Style::new().color(Color::hex(0x2563eb)).border_color(Color::hex(0x2563eb));
/// row().gap(8).child(text("全部").style(&active)).child(text("未读").style(&chip));
/// ```
#[derive(Clone, Default)]
pub struct Style(Option<Rc<Inner>>);

#[derive(Clone, Default)]
struct Inner {
    /// 平时的值，每种属性一个
    base: Vec<Decl>,
    /// 各状态（掩码）下的值，按优先级从高到低排
    layers: Vec<Layer>,
    animations: Vec<(Property, Curve)>,
    extras: Vec<Rc<dyn Fn(Element) -> Element>>,
}

#[derive(Clone)]
struct Layer {
    mask: u16,
    decls: Vec<Decl>,
    /// 只在这个状态会出现的元素上声明的过渡
    animations: Vec<(Property, Curve)>,
}

/// 同一种属性后设的换掉先设的
fn put(decls: &mut Vec<Decl>, decl: Decl) {
    let id = decl.id;
    match decls.iter_mut().find(|d| d.id == id) {
        Some(existing) => *existing = decl,
        None => decls.push(decl),
    }
}

fn put_animation(animations: &mut Vec<(Property, Curve)>, property: Property, curve: Curve) {
    match animations.iter_mut().find(|(p, _)| *p == property) {
        Some(existing) => existing.1 = curve,
        None => animations.push((property, curve)),
    }
}

/// 用到元素上之后，有状态的一种属性：平时的值、各状态下的值、建出来的绑定的身份
#[derive(Clone)]
struct Applied {
    key: PropId,
    base: Decl,
    layers: Vec<(u16, Decl)>,
    id: *const (),
}

/// 元素上记着的：样式对象建的有状态的属性，和用的状态。后面再用样式对象时和它们按状态合起来
#[derive(Clone)]
pub(crate) struct StyleMemo {
    applied: Vec<Applied>,
    states: States,
}

impl Style {
    /// 空的样式对象
    pub fn new() -> Self {
        Self(None)
    }

    /// 什么都没设
    pub fn is_empty(&self) -> bool {
        self.0.as_ref().is_none_or(|s| s.base.is_empty() && s.layers.is_empty() && s.animations.is_empty() && s.extras.is_empty())
    }

    fn inner(&mut self) -> &mut Inner {
        Rc::make_mut(self.0.get_or_insert_with(Default::default))
    }

    fn decl(mut self, id: PropId, slot: Slot) -> Self {
        put(&mut self.inner().base, Decl { id, value: Value::from_slot(slot) });
        self
    }

    /// 按键设一个属性：`Style::new().set(property::OPACITY, 0.5)`，和 `.opacity(0.5)` 一样（属性的方法就是调用它），
    /// 能转换的和链式方法一样转换（[`FromPropValue`]）
    pub fn set<T: FromPropValue<V>, V: 'static>(self, key: PropKey<T>, value: impl IntoProp<V>) -> Self {
        self.decl(key.id(), value.into_slot(T::from_prop_value))
    }

    /// 其他元素方法：用到元素上时调用（在属性之后）。只能放在平时的值里，不能放进状态
    ///
    /// ```
    /// # use aureole::prelude::*;
    /// let caption = Style::new().font_size(12).chain(|el| el.max_lines(1).tabular_numbers(true));
    /// ```
    pub fn chain(mut self, f: impl Fn(Element) -> Element + 'static) -> Self {
        self.inner().extras.push(Rc::new(f));
        self
    }

    // ---- 状态 ----

    /// 在某个状态下的值：`f` 收到一个空的样式对象，在里面设这个状态下的属性，也可以再嵌套状态
    pub fn when(mut self, state: State, f: impl FnOnce(Style) -> Style) -> Self {
        let nested = f(Style::new());
        if let Some(nested) = nested.0 {
            debug_assert!(nested.extras.is_empty(), "状态里只能放属性和过渡，不能放 `.chain(..)`");
            let inner = self.inner();
            let bit = state.bit();
            inner.merge_layer(bit, &nested.base, &nested.animations);
            for layer in &nested.layers {
                inner.merge_layer(bit | layer.mask, &layer.decls, &layer.animations);
            }
        }
        self
    }

    /// 悬停时（禁用时不算）
    pub fn hovered(self, f: impl FnOnce(Style) -> Style) -> Self {
        self.when(State::Hovered, f)
    }

    /// 显示焦点时：用键盘得到焦点（画焦点环的时候）；输入框有焦点就算
    pub fn focused(self, f: impl FnOnce(Style) -> Style) -> Self {
        self.when(State::Focused, f)
    }

    /// 按下时（禁用时不算）
    pub fn pressed(self, f: impl FnOnce(Style) -> Style) -> Self {
        self.when(State::Pressed, f)
    }

    /// 选中时（控件的部位：下拉列表里当前的那一项；自己的元素用 [`Element::style_with`] 给出选中状态）
    pub fn selected(self, f: impl FnOnce(Style) -> Style) -> Self {
        self.when(State::Selected, f)
    }

    /// 勾选、打开时（复选框、开关、单选的一项）
    pub fn checked(self, f: impl FnOnce(Style) -> Style) -> Self {
        self.when(State::Checked, f)
    }

    /// 展开时（下拉框打开着）
    pub fn open(self, f: impl FnOnce(Style) -> Style) -> Self {
        self.when(State::Open, f)
    }

    /// 禁用时
    pub fn disabled(self, f: impl FnOnce(Style) -> Style) -> Self {
        self.when(State::Disabled, f)
    }

    // ---- 合起来 ----

    /// 合进 `other`：按属性、按状态逐个合。`other` 在某个状态下（平时也算）设了的属性，这里同一个状态、
    /// 以及这个状态再加上悬停、焦点、按下的组合下的值都让给它：`other` 写了"勾选"，这里的"勾选并且悬停"不再生效；
    /// `other` 只写了平时的值，这里悬停、按下时的值也让掉。勾选、选中、展开、禁用是别的样子，这里写了的留着。
    /// 过渡和 `.chain(..)` 接在后面
    pub fn extend(mut self, other: &Style) -> Self {
        let Some(other) = &other.0 else { return self };
        if self.0.is_none() {
            self.0 = Some(other.clone());
            return self;
        }
        let inner = self.inner();
        for decl in &other.base {
            inner.yield_layers(decl.id, 0);
        }
        for layer in &other.layers {
            for decl in &layer.decls {
                inner.yield_layers(decl.id, layer.mask);
            }
        }
        for decl in &other.base {
            put(&mut inner.base, decl.clone());
        }
        for layer in &other.layers {
            inner.merge_layer(layer.mask, &layer.decls, &layer.animations);
        }
        for &(property, curve) in &other.animations {
            put_animation(&mut inner.animations, property, curve);
        }
        inner.extras.extend(other.extras.iter().cloned());
        self
    }

    /// 用到的状态
    fn used(&self) -> u16 {
        self.0.as_ref().map_or(0, |s| s.layers.iter().fold(0, |m, l| m | l.mask))
    }

    fn has_layers(&self) -> bool {
        self.0.as_ref().is_some_and(|s| !s.layers.is_empty())
    }

    /// 设了的属性（平时的值和各状态下的）
    fn touches(&self, id: PropId) -> bool {
        self.0.as_ref().is_some_and(|s| s.base.iter().any(|d| d.id == id) || s.layers.iter().any(|l| l.decls.iter().any(|d| d.id == id)))
    }

    /// 没有状态：设上平时的值
    fn apply_plain(&self, mut el: Element) -> Element {
        let Some(inner) = &self.0 else { return el };
        for decl in &inner.base {
            el.props.put(decl.id, decl.value.slot(), true);
        }
        inner.finish(el)
    }
}

impl Inner {
    /// 后面的样式对象在 `mask` 这个状态下设了 `id`：这里这个状态、和它再加上悬停、焦点、按下的组合下的 `id` 去掉
    /// （空了的层也去掉）。多出勾选、选中、展开、禁用的组合是别的样子，留着
    fn yield_layers(&mut self, id: PropId, mask: u16) {
        for layer in &mut self.layers {
            if layer.mask & mask == mask && layer.mask & !mask & !FEEDBACK == 0 {
                layer.decls.retain(|d| d.id != id);
            }
        }
        self.layers.retain(|l| !l.decls.is_empty() || !l.animations.is_empty());
    }

    fn merge_layer(&mut self, mask: u16, decls: &[Decl], animations: &[(Property, Curve)]) {
        // 空的层（只用来嵌套的外层）不记：不然元素会白记一个用不到的状态
        if decls.is_empty() && animations.is_empty() {
            return;
        }
        let index = match self.layers.iter().position(|l| l.mask <= mask) {
            Some(i) if self.layers[i].mask == mask => i,
            Some(i) => {
                self.layers.insert(i, Layer { mask, decls: Vec::new(), animations: Vec::new() });
                i
            }
            None => {
                self.layers.push(Layer { mask, decls: Vec::new(), animations: Vec::new() });
                self.layers.len() - 1
            }
        };
        let layer = &mut self.layers[index];
        for decl in decls {
            put(&mut layer.decls, decl.clone());
        }
        for &(property, curve) in animations {
            put_animation(&mut layer.animations, property, curve);
        }
    }

    /// 过渡和 `.chain(..)`
    fn finish(&self, mut el: Element) -> Element {
        for &(property, curve) in &self.animations {
            el = el.transition(property, curve);
        }
        for f in &self.extras {
            el = f(el);
        }
        el
    }

    /// 有状态的样式：只看可能成立的状态（不会出现的状态不建绑定），有状态的属性各一个绑定，其余照常设。
    /// 返回建了绑定的属性
    fn apply_layers(&self, mut el: Element, states: &States, applied: &mut Vec<Applied>) -> Element {
        let live = states.live();
        let layers = || self.layers.iter().filter(move |l| l.mask & !live == 0);
        let mut keyed = PropSet::EMPTY;
        for layer in layers() {
            for decl in &layer.decls {
                keyed.insert(decl.id);
            }
        }
        for decl in &self.base {
            if !keyed.contains(decl.id) {
                el.props.put(decl.id, decl.value.slot(), true);
            }
        }
        if !keyed.is_empty() {
            // 按属性在平时的值里出现的顺序，再是只在状态里出现的
            let mut keys: Vec<PropId> = self.base.iter().map(|d| d.id).filter(|k| keyed.contains(*k)).collect();
            for layer in layers() {
                for decl in &layer.decls {
                    if !keys.contains(&decl.id) {
                        keys.push(decl.id);
                    }
                }
            }
            let mut picked: Vec<(u16, &Decl)> = Vec::new();
            for key in keys {
                picked.clear();
                picked.extend(layers().filter_map(|l| l.decls.iter().find(|d| d.id == key).map(|d| (l.mask, d))));
                let base = self.base.iter().find(|d| d.id == key);
                let (base, id) = combine(key, base, &picked, &mut el, states);
                if let Some(id) = id {
                    let layers = picked.iter().map(|(m, d)| (*m, (*d).clone())).collect();
                    applied.push(Applied { key, base, layers, id });
                }
            }
        }
        for layer in layers() {
            for &(property, curve) in &layer.animations {
                el = el.transition(property, curve);
            }
        }
        self.finish(el)
    }
}

impl StyleMemo {
    /// 后面又用了一个样式对象：上次建的绑定还在元素上、这次又设了的属性，按状态合进这次的样式对象；
    /// 被链式方法换掉了的不再记；这次没碰的留着。返回合好的样式对象、合起来的状态、留着的
    fn merge(self, el: &mut Element, style: &Style, states: &States) -> (Style, States, Vec<Applied>) {
        let mut prev = Inner::default();
        let mut kept = Vec::new();
        for a in self.applied {
            if !holds(el, a.key, a.id) {
                continue;
            }
            if style.touches(a.key) {
                put(&mut prev.base, a.base);
                for (mask, decl) in a.layers {
                    prev.merge_layer(mask, &[decl], &[]);
                }
            } else {
                kept.push(a);
            }
        }
        let style = if prev.base.is_empty() { style.clone() } else { Style(Some(Rc::new(prev))).extend(style) };
        (style, self.states.overlay(states), kept)
    }
}

impl Add<&Style> for Style {
    type Output = Style;

    /// 同 [`Style::extend`]
    fn add(self, other: &Style) -> Style {
        self.extend(other)
    }
}

impl Add<Style> for Style {
    type Output = Style;

    /// 同 [`Style::extend`]
    fn add(self, other: Style) -> Style {
        self.extend(&other)
    }
}

// ---- 简写 ----
// 属性本身的方法（`background`、`padding`……）由属性表（`tree/property_table.rs`）生成，和元素上的同名方法设的是同一个属性

impl Style {
    /// 宽和高
    pub fn size<W: Into<Length> + 'static, H: Into<Length> + 'static>(self, width: impl IntoProp<W>, height: impl IntoProp<H>) -> Self {
        self.width(width).height(height)
    }

    /// 宽度撑满父元素，同 [`Element::full_width`]
    pub fn full_width(self, full: bool) -> Self {
        self.width(if full { pct(100) } else { Length::Auto })
    }

    /// 高度撑满父元素，同 [`Element::full_height`]
    pub fn full_height(self, full: bool) -> Self {
        self.height(if full { pct(100) } else { Length::Auto })
    }

    /// 宽高都撑满父元素
    pub fn full_size(self, full: bool) -> Self {
        self.full_width(full).full_height(full)
    }

    /// 四边相同的外边距，同 [`Element::margin`]
    pub fn margin<L: Into<Length> + 'static>(self, margin: impl IntoProp<L>) -> Self {
        let [left, right, top, bottom] = split(margin.into_slot(L::into));
        self.decl(PropId::MARGIN_LEFT, left).decl(PropId::MARGIN_RIGHT, right).decl(PropId::MARGIN_TOP, top).decl(PropId::MARGIN_BOTTOM, bottom)
    }

    /// 边框：宽度和颜色（纯色或渐变）
    pub fn border<T: Into<Fill> + 'static>(self, width: impl IntoProp<f32>, color: impl IntoProp<T>) -> Self {
        self.border_width(width).border_color(color)
    }

    /// 平移，逻辑像素
    pub fn translation(self, x: impl IntoProp<f32>, y: impl IntoProp<f32>) -> Self {
        self.translation_x(x).translation_y(y)
    }

    // ---- 过渡 ----

    /// 属性变化时按曲线过渡，同 [`Element::transition`]：状态之间切换时按它变过去。写在状态里的只在这个状态会出现的元素上声明
    ///
    /// ```
    /// # use aureole::prelude::*;
    /// let soft = Style::new()
    ///     .background(Color::WHITE)
    ///     .transition(Property::Background, Tween::new(ms(120)).ease_out())
    ///     .hovered(|s| s.background(Color::hex(0xf0f2f5)));
    /// ```
    pub fn transition(mut self, property: Property, curve: impl Into<Curve>) -> Self {
        put_animation(&mut self.inner().animations, property, curve.into());
        self
    }
}

// ---- 用到元素上 ----

/// 元素自己记的状态：一个信号存着悬停、焦点、按下、禁用，`tracked` 是已经装了处理器的
#[derive(Clone, Copy)]
pub(crate) struct OwnStates {
    pub bits: Signal<u16>,
    pub tracked: u16,
}

impl Element {
    /// 用样式对象：按顺序设上它里面的属性，有状态的属性各建一个绑定。状态（悬停、焦点、按下、禁用）由元素自己记：
    /// 只在样式对象用到时才记，和自己加的 `on_hover`、`on_press` 同时生效。
    ///
    /// 和其他链式方法一样**后调用的算数**，一种属性一种属性地看：
    /// - 链式方法（`.background(..)`）设的值在所有状态下都是它：换掉前面样式对象给这个属性的平时的值和各状态下的值；
    /// - 样式对象设的值按状态合（同 [`Style::extend`]）：它给了某个状态下的值（平时也算；链式方法设的也算平时的值），
    ///   就换掉前面在这个状态、以及这个状态再加上悬停、焦点、按下时的值；前面在勾选、选中、展开、禁用这些别的样子下的值留着。
    ///   所以 `.style(&a).style(&b)` 和 `.style(&(a + &b))` 一样；
    /// - 基础控件默认的样子也是这样先用上的样式对象，后面的样式对象、链式方法照这两条盖过它。
    ///
    /// 状态同时成立时谁优先见 [`State`]。
    ///
    /// ```
    /// # use aureole::prelude::*;
    /// let card = Style::new()
    ///     .padding(16)
    ///     .radius(8)
    ///     .background(Color::WHITE)
    ///     .border(1, Color::hex(0xd9dde3))
    ///     .hovered(|s| s.border_color(Color::hex(0x98a1ac)));
    /// column().style(&card).child(text("卡片")).on_click(|| println!("点了"));
    /// column().style(&card).background(Color::hex(0xf6f7f9));   // 这一张换个底色，悬停照样变描边
    /// ```
    pub fn style(self, style: &Style) -> Self {
        self.style_with(style, &States::new())
    }

    /// 按给出的状态用样式对象：自己的元素有"选中""勾选""展开"时交给它（见 [`States`]）。
    /// `states` 里没给的悬停、焦点、按下、禁用由元素自己记；先后规则同 [`Element::style`]
    pub fn style_with(mut self, style: &Style, states: &States) -> Self {
        if style.0.is_none() {
            return self;
        }
        let memo = self.input.as_mut().and_then(|i| i.memo.take());
        if memo.is_none() && !style.has_layers() {
            return style.apply_plain(self);
        }
        let (style, states, mut applied) = match memo {
            Some(memo) => memo.merge(&mut self, style, states),
            None => (style.clone(), states.clone(), Vec::new()),
        };
        let Some(inner) = style.0.clone() else { return self };
        let own = style.used() & TRACKABLE & !states.provided();
        let (el, states) = if own != 0 {
            let (el, bits) = self.track_states(own);
            (el, states.with_bits(bits, own))
        } else {
            (self, states)
        };
        let mut el = inner.apply_layers(el, &states, &mut applied);
        // 记下用的状态（控件给的勾选、选中这些）：构建出来的元素上后面再用样式对象时照样有
        el.interaction().memo = Some(Box::new(StyleMemo { applied, states }));
        el
    }

    /// 让元素记下这些状态（悬停、焦点、按下；禁用在挂载时从元素的 `.disabled(..)` 来），返回存状态的信号
    pub(crate) fn track_states(self, mask: u16) -> (Element, Signal<u16>) {
        let bits = self.input.as_ref().and_then(|i| i.states).map_or_else(|| signal(0u16), |own| own.bits);
        (self.track_into(bits, mask), bits)
    }

    /// 让元素把这些状态记进 `bits`（控件建好的信号）。元素已经在记的不再装处理器
    pub(crate) fn track_into(mut self, bits: Signal<u16>, mask: u16) -> Element {
        let tracked = match self.input.as_ref().and_then(|i| i.states) {
            Some(own) if own.bits == bits => own.tracked,
            _ => 0,
        };
        let new = mask & !tracked;
        self.interaction().states = Some(OwnStates { bits, tracked: tracked | mask });
        // 只写不读：元素卸载、作用域释放之后才到的处理器（浮层关上时失去焦点）写一个已释放的信号什么都不做
        let set = move |bit: u16, on: bool| bits.update(|b| if on { *b |= bit } else { *b &= !bit });
        if new & HOVERED != 0 {
            self = self.on_hover(move |inside| set(HOVERED, inside));
        }
        if new & PRESSED != 0 {
            self = self.on_press(move |down| set(PRESSED, down));
        }
        if new & FOCUSED != 0 {
            // 输入框有焦点就显示焦点（插入点在里面），别的元素只在用键盘得到焦点时
            if self.kind::<crate::editor::EditorBlock>().is_some() {
                self = self.on_focus(move |focused| set(FOCUSED, focused));
            } else {
                self = self.on_keyboard_focus(move |focused| set(FOCUSED, focused));
            }
        }
        self
    }
}

/// 挂载时：元素禁用写进它记状态的信号（在属性绑定之前，绑定第一次读到的就是对的）
pub(crate) fn wire_disabled(states: OwnStates, props: &mut Props) {
    let bits = states.bits;
    let write = move |on: bool| {
        let current = bits.get_untracked();
        let next = if on { current | DISABLED } else { current & !DISABLED };
        if next != current {
            bits.set(next);
        }
    };
    let Some(entry) = props.take(PropId::DISABLED) else { return };
    let slot = match entry.slot {
        Slot::Static(stored) => {
            if let Some(d) = bool::peek(&stored) {
                write(d);
            }
            Slot::Static(stored)
        }
        Slot::Bound(f) => wire(Rc::from(f), write),
        Slot::Shared(f) => wire(f, write),
    };
    props.put(PropId::DISABLED, slot, entry.span);
}

/// 禁用是绑定：一个副作用写状态，元素上的属性共用同一个闭包
fn wire(f: Rc<dyn Eval>, write: impl Fn(bool) + 'static) -> Slot {
    let g = f.clone();
    reactive::Effect::new(move || {
        if let Some(d) = eval::<bool>(&*g) {
            write(d);
        }
    });
    Slot::Shared(f)
}
