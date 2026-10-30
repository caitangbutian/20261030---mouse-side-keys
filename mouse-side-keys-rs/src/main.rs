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

// 子系统设为 Windows（等价于链接器 /SUBSYSTEM:WINDOWS）：
// 运行时无控制台窗口，也不会因为启动它的控制台被关闭而被连带杀掉 ——
// 对应 Python 版用 pythonw 后台化的效果。需要看输出时从 cmd/终端启动，
// 程序会 AttachConsole 到父控制台再打印（--test / --stop）。
//
// 重要：必须用 crate 级属性，不能改用 .cargo/config.toml 里的
// `-C link-args=/SUBSYSTEM:WINDOWS`。后者是全局 rustflags，会连带作用于依赖的
// build script；构建脚本本身是控制台程序（入口 main），被强制要求 WinMain 入口
// 后会在链接期报 `LNK2019: unresolved external symbol WinMain` 而导致整个编译失败。
// crate 级属性只作用于最终二进制，不影响 build script。
#![windows_subsystem = "windows"]

use std::io::Write;
use std::os::windows::process::CommandExt;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    BOOL, CloseHandle, GetLastError, HANDLE, HINSTANCE, HMODULE, HWND, LRESULT, LPARAM, WPARAM,
};
use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_INFORMATION_CLASS};
use windows_sys::Win32::System::Console::{
    AttachConsole, GetConsoleWindow, GetStdHandle, SetConsoleCtrlHandler, WriteConsoleW,
};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, GetCurrentThreadId, OpenMutexW, OpenProcess,
};

// 这几个函数在 windows-sys 0.52 里的模块位置和预期不符（OpenProcessToken 不在
// Win32::Security、GetModuleHandleW 不在 System::Threading、UnhookWindowsHookEx
// 需要单独导入、PSID 类型别名根本没有导出），与其反复去猜模块路径，不如直接
// 声明 FFI：它们分别位于 kernel32 / user32 / advapi32，而这些导入库已经被上面
// windows-sys 的调用带入链接，符号一定能解析。
//
// 重要：windows-sys 0.52 的句柄类型（HANDLE / HWND / HHOOK / HMODULE）都是 isize，
// **不是指针**。所以这里参数与返回值一律用 isize，调用处也不能再传 null_mut()，
// 判断空句柄要写 `== 0` 而不是 `.is_null()`。
unsafe extern "system" {
    fn GetCurrentProcessId() -> u32;
    fn GetModuleHandleW(lpModuleName: *const u16) -> isize;
    fn UnhookWindowsHookEx(hhk: isize) -> BOOL;
    fn OpenProcessToken(process: isize, desired_access: u32, token_handle: *mut isize) -> BOOL;
    fn GetSidSubAuthorityCount(pSid: *mut std::ffi::c_void) -> *mut u8;
    fn GetSidSubAuthority(pSid: *mut std::ffi::c_void, nSubAuthority: u32) -> *mut u32;
}
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

const MB_OK: u32 = 0x0000_0000;
const MB_YESNOCANCEL: u32 = 0x0000_0003;
const MB_ICONQUESTION: u32 = 0x0000_0020;

// MessageBoxW 的按钮返回值（现在只有建窗失败的兜底分支会用到）
#[allow(dead_code)]
const IDNO: i32 = 7;
const IDCANCEL: i32 = 2;

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

/// 已输出内容的副本：无控制台（双击运行）时，命令结束前用它弹一个窗，
/// 否则用户完全看不到 --stop / --test / --list-presets 的结果。
static OUT_BUF: OnceLock<Mutex<String>> = OnceLock::new();

fn out_buf() -> &'static Mutex<String> {
    OUT_BUF.get_or_init(|| Mutex::new(String::new()))
}

/// 是否有可用控制台（只有从 cmd / 终端启动时才有）。
fn has_console() -> bool {
    unsafe { GetConsoleWindow() != 0 }
}

/// 向（已附加的）父控制台写文本。等价于 Python 的 _note。
/// 无论有没有控制台，都同时记进 OUT_BUF，供 finish_output() 兜底弹窗。
fn out(s: &str) {
    let wide = to_wide(s);
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        if (h as isize) != INVALID_HANDLE_VALUE {
            let mut written: u32 = 0;
            let _ = WriteConsoleW(
                h,
                wide.as_ptr() as *const std::ffi::c_void,
                (wide.len() - 1) as u32,
                &mut written,
                std::ptr::null_mut(),
            );
        }
    }
    if let Ok(mut b) = out_buf().lock() {
        b.push_str(s);
    }
}

