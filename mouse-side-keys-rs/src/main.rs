//! mouse_side_keys_rs - 用鼠标侧键切换 Windows 虚拟桌面（Rust 版，零运行时）
//!
//! 侧键 1（后退键 / XBUTTON1） -> Ctrl + Win + Right   切到右侧虚拟桌面
//! 侧键 2（前进键 / XBUTTON2） -> Ctrl + Win + Left    切到左侧虚拟桌面
//!
//! （默认对调方向，即用户习惯；标准方向用 --no-swap。）
//!
//! 只依赖 windows-sys 对 Win32 的纯链接绑定，不需要 .NET / Python / 任何运行时。
//! 编译产物是一个几 KB~几百 KB 的原生 exe，拷走即用。
//!
//! 行为与 Python 版（mouse_side_keys.py）完全一致：
//!   - 全局低级鼠标钩子 WH_MOUSE_LL，回调里只投递消息、真正按键在主循环发
//!   - 默认按键合成方式 rawsplit：扫描码(wVk=0) + 逐个发送 + 10ms 间隔
//!     （实测唯一能同时绕开「Win 键按虚拟键码被过滤」和「整批 SendInput 被拒」）
//!   - 命名互斥体单实例、UIPI 完整性级别检测与弹窗、UAC 提权、后台常驻
//!
//! 用法：
//!   mouse_side_keys_rs.exe                 启动（后台常驻，无窗口）
//!   mouse_side_keys_rs.exe --foreground    前台运行（调试用，Ctrl+C 停止）
//!   mouse_side_keys_rs.exe --no-swap       标准方向（侧键1 切左、侧键2 切右）
//!   mouse_side_keys_rs.exe --no-block      不吞掉侧键原有前进/后退功能
//!   mouse_side_keys_rs.exe --debounce 150  去抖毫秒（默认 80，0=关闭）
//!   mouse_side_keys_rs.exe --list-presets  列出所有按键合成方式
//!   mouse_side_keys_rs.exe --preset raw    换一种合成方式（默认 rawsplit）
//!   mouse_side_keys_rs.exe --gap 20        修饰键间隔（兜底参数）
//!   mouse_side_keys_rs.exe --split 20      每个事件单独发一次，间隔毫秒
//!   mouse_side_keys_rs.exe --no-raw        强制用虚拟键码而不是扫描码
//!   mouse_side_keys_rs.exe --test          自检发送一次 Ctrl+Win+Right
//!   mouse_side_keys_rs.exe --stop          停止正在运行的实例
//!   mouse_side_keys_rs.exe --elevate       通过 UAC 以管理员身份重启（解决 UIPI 拦截）

use std::io::Write;
use std::os::windows::process::CommandExt;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    BOOL, CloseHandle, GetCurrentProcessId, GetLastError, HANDLE, HINSTANCE, HMODULE, HWND,
    LRESULT, LPARAM, WPARAM,
};
use windows_sys::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, OpenProcessToken,
    TOKEN_INFORMATION_CLASS, PSID,
};
use windows_sys::Win32::System::Console::{
    AttachConsole, GetConsoleWindow, GetStdHandle, SetConsoleCtrlHandler, WriteConsoleW,
};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, GetCurrentThreadId, GetModuleHandleW, OpenMutexW, OpenProcess,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    keybd_event, MapVirtualKeyW, SendInput, INPUT, INPUT_0, KEYBDINPUT,
};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetForegroundWindow, GetWindowThreadProcessId, MessageBoxW,
    MsgWaitForMultipleObjectsEx, PeekMessageW, PostThreadMessageW, SetWindowsHookExW,
    TranslateMessage, HHOOK, HOOKPROC, MSLLHOOKSTRUCT, MSG, WH_MOUSE_LL,
};

// --------------------------------------------------------------------------
// Win32 常量
// --------------------------------------------------------------------------
const WM_XBUTTONDOWN: u32 = 0x020B;
const WM_XBUTTONUP: u32 = 0x020C;
const WM_XBUTTONDBLCLK: u32 = 0x020D;
const WM_QUIT: u32 = 0x0012;
const WM_APP_TRIGGER: u32 = 0x8001; // 自定义：钩子里只发这个，真正按键在主循环里送

const XBUTTON1: u32 = 0x0001; // 侧键：后退
const XBUTTON2: u32 = 0x0002; // 侧键：前进

