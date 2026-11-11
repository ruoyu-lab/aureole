//! 单实例：同一个用户只运行一个实例，再启动时把参数转给已经在运行的那个，然后退出。
//!
//! 每个标识一把锁加一个本地通道，只有这个用户能用：
//!
//! - macOS、Linux：锁文件（`flock`）+ Unix 域套接字，放在每个用户自己的临时目录（macOS，同 `$TMPDIR`）或
//!   `$XDG_RUNTIME_DIR`（Linux）里，没有时用临时目录（名字里带用户号）。拿到锁的是第一个实例：
//!   删掉上次崩溃留下的套接字文件，重新绑定。锁随进程结束由内核释放，崩溃了也不会一直锁着；
//!   正常退出时删掉两个文件。接受连接时核对对方的用户号。
//! - Windows：命名互斥量（`Local\…`，每个登录会话一个）+ 命名管道（名字带用户名，不接受别的机器的连接）。
//!   转发前允许第一个实例把窗口带到最前面（`AllowSetForegroundWindow`）。
//!
//! 两个进程同时启动时只有一个拿到锁；另一个连接通道（第一个可能还没开始监听，隔一会儿再试），
//! 转发后等第一个实例确认收到。消息是带长度的二进制帧：参数、网址、文件，路径按系统的原始编码传。

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[cfg(unix)]
use unix as sys;
#[cfg(windows)]
use windows as sys;
#[cfg(not(any(unix, windows)))]
use unsupported as sys;

/// 后启动的实例转给第一个实例的东西
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Message {
    /// 命令行参数（不含程序名）
    pub args: Vec<OsString>,
    /// 要打开的网址
    pub urls: Vec<String>,
    /// 要打开的文件（绝对路径）
    pub files: Vec<PathBuf>,
}

/// 这个进程是第一个实例，还是已经有实例在运行
pub enum InstanceRole {
    /// 拿到了锁：用 [`Primary::listen`] 接收后启动的实例转来的消息
    Primary(Primary),
    /// 已经有实例在运行，连上了它：用 [`Secondary::send`] 转发
    Secondary(Secondary),
}

/// 第一个实例：拿着锁和监听的通道
pub struct Primary(PrimaryKind);

enum PrimaryKind {
    System(sys::Primary),
    InProcess(InProcessClaim),
}

/// 连上了第一个实例
pub struct Secondary(SecondaryKind);

enum SecondaryKind {
    System(sys::Secondary),
    InProcess { slots: Slots, id: String },
}

/// 正在接收消息（后台线程）；释放时停止监听、放开锁
pub struct Listening {
    stop: Arc<AtomicBool>,
    inner: ListeningKind,
}

