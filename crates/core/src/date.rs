//! 日期、时间的值：日期选择、时间选择绑定的类型。只是日历上的一天、钟面上的一刻，不带时区。
//!
//! 框架不在公开接口里绑定第三方的日期库：应用用哪个都行，用 `year()`、`month()`、`day()` 和 [`Date::new`] 互相转换。
//! 日期的计算用公历（往前也按公历推），和常见的日期库一致。

use std::fmt;
use std::str::FromStr;

/// 星期几
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Weekday {
    /// 星期一
    Monday,
    /// 星期二
    Tuesday,
    /// 星期三
    Wednesday,
    /// 星期四
    Thursday,
    /// 星期五
    Friday,
    /// 星期六
    Saturday,
    /// 星期日
    Sunday,
}

impl Weekday {
    /// 从星期一开始的一周
    pub const ALL: [Self; 7] = [Self::Monday, Self::Tuesday, Self::Wednesday, Self::Thursday, Self::Friday, Self::Saturday, Self::Sunday];

    /// 星期一是 0，星期日是 6
    pub const fn from_monday(self) -> u8 {
        self as u8
    }

    /// 从 `first` 这一天算起是第几天（0..7）
    pub const fn days_since(self, first: Self) -> u8 {
        (self.from_monday() + 7 - first.from_monday()) % 7
    }
}

/// 日历上的一天：年、月（1..=12）、日（1..=当月天数）。比较时早的在前。
///
/// ```
/// # use aureole_core::{Date, Weekday};
/// let day = Date::new(2026, 11, 5);
/// assert_eq!(day.weekday(), Weekday::Thursday);
/// assert_eq!(day.add_months(3), Date::new(2027, 2, 5));
/// assert_eq!(Date::new(2026, 1, 31).add_months(1), Date::new(2026, 2, 28), "没有这一天时取当月最后一天");
/// assert_eq!(day.to_string(), "2026-11-05");
/// assert_eq!("2026/11/5".parse(), Ok(day));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date {
    year: i32,
    month: u8,
    day: u8,
}

/// 这一年是不是闰年
const fn is_leap(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

impl Date {
    /// 这一天。没有这一天时 panic：写死的日期用它，来自用户、文件的用 [`Date::checked`] 或 `parse`
    #[track_caller]
    pub const fn new(year: i32, month: u8, day: u8) -> Self {
        match Self::checked(year, month, day) {
            Some(date) => date,
            None => panic!("没有这一天"),
        }
    }

    /// 这一天；没有这一天（13 月、2 月 30 日）时是 `None`
    pub const fn checked(year: i32, month: u8, day: u8) -> Option<Self> {
        if month >= 1 && month <= 12 && day >= 1 && day <= Self::days_in_month(year, month) { Some(Self { year, month, day }) } else { None }
    }

    /// 年
    pub const fn year(self) -> i32 {
        self.year
    }

    /// 月，1..=12
    pub const fn month(self) -> u8 {
        self.month
    }

    /// 日，从 1 开始
    pub const fn day(self) -> u8 {
        self.day
    }

    /// 这个月有多少天。`month` 不在 1..=12 时是 0
    pub const fn days_in_month(year: i32, month: u8) -> u8 {
        match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if is_leap(year) => 29,
            2 => 28,
            _ => 0,
        }
    }

    /// 从 1970-01-01 算起第几天（之前的是负数）
    pub const fn days_from_epoch(self) -> i64 {
        // 把年的开头挪到 3 月：闰日在一年的最后
        let year = self.year as i64 - if self.month <= 2 { 1 } else { 0 };
        let era = year.div_euclid(400);
        let year_of_era = year.rem_euclid(400);
        let shifted = (self.month as i64 + 9) % 12;
        let day_of_year = (153 * shifted + 2) / 5 + self.day as i64 - 1;
        let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        era * 146_097 + day_of_era - 719_468
    }

    /// 从 1970-01-01 算起第 `days` 天。超出 `i32` 能表示的年份时停在两头
    pub const fn from_days_from_epoch(days: i64) -> Self {
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let day_of_era = z.rem_euclid(146_097);
        let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let shifted = (5 * day_of_year + 2) / 153;
        let day = (day_of_year - (153 * shifted + 2) / 5 + 1) as u8;
        let month = (if shifted < 10 { shifted + 3 } else { shifted - 9 }) as u8;
        let year = year_of_era + era * 400 + if month <= 2 { 1 } else { 0 };
        if year > i32::MAX as i64 {
            Self { year: i32::MAX, month: 12, day: 31 }
        } else if year < i32::MIN as i64 {
            Self { year: i32::MIN, month: 1, day: 1 }
        } else {
            Self { year: year as i32, month, day }
        }
    }

    /// 星期几
    pub const fn weekday(self) -> Weekday {
        // 1970-01-01 是星期四
        Weekday::ALL[(self.days_from_epoch() + 3).rem_euclid(7) as usize]
    }

    /// 往后（负数往前）这么多天
    pub const fn add_days(self, days: i64) -> Self {
        Self::from_days_from_epoch(self.days_from_epoch().saturating_add(days))
    }

    /// 往后（负数往前）这么多个月。新的月份没有这一天时取当月最后一天
    pub const fn add_months(self, months: i32) -> Self {
        let total = self.year as i64 * 12 + self.month as i64 - 1 + months as i64;
        let year = total.div_euclid(12);
        let year = if year > i32::MAX as i64 {
            i32::MAX
        } else if year < i32::MIN as i64 {
            i32::MIN
        } else {
            year as i32
        };
        let month = total.rem_euclid(12) as u8 + 1;
        let last = Self::days_in_month(year, month);
        Self { year, month, day: if self.day > last { last } else { self.day } }
    }

    /// 往后（负数往前）这么多年。2 月 29 日到了平年取 2 月 28 日
    pub const fn add_years(self, years: i32) -> Self {
        self.add_months(years.saturating_mul(12))
    }

    /// 这个月的第一天
    pub const fn first_of_month(self) -> Self {
        Self { year: self.year, month: self.month, day: 1 }
    }
}