const INPUT_KEYBOARD: u32 = 1;
const KEYEVENTF_EXTENDEDKEY: u32 = 0x0001;
const KEYEVENTF_KEYUP: u32 = 0x0002;
const KEYEVENTF_SCANCODE: u32 = 0x0008; // 指定后 wScan 生效、wVk 被忽略（硬件级模拟）

const VK_LWIN: u32 = 0x5B;
const VK_RWIN: u32 = 0x5C;
const VK_LCONTROL: u32 = 0xA2;
const VK_RCONTROL: u32 = 0xA3;
const VK_CONTROL: u32 = 0x11; // 通用 Ctrl，不区分左右
const VK_LEFT: u32 = 0x25;
const VK_RIGHT: u32 = 0x27;

// 真实键盘上属于「扩展键区」的按键，必须带 E0 前缀（KEYEVENTF_EXTENDEDKEY）
const EXTENDED_KEYS: &[u32] = &[
    0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x2D, 0x2E, 0x5B, 0x5C, 0xA3, 0xA5,
];

const MAPVK_VK_TO_VSC: u32 = 0;

const PM_REMOVE: u32 = 0x0001;
const QS_ALLINPUT: u32 = 0x04FF;
const MWMO_INPUTAVAILABLE: u32 = 0x0004;

const ERROR_ALREADY_EXISTS: u32 = 183;
const SYNCHRONIZE: u32 = 0x0010_0000;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const EXTRA_INFO: usize = 0x4D53_4B31; // "MSK1"，给自己的合成事件打标记

const MUTEX_NAME: &str = "Local\\MouseSideKeysDesktopSwitch";

const TOKEN_QUERY: u32 = 0x0008;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF;
const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5;
const INVALID_HANDLE_VALUE: isize = -1;
const MB_ICONWARNING: u32 = 0x0000_0030;
const MB_SETFOREGROUND: u32 = 0x0001_0000;
const SW_SHOWNORMAL: i32 = 1;

// GetTokenInformation 的 TokenInformationClass 取值（避免依赖枚举变体名）
const TOKEN_INTEGRITY_LEVEL: i32 = 25;

// windows-sys 不保证导出 TOKEN_MANDATORY_LABEL，这里按 Windows 定义自行声明，
// 布局必须严格匹配（#repr(C)），否则解析完整性级别会读到错数据。
#[repr(C)]
struct SID_AND_ATTRIBUTES {
    Sid: *mut std::ffi::c_void,
    Attributes: u32,
}
#[repr(C)]
struct TOKEN_MANDATORY_LABEL {
    Label: SID_AND_ATTRIBUTES,
}

// --------------------------------------------------------------------------
// 全局状态（钩子回调是 extern "system" fn，无法捕获，只能走全局）
// --------------------------------------------------------------------------
struct Config {
    swap: bool,
    block: bool,
    debounce_ms: u64,
    raw_scancode: bool,
    use_extended: bool,
    gap_ms: u64,
    split_ms: u64,
}

static CONFIG: OnceLock<Config> = OnceLock::new();
static MAIN_TID: AtomicU32 = AtomicU32::new(0);
static RUNNING: AtomicBool = AtomicBool::new(true);
static LAST_FIRE: AtomicU64 = AtomicU64::new(0);
static UIPI_SHOWN: AtomicBool = AtomicBool::new(false);
static START_INSTANT: OnceLock<Instant> = OnceLock::new();

// --------------------------------------------------------------------------
// 辅助：宽字符串 / 控制台输出
// --------------------------------------------------------------------------
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 向（已附加的）父控制台写文本。后台无控制台时静默，等价于 Python 的 _note。
fn out(s: &str) {
    let wide = to_wide(s);
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        if (h as isize) != INVALID_HANDLE_VALUE {
            let mut written: u32 = 0;
            let _ = WriteConsoleW(
                h,
                wide.as_ptr(),
                (wide.len() - 1) as u32,
                &mut written,
                std::ptr::null_mut(),
            );
        }
    }
}

