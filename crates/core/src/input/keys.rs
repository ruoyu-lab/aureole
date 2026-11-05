//! 按键、物理键、修饰键：窗口给的按键换算成这里的类型（见 `translate.rs`），元素树和应用只认它们

use smol_str::SmolStr;

/// 逻辑按键：按当前键盘布局和修饰键得到的键
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    /// 有名字的键（回车、方向键、修饰键、功能键……）
    Named(NamedKey),
    /// 输入字符的键：按当前布局和修饰键对应的字符，如 `"a"`、`"A"`、`"?"`、`"中"`。
    /// 写字面量用 `Key::Character("a".into())`
    Character(SmolStr),
    /// 死键（先按它、再按字母打出带重音的字母），带着它会加上的符号（知道时）
    Dead(Option<char>),
    /// 认不出的键
    Unidentified,
}

/// 按键在键盘上的位置，和布局无关：按下和松开按它配对，终端、快捷键编辑器也可以按它认键。
/// 名字是键位的名字，如 `KeyA`、`Digit1`、`ShiftLeft`、`ArrowUp`、`Space`；平台认不出的键是 `Unidentified:…`
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PhysicalKey(SmolStr);

impl PhysicalKey {
    /// 按名字建，如 `PhysicalKey::new("KeyA")`
    pub fn new(name: impl Into<SmolStr>) -> Self {
        Self(name.into())
    }

    /// 键位的名字
    pub fn name(&self) -> &str {
        &self.0
    }
}

bitflags::bitflags! {
    /// 按住的修饰键
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
    pub struct Modifiers: u8 {
        /// Shift
        const SHIFT = 1;
        /// Control
        const CONTROL = 1 << 1;
        /// Alt（macOS 上是 Option）
        const ALT = 1 << 2;
        /// Command（macOS）、Windows 键（Windows）、Super（Linux）
        const SUPER = 1 << 3;
    }
}

impl Modifiers {
    /// 按着 Shift
    pub fn has_shift(self) -> bool {
        self.contains(Self::SHIFT)
    }

    /// 按着 Control
    pub fn has_control(self) -> bool {
        self.contains(Self::CONTROL)
    }

    /// 按着 Alt（Option）
    pub fn has_alt(self) -> bool {
        self.contains(Self::ALT)
    }

    /// 按着 Command（Windows 键、Super）
    pub fn has_super(self) -> bool {
        self.contains(Self::SUPER)
    }
}