enum ListeningKind {
    System(sys::Listening),
    InProcess(#[allow(dead_code)] InProcessClaim),
}

/// 收到消息时调用（在后台线程里）
pub type OnMessage = Arc<dyn Fn(Message) + Send + Sync>;

/// 按标识 `id`（同一个用户、同一个标识只运行一个）决定这个进程的角色。
/// 锁在别人手里而通道还连不上时（第一个实例刚拿到锁、还没开始监听，或者正在退出）隔一会儿再试，最多 3 秒。
///
/// `wait` 为真时不当后启动的那个，一直等到拿到锁（最多 10 秒）：开发工具重启进程时，旧进程退出前新进程已经起来了
pub fn acquire(id: &str, wait: bool) -> io::Result<InstanceRole> {
    let deadline = Instant::now() + Duration::from_secs(if wait { 10 } else { 3 });
    loop {
        if let Some(primary) = sys::try_primary(id)? {
            return Ok(InstanceRole::Primary(Primary(PrimaryKind::System(primary))));
        }
        if !wait && let Some(secondary) = sys::try_connect(id)? {
            return Ok(InstanceRole::Secondary(Secondary(SecondaryKind::System(secondary))));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "已经有实例拿着锁，但连不上它的通道"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

impl Primary {
    /// 在后台线程上接收消息：每个连接一个短命的线程，读完消息调用 `on_message`，再回复确认
    pub fn listen(self, on_message: impl Fn(Message) + Send + Sync + 'static) -> Listening {
        let stop = Arc::new(AtomicBool::new(false));
        let inner = match self.0 {
            PrimaryKind::System(primary) => ListeningKind::System(sys::listen(primary, stop.clone(), Arc::new(on_message))),
            PrimaryKind::InProcess(claim) => {
                lock(&claim.slots).insert(claim.id.clone(), Some(Arc::new(on_message)));
                ListeningKind::InProcess(claim)
            }
        };
        Listening { stop, inner }
    }
}

impl Secondary {
    /// 转发，等第一个实例确认收到（最多约 5 秒）
    pub fn send(self, message: &Message) -> io::Result<()> {
        match self.0 {
            SecondaryKind::System(secondary) => sys::send(secondary, message),
            SecondaryKind::InProcess { slots, id } => {
                let listener = lock(&slots).get(&id).cloned().flatten();
                let listener = listener.ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "第一个实例已经不在了"))?;
                listener(message.clone());
                Ok(())
            }
        }
    }
}

impl Drop for Listening {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let ListeningKind::System(inner) = &mut self.inner {
            sys::stop(inner);
        }
    }
}

// ---------- 进程内的单实例（无头后端用） ----------

/// 标识 → 第一个实例的监听回调（拿到了锁、还没开始监听时是 `None`）
type Slots = Arc<Mutex<HashMap<String, Option<OnMessage>>>>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// 进程内的一把"锁"：拿着时登记表里有这个标识，释放时去掉
struct InProcessClaim {
    slots: Slots,
    id: String,
}

impl Drop for InProcessClaim {
    fn drop(&mut self) {
        lock(&self.slots).remove(&self.id);
    }
}

/// 进程内的单实例：同一个登记表里同一个标识只有一个第一个实例，转发直接调用它的监听回调。
/// 和真的单实例同样的规则（还没开始监听时隔一会儿再试、`wait` 时等锁），只是不碰文件和套接字
#[derive(Clone, Default)]
pub(crate) struct InProcessInstances {
    slots: Slots,
}

impl InProcessInstances {
    pub(crate) fn acquire(&self, id: &str, wait: bool) -> io::Result<InstanceRole> {
        let deadline = Instant::now() + Duration::from_secs(if wait { 10 } else { 3 });
        loop {
            {
                let mut slots = lock(&self.slots);
                match slots.get(id) {
                    None => {
                        slots.insert(id.to_owned(), None);
                        let claim = InProcessClaim { slots: self.slots.clone(), id: id.to_owned() };
                        return Ok(InstanceRole::Primary(Primary(PrimaryKind::InProcess(claim))));
                    }
                    Some(Some(_)) if !wait => {
                        return Ok(InstanceRole::Secondary(Secondary(SecondaryKind::InProcess { slots: self.slots.clone(), id: id.to_owned() })));
                    }
                    _ => {}
                }
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "已经有实例拿着锁，但它还没开始监听"));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// 有实例拿着这个标识
    pub(crate) fn is_held(&self, id: &str) -> bool {
        lock(&self.slots).contains_key(id)
    }
}

/// 标识换成文件名、管道名里的一段：FNV-1a，各版本、各平台一样
fn hash(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

// ---------- 消息的格式 ----------

const MAGIC: &[u8; 8] = b"AUREOLE\x01";
const ACK: &[u8; 2] = b"OK";
/// 防止坏数据让第一个实例分配大块内存
const MAX_ITEMS: u32 = 4096;
const MAX_LEN: u32 = 1 << 20;

/// 写一条消息：魔数、项数，每项是类型（参数 / 网址 / 文件）、长度、内容
pub(crate) fn write_message(w: &mut impl Write, message: &Message) -> io::Result<()> {
    let mut buffer = Vec::from(&MAGIC[..]);
    let count = message.args.len() + message.urls.len() + message.files.len();
    buffer.extend_from_slice(&(count as u32).to_le_bytes());
    let mut item = |tag: u8, bytes: Vec<u8>| {
        buffer.push(tag);
        buffer.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        buffer.extend_from_slice(&bytes);
    };
    for arg in &message.args {
        item(b'A', os_to_bytes(arg.clone()));
    }
    for url in &message.urls {
        item(b'U', url.as_bytes().to_vec());
    }
    for file in &message.files {
        item(b'F', os_to_bytes(file.clone().into_os_string()));
    }
    w.write_all(&buffer)?;
    w.flush()
}

pub(crate) fn read_message(r: &mut impl Read) -> io::Result<Message> {
    let invalid = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_owned());
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(invalid("不是单实例的消息"));
    }
    let mut word = [0u8; 4];
    r.read_exact(&mut word)?;
    let count = u32::from_le_bytes(word);
    if count > MAX_ITEMS {
        return Err(invalid("项数太多"));
    }
    let mut message = Message::default();
    for _ in 0..count {
        let mut tag = [0u8; 1];
        r.read_exact(&mut tag)?;
        r.read_exact(&mut word)?;
        let len = u32::from_le_bytes(word);
        if len > MAX_LEN {
            return Err(invalid("一项太长"));
        }
        let mut bytes = vec![0u8; len as usize];
        r.read_exact(&mut bytes)?;
        match tag[0] {
            b'A' => message.args.push(bytes_to_os(bytes).ok_or_else(|| invalid("参数的编码不对"))?),
            b'U' => message.urls.push(String::from_utf8(bytes).map_err(|_| invalid("网址不是 UTF-8"))?),
            b'F' => message.files.push(bytes_to_os(bytes).ok_or_else(|| invalid("路径的编码不对"))?.into()),
            _ => return Err(invalid("不认识的项")),
        }
    }
    Ok(message)
}

/// 读消息、回复确认、再交给回调。先确认：回调里第一个实例可能马上退出（或者处理得很慢），
/// 确认晚了，第二个实例就会以为转发失败
fn serve(stream: &mut (impl Read + Write), on_message: &OnMessage) -> io::Result<()> {
    let message = read_message(stream)?;
    stream.write_all(ACK)?;
    stream.flush()?;
    on_message(message);
    Ok(())
}

/// 发消息、等确认
fn exchange(stream: &mut (impl Read + Write), message: &Message) -> io::Result<()> {
    write_message(stream, message)?;
    let mut ack = [0u8; 2];
    stream.read_exact(&mut ack)?;
    if &ack == ACK { Ok(()) } else { Err(io::Error::new(io::ErrorKind::InvalidData, "第一个实例的回复不对")) }
}

#[cfg(unix)]
fn os_to_bytes(text: OsString) -> Vec<u8> {
    use std::os::unix::ffi::OsStringExt;
    text.into_vec()
}

#[cfg(unix)]
fn bytes_to_os(bytes: Vec<u8>) -> Option<OsString> {
    use std::os::unix::ffi::OsStringExt;
    Some(OsString::from_vec(bytes))
}

/// Windows 的字符串是 UTF-16（可能不成对），按小端的 16 位整数传
#[cfg(windows)]
fn os_to_bytes(text: OsString) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    text.encode_wide().flat_map(u16::to_le_bytes).collect()
}