/// 若当前没有控制台、但有父控制台（从 cmd/终端启动），则附加过去，
/// 这样 --test / --list-presets / --stop 等命令的输出才可见。
fn attach_parent_console() {
    unsafe {
        if GetConsoleWindow().is_null() {
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

fn sleep_ms(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

// --------------------------------------------------------------------------
// PID 文件
// --------------------------------------------------------------------------
fn pid_file_path() -> String {
    format!("{}\\mouse_side_keys_rs.pid", std::env::temp_dir().display())
}

fn write_pid_file() {
    let pid = std::process::id();
    if let Ok(mut f) = std::fs::File::create(pid_file_path()) {
        let _ = write!(f, "{}", pid);
    }
}

fn remove_pid_file() {
    let _ = std::fs::remove_file(pid_file_path());
}

// --------------------------------------------------------------------------
// 单实例：命名互斥体
// --------------------------------------------------------------------------
fn check_single_instance() -> Option<HANDLE> {
    let name_w = to_wide(MUTEX_NAME);
    unsafe {
        let h = CreateMutexW(std::ptr::null(), 0, name_w.as_ptr());
        if h.is_null() {
            return None;
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(h);
            return None;
        }
        Some(h) // 句柄有意持有至进程退出：本进程自此成为互斥体持有者
    }
}

fn mutex_exists() -> bool {
    let name_w = to_wide(MUTEX_NAME);
    unsafe {
        let h = OpenMutexW(SYNCHRONIZE, 0, name_w.as_ptr());
        if !h.is_null() {
            let _ = CloseHandle(h);
            true
        } else {
            false
        }
    }
}

fn takeover_old_instance() {
    let path = pid_file_path();
    if !std::path::Path::new(&path).exists() {
        return;
    }
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let pid: u32 = content.trim().parse().unwrap_or(0);
    if pid != 0 {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }
    // taskkill /F 返回不代表目标进程已完全销毁，互斥体释放是异步的。
    // 固定 sleep 在系统繁忙时不够，必须轮询等互斥体真正释放，否则新实例
    // 会误判「仍有实例在运行」而退出 —— 新旧两空。
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !mutex_exists() {
            break;
        }
        sleep_ms(100);
    }
}

fn cmd_stop() {
    let path = pid_file_path();
    if !std::path::Path::new(&path).exists() {
        out("没有找到正在运行的实例（缺少 PID 文件）。\n");
        return;
    }
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let pid: u32 = content.trim().parse().unwrap_or(0);
    if pid == 0 {
        remove_pid_file();
        out("PID 文件已损坏，已清理。\n");
        return;
    }
    let status = std::process::Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    match status {
        Ok(s) if s.success() => {
            remove_pid_file();
            out(&format!("已停止鼠标侧键切换（PID {}）。\n", pid));
        }
        _ => {
            out("结束进程失败。\n");
            remove_pid_file();
        }
    }
}

// --------------------------------------------------------------------------
// 按键合成
// --------------------------------------------------------------------------
fn key_input(vk: u32, key_up: bool, raw_scancode: bool, use_extended: bool) -> INPUT {
    let scan = unsafe { MapVirtualKeyW(vk, MAPVK_VK_TO_VSC) } & 0xFF;
    let mut flags: u32 = 0;
    if raw_scancode {
        flags |= KEYEVENTF_SCANCODE;
    }
    if (raw_scancode || use_extended) && EXTENDED_KEYS.contains(&vk) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if key_up {
        flags |= KEYEVENTF_KEYUP;
    }
    let wvk: u16 = if raw_scancode { 0 } else { vk as u16 };
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: wvk,
                wScan: scan as u16,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: EXTRA_INFO,
            },
        },
    }
}

/// 发送一批事件；返回是否全部发送成功。失败时强制抬修饰键防卡键。
fn flush(seq: &[INPUT]) -> bool {
    if seq.is_empty() {
        return true;
    }
    unsafe {
        let sent = SendInput(
            seq.len() as u32,
            seq.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        );
        if sent != seq.len() as u32 {
            let _err = GetLastError();
            if sent == 0 {
                // 一个都发不出去，基本就是权限隔离（UIPI）或输入被阻塞
                if let Some(hint) = uipi_check() {
                    warn_uipi_dialog(&hint);
                }
            }
            release_modifiers();
            false
        } else {
            true
        }
    }
}

fn release_modifiers() {
    for vk in [
        VK_LCONTROL as u8,
        VK_RCONTROL as u8,
        VK_LWIN as u8,
        VK_RWIN as u8,
        VK_CONTROL as u8,
    ] {
        unsafe {
            keybd_event(vk, 0, KEYEVENTF_KEYUP, EXTRA_INFO);
        }
    }
}

