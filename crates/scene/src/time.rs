//! 时长和时刻的字面量：公开 API 里的时间一律 [`Duration`]，写的时候用 [`ms`]、[`secs`] 两个单位函数，和 `px`、`pct`、`deg` 一样。

use std::time::Duration;

use crate::angle::Number;

/// 毫秒：`ms(300)`、`ms(16.5)`。负数和非数当 0
pub fn ms(millis: impl Number) -> Duration {
    from_nanos(millis.to_f64() * 1e6)
}

/// 秒：`secs(0.5)`、`secs(2)`。负数和非数当 0
pub fn secs(seconds: impl Number) -> Duration {
    from_nanos(seconds.to_f64() * 1e9)
}

fn from_nanos(nanos: f64) -> Duration {
    Duration::from_nanos(nanos.round().max(0.0) as u64)
}