#[cfg(windows)]
fn bytes_to_os(bytes: Vec<u8>) -> Option<OsString> {
    use std::os::windows::ffi::OsStringExt;
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let wide: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
    Some(OsString::from_wide(&wide))
}

#[cfg(not(any(unix, windows)))]
fn os_to_bytes(text: OsString) -> Vec<u8> {
    text.to_string_lossy().into_owned().into_bytes()
}

#[cfg(not(any(unix, windows)))]
fn bytes_to_os(bytes: Vec<u8>) -> Option<OsString> {
    String::from_utf8(bytes).ok().map(OsString::from)
}

#[cfg(unix)]
mod unix {
    use std::fs::{File, OpenOptions, Permissions};
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::{Message, OnMessage};

    pub struct Primary {
        listener: UnixListener,
        held: Held,
    }

    pub struct Secondary(UnixStream);

    pub struct Listening {
        socket: PathBuf,
        held: Option<Held>,
    }

    /// 拿着的锁。释放时先删掉套接字文件和锁文件、再放开锁（关掉文件）：等在旧锁文件上的进程拿到锁后，
    /// 发现路径上已经不是这个文件了，会重来（见 `try_primary`），所以删锁文件不会让两个进程都当上第一个
    struct Held {
        /// 关掉文件就放开了锁
        _lock: File,
        lock_path: PathBuf,
        socket: PathBuf,
    }