/// 发送 Win + Ctrl + 方向键。顺序：Win -> Ctrl -> arrow 按下，反向抬起。
/// 返回是否整体成功（供 --test 判断）。
fn send_with(arrow: u32, raw: bool, ext: bool, gap: u64, split: u64) -> bool {
    let holds = [VK_LWIN, VK_LCONTROL];
    let mut seq: Vec<INPUT> = Vec::new();
    let mut ok = true;

    for &vk in &holds {
        seq.push(key_input(vk, false, raw, ext));
        if gap > 0 {
            if !flush(&seq) {
                ok = false;
            }
            seq.clear();
            sleep_ms(gap);
        }
    }
    seq.push(key_input(arrow, false, raw, ext));
    seq.push(key_input(arrow, true, raw, ext));
    for &vk in holds.iter().rev() {
        seq.push(key_input(vk, true, raw, ext));
        if gap > 0 {
            if !flush(&seq) {
                ok = false;
            }
            seq.clear();
            sleep_ms(gap);
        }
    }

    if split > 0 {
        // 一次只发一个事件，中间留间隔。某些环境下整批发送会被整体拒绝。
        for item in seq {
            if !flush(&[item]) {
                ok = false;
            }
            sleep_ms(split);
        }
        return ok;
    }
    if !flush(&seq) {
        ok = false;
    }
    ok
}

/// 常驻主循环用：按 CONFIG 里的预设发送。
fn send_ctrl_win_arrow(arrow: u32) {
    let cfg = CONFIG.get().expect("config not initialized");
    send_with(arrow, cfg.raw_scancode, cfg.use_extended, cfg.gap_ms, cfg.split_ms);
}

// --------------------------------------------------------------------------
// UIPI（用户界面特权隔离）检测
// --------------------------------------------------------------------------
fn integrity_level(pid: Option<u32>) -> Option<u32> {
    unsafe {
        let proc = match pid {
            None => {
                let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, GetCurrentProcessId());
                if h.is_null() {
                    return None;
                }
                h
            }
            Some(p) => {
                let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, p);
                if h.is_null() {
                    return None;
                }
                h
            }
        };
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(proc, TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let mut buf = [0u8; 256];
        let mut needed: u32 = 0;
        if GetTokenInformation(
            token,
            std::mem::transmute::<i32, TOKEN_INFORMATION_CLASS>(TOKEN_INTEGRITY_LEVEL),
            buf.as_mut_ptr() as *mut std::ffi::c_void,
            256,
            &mut needed,
        ) == 0
        {
            return None;
        }
        let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
        let sid = label.Label.Sid;
        let cnt = GetSidSubAuthorityCount(sid as PSID);
        if cnt.is_null() {
            return None;
        }
        let sub = GetSidSubAuthority(sid as PSID, (*cnt as u32).wrapping_sub(1));
        if sub.is_null() {
            return None;
        }
        Some(*sub)
    }
}

fn describe_level(level: u32) -> String {
    let name = match level {
        0x0000 => "Untrusted(不受信任)",
        0x1000 => "Low(低)",
        0x2000 => "Medium(中，普通程序默认)",
        0x2100 => "Medium Plus(中高)",
        0x3000 => "High(高，管理员运行的程序)",
        0x4000 => "System(系统)",
        _ => "未知",
    };
    format!("0x{:04X}({})", level, name)
}

fn uipi_check() -> Option<String> {
    let mine = integrity_level(None)?;
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_null() {
        return None;
    }
    let mut pid: u32 = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut pid);
    }
    if pid == 0 {
        return None;
    }
    let fg = integrity_level(Some(pid))?;
    if mine >= fg {
        return None;
    }
    Some(format!(
        "UIPI 阻止：本进程 {} 低于前台窗口 {}，Windows 拒绝模拟按键。请以管理员身份运行，\
         或让前台的高权限程序不要以管理员运行。",
        describe_level(mine),
        describe_level(fg)
    ))
}

