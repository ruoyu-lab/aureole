//! 本进程的资源用量：常驻内存、物理内存占用、CPU 时间、唤醒次数、从进程启动到现在多久。
//!
//! 只用不需要任何权限的系统接口（macOS：`task_info`、`proc_pid_rusage`、`getrusage`；Linux：`/proc/self`、
//! `getrusage`；Windows：`GetProcessMemoryInfo`、`GetProcessTimes`）。拿不到的项返回 `None`。

use std::time::Duration;

/// 常驻内存（字节）：macOS 上是 `mach_task_basic_info` 的 `resident_size`，Linux 上是 `/proc/self/statm`
/// 的常驻页，Windows 上是工作集
pub fn resident_memory() -> Option<u64> {
    imp::resident_memory()
}

/// 物理内存占用（字节，macOS 的 `phys_footprint`：系统按它算这个进程用了多少内存，不含共享的只读页面）；
/// 其他平台返回 `None`
pub fn footprint() -> Option<u64> {
    imp::footprint()
}

/// 本进程到现在用掉的 CPU 时间（所有线程的用户态 + 内核态）
pub fn cpu_time() -> Option<Duration> {
    imp::cpu_time()
}

/// 本进程到现在被唤醒的次数（中断唤醒 + 从空闲状态唤醒，macOS 的 `proc_pid_rusage`）；其他平台返回 `None`
pub fn wakeup_count() -> Option<u64> {
    imp::wakeup_count()
}

/// 从进程启动到现在过了多久：一般从内核创建进程算起（含系统装入可执行文件和动态库的时间）。
///
/// 进程先跑了别的程序、过了一阵才换成这个程序时（`cargo run` 先构建、再在同一个进程里换成要运行的程序），
/// 内核记的创建时刻要早得多：两者差了 [`EXEC_GAP`] 以上时，从这个程序装入（进程的初始化函数，`main` 之前）算起。
/// Windows 上 `cargo run` 另起一个子进程，内核的时刻就是对的
pub fn since_start() -> Option<Duration> {
    started(imp::since_start(), loaded::since_load())
}

/// 内核记的时刻（`kernel`）和程序装入的时刻（`image`）里选一个，见 [`since_start`]
fn started(kernel: Option<Duration>, image: Option<Duration>) -> Option<Duration> {
    match (kernel, image) {
        (Some(kernel), Some(image)) if kernel > image + EXEC_GAP => Some(image),
        (Some(kernel), _) => Some(kernel),
        (None, image) => image,
    }
}

/// 内核记的进程创建时刻比程序装入早了这么多，就当作进程先跑过别的程序（见 [`since_start`]）。
/// 系统装入可执行文件和动态库（含第一次运行时的签名检查）一般在这以内，`cargo run` 构建一次要比它久
pub const EXEC_GAP: Duration = Duration::from_secs(1);

/// 程序装入的时刻：进程的初始化函数（动态链接之后、`main` 之前）记下。只在 Linux、macOS 上有：
/// 那里 `cargo run` 在同一个进程里换成要运行的程序，内核的创建时刻包含了构建的时间
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
mod loaded {
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    static LOADED: OnceLock<Instant> = OnceLock::new();

    /// 初始化函数：只读一次单调时钟、写一次 `OnceLock`，不分配，不碰标准库里要先初始化的东西（标准输出、线程局部变量）
    extern "C" fn record() {
        let _ = LOADED.set(Instant::now());
    }

    // 放进初始化函数表，装入时由动态链接器（Linux 是 C 运行库）调用。`#[used]` 让编译器不丢掉它；
    // `since_load` 读它一次，链接时含它的目标文件一定被带上
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[used]
    // SAFETY: `.init_array` 里放的是不带参数（多给的参数不看）、C 调用约定的函数指针，`record` 正是这样；
    // 它不依赖还没初始化的东西，在 `main` 之前调用是安全的
    #[unsafe(link_section = ".init_array")]
    static INIT: extern "C" fn() = record;

    #[cfg(target_vendor = "apple")]
    #[used]
    // SAFETY: `__DATA,__mod_init_func` 里放的是 C 调用约定的初始化函数指针，dyld 在 `main` 之前逐个调用，`record` 正是这样；
    // 它不依赖还没初始化的东西
    #[unsafe(link_section = "__DATA,__mod_init_func")]
    static INIT: extern "C" fn() = record;

    /// 程序装入以后过了多久
    pub fn since_load() -> Option<Duration> {
        std::hint::black_box(INIT);
        LOADED.get().map(Instant::elapsed)
    }
}

/// 别的平台不记装入的时刻（Windows 上 `cargo run` 另起子进程，内核的时刻就是对的）
#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
mod loaded {
    pub fn since_load() -> Option<std::time::Duration> {
        None
    }
}

#[cfg(unix)]
fn rusage_cpu() -> Option<Duration> {
    // SAFETY: rusage 只由整数和 timeval（也是整数）组成，全零是有效的值
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: RUSAGE_SELF 是合法的 who；指针指向上面这个可写的结构体，getrusage 只往里写
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
        return None;
    }
    let time = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    Some(time(usage.ru_utime) + time(usage.ru_stime))
}

// libc 建议这几个 mach 接口改用另一个 crate，接口本身在系统里没有废弃
#[cfg(target_os = "macos")]
#[allow(deprecated)]
mod imp {
    use std::time::Duration;