/// 弹一个消息框，返回用户点的按钮（IDYES / IDNO / IDCANCEL ...）。
fn show_message(title: &str, text: &str, flags: u32) -> i32 {
    let w = to_wide(text);
    let cap = to_wide(title);
    unsafe { MessageBoxW(0, w.as_ptr(), cap.as_ptr(), flags) }
}

// --------------------------------------------------------------------------
// 「已在运行」对话框
//
// 系统 MessageBoxW 的按钮文字写死成「是 / 否 / 取消」，改不了；要显示
// 「重新运行 / 立即结束」只能自己建一个 Win32 窗口。纯 user32 + gdi32（仅取字体），
// 不引入任何 crate 依赖，体积不变。
//
// 安全说明：这里**可以**阻塞当前线程 —— 本对话框只在 main() 里、安装
// WH_MOUSE_LL 钩子**之前**调用，此时阻塞不会冻结鼠标输入。
// （对比：UIPI 提示框必须走独立线程，因为那时钩子已经装上了。）
// --------------------------------------------------------------------------

/// 用户对「已在运行」的选择。
enum RunningChoice {
    Keep,    // 什么都不做，让现有实例继续跑
    Restart, // 结束旧实例并重新启动
    Stop,    // 结束旧实例并且不再启动
}

const WS_POPUP: u32 = 0x8000_0000;
const WS_CAPTION: u32 = 0x00C0_0000;
const WS_SYSMENU: u32 = 0x0008_0000;
const WS_VISIBLE: u32 = 0x1000_0000;
const WS_DLGFRAME: u32 = 0x0040_0000;
const WS_CHILD: u32 = 0x4000_0000;
const WS_TABSTOP: u32 = 0x0001_0000;
const BS_DEFPUSHBUTTON: u32 = 0x0000_0001;
const WS_EX_TOPMOST: u32 = 0x0000_0008;
const WS_EX_DLGMODALFRAME: u32 = 0x0000_0001;

const WM_SETFONT: u32 = 0x0030;
const WM_CLOSE: u32 = 0x0010;
const WM_DESTROY: u32 = 0x0002;
const WM_COMMAND: u32 = 0x0111;
const SM_CXSCREEN: i32 = 0;
const SM_CYSCREEN: i32 = 1;
const DEFAULT_GUI_FONT: i32 = 17;

const ID_RESTART: isize = 1001;
const ID_STOP: isize = 1002;

/// 对话框结果：0 = 保持，1 = 重新运行，2 = 立即结束。
static DLG_RESULT: AtomicU32 = AtomicU32::new(0);

#[repr(C)]
struct WNDCLASSW {
    style: u32,
    lpfn_wnd_proc: Option<unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT>,
    cb_cls_extra: i32,
    cb_wnd_extra: i32,
    h_instance: HINSTANCE,
    h_icon: HANDLE,
    h_cursor: HANDLE,
    h_brush_background: HANDLE,
    lpsz_menu_name: *const u16,
    lpsz_class_name: *const u16,
}

// 这些函数在 windows-sys 0.52 里模块位置不好猜，直接手写 FFI；user32 已被
// 上面的调用带入链接。注意句柄类型一律是 isize，不是指针。
unsafe extern "system" {
    fn RegisterClassW(lpWndClass: *const WNDCLASSW) -> u16;
    fn CreateWindowExW(
        dw_ex_style: u32,
        lp_class_name: *const u16,
        lp_window_name: *const u16,
        dw_style: u32,
        x: i32,
        y: i32,
        n_width: i32,
        n_height: i32,
        h_wnd_parent: HWND,
        h_menu: isize,
        h_instance: HINSTANCE,
        lp_param: *const std::ffi::c_void,
    ) -> HWND;
    fn DefWindowProcW(h_wnd: HWND, msg: u32, w_param: WPARAM, l_param: LPARAM) -> LRESULT;
    fn GetMessageW(lp_msg: *mut MSG, h_wnd: HWND, filter_min: u32, filter_max: u32) -> BOOL;
    fn DestroyWindow(h_wnd: HWND) -> BOOL;
    fn SendMessageW(h_wnd: HWND, msg: u32, w_param: WPARAM, l_param: LPARAM) -> LRESULT;
    fn SetForegroundWindow(h_wnd: HWND) -> BOOL;
    fn SetFocus(h_wnd: HWND) -> HWND;
    fn GetSystemMetrics(n_index: i32) -> i32;
    fn PostQuitMessage(n_exit_code: i32);
}