/// `2026-11-05`
impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// 解析日期、时间失败：写法不认识，或者没有这一天（这一刻）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseDateError;

impl fmt::Display for ParseDateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("不是有效的日期或时间")
    }
}

impl std::error::Error for ParseDateError {}

/// 认这些写法：`2026-11-05`、`2026/11/5`、`2026.11.5`、`2026年11月5日`、`20261105`
impl FromStr for Date {
    type Err = ParseDateError;

    fn from_str(text: &str) -> Result<Self, ParseDateError> {
        let text = text.trim();
        let digits = text.chars().all(|c| c.is_ascii_digit());
        let (year, month, day) = if digits && text.len() == 8 {
            (&text[..4], &text[4..6], &text[6..])
        } else {
            let mut parts = text.split(['-', '/', '.', '年', '月', '日']).filter(|p| !p.is_empty());
            let found = (parts.next(), parts.next(), parts.next(), parts.next());
            let (Some(year), Some(month), Some(day), None) = found else { return Err(ParseDateError) };
            (year.trim(), month.trim(), day.trim())
        };
        let year = year.parse().map_err(|_| ParseDateError)?;
        let month = month.parse().map_err(|_| ParseDateError)?;
        let day = day.parse().map_err(|_| ParseDateError)?;
        Self::checked(year, month, day).ok_or(ParseDateError)
    }
}

/// 钟面上的一刻：时（0..24）、分（0..60）、秒（0..60）。比较时早的在前。
///
/// ```
/// # use aureole_core::Time;
/// let at = Time::new(14, 30);
/// assert_eq!(at.add_minutes(45), Time::new(15, 15));
/// assert_eq!(Time::new(23, 50).add_minutes(20), Time::new(0, 10), "过了午夜绕回来");
/// assert_eq!(at.to_string(), "14:30");
/// assert_eq!("14:30:05".parse(), Ok(at.with_second(5)));
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Time {
    hour: u8,
    minute: u8,
    second: u8,
}

/// 一天有多少秒
const SECONDS_PER_DAY: i64 = 86_400;

impl Time {
    /// 午夜 `00:00`
    pub const MIDNIGHT: Self = Self { hour: 0, minute: 0, second: 0 };