    pub fn resident_memory() -> Option<u64> {
        // SAFETY: mach_task_basic_info 只由整数和 time_value（也是整数）组成，全零是有效的值
        let mut info: libc::mach_task_basic_info = unsafe { std::mem::zeroed() };
        let mut count = libc::MACH_TASK_BASIC_INFO_COUNT;
        // SAFETY: mach_task_self 没有参数、不会失败，返回本进程的任务端口
        let task = unsafe { libc::mach_task_self() };
        // SAFETY: count 是 MACH_TASK_BASIC_INFO_COUNT，正好是 info 的大小（按 natural_t 计），task_info 最多写这么多
        let result = unsafe { libc::task_info(task, libc::MACH_TASK_BASIC_INFO, (&raw mut info).cast(), &mut count) };
        (result == libc::KERN_SUCCESS).then_some(info.resident_size)
    }

    /// `proc_pid_rusage` 第 4 版：内存、CPU、唤醒、启动时间都在里面
    fn rusage() -> Option<libc::rusage_info_v4> {
        // SAFETY: rusage_info_v4 只由整数组成，全零是有效的值
        let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
        // SAFETY: 版本号 RUSAGE_INFO_V4 和传进去的结构体 rusage_info_v4 对应，proc_pid_rusage 按版本只写这么大；
        // getpid 不会失败
        let result = unsafe { libc::proc_pid_rusage(libc::getpid(), libc::RUSAGE_INFO_V4, (&raw mut info).cast()) };
        (result == 0).then_some(info)
    }

    pub fn footprint() -> Option<u64> {
        rusage().map(|r| r.ri_phys_footprint)
    }

    pub fn cpu_time() -> Option<Duration> {
        super::rusage_cpu()
    }

    pub fn wakeup_count() -> Option<u64> {
        rusage().map(|r| r.ri_interrupt_wkups + r.ri_pkg_idle_wkups)
    }

    pub fn since_start() -> Option<Duration> {
        let start = rusage()?.ri_proc_start_abstime;
        // SAFETY: mach_absolute_time 没有参数、不会失败，只读系统时钟
        let now = unsafe { libc::mach_absolute_time() };
        let mut base = libc::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: 指针指向上面这个可写的结构体，mach_timebase_info 只往里写两个整数
        if unsafe { libc::mach_timebase_info(&mut base) } != 0 || base.denom == 0 {
            return None;
        }
        let ticks = u128::from(now.checked_sub(start)?);
        Some(Duration::from_nanos((ticks * u128::from(base.numer) / u128::from(base.denom)) as u64))
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::time::Duration;

    pub fn resident_memory() -> Option<u64> {
        let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
        let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        // SAFETY: _SC_PAGESIZE 是合法的参数名；sysconf 不碰内存，出错时返回 -1（下面只接受正数）
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        (page > 0).then(|| pages * page as u64)
    }

    pub fn footprint() -> Option<u64> {
        None
    }

    pub fn cpu_time() -> Option<Duration> {
        super::rusage_cpu()
    }

    pub fn wakeup_count() -> Option<u64> {
        None
    }

    pub fn since_start() -> Option<Duration> {
        // /proc/self/stat 第 22 项：开机后第几个时钟周期启动；/proc/uptime：开机了多少秒
        let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
        let after_name = &stat[stat.rfind(')')? + 2..];
        let start: f64 = after_name.split_whitespace().nth(19)?.parse().ok()?;
        // SAFETY: _SC_CLK_TCK 是合法的参数名；sysconf 不碰内存，出错时返回 -1（下面只接受正数）
        let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
        let uptime: f64 = std::fs::read_to_string("/proc/uptime").ok()?.split_whitespace().next()?.parse().ok()?;
        (ticks > 0.0).then(|| Duration::from_secs_f64((uptime - start / ticks).max(0.0)))
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use std::time::Duration;

    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    fn ticks(time: FILETIME) -> u64 {
        (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
    }

    /// 创建时间、内核态时间、用户态时间（100 纳秒为单位）
    fn times() -> Option<[u64; 3]> {
        let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
        // SAFETY: 只写传进去的四个结构体
        let ok = unsafe { GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user) };
        (ok != 0).then(|| [ticks(created), ticks(kernel), ticks(user)])
    }

    pub fn resident_memory() -> Option<u64> {
        // SAFETY: PROCESS_MEMORY_COUNTERS 只由整数组成，全零是有效的值
        let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
        let size = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        // SAFETY: 给出的大小就是 counters 的大小，函数最多写这么多；GetCurrentProcess 返回的伪句柄总是有效、不用关
        let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, size) };
        (ok != 0).then_some(counters.WorkingSetSize as u64)
    }

    pub fn footprint() -> Option<u64> {
        None
    }

    pub fn cpu_time() -> Option<Duration> {
        let [_, kernel, user] = times()?;
        Some(Duration::from_nanos((kernel + user) * 100))
    }

    pub fn wakeup_count() -> Option<u64> {
        None
    }

    pub fn since_start() -> Option<Duration> {
        let [created, ..] = times()?;
        let mut now = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        // SAFETY: 只写传进去的结构体
        unsafe { GetSystemTimeAsFileTime(&mut now) };
        Some(Duration::from_nanos(ticks(now).checked_sub(created)? * 100))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
mod imp {
    use std::time::Duration;

    pub fn resident_memory() -> Option<u64> {
        None
    }

    pub fn footprint() -> Option<u64> {
        None
    }

    pub fn cpu_time() -> Option<Duration> {
        #[cfg(unix)]
        return super::rusage_cpu();
        #[cfg(not(unix))]
        None
    }

    pub fn wakeup_count() -> Option<u64> {
        None
    }

    pub fn since_start() -> Option<Duration> {
        None
    }
}