fn warn_uipi_dialog(hint: &str) {
    if UIPI_SHOWN.swap(true, Ordering::Relaxed) {
        return; // 每次会话最多弹一次，避免反复打扰
    }
    let text = format!(
        "{}\n\n典型场景：360安全卫士等以管理员权限运行的窗口处于前台。\n\
         雷蛇等厂商宏能正常是因为它们走内核驱动注入，不受此限制。\n\n\
         解决办法：用本程序以管理员身份重启（--elevate），\n\
         或对 exe 设置「以管理员身份运行」兼容性选项。",
        hint
    );
    let w = to_wide(&text);
    let cap = to_wide("鼠标侧键切换 —— 按键被系统拦截");
    unsafe {
        let _ = MessageBoxW(
            std::ptr::null_mut(),
            w.as_ptr(),
            cap.as_ptr(),
            MB_ICONWARNING | MB_SETFOREGROUND,
        );
    }
}

// --------------------------------------------------------------------------
// 钩子回调 + 主循环
// --------------------------------------------------------------------------
fn pick_arrow(x_button: u32) -> u32 {
    let swap = CONFIG.get().map_or(false, |c| c.swap);
    let mut arrow = if x_button == XBUTTON1 { VK_LEFT } else { VK_RIGHT };
    if swap {
        arrow = if arrow == VK_LEFT { VK_RIGHT } else { VK_LEFT };
    }
    arrow
}

/// 去抖。命中窗口内直接丢弃，避免鼠标连击重复切换。
fn accept() -> bool {
    let cfg = CONFIG.get().expect("config not initialized");
    let deb = cfg.debounce_ms;
    if deb == 0 {
        return true;
    }
    let now = START_INSTANT.get().expect("start not set").elapsed().as_millis() as u64;
    let last = LAST_FIRE.load(Ordering::Relaxed);
    if now - last < deb {
        return false;
    }
    LAST_FIRE.store(now, Ordering::Relaxed);
    true
}

/// 钩子回调：只做判断和投递，绝不做重活（SendInput 必须在主循环发）。
unsafe extern "system" fn hook_proc(n_code: i32, w_param: WPARAM, l_param: LPARAM) -> LRESULT {
    if n_code >= 0 {
        let wp = w_param as u32;
        if wp == WM_XBUTTONDOWN || wp == WM_XBUTTONDBLCLK || wp == WM_XBUTTONUP {
            let info = *(l_param as *const MSLLHOOKSTRUCT);
            let x_button = (info.mouseData >> 16) & 0xFFFF;
            if x_button == XBUTTON1 || x_button == XBUTTON2 {
                if wp != WM_XBUTTONUP {
                    let arrow = pick_arrow(x_button);
                    if accept() {
                        let tid = MAIN_TID.load(Ordering::Relaxed);
                        let _ = PostThreadMessageW(tid, WM_APP_TRIGGER, arrow as usize, 0);
                    }
                }
                if CONFIG.get().map_or(true, |c| c.block) {
                    return 1; // 吞掉事件，屏蔽原有的前进 / 后退
                }
            }
        }
    }
    CallNextHookEx(std::ptr::null_mut(), n_code, w_param, l_param)
}

unsafe extern "system" fn ctrl_handler(_ctrl: u32) -> BOOL {
    RUNNING.store(false, Ordering::Relaxed);
    let tid = MAIN_TID.load(Ordering::Relaxed);
    let _ = PostThreadMessageW(tid, WM_QUIT, 0, 0);
    1
}

fn install_ctrl_handler() {
    unsafe {
        let _ = SetConsoleCtrlHandler(Some(ctrl_handler), 1);
    }
}