/// 有名字的键的清单：一份清单生成 [`NamedKey`]、它的名字和平台按键的换算（运行时的 `translate.rs`）。
/// 运行时要用，所以导出在 crate 的根上（不是公开 API）
#[doc(hidden)]
#[macro_export]
macro_rules! named_keys {
    ($callback:ident) => {
        $callback! {
            // ---- 修饰键 ----
            /// Alt（macOS 上是 Option）
            Alt,
            /// AltGr：打键位上第三个字符
            AltGraph,
            /// 大写锁定
            CapsLock,
            /// Control
            Control,
            /// Fn
            Fn,
            /// Fn 锁定
            FnLock,
            /// 数字锁定
            NumLock,
            /// 滚动锁定
            ScrollLock,
            /// Shift
            Shift,
            /// 符号键
            Symbol,
            /// 符号锁定
            SymbolLock,
            /// Meta
            Meta,
            /// Hyper
            Hyper,
            /// Command（macOS）、Windows 键（Windows）、Super（Linux）
            Super,
            // ---- 空白 ----
            /// 回车
            Enter,
            /// Tab
            Tab,
            /// 空格
            Space,
            // ---- 移动 ----
            /// ↓
            ArrowDown,
            /// ←
            ArrowLeft,
            /// →
            ArrowRight,
            /// ↑
            ArrowUp,
            /// End
            End,
            /// Home
            Home,
            /// 向下翻页
            PageDown,
            /// 向上翻页
            PageUp,
            // ---- 编辑 ----
            /// 退格
            Backspace,
            /// 清除
            Clear,
            /// 复制
            Copy,
            /// CrSel
            CrSel,
            /// 剪切
            Cut,
            /// 向后删除（Delete）
            Delete,
            /// 删到字段末尾
            EraseEof,
            /// ExSel
            ExSel,
            /// Insert
            Insert,
            /// 粘贴
            Paste,
            /// 重做
            Redo,
            /// 撤销
            Undo,
            // ---- 界面 ----
            /// 接受
            Accept,
            /// 再来一次
            Again,
            /// Attn
            Attn,
            /// 取消
            Cancel,
            /// 右键菜单键
            ContextMenu,
            /// Esc
            Escape,
            /// 执行
            Execute,
            /// 查找
            Find,
            /// 帮助
            Help,
            /// Pause
            Pause,
            /// 播放
            Play,
            /// 属性
            Props,
            /// 选择
            Select,
            /// 放大
            ZoomIn,
            /// 缩小
            ZoomOut,
            // ---- 设备 ----
            /// 调暗屏幕
            BrightnessDown,
            /// 调亮屏幕
            BrightnessUp,
            /// 弹出
            Eject,
            /// 注销
            LogOff,
            /// 电源
            Power,
            /// 关机
            PowerOff,
            /// 截屏
            PrintScreen,
            /// 休眠
            Hibernate,
            /// 待机
            Standby,
            /// 唤醒
            WakeUp,
            // ---- 输入法 ----
            /// 显示全部候选
            AllCandidates,
            /// 字母数字
            Alphanumeric,
            /// 按编码输入
            CodeInput,
            /// 组合键（Compose）
            Compose,
            /// 变换
            Convert,
            /// 最终模式
            FinalMode,
            /// 第一个键盘组
            GroupFirst,
            /// 最后一个键盘组
            GroupLast,
            /// 下一个键盘组
            GroupNext,
            /// 上一个键盘组
            GroupPrevious,
            /// 切换输入模式
            ModeChange,
            /// 下一个候选
            NextCandidate,
            /// 无变换
            NonConvert,
            /// 上一个候选
            PreviousCandidate,
            /// 输入法正在处理
            Process,
            /// 单个候选
            SingleCandidate,
            /// 韩文模式
            HangulMode,
            /// 汉字模式（韩文输入法）
            HanjaMode,
            /// Junja 模式
            JunjaMode,
            /// 英数（日文键盘）
            Eisu,
            /// 半角
            Hankaku,
            /// 平假名
            Hiragana,
            /// 平假名 / 片假名
            HiraganaKatakana,
            /// 假名模式
            KanaMode,
            /// 汉字模式（日文输入法）
            KanjiMode,
            /// 片假名
            Katakana,
            /// 罗马字
            Romaji,
            /// 全角
            Zenkaku,
            /// 全角 / 半角
            ZenkakuHankaku,
            // ---- 媒体 ----
            /// 播放 / 暂停
            MediaPlayPause,
            /// 播放（媒体键）
            MediaPlay,
            /// 暂停（媒体键）
            MediaPause,
            /// 停止
            MediaStop,
            /// 下一首
            MediaTrackNext,
            /// 上一首
            MediaTrackPrevious,
            /// 快进
            MediaFastForward,
            /// 快退
            MediaRewind,
            /// 录制
            MediaRecord,
            /// 音量减
            AudioVolumeDown,
            /// 音量加
            AudioVolumeUp,
            /// 静音
            AudioVolumeMute,
            // ---- 功能键 ----
            /// F1
            F1,
            /// F2
            F2,
            /// F3
            F3,
            /// F4
            F4,
            /// F5
            F5,
            /// F6
            F6,
            /// F7
            F7,
            /// F8
            F8,
            /// F9
            F9,
            /// F10
            F10,
            /// F11
            F11,
            /// F12
            F12,
            /// F13
            F13,
            /// F14
            F14,
            /// F15
            F15,
            /// F16
            F16,
            /// F17
            F17,
            /// F18
            F18,
            /// F19
            F19,
            /// F20
            F20,
            /// F21
            F21,
            /// F22
            F22,
            /// F23
            F23,
            /// F24
            F24,
            /// F25
            F25,
            /// F26
            F26,
            /// F27
            F27,
            /// F28
            F28,
            /// F29
            F29,
            /// F30
            F30,
            /// F31
            F31,
            /// F32
            F32,
            /// F33
            F33,
            /// F34
            F34,
            /// F35
            F35,
        }
    };
}

macro_rules! define_named_key {
    ($($(#[$doc:meta])* $name:ident,)*) => {
        /// 有名字的键。清单外的键（平台少见的专用键）是 [`Key::Unidentified`]
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum NamedKey {
            $($(#[$doc])* $name,)*
        }

        impl NamedKey {
            /// 键名，和变体同名（`"Enter"`、`"ArrowUp"`）：录下的输入里用它
            pub fn name(self) -> &'static str {
                match self {
                    $(Self::$name => stringify!($name),)*
                }
            }

            /// 按键名找（区分大小写），见 [`Self::name`]
            pub fn from_name(name: &str) -> Option<Self> {
                match name {
                    $(stringify!($name) => Some(Self::$name),)*
                    _ => None,
                }
            }
        }
    };
}
named_keys!(define_named_key);