#[link(name = "gdi32")]
unsafe extern "system" {
    fn GetStockObject(i: i32) -> HANDLE;
}

unsafe extern "system" fn ask_dlg_proc(
    h_wnd: HWND,
    msg: u32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = (w_param & 0xFFFF) as isize;
            if id == ID_RESTART {
                DLG_RESULT.store(1, Ordering::Relaxed);
                DestroyWindow(h_wnd);
            } else if id == ID_STOP {
                DLG_RESULT.store(2, Ordering::Relaxed);
                DestroyWindow(h_wnd);
            }
            0
        }
        WM_CLOSE => {
            // 关窗 = 保持现状（什么都不做），等价于原来的「否」
            DestroyWindow(h_wnd);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(h_wnd, msg, w_param, l_param),
    }
}

/// 模态消息循环：跑到这里阻塞，直到 PostQuitMessage 被调用。
/// （这两个对话框都在安装钩子之前弹出，所以阻塞不会冻结鼠标。）
unsafe fn modal_loop() {
    let mut msg: MSG = std::mem::zeroed();
    while GetMessageW(&mut msg, 0, 0, 0) > 0 {
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

/// 弹出「程序已在运行」对话框，阻塞直到用户做出选择。
fn ask_running_choice() -> RunningChoice {
    const W: i32 = 440;
    const H: i32 = 210;

    unsafe {
        let class_name = to_wide("MouseSideKeysAskClass");
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfn_wnd_proc = Some(ask_dlg_proc);
        wc.h_instance = 0;
        wc.h_cursor = 0;
        wc.h_icon = 0;
        wc.h_brush_background = 16; // COLOR_BTNFACE + 1（系统颜色刷，无需创建 GDI 对象）
        wc.lpsz_class_name = class_name.as_ptr();
        // 同一进程重复注册同名类会失败，忽略返回值即可（类已存在照样能用）
        let _ = RegisterClassW(&wc);

        let title = to_wide("鼠标侧键切换");
        let body = to_wide("程序已在运行，请确认已开启多窗口！\n（win+tab可查看多窗口）");
        let hint = to_wide("直接关闭本窗口 = 保持不变，继续运行");
        let b1_text = to_wide("重新运行");
        let b2_text = to_wide("立即结束");

        let sx = GetSystemMetrics(SM_CXSCREEN);
        let sy = GetSystemMetrics(SM_CYSCREEN);
        let x = if sx > W { (sx - W) / 2 } else { 0 };
        let y = if sy > H { (sy - H) / 2 } else { 0 };

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_DLGMODALFRAME,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_VISIBLE | WS_DLGFRAME,
            x,
            y,
            W,
            H,
            0,
            0,
            0,
            std::ptr::null(),
        );
        if hwnd == 0 {
            // 极少数情况下建窗失败：退回系统消息框，保证功能不丢。
            let r = show_message(
                "鼠标侧键切换 —— 已经在运行",
                "本程序已经在运行了。\n\n\
                 是(Y) —— 结束旧实例并重新启动\n\
                 否(N) —— 保持不变，让现在这个实例继续运行\n\
                 取消   —— 结束旧实例，并且不再启动\n",
                MB_YESNOCANCEL | MB_ICONQUESTION,
            );
            return match r {
                IDYES => RunningChoice::Restart,
                IDCANCEL => RunningChoice::Stop,
                _ => RunningChoice::Keep,
            };
        }

        let font = GetStockObject(DEFAULT_GUI_FONT);
        let static_cls = to_wide("Static");
        let button_cls = to_wide("Button");

        let h_body = CreateWindowExW(
            0,
            static_cls.as_ptr(),
            body.as_ptr(),
            WS_CHILD | WS_VISIBLE,
            24,
            18,
            392,
            54,
            hwnd,
            0,
            0,
            std::ptr::null(),
        );
        SendMessageW(h_body, WM_SETFONT, font as WPARAM, 1);

        let h_hint = CreateWindowExW(
            0,
            static_cls.as_ptr(),
            hint.as_ptr(),
            WS_CHILD | WS_VISIBLE,
            24,
            124,
            392,
            20,
            hwnd,
            0,
            0,
            std::ptr::null(),
        );
        SendMessageW(h_hint, WM_SETFONT, font as WPARAM, 1);

        let h_restart = CreateWindowExW(
            0,
            button_cls.as_ptr(),
            b1_text.as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_DEFPUSHBUTTON,
            68,
            80,
            132,
            34,
            hwnd,
            ID_RESTART,
            0,
            std::ptr::null(),
        );
        SendMessageW(h_restart, WM_SETFONT, font as WPARAM, 1);

        let h_stop = CreateWindowExW(
            0,
            button_cls.as_ptr(),
            b2_text.as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            224,
            80,
            132,
            34,
            hwnd,
            ID_STOP,
            0,
            std::ptr::null(),
        );
        SendMessageW(h_stop, WM_SETFONT, font as WPARAM, 1);

        // 置前并聚焦，避免窗口被别的窗口盖住而用户以为没反应
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(h_restart);

        modal_loop();
    }

    match DLG_RESULT.swap(0, Ordering::Relaxed) {
        1 => RunningChoice::Restart,
        2 => RunningChoice::Stop,
        _ => RunningChoice::Keep,
    }
}

/// 命令模式（--stop / --test / --list-presets / --elevate）结束前调用：
/// 没有控制台时把本次输出汇总弹出来，避免双击运行后什么反馈都看不到。
fn finish_output() {
    if has_console() {
        return;
    }
    let text = match out_buf().lock() {
        Ok(b) => b.clone(),
        Err(_) => return,
    };
    let t = text.trim_end().to_string();
    if t.is_empty() {
        return;
    }
    show_message("鼠标侧键切换", &t, MB_OK | MB_SETFOREGROUND);
}

/// 若当前没有控制台、但有父控制台（从 cmd/终端启动），则附加过去，
/// 这样 --test / --list-presets / --stop 等命令的输出才可见。
fn attach_parent_console() {
    unsafe {
        // HWND 在 windows-sys 0.52 里是 isize，不能用 .is_null()
        if GetConsoleWindow() == 0 {
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
        if h == 0 {
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
        if h != 0 {
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
                if let Some(hint) = cached_uipi_hint() {
                    warn_uipi_dialog(hint);
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
                if h == 0 {
                    return None;
                }
                h
            }
            Some(p) => {
                let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, p);
                if h == 0 {
                    return None;
                }
                h
            }
        };
        let mut token: HANDLE = 0;
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
        let cnt = GetSidSubAuthorityCount(sid);
        if cnt.is_null() {
            return None;
        }
        let sub = GetSidSubAuthority(sid, (*cnt as u32).wrapping_sub(1));
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
    if hwnd == 0 {
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

/// UIPI 检测结果缓存：uipi_check() 要 OpenProcess + OpenProcessToken +
/// GetTokenInformation，逐个事件都查一遍既慢又没必要（结果不会变）。
static UIPI_HINT: OnceLock<Option<String>> = OnceLock::new();

fn cached_uipi_hint() -> Option<&'static str> {
    UIPI_HINT.get_or_init(|| uipi_check()).as_deref()
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
    // 极其关键：MessageBoxW 是模态的，会一直阻塞调用线程直到用户点掉。
    // 调用方是 run() 的主循环线程，而低级鼠标钩子（WH_MOUSE_LL）就装在这个线程上；
    // 一旦在这里被阻塞，系统鼠标输入会整体卡死 / 严重卡顿 ——
    // 这正是「一运行起来鼠标就变得非常卡顿」的根因。
    // 所以弹窗必须扔到独立线程去弹，主循环继续泵消息，鼠标不受影响。
    std::thread::spawn(move || {
        show_message(
            "鼠标侧键切换 —— 按键被系统拦截",
            &text,
            MB_ICONWARNING | MB_SETFOREGROUND,
        );
    });
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
    CallNextHookEx(0, n_code, w_param, l_param)
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
        if hook == 0 {
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
            while PeekMessageW(&mut msg, 0, 0, 0, PM_REMOVE) != 0 {
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
            0,
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
// 开机启动：安装 / 注册计划任务
//
// 为什么用「计划任务」而不是注册表 Run 项：
//   Run 项启动的进程是标准权限（Medium），侧键会被 UIPI 拦掉（360 之类
//   High 权限窗口聚焦时失效）。计划任务可以设 RunLevel=HIGHEST，登录时
//   静默以最高权限启动，不弹 UAC 打断。
//   实测：不勾「使用最高权限运行」，登录时会 ERROR_ELEVATION_REQUIRED 静默失败；
//   笔记本还要取消「只在交流电源时启动」，否则电池状态下开机不触发。
// --------------------------------------------------------------------------

const TASK_NAME: &str = "MouseSideKeys";
const INSTALLED_EXE_NAME: &str = "MouseSideKeys.exe";
const MAX_PATH: usize = 260;

/// 计划任务是否已注册。只看进程返回码，不解析输出（中文系统是 GBK，解析易错）。
fn startup_task_registered() -> bool {
    match std::process::Command::new("schtasks")
        .args(["/Query", "/TN", TASK_NAME])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(o) => o.status.success(),
        Err(_) => false,
    }
}

fn default_install_dir() -> String {
    let base = std::env::var("LOCALAPPDATA")
        .or_else(|_| std::env::var("APPDATA"))
        .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().to_string());
    let base = base.trim_end_matches(|c| c == '\\' || c == '/').to_string();
    format!("{}\\MouseSideKeys", base)
}

fn install_skip_path() -> String {
    format!("{}\\.skip_install", default_install_dir())
}

/// 用户点过「以后再说」后不再每次启动都弹安装框。
fn install_skipped() -> bool {
    std::path::Path::new(&install_skip_path()).exists()
}

fn mark_install_skipped() {
    let p = install_skip_path();
    if let Some(d) = std::path::Path::new(&p).parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(&p, "1");
}

fn same_path(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.replace('/', "\\").to_lowercase();
    norm(a) == norm(b)
}

/// 执行安装：复制自身到 dir → 注册计划任务 → 从新位置启动一次。
/// 必须以管理员权限运行，否则 /RL HIGHEST 会失败。
fn do_install(dir: &str) {
    let dir = dir.trim_end_matches(|c| c == '\\' || c == '/').to_string();
    if dir.is_empty() {
        out("[错误] 安装目录为空。\n");
        return;
    }
    let src = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let dst = format!("{}\\{}", dir, INSTALLED_EXE_NAME);

    if same_path(&src, &dst) {
        out("已经在目标位置，跳过复制。\n");
    } else {
        if let Err(e) = std::fs::create_dir_all(&dir) {
            out(&format!("[错误] 无法创建目录 {}：{}\n", dir, e));
            return;
        }
        match std::fs::copy(&src, &dst) {
            Ok(_) => out(&format!("已复制自身到：{}\n", dst)),
            Err(e) => {
                out(&format!("[错误] 复制失败：{}\n", e));
                return;
            }
        }
    }

    let tr = format!("\"{}\"", dst);
    match std::process::Command::new("schtasks")
        .args([
            "/Create", "/TN", TASK_NAME, "/TR", tr.as_str(), "/SC", "ONLOGON", "/RL", "HIGHEST",
            "/F",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(o) if o.status.success() => out(&format!("已注册开机启动任务：{}\n", TASK_NAME)),
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            let err2 = String::from_utf8_lossy(&o.stdout);
            out(&format!(
                "[错误] 注册计划任务失败（需要管理员权限）。\n{}\n",
                if err.trim().is_empty() { err2.trim() } else { err.trim() }
            ));
            return;
        }
        Err(e) => {
            out(&format!("[错误] 无法执行 schtasks：{}\n", e));
            return;
        }
    }

    // 笔记本：取消「只在交流电源时启动」，否则电池状态下开机不触发。尽力而为，失败不影响。
    let ps = format!(
        "$s = New-ScheduledTaskSettingsSet -DisallowStartIfOnBatteries:$false -AllowStartIfOnBatteries:$true -DontStopIfGoingOnBatteries:$true; Set-ScheduledTask -TaskName '{}' -Settings $s",
        TASK_NAME
    );
    let _ = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", ps.as_str()])
        .creation_flags(CREATE_NO_WINDOW)
        .output();

    // 从新位置启动一次，完成「安装即可用」。
    match std::process::Command::new(&dst)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        Ok(_) => out("已从新位置启动一次，功能即刻生效。\n"),
        Err(e) => out(&format!("[警告] 启动新实例失败：{}\n", e)),
    }
}

/// 以管理员权限重启自身，并带上指定命令行参数（用于 --install 的安装流程）。
fn relaunch_elevated_with(args_line: &str) -> bool {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    if exe.is_empty() {
        return false;
    }
    let exe_w = to_wide(&exe);
    let params_w = to_wide(args_line);
    let runas_w = to_wide("runas");
    unsafe {
        let r = ShellExecuteW(
            0,
            runas_w.as_ptr(),
            exe_w.as_ptr(),
            params_w.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
        (r as isize) > 32
    }
}

fn parse_install_arg() -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--install" && i + 1 < args.len() {
            return Some(args[i + 1].clone());
        }
        if let Some(v) = args[i].strip_prefix("--install=") {
            return Some(v.to_string());
        }
        i += 1;
    }
    None
}

// ---- 安装位置选择对话框（纯 Win32，无依赖） ----

const ES_AUTOHSCROLL: u32 = 0x0080;
const WS_BORDER: u32 = 0x0080_0000;
const BIF_RETURNONLYFSDIRS: u32 = 0x0001;
const BIF_NEWDIALOGSTYLE: u32 = 0x0040;
const BIF_EDITBOX: u32 = 0x0010;
const COINIT_APARTMENTTHREADED: u32 = 0x2;

const ID_EDIT: isize = 2001;
const ID_BROWSE: isize = 2002;
const ID_INSTALL: isize = 2003;
const ID_LATER: isize = 2004;

static INSTALL_EDIT: AtomicIsize = AtomicIsize::new(0);
static INSTALL_CHOICE: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn install_choice() -> &'static Mutex<Option<String>> {
    INSTALL_CHOICE.get_or_init(|| Mutex::new(None))
}

#[repr(C)]
struct BROWSEINFOW {
    hwnd_owner: HWND,
    pidl_root: isize,
    psz_display_name: *mut u16,
    lpsz_title: *const u16,
    ul_flags: u32,
    lpfn: isize,
    l_param: LPARAM,
    i_image: i32,
}

unsafe extern "system" {
    fn GetWindowTextW(h_wnd: HWND, lp_string: *mut u16, n_max_count: i32) -> i32;
    fn SetWindowTextW(h_wnd: HWND, lp_string: *const u16) -> BOOL;
    fn SHBrowseForFolderW(lpbi: *mut BROWSEINFOW) -> isize;
    fn SHGetPathFromIDListW(pidl: isize, psz_path: *mut u16) -> BOOL;
}

#[link(name = "ole32")]
unsafe extern "system" {
    fn CoInitializeEx(pv_reserved: *const std::ffi::c_void, co_init: u32) -> i32;
    fn CoTaskMemFree(pv: *const std::ffi::c_void);
}

unsafe extern "system" fn install_dlg_proc(
    h_wnd: HWND,
    msg: u32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    match msg {
        WM_COMMAND => {
            match (w_param & 0xFFFF) as isize {
                ID_BROWSE => {
                    let title = to_wide("选择安装位置");
                    let mut bi: BROWSEINFOW = std::mem::zeroed();
                    bi.hwnd_owner = h_wnd;
                    bi.lpsz_title = title.as_ptr();
                    bi.ul_flags = BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE | BIF_EDITBOX;
                    let pidl = SHBrowseForFolderW(&mut bi);
                    if pidl != 0 {
                        let mut buf = [0u16; MAX_PATH];
                        if SHGetPathFromIDListW(pidl, buf.as_mut_ptr()) != 0 {
                            let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                            let picked = String::from_utf16_lossy(&buf[..len]);
                            if !picked.is_empty() {
                                let w = to_wide(&picked);
                                SetWindowTextW(INSTALL_EDIT.load(Ordering::Relaxed), w.as_ptr());
                            }
                        }
                        CoTaskMemFree(pidl as *const std::ffi::c_void);
                    }
                    0
                }
                ID_INSTALL => {
                    let h = INSTALL_EDIT.load(Ordering::Relaxed);
                    let mut buf = [0u16; MAX_PATH];
                    let n = GetWindowTextW(h, buf.as_mut_ptr(), MAX_PATH as i32);
                    let s = if n > 0 {
                        String::from_utf16_lossy(&buf[..(n as usize).min(MAX_PATH)])
                    } else {
                        String::new()
                    };
                    let dir = s.trim().to_string();
                    if dir.is_empty() {
                        show_message(
                            "鼠标侧键切换",
                            "请先选择安装位置（或点「浏览...」）。",
                            MB_ICONWARNING | MB_SETFOREGROUND,
                        );
                        return 0;
                    }
                    if let Ok(mut g) = install_choice().lock() {
                        *g = Some(dir);
                    }
                    DestroyWindow(h_wnd);
                    0
                }
                ID_LATER => {
                    if let Ok(mut g) = install_choice().lock() {
                        *g = None;
                    }
                    DestroyWindow(h_wnd);
                    0
                }
                _ => 0,
            }
        }
        WM_CLOSE => {
            DestroyWindow(h_wnd);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(h_wnd, msg, w_param, l_param),
    }
}

/// 弹出「尚未加入开机启动」安装框，返回用户选择的安装目录（None = 以后再说）。
fn ask_install_location(default: &str) -> Option<String> {
    const W: i32 = 500;
    const H: i32 = 264;

    unsafe {
        // SHBrowseForFolderW 需要 COM 初始化
        let _ = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED);

        let class_name = to_wide("MouseSideKeysInstallClass");
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfn_wnd_proc = Some(install_dlg_proc);
        wc.h_instance = 0;
        wc.h_cursor = 0;
        wc.h_icon = 0;
        wc.h_brush_background = 16;
        wc.lpsz_class_name = class_name.as_ptr();
        let _ = RegisterClassW(&wc);

        let title = to_wide("鼠标侧键切换 —— 安装到开机启动");
        let line1 = to_wide("尚未加入开机启动。请选择安装位置，程序会把自己复制过去，");
        let line2 = to_wide("并注册为开机启动任务（最高权限运行，避免侧键被系统拦截）。");
        let line3 = to_wide("安装完成后会自动从新位置启动一次；以后开机即自动运行。");
        let b_browse = to_wide("浏览...");
        let b_install = to_wide("安装并启动");
        let b_later = to_wide("以后再说");
        let default_w = to_wide(default);

        let sx = GetSystemMetrics(SM_CXSCREEN);
        let sy = GetSystemMetrics(SM_CYSCREEN);
        let x = if sx > W { (sx - W) / 2 } else { 0 };
        let y = if sy > H { (sy - H) / 2 } else { 0 };

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_DLGMODALFRAME,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_VISIBLE | WS_DLGFRAME,
            x,
            y,
            W,
            H,
            0,
            0,
            0,
            std::ptr::null(),
        );
        if hwnd == 0 {
            return None;
        }

        let font = GetStockObject(DEFAULT_GUI_FONT);
        let static_cls = to_wide("Static");
        let button_cls = to_wide("Button");
        let edit_cls = to_wide("Edit");

        let mk_static = |text: *const u16, y: i32, h: i32| -> HWND {
            let h1 = CreateWindowExW(
                0,
                static_cls.as_ptr(),
                text,
                WS_CHILD | WS_VISIBLE,
                24,
                y,
                452,
                h,
                hwnd,
                0,
                0,
                std::ptr::null(),
            );
            SendMessageW(h1, WM_SETFONT, font as WPARAM, 1);
            h1
        };
        let mk_button = |text: *const u16, x: i32, y: i32, w: i32, h: i32, id: isize, def: bool| -> HWND {
            let st = WS_CHILD | WS_VISIBLE | WS_TABSTOP | (if def { BS_DEFPUSHBUTTON } else { 0 });
            let h1 = CreateWindowExW(
                0,
                button_cls.as_ptr(),
                text,
                st,
                x,
                y,
                w,
                h,
                hwnd,
                id,
                0,
                std::ptr::null(),
            );
            SendMessageW(h1, WM_SETFONT, font as WPARAM, 1);
            h1
        };

        mk_static(line1.as_ptr(), 16, 20);
        mk_static(line2.as_ptr(), 38, 20);
        mk_static(line3.as_ptr(), 186, 20);

        let h_edit = CreateWindowExW(
            0,
            edit_cls.as_ptr(),
            default_w.as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_BORDER | ES_AUTOHSCROLL,
            24,
            78,
            372,
            26,
            hwnd,
            ID_EDIT,
            0,
            std::ptr::null(),
        );
        SendMessageW(h_edit, WM_SETFONT, font as WPARAM, 1);
        INSTALL_EDIT.store(h_edit, Ordering::Relaxed);

        mk_button(b_browse.as_ptr(), 404, 77, 72, 28, ID_BROWSE, false);
        let h_install = mk_button(b_install.as_ptr(), 132, 132, 124, 34, ID_INSTALL, true);
        mk_button(b_later.as_ptr(), 264, 132, 96, 34, ID_LATER, false);

        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(h_install);

        modal_loop();
    }

    install_choice().lock().ok().and_then(|mut g| g.take())
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

fn build_chain(preset: &str) -> Vec<String> {
    let mut v: Vec<String> = vec![preset.to_string()];
    for p in ["raw", "split", "default"] {
        if p != preset {
            v.push(p.to_string());
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
        out(&format!("--> 方式 {:<8} {}\n", name, preset_desc(&name)));
        out("    正在发送一次 Ctrl+Win+Right，请观察桌面是否切换 ...\n");
        let (raw, ext, gap, split) = preset_params(&name);
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
        finish_output();
        return;
    }
    if stop {
        cmd_stop();
        finish_output();
        return;
    }

    CONFIG.get_or_init(|| cfg);

    // 安装模式（--install <目录>）：由提权后的实例执行，完成后直接退出。
    if let Some(dir) = parse_install_arg() {
        if integrity_level(None).unwrap_or(0) < 0x3000 {
            // 注册 RunLevel=HIGHEST 的计划任务必须管理员权限
            if relaunch_elevated_with(&format!("--install \"{}\"", dir)) {
                out("安装需要管理员权限，已请求提权（请在 UAC 弹窗中确认）。\n");
            } else {
                out("[错误] 提权被取消或失败，无法注册开机启动。\n");
            }
            finish_output();
            return;
        }
        do_install(&dir);
        finish_output();
        return;
    }

    if elevate {
        if integrity_level(None).unwrap_or(0) >= 0x3000 {
            out("当前已是管理员权限，无需提权。\n");
        } else if relaunch_elevated() {
            out("已请求管理员权限重启（请在 UAC 弹窗中确认）。\n");
        } else {
            out("[错误] 提权重启被取消或失败（UAC 弹窗被拒绝？）。\n");
        }
        finish_output();
        return;
    }

    if test {
        cmd_test(&preset);
        finish_output();
        return;
    }

    // 开机启动：还没注册过就引导安装（复制自己到目标目录 + 注册最高权限计划任务）。
    // 放在单实例检查之前：安装动作本身不该被「已有实例在跑」挡住。
    if !startup_task_registered() && !install_skipped() {
        if has_console() {
            // 从终端启动：只提示，不弹窗打扰脚本场景
            out("提示：尚未加入开机启动。可用 --install <目录> 安装（需管理员权限）。\n");
        } else {
            match ask_install_location(&default_install_dir()) {
                Some(dir) => {
                    if integrity_level(None).unwrap_or(0) >= 0x3000 {
                        // 已经是管理员，直接装
                        do_install(&dir);
                    } else if relaunch_elevated_with(&format!("--install \"{}\"", dir)) {
                        out("安装需要管理员权限，已请求提权（请在 UAC 弹窗中确认）。\n");
                    } else {
                        out("[错误] 提权被取消或失败，未安装开机启动。\n");
                    }
                    finish_output();
                    return;
                }
                None => {
                    // 用户选了「以后再说」：写个标记，之后不再每次启动都弹
                    mark_install_skipped();
                }
            }
        }
    }

    // 单实例：先看有没有已经在跑的实例。
    //
    // 双击运行时没有控制台，out() 写的东西用户根本看不到，所以不能只靠打印，
    // 必须弹窗让用户明确选择怎么处理已经在运行的那个实例。
    let _mutex = if mutex_exists() {
        if has_console() {
            // 从终端启动：保持原来脚本友好的行为，直接接管重启（不弹窗打扰）。
            out("检测到已有实例在运行：结束旧实例并重新启动。\n");
            takeover_old_instance();
            match check_single_instance() {
                Some(h) => h,
                None => {
                    out("[错误] 结束旧实例失败，或旧实例尚未退出。\n");
                    return;
                }
            }
        } else {
            // 双击运行、无控制台：弹自定义对话框（按钮 = 重新运行 / 立即结束）
            let choice = ask_running_choice();
            match choice {
                RunningChoice::Stop => {
                    // 结束旧实例，并且不再启动
                    takeover_old_instance();
                    remove_pid_file();
                    return;
                }
                RunningChoice::Keep => return, // 保持现状：旧实例继续跑，本进程直接退出
                RunningChoice::Restart => {
                    // 接管（结束旧实例）后继续启动
                    takeover_old_instance();
                    match check_single_instance() {
                        Some(h) => h,
                        None => {
                            show_message(
                                "鼠标侧键切换",
                                "结束旧实例失败，或旧实例还没有完全退出。\n\
                                 请稍候再试，或用任务管理器结束后重试。",
                                MB_ICONWARNING | MB_SETFOREGROUND,
                            );
                            return;
                        }
                    }
                }
            }
        }
    } else {
        // 没有其它实例：仍然清理上一版残留（--stop 没跑干净 / 旧版进程），
        // 避免「改了代码却还在跑旧版」。
        takeover_old_instance();
        match check_single_instance() {
            Some(h) => h,
            None => {
                show_message(
                    "鼠标侧键切换",
                    "启动失败：无法创建单实例互斥体。",
                    MB_ICONWARNING | MB_SETFOREGROUND,
                );
                return;
            }
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