fn run() {
    write_pid_file();
    let tid = unsafe { GetCurrentThreadId() };
    MAIN_TID.store(tid, Ordering::Relaxed);
    install_ctrl_handler();

    unsafe {
        let hmod: HMODULE = GetModuleHandleW(std::ptr::null());
        let hook = SetWindowsHookExW(WH_MOUSE_LL, Some(hook_proc), hmod as HINSTANCE, 0);
        if hook.is_null() {
            out(&format!(
                "[错误] SetWindowsHookExW 失败，错误码 {}\n",
                GetLastError()
            ));
            remove_pid_file();
            return;
        }

        if let Some(hint) = uipi_check() {
            out(&format!("[警告] {}\n侧键很可能不会有效果，请以管理员身份运行。\n", hint));
        }

        let mut msg: MSG = std::mem::zeroed();
        while RUNNING.load(Ordering::Relaxed) {
            // 用阻塞等待代替 sleep 轮询：线程处于消息等待状态时，
            // 系统才能及时回调低级钩子，否则事件会被延迟甚至绕过钩子。
            let _ = MsgWaitForMultipleObjectsEx(
                0,
                std::ptr::null(),
                200,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            );
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                if msg.message == WM_APP_TRIGGER {
                    send_ctrl_win_arrow(msg.wParam as u32);
                } else if msg.message == WM_QUIT {
                    RUNNING.store(false, Ordering::Relaxed);
                    break;
                } else {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }

        let _ = UnhookWindowsHookEx(hook);
    }
    remove_pid_file();
}

// --------------------------------------------------------------------------
// 提权重启（--elevate）
// --------------------------------------------------------------------------
fn relaunch_elevated() -> bool {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    if exe.is_empty() {
        return false;
    }
    // 把除 --elevate 外的参数原样带过去；带空格的参数加引号
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| a != "--elevate")
        .map(|a| {
            if a.contains(' ') {
                format!("\"{}\"", a)
            } else {
                a
            }
        })
        .collect();
    let params = args.join(" ");

    let exe_w = to_wide(&exe);
    let params_w = to_wide(&params);
    let runas_w = to_wide("runas");
    unsafe {
        let r = ShellExecuteW(
            std::ptr::null_mut(),
            runas_w.as_ptr(),
            exe_w.as_ptr(),
            if params.is_empty() {
                std::ptr::null()
            } else {
                params_w.as_ptr()
            },
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
        (r as isize) > 32
    }
}

// --------------------------------------------------------------------------
// 预设 / 自检 / 列表
// --------------------------------------------------------------------------
fn preset_params(name: &str) -> (bool, bool, u64, u64) {
    match name {
        "rawsplit" => (true, true, 0, 10), // 默认：扫描码 + 逐个发送 + 10ms
        "raw" => (true, true, 0, 0),
        "split" => (false, true, 0, 10),
        "no-ext" => (true, false, 0, 0),
        _ => (false, true, 0, 0), // default：虚拟键码整批（常被拒）
    }
}

fn preset_desc(name: &str) -> &'static str {
    match name {
        "rawsplit" => "【默认】扫描码(wVk=0) + 逐个发送 + 10ms 间隔",
        "raw" => "只发扫描码，但整批一次发送",
        "split" => "虚拟键码 + 逐个发送，间隔 10ms",
        "default" => "虚拟键码整批发送（常被拒）",
        "no-ext" => "扫描码，但不带扩展键标志",
        _ => "未知预设",
    }
}

fn build_chain(preset: &str) -> Vec<&'static str> {
    let mut v: Vec<&str> = vec![preset];
    for p in ["raw", "split", "default"] {
        if p != preset {
            v.push(p);
        }
    }
    v
}

fn list_presets_cmd() {
    out("可用的按键合成方式：\n");
    for name in ["rawsplit", "raw", "split", "default", "no-ext"] {
        let mark = if name == "rawsplit" { "   <- 默认" } else { "" };
        out(&format!("  {:<8} {}{}\n", name, preset_desc(name), mark));
    }
}

fn cmd_test(preset: &str) {
    out(&format!(
        "本进程权限级别：{}\n",
        describe_level(integrity_level(None).unwrap_or(0))
    ));
    if let Some(hint) = uipi_check() {
        out(&format!("[警告] {}\n", hint));
    }
    out("\n");
    for name in build_chain(preset) {
        out(&format!("--> 方式 {:<8} {}\n", name, preset_desc(name)));
        out!("    正在发送一次 Ctrl+Win+Right，请观察桌面是否切换 ...\n");
        let (raw, ext, gap, split) = preset_params(name);
        let ok = send_with(VK_RIGHT, raw, ext, gap, split);
        out(&format!(
            "    结果：{}\n",
            if ok { "发送成功" } else { "被拒绝（SendInput 返回 0）" }
        ));
        if ok {
            out("\n桌面如果切换了，就用这个方式常驻：\n");
            out(&format!("    mouse_side_keys_rs.exe --preset {}\n", name));
            return;
        }
        out("\n");
    }
    out("所有方式都被系统拒绝。\n");
    out("先看权限级别：若不是 High(0x3000)，用 --elevate 提权后重试。\n");
    out("提权后仍失败，多半是安全软件拦截了合成输入，把本目录加入白名单再试。\n");
}

