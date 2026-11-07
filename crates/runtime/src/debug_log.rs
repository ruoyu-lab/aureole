//! 调试构建里应用没装 `tracing` 的订阅者时，框架自己的警告和错误打到标准错误。
//!
//! 框架发现的用法问题（滚动视口的高度是 0、属性设在不适用的元素上、快捷键写错……）都走 `tracing`。
//! 没有订阅者时它们哪儿都看不到，所以 [`crate::App::run`] 开始时补一个只认 `aureole` 开头的目标、只收警告和错误的订阅者。
//! 应用在这之前装了自己的就不动；发布构建不装。

use std::fmt::{self, Write as _};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::Interest;
use tracing::{Event, Level, Metadata, Subscriber};

/// 调试构建、还没有全局的订阅者时装上。返回装了没有
pub(crate) fn install() -> bool {
    if !cfg!(debug_assertions) || tracing::dispatcher::has_been_set() {
        return false;
    }
    tracing::subscriber::set_global_default(FrameworkWarnings).is_ok()
}

struct FrameworkWarnings;

impl FrameworkWarnings {
    fn wants(metadata: &Metadata<'_>) -> bool {
        *metadata.level() <= Level::WARN && metadata.target().starts_with("aureole")
    }
}

impl Subscriber for FrameworkWarnings {
    fn register_callsite(&self, metadata: &'static Metadata<'static>) -> Interest {
        if Self::wants(metadata) { Interest::always() } else { Interest::never() }
    }

    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        Self::wants(metadata)
    }

    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::WARN)
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    #[allow(clippy::print_stderr, reason = "替没装订阅者的应用把框架的警告打到终端，这就是它的用途")]
    fn event(&self, event: &Event<'_>) {
        let mut line = Line::default();
        event.record(&mut line);
        let metadata = event.metadata();
        eprintln!("{:>5} {}: {}{}", metadata.level(), metadata.target(), line.message, line.fields);
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

/// 一条事件的消息和其余字段（`键=值`）
#[derive(Default)]
struct Line {
    message: String,
    fields: String,
}

impl Visit for Line {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            let _ = write!(self.fields, " {}={value}", field.name());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.fields, " {}={value:?}", field.name());
        }
    }
}