    impl Drop for Held {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.socket);
            let _ = std::fs::remove_file(&self.lock_path);
        }
    }

    /// 套接字和锁文件的路径
    pub fn paths(id: &str) -> (PathBuf, PathBuf) {
        // SAFETY: getuid 没有参数、不碰内存，总是成功
        let uid = unsafe { libc::getuid() };
        let name = format!("aureole-{:016x}", super::hash(&format!("{id}\0{uid}")));
        let mut dir = runtime_dir();
        // 套接字路径最长约 104 字节（macOS）
        if dir.join(format!("{name}.sock")).as_os_str().len() > 100 {
            dir = PathBuf::from("/tmp");
        }
        (dir.join(format!("{name}.sock")), dir.join(format!("{name}.lock")))
    }

    fn runtime_dir() -> PathBuf {
        #[cfg(target_os = "linux")]
        if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|d| d.is_absolute() && d.is_dir()) {
            return dir;
        }
        #[cfg(target_os = "macos")]
        if let Some(dir) = user_temp_dir() {
            return dir;
        }
        std::env::temp_dir()
    }

    /// macOS 每个用户的临时目录：和 `$TMPDIR` 是同一个，但不看环境变量，从终端、访达、`open` 启动都一样
    #[cfg(target_os = "macos")]
    fn user_temp_dir() -> Option<PathBuf> {
        use std::os::unix::ffi::OsStringExt;
        let mut buffer = vec![0u8; 1024];
        // SAFETY: 缓冲区的长度如实给出；返回需要的长度（含结尾的 0），0 是失败
        let len = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, buffer.as_mut_ptr().cast(), buffer.len()) };
        if len == 0 || len > buffer.len() {
            return None;
        }
        buffer.truncate(len - 1);
        let dir = PathBuf::from(std::ffi::OsString::from_vec(buffer));
        dir.is_dir().then_some(dir)
    }

    pub fn try_primary(id: &str) -> io::Result<Option<Primary>> {
        let (socket, lock_path) = paths(id);
        let lock = OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(&lock_path)?;
        // SAFETY: lock 是刚打开的文件，借着它调用，描述符在调用期间一直有效；flock 不碰内存
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(libc::EWOULDBLOCK) { Ok(None) } else { Err(error) };
        }
        // 锁住的是上一个实例退出时删掉了的文件：不算，下一轮重新打开
        let held = lock.metadata()?;
        if !std::fs::metadata(&lock_path).is_ok_and(|now| now.ino() == held.ino() && now.dev() == held.dev()) {
            return Ok(None);
        }
        // 拿到了锁：之前的实例已经不在了，留下的套接字文件是死的
        match std::fs::remove_file(&socket) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let held = Held { _lock: lock, lock_path, socket };
        let listener = UnixListener::bind(&held.socket)?;
        std::fs::set_permissions(&held.socket, Permissions::from_mode(0o600))?;
        Ok(Some(Primary { listener, held }))
    }

    /// 连上第一个实例的通道；连不上（还没开始监听、正在退出）时是 `None`，隔一会儿再试
    pub fn try_connect(id: &str) -> io::Result<Option<Secondary>> {
        Ok(UnixStream::connect(paths(id).0).ok().map(Secondary))
    }

    pub fn listen(primary: Primary, stop: Arc<AtomicBool>, on_message: OnMessage) -> Listening {
        let Primary { listener, held } = primary;
        let socket = held.socket.clone();
        let spawned = std::thread::Builder::new().name("single-instance".into()).spawn(move || {
            for stream in listener.incoming() {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(mut stream) = stream else { continue };
                if !same_user(&stream) {
                    continue;
                }
                let on_message = on_message.clone();
                let _ = std::thread::Builder::new().name("single-instance: message".into()).spawn(move || {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                    let _ = super::serve(&mut stream, &on_message);
                });
            }
        });
        if let Err(e) = spawned {
            tracing::warn!("单实例：启动监听线程失败：{e}");
        }
        Listening { socket, held: Some(held) }
    }

    pub fn stop(listening: &mut Listening) {
        // 连一下，让阻塞在 accept 上的线程醒来看到停止标记；再删文件、放开锁
        let _ = UnixStream::connect(&listening.socket);
        listening.held = None;
    }

    pub fn send(secondary: Secondary, message: &Message) -> io::Result<()> {
        let mut stream = secondary.0;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        super::exchange(&mut stream, message)
    }

    /// 连接的另一端是不是同一个用户
    fn same_user(stream: &UnixStream) -> bool {
        // SAFETY: getuid 没有参数、不碰内存，总是成功
        peer_uid(stream.as_raw_fd()) == Some(unsafe { libc::getuid() })
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn peer_uid(fd: libc::c_int) -> Option<libc::uid_t> {
        // SAFETY: 只写给出的结构体，长度是它的大小
        unsafe {
            let mut cred: libc::ucred = std::mem::zeroed();
            let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
            let ok = libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_PEERCRED, (&mut cred as *mut libc::ucred).cast(), &mut len) == 0;
            ok.then_some(cred.uid)
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn peer_uid(fd: libc::c_int) -> Option<libc::uid_t> {
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: 只写给出的两个变量
        (unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } == 0).then_some(uid)
    }
}