// --------------------------------------------------------------------------
// 参数解析
// --------------------------------------------------------------------------
fn parse_args() -> (Config, bool, bool, bool, bool, bool, String) {
    let mut swap = true;
    let mut block = true;
    let mut debounce: i64 = 80;
    let mut foreground = false;
    let mut elevate = false;
    let mut stop = false;
    let mut test = false;
    let mut list_presets = false;
    let mut preset = "rawsplit".to_string();
    let mut gap: i64 = -1;
    let mut split: i64 = -1;
    let mut no_raw = false;

    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--no-swap" => swap = false,
            "--no-block" => block = false,
            "--swap" => swap = true,
            "--foreground" => foreground = true,
            "--elevate" => elevate = true,
            "--stop" => stop = true,
            "--test" => test = true,
            "--list-presets" => list_presets = true,
            "--no-raw" => no_raw = true,
            "--debounce" => debounce = it.next().and_then(|s| s.parse().ok()).unwrap_or(80),
            "--preset" => preset = it.next().unwrap_or_default(),
            "--gap" => gap = it.next().and_then(|s| s.parse().ok()).unwrap_or(-1),
            "--split" => split = it.next().and_then(|s| s.parse().ok()).unwrap_or(-1),
            _ => {}
        }
    }

    let mut raw_scancode = false;
    let mut use_extended = true;
    let mut p_split: u64 = 0;
    let mut p_gap: u64 = 0;
    match preset.as_str() {
        "rawsplit" => {
            raw_scancode = true;
            p_split = 10;
        }
        "raw" => raw_scancode = true,
        "split" => p_split = 10,
        "no-ext" => {
            raw_scancode = true;
            use_extended = false;
        }
        _ => {}
    }
    if gap >= 0 {
        p_gap = gap as u64;
    }
    if split >= 0 {
        p_split = split as u64;
    }
    if no_raw {
        raw_scancode = false;
    }

    let cfg = Config {
        swap,
        block,
        debounce_ms: debounce.max(0) as u64,
        raw_scancode,
        use_extended,
        gap_ms: p_gap,
        split_ms: p_split,
    };
    (cfg, foreground, elevate, stop, test, list_presets, preset)
}

// --------------------------------------------------------------------------
// 入口
// --------------------------------------------------------------------------
fn main() {
    START_INSTANT.get_or_init(Instant::now);
    let (cfg, _foreground, elevate, stop, test, list_presets, preset) = parse_args();
    attach_parent_console();

    if list_presets {
        list_presets_cmd();
        return;
    }
    if stop {
        cmd_stop();
        return;
    }

    CONFIG.get_or_init(|| cfg);

    if elevate {
        if integrity_level(None).unwrap_or(0) >= 0x3000 {
            out("当前已是管理员权限，无需提权。\n");
            return;
        }
        if relaunch_elevated() {
            out("已请求管理员权限重启（请在 UAC 弹窗中确认）。\n");
            return;
        }
        out("[错误] 提权重启被取消或失败（UAC 弹窗被拒绝？）。\n");
        return;
    }

    if test {
        cmd_test(&preset);
        return;
    }

    // 先接管上一版残留的进程，避免「改了代码却还在跑旧版」。
    takeover_old_instance();

    // 关键：单实例互斥体。返回 None 说明已有实例在跑。
    let _mutex = match check_single_instance() {
        Some(h) => h, // 持有至 main 结束（进程退出时由系统释放）
        None => {
            out("仍有另一个实例在运行（或其它目录的副本）。请先运行 --stop。\n");
            return;
        }
    };

    let level = describe_level(integrity_level(None).unwrap_or(0));
    let (d1, d2) = if CONFIG.get().map_or(true, |c| c.swap) {
        ("Right", "Left")
    } else {
        ("Left", "Right")
    };
    out(&format!("本进程权限级别：{}\n", level));
    out(&format!(
        "鼠标侧键切换已启动。侧键1 -> Ctrl+Win+{}，侧键2 -> Ctrl+Win+{}（{}方向）\n",
        d1,
        d2,
        if CONFIG.get().map_or(true, |c| c.swap) {
            "对调"
        } else {
            "标准"
        }
    ));
    out("停止方式：运行 mouse_side_keys_rs.exe --stop\n");

    run();
    // _mutex 在此离开作用域；我们不主动 CloseHandle，让它随进程退出释放。
}