    /// 这一刻，秒是 0。不在范围里时 panic：写死的时间用它，来自用户、文件的用 [`Time::checked`] 或 `parse`
    #[track_caller]
    pub const fn new(hour: u8, minute: u8) -> Self {
        match Self::checked(hour, minute, 0) {
            Some(time) => time,
            None => panic!("没有这一刻"),
        }
    }

    /// 这一刻；不在范围里时是 `None`
    pub const fn checked(hour: u8, minute: u8, second: u8) -> Option<Self> {
        if hour < 24 && minute < 60 && second < 60 { Some(Self { hour, minute, second }) } else { None }
    }

    /// 换掉秒。超出范围时取 59
    pub const fn with_second(mut self, second: u8) -> Self {
        self.second = if second > 59 { 59 } else { second };
        self
    }

    /// 时，0..24
    pub const fn hour(self) -> u8 {
        self.hour
    }

    /// 分，0..60
    pub const fn minute(self) -> u8 {
        self.minute
    }

    /// 秒，0..60
    pub const fn second(self) -> u8 {
        self.second
    }

    /// 从午夜算起第几秒
    pub const fn seconds_from_midnight(self) -> u32 {
        self.hour as u32 * 3600 + self.minute as u32 * 60 + self.second as u32
    }

    /// 从午夜算起第 `seconds` 秒，一天之外的绕回来
    pub const fn from_seconds_from_midnight(seconds: i64) -> Self {
        let seconds = seconds.rem_euclid(SECONDS_PER_DAY);
        Self { hour: (seconds / 3600) as u8, minute: (seconds / 60 % 60) as u8, second: (seconds % 60) as u8 }
    }

    /// 往后（负数往前）这么多分钟，过了午夜绕回来
    pub const fn add_minutes(self, minutes: i64) -> Self {
        Self::from_seconds_from_midnight(self.seconds_from_midnight() as i64 + minutes.rem_euclid(1440) * 60)
    }
}

/// 秒是 0 时 `14:30`，否则 `14:30:05`
impl fmt::Display for Time {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:{:02}", self.hour, self.minute)?;
        if self.second != 0 {
            write!(f, ":{:02}", self.second)?;
        }
        Ok(())
    }
}

/// 认这些写法：`14:30`、`14:30:05`、`9:5`、`1430`、`143005`，全角冒号，后面或前面带上午下午（`2:30 PM`、`下午2:30`）
impl FromStr for Time {
    type Err = ParseDateError;

    fn from_str(text: &str) -> Result<Self, ParseDateError> {
        let lower = text.trim().to_lowercase();
        let (rest, period) = split_period(&lower);
        let rest = rest.trim();
        let digits = rest.chars().all(|c| c.is_ascii_digit());
        let (hour, minute, second) = if digits && matches!(rest.len(), 3 | 4 | 6) {
            let hour_len = if rest.len() == 3 { 1 } else { 2 };
            let (hour, tail) = rest.split_at(hour_len);
            (hour, &tail[..2], tail.get(2..).filter(|s| !s.is_empty()))
        } else {
            let mut parts = rest.split([':', '：']);
            let found = (parts.next(), parts.next(), parts.next(), parts.next());
            let (Some(hour), Some(minute), second, None) = found else { return Err(ParseDateError) };
            (hour, minute, second)
        };
        let number = |part: &str| part.trim().parse::<u8>().map_err(|_| ParseDateError);
        let hour = number(hour)?;
        let hour = match period {
            None => hour,
            Some(_) if hour == 0 || hour > 12 => return Err(ParseDateError),
            Some(Period::Morning) => hour % 12,
            Some(Period::Afternoon) => hour % 12 + 12,
        };
        Self::checked(hour, number(minute)?, second.map_or(Ok(0), number)?).ok_or(ParseDateError)
    }
}

enum Period {
    Morning,
    Afternoon,
}

/// 去掉前面或后面的上午、下午
fn split_period(text: &str) -> (&str, Option<Period>) {
    const MARKS: [(&str, Period); 4] = [("上午", Period::Morning), ("am", Period::Morning), ("下午", Period::Afternoon), ("pm", Period::Afternoon)];
    for (mark, period) in MARKS {
        if let Some(rest) = text.strip_prefix(mark).or_else(|| text.strip_suffix(mark)) {
            return (rest, Some(period));
        }
    }
    (text, None)
}