#[cfg(windows)]
mod windows {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
    use std::sync::{Arc, OnceLock};
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
    use windows_sys::Win32::Security::{EqualSid, GetTokenInformation, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION};
    use windows_sys::Win32::System::IO::CancelIoEx;
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeServerProcessId, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
        PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
    };
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows_sys::Win32::System::Threading::{
        CreateMutexW, GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;

    use super::{Message, OnMessage};

    /// 管道上的一次收发最多等这么久：对方卡住（被挂起、停在调试器里）时取消，不一直等下去
    const IO_TIMEOUT: Duration = Duration::from_secs(5);

    /// 句柄跨线程用：只在一个线程里用它
    struct Handle(HANDLE);
    // SAFETY: 内核对象的句柄可以在任何线程里用和关闭
    unsafe impl Send for Handle {}

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: 句柄有效，只关一次
            unsafe { CloseHandle(self.0) };
        }
    }

    pub struct Primary {
        mutex: Handle,
        pipe: Handle,
        name: String,
    }

    pub struct Secondary(Arc<File>);

    pub struct Listening {
        name: String,
        mutex: Option<Handle>,
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    /// 进程令牌里的用户（`TOKEN_USER`）：按 8 字节对齐的缓冲区，里面的 SID 指针指向缓冲区自己
    struct TokenUserBuffer(Vec<u64>);

    impl TokenUserBuffer {
        /// 进程（句柄要有查询的权限）的用户；拿不到时是 `None`
        fn of(process: HANDLE) -> Option<Self> {
            // SAFETY: 令牌句柄用完关掉；先问大小，再写进够大的、对齐的缓冲区
            unsafe {
                let mut token = std::ptr::null_mut();
                if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
                    return None;
                }
                let token = Handle(token);
                let mut size = 0u32;
                GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut size);
                if size == 0 {
                    return None;
                }
                let mut buffer = vec![0u64; (size as usize).div_ceil(8)];
                (GetTokenInformation(token.0, TokenUser, buffer.as_mut_ptr().cast(), size, &mut size) != 0).then_some(Self(buffer))
            }
        }

        fn sid(&self) -> PSID {
            // SAFETY: 缓冲区是 GetTokenInformation(TokenUser) 填的，开头是 TOKEN_USER，按 8 字节对齐
            unsafe { (*self.0.as_ptr().cast::<TOKEN_USER>()).User.Sid }
        }
    }

    // SAFETY: 缓冲区只读；里面的指针指向它自己的堆内存，跟着它走
    unsafe impl Send for TokenUserBuffer {}
    // SAFETY: 同上，只读
    unsafe impl Sync for TokenUserBuffer {}

    /// 这个进程的用户和它的 SID 的文字形式（`S-1-5-21-…`）
    fn own_user() -> io::Result<&'static (TokenUserBuffer, String)> {
        static USER: OnceLock<Option<(TokenUserBuffer, String)>> = OnceLock::new();
        let user = USER.get_or_init(|| {
            // SAFETY: 当前进程的伪句柄总是有效、不用关；转出来的字符串用 LocalFree 还掉
            unsafe {
                let user = TokenUserBuffer::of(GetCurrentProcess())?;
                let mut text = std::ptr::null_mut();
                if ConvertSidToStringSidW(user.sid(), &mut text) == 0 || text.is_null() {
                    return None;
                }
                let len = (0..).take_while(|&i| *text.add(i) != 0).count();
                let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
                LocalFree(text.cast());
                Some((user, sid))
            }
        });
        user.as_ref().ok_or_else(|| io::Error::other("拿不到这个进程的用户"))
    }

    /// 这个进程所在的登录会话
    fn session() -> u32 {
        let mut session = 0u32;
        // SAFETY: 只写栈上的整数
        unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) };
        session
    }

    /// 互斥量和管道的名字：按标识、用户（SID，不是可以改的环境变量）分开；互斥量在会话自己的名字空间里（`Local\`），
    /// 管道是全机器的，名字里再带上会话编号（同一个用户在两个会话里登录时各管各的）
    pub(super) fn names_for(id: &str, user: &str, session: u32) -> (String, String) {
        let key = format!("{:016x}", super::hash(&format!("{id}\0{user}")));
        (format!("Local\\aureole-{key}"), format!(r"\\.\pipe\aureole-{key}-{session}"))
    }

    fn names(id: &str) -> io::Result<(String, String)> {
        Ok(names_for(id, &own_user()?.1, session()))
    }

    /// 只有这个用户能用的安全描述（`D:P(A;;GA;;;<SID>)`）：别的用户打不开互斥量、连不上管道，也建不了同名的实例
    struct OnlyUser(PSECURITY_DESCRIPTOR);

    impl OnlyUser {
        fn new() -> io::Result<Self> {
            let sddl = wide(&format!("D:P(A;;GA;;;{})", own_user()?.1));
            let mut descriptor = std::ptr::null_mut();
            // SAFETY: 描述以 0 结尾；转出来的安全描述由 Drop 用 LocalFree 还掉
            let ok = unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut descriptor, std::ptr::null_mut()) };
            if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(Self(descriptor)) }
        }

        fn attributes(&self) -> SECURITY_ATTRIBUTES {
            SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: self.0, bInheritHandle: 0 }
        }
    }

    impl Drop for OnlyUser {
        fn drop(&mut self) {
            // SAFETY: 安全描述是 ConvertStringSecurityDescriptorToSecurityDescriptorW 分配的，只还一次
            unsafe { LocalFree(self.0) };
        }
    }

    fn create_pipe(name: &str, first: bool, security: &OnlyUser) -> io::Result<Handle> {
        let mode = PIPE_ACCESS_DUPLEX | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
        let name = wide(name);
        let attributes = security.attributes();
        // SAFETY: 名字以 0 结尾；安全属性和它指向的安全描述活到调用结束
        let pipe = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                4096,
                4096,
                0,
                &attributes,
            )
        };
        if pipe == INVALID_HANDLE_VALUE { Err(io::Error::last_os_error()) } else { Ok(Handle(pipe)) }
    }

    pub fn try_primary(id: &str) -> io::Result<Option<Primary>> {
        let (mutex_name, name) = names(id)?;
        let security = OnlyUser::new()?;
        let attributes = security.attributes();
        let wide_name = wide(&mutex_name);
        // SAFETY: 名字以 0 结尾，安全属性活到调用结束；GetLastError 紧跟在 CreateMutexW 后面
        let (mutex, error) = unsafe {
            let mutex = CreateMutexW(&attributes, 0, wide_name.as_ptr());
            (mutex, GetLastError())
        };
        if mutex.is_null() {
            // 别的权限建的（以管理员身份运行的实例）：打不开，也就是已经有实例了
            if error == ERROR_ACCESS_DENIED {
                return Ok(None);
            }
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        let mutex = Handle(mutex);
        if error == ERROR_ALREADY_EXISTS {
            return Ok(None);
        }
        let pipe = create_pipe(&name, true, &security)?;
        Ok(Some(Primary { mutex, pipe, name }))
    }

    /// 打开管道（客户端）：只许服务端认出是谁（`SECURITY_IDENTIFICATION`），不许它冒充这个进程
    fn open_pipe(name: &str) -> io::Result<File> {
        OpenOptions::new().read(true).write(true).security_qos_flags(SECURITY_IDENTIFICATION).open(name)
    }

    /// 进程 `pid` 是不是这个用户的（拿不到它的令牌时不算）
    fn same_user(pid: u32) -> bool {
        let Ok((own, _)) = own_user() else { return false };
        // SAFETY: 进程句柄用完关掉；两个 SID 都指向活着的缓冲区
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                return false;
            }
            let process = Handle(process);
            TokenUserBuffer::of(process.0).is_some_and(|theirs| EqualSid(theirs.sid(), own.sid()) != 0)
        }
    }

    /// 连上第一个实例的通道。还没开始监听、正忙、正在退出时是 `Ok(None)`（隔一会儿再试）；
    /// 连上了却不是这个用户的进程开的、或者权限不够连不上（第一个实例以管理员身份运行）时是错误，不再试
    pub fn try_connect(id: &str) -> io::Result<Option<Secondary>> {
        let name = names(id)?.1;
        match open_pipe(&name) {
            Ok(file) => {
                let mut pid = 0u32;
                // SAFETY: 句柄是刚打开的管道，只写栈上的整数
                let known = unsafe { GetNamedPipeServerProcessId(file.as_raw_handle() as HANDLE, &mut pid) } != 0;
                if !known || !same_user(pid) {
                    return Err(io::Error::new(io::ErrorKind::PermissionDenied, "通道不是这个用户的进程开的（或者认不出来），不转发"));
                }
                // SAFETY: 允许第一个实例把窗口带到最前面（这个进程是用户刚启动的，有这个权利）
                unsafe { AllowSetForegroundWindow(pid) };
                Ok(Some(Secondary(Arc::new(file))))
            }
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                let name = wide(&name);
                // SAFETY: 名字以 0 结尾
                unsafe { WaitNamedPipeW(name.as_ptr(), 500) };
                Ok(None)
            }
            Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("连不上第一个实例的通道（权限不同，比如它以管理员身份运行）：{e}")))
            }
            Err(_) => Ok(None),
        }
    }

    /// 看着一次收发：超过 [`IO_TIMEOUT`] 还没做完（这个守卫还没释放）时取消管道上进行中的读写，读写返回错误
    struct Watchdog {
        _done: Sender<()>,
    }

    fn watchdog(file: &Arc<File>) -> Watchdog {
        let (done, finished) = channel::<()>();
        let file = file.clone();
        let _ = std::thread::Builder::new().name("single-instance: timeout".into()).spawn(move || {
            if finished.recv_timeout(IO_TIMEOUT) == Err(RecvTimeoutError::Timeout) {
                // SAFETY: 句柄活着（`file` 拿着）；取消这个进程在它上面进行中的读写
                unsafe { CancelIoEx(file.as_raw_handle() as HANDLE, std::ptr::null()) };
            }
        });
        Watchdog { _done: done }
    }

    pub fn listen(primary: Primary, stop: Arc<AtomicBool>, on_message: OnMessage) -> Listening {
        let Primary { mutex, pipe, name } = primary;
        let pipe_name = name.clone();
        let spawned = std::thread::Builder::new().name("single-instance".into()).spawn(move || {
            let security = match OnlyUser::new() {
                Ok(security) => security,
                Err(e) => {
                    tracing::warn!("单实例：建不了管道的安全描述，不再接收：{e}");
                    return;
                }
            };
            let mut pipe = Some(pipe);
            loop {
                let Some(current) = pipe.take() else { break };
                // SAFETY: 句柄是有效的管道；客户端在创建和等待之间连上时返回 ERROR_PIPE_CONNECTED，也算连上
                let connected = unsafe { ConnectNamedPipe(current.0, std::ptr::null_mut()) != 0 || GetLastError() == ERROR_PIPE_CONNECTED };
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                if connected {
                    // 交给 File 管：关掉它就断开了这个连接
                    let raw = current.0 as RawHandle;
                    std::mem::forget(current);
                    // SAFETY: 句柄的所有权转给 File
                    let file = Arc::new(unsafe { File::from_raw_handle(raw) });
                    let on_message = on_message.clone();
                    let _ = std::thread::Builder::new().name("single-instance: message".into()).spawn(move || {
                        let _watch = watchdog(&file);
                        let mut stream: &File = &file;
                        if super::serve(&mut stream, &on_message).is_ok() {
                            // 等对方读完确认再断开
                            let _ = file.sync_all();
                        }
                    });
                }
                match create_pipe(&pipe_name, false, &security) {
                    Ok(next) => pipe = Some(next),
                    Err(e) => {
                        tracing::warn!("单实例：创建管道失败，不再接收：{e}");
                        break;
                    }
                }
            }
        });
        if let Err(e) = spawned {
            tracing::warn!("单实例：启动监听线程失败：{e}");
        }
        Listening { name, mutex: Some(mutex) }
    }

    pub fn stop(listening: &mut Listening) {
        // 连一下，让等在 ConnectNamedPipe 上的线程醒来看到停止标记
        let _ = open_pipe(&listening.name);
        listening.mutex = None;
    }

    pub fn send(secondary: Secondary, message: &Message) -> io::Result<()> {
        let file = secondary.0;
        let _watch = watchdog(&file);
        let mut stream: &File = &file;
        super::exchange(&mut stream, message)
    }

}

#[cfg(not(any(unix, windows)))]
mod unsupported {
    use std::io;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use super::{Message, OnMessage};

    pub struct Primary;
    pub struct Secondary;
    pub struct Listening;

    /// 不支持的平台：每个进程都当第一个实例，收不到消息
    pub fn try_primary(_id: &str) -> io::Result<Option<Primary>> {
        Ok(Some(Primary))
    }

    pub fn try_connect(_id: &str) -> io::Result<Option<Secondary>> {
        Ok(None)
    }

    pub fn listen(_primary: Primary, _stop: Arc<AtomicBool>, _on_message: OnMessage) -> Listening {
        Listening
    }

    pub fn stop(_listening: &mut Listening) {}

    pub fn send(_secondary: Secondary, _message: &Message) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "这个平台没有单实例"))
    }
}

/// 套接字文件和锁文件的路径（测试用）
#[cfg(unix)]
#[doc(hidden)]
pub fn paths(id: &str) -> (PathBuf, PathBuf) {
    unix::paths(id)
}
