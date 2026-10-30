#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
mouse_side_keys.py - 用鼠标侧键切换 Windows 虚拟桌面

    侧键 1（后退键 / XBUTTON1） -> Ctrl + Win + Right   切到右侧虚拟桌面
    侧键 2（前进键 / XBUTTON2） -> Ctrl + Win + Left    切到左侧虚拟桌面

（以上是默认方向，即用户习惯的对调方向；想恢复标准方向用 --no-swap。）

零第三方依赖，只用 ctypes 调用 Win32 API。适用 Windows 10 / 11。

用法：
    py mouse_side_keys.py                 启动（自动转入后台运行，关闭窗口不影响）
    py mouse_side_keys.py --foreground    前台运行（调试时用；关窗口会停止）
    py mouse_side_keys.py --no-swap       恢复标准方向（侧键1 切左、侧键2 切右）
    py mouse_side_keys.py --no-block      保留侧键原有的前进 / 后退功能（叠加触发）
    py mouse_side_keys.py --debounce 150  连按去抖，单位毫秒（0 = 关闭，默认 80）
    py mouse_side_keys.py --list-presets  列出所有按键合成方式
    py mouse_side_keys.py --preset raw    换一种按键合成方式（默认 rawsplit）
    py mouse_side_keys.py --elevate       通过 UAC 以管理员身份重启（前台是
                                          360 等管理员窗口时按键会被 UIPI 拦截）
    py mouse_side_keys.py --split 20      每个事件单独发一次，间隔 20ms
    py mouse_side_keys.py --no-raw        强制用虚拟键码而不是扫描码
    py mouse_side_keys.py --test          发送一次 Ctrl+Win+Right 自检，失败会自动换方式重试
    py mouse_side_keys.py --stop          停止正在运行的实例

按 Ctrl+C（前台模式）或运行 stop-admin.bat / --stop 停止。
开机自启：把 start-admin.bat 的快捷方式放进 shell:startup。

排障：
    本程序不写日志文件。后台运行时所有提示静默；想看运行状态就加
    --foreground 前台跑，按键被拒、权限不足等警告会直接打在窗口里。
    侧键完全没反应就跑 --test：它会依次尝试各种合成方式并报告哪种被系统接受。
"""

import argparse
import ctypes
import os
import signal
import subprocess
import sys
import tempfile
import time
from ctypes import wintypes

# --------------------------------------------------------------------------
# Win32 常量
# --------------------------------------------------------------------------
WH_MOUSE_LL = 14          # 全局低级鼠标钩子
HC_ACTION = 0

WM_XBUTTONDOWN = 0x020B
WM_XBUTTONUP = 0x020C
WM_XBUTTONDBLCLK = 0x020D
WM_QUIT = 0x0012
WM_APP_TRIGGER = 0x8001   # 自定义：钩子里只发这个，真正按键在主循环里送

# Win32 标准值：mouseData 高位 1 = 后退键，2 = 前进键
XBUTTON1 = 0x0001         # 侧键：后退
XBUTTON2 = 0x0002         # 侧键：前进

INPUT_KEYBOARD = 1
KEYEVENTF_EXTENDEDKEY = 0x0001
KEYEVENTF_KEYUP = 0x0002
KEYEVENTF_SCANCODE = 0x0008   # 指定后 wScan 生效、wVk 被忽略（硬件级模拟）

VK_LWIN = 0x5B
VK_RWIN = 0x5C
VK_LCONTROL = 0xA2
VK_RCONTROL = 0xA3
VK_CONTROL = 0x11             # 通用 Ctrl，不区分左右
VK_LEFT = 0x25
VK_RIGHT = 0x27

# 真实键盘上属于"扩展键区"的按键，必须带 E0 前缀（KEYEVENTF_EXTENDEDKEY）
EXTENDED_KEYS = frozenset((0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28,
                           0x2D, 0x2E, 0x5B, 0x5C, 0xA3, 0xA5))

MAPVK_VK_TO_VSC = 0

PM_REMOVE = 0x0001
QS_ALLINPUT = 0x04FF
MWMO_INPUTAVAILABLE = 0x0004

ERROR_ALREADY_EXISTS = 183
CREATE_NO_WINDOW = 0x08000000
DETACHED_PROCESS = 0x00000008
CREATE_NEW_PROCESS_GROUP = 0x00000200
EXTRA_INFO = 0x4D534B31   # "MSK1"，给自己的合成事件打标记

MUTEX_NAME = "Local\\MouseSideKeysDesktopSwitch"
PID_FILE = os.path.join(tempfile.gettempdir(), "mouse_side_keys.pid")

user32 = ctypes.WinDLL("user32", use_last_error=True)
kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)

LRESULT = ctypes.c_ssize_t
ULONG_PTR = ctypes.c_size_t


# --------------------------------------------------------------------------
# Win32 结构体
# --------------------------------------------------------------------------
class MSLLHOOKSTRUCT(ctypes.Structure):
    _fields_ = [
        ("pt", wintypes.POINT),
        ("mouseData", wintypes.DWORD),
        ("flags", wintypes.DWORD),
        ("time", wintypes.DWORD),
        ("dwExtraInfo", ULONG_PTR),
    ]


class KEYBDINPUT(ctypes.Structure):
    _fields_ = [
        ("wVk", wintypes.WORD),
        ("wScan", wintypes.WORD),
        ("dwFlags", wintypes.DWORD),
        ("time", wintypes.DWORD),
        ("dwExtraInfo", ULONG_PTR),
    ]


class MOUSEINPUT(ctypes.Structure):
    _fields_ = [
        ("dx", wintypes.LONG),
        ("dy", wintypes.LONG),
        ("mouseData", wintypes.DWORD),
        ("dwFlags", wintypes.DWORD),
        ("time", wintypes.DWORD),
        ("dwExtraInfo", ULONG_PTR),
    ]


class HARDWAREINPUT(ctypes.Structure):
    _fields_ = [
        ("uMsg", wintypes.DWORD),
        ("wParamL", wintypes.WORD),
        ("wParamH", wintypes.WORD),
    ]


class _INPUTUNION(ctypes.Union):
    _fields_ = [("ki", KEYBDINPUT), ("mi", MOUSEINPUT), ("hi", HARDWAREINPUT)]


class INPUT(ctypes.Structure):
    _anonymous_ = ("u",)
    _fields_ = [("type", wintypes.DWORD), ("u", _INPUTUNION)]


HOOKPROC = ctypes.WINFUNCTYPE(LRESULT, ctypes.c_int, wintypes.WPARAM, wintypes.LPARAM)


def _declare_api():
    user32.SetWindowsHookExW.argtypes = [ctypes.c_int, HOOKPROC, wintypes.HINSTANCE, wintypes.DWORD]
    user32.SetWindowsHookExW.restype = wintypes.HHOOK
    user32.UnhookWindowsHookEx.argtypes = [wintypes.HHOOK]
    user32.UnhookWindowsHookEx.restype = wintypes.BOOL
    user32.CallNextHookEx.argtypes = [wintypes.HHOOK, ctypes.c_int, wintypes.WPARAM, wintypes.LPARAM]
    user32.CallNextHookEx.restype = LRESULT
    user32.SendInput.argtypes = [wintypes.UINT, ctypes.POINTER(INPUT), ctypes.c_int]
    user32.SendInput.restype = wintypes.UINT
    user32.keybd_event.argtypes = [wintypes.BYTE, wintypes.BYTE, wintypes.DWORD, ULONG_PTR]
    user32.MapVirtualKeyW.argtypes = [wintypes.UINT, wintypes.UINT]
    user32.MapVirtualKeyW.restype = wintypes.UINT
    user32.PeekMessageW.argtypes = [ctypes.POINTER(wintypes.MSG), wintypes.HWND,
                                    wintypes.UINT, wintypes.UINT, wintypes.UINT]
    user32.PeekMessageW.restype = wintypes.BOOL
    user32.MsgWaitForMultipleObjectsEx.argtypes = [wintypes.DWORD, ctypes.c_void_p,
                                                   wintypes.DWORD, wintypes.DWORD, wintypes.DWORD]
    user32.MsgWaitForMultipleObjectsEx.restype = wintypes.DWORD
    user32.TranslateMessage.argtypes = [ctypes.POINTER(wintypes.MSG)]
    user32.DispatchMessageW.argtypes = [ctypes.POINTER(wintypes.MSG)]
    user32.PostThreadMessageW.argtypes = [wintypes.DWORD, wintypes.UINT,
                                          wintypes.WPARAM, wintypes.LPARAM]
    user32.PostThreadMessageW.restype = wintypes.BOOL
    kernel32.GetModuleHandleW.argtypes = [wintypes.LPCWSTR]
    kernel32.GetModuleHandleW.restype = wintypes.HMODULE
    kernel32.GetCurrentThreadId.argtypes = []
    kernel32.GetCurrentThreadId.restype = wintypes.DWORD
    kernel32.CreateMutexW.argtypes = [ctypes.c_void_p, wintypes.BOOL, wintypes.LPCWSTR]
    kernel32.CreateMutexW.restype = wintypes.HANDLE
    kernel32.OpenMutexW.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.LPCWSTR]
    kernel32.OpenMutexW.restype = wintypes.HANDLE
    kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
    kernel32.GetConsoleWindow.argtypes = []
    kernel32.GetConsoleWindow.restype = wintypes.HWND
    kernel32.AttachConsole.argtypes = [wintypes.DWORD]
    kernel32.AttachConsole.restype = wintypes.BOOL
    shell32 = ctypes.WinDLL("shell32", use_last_error=True)
    shell32.ShellExecuteW.argtypes = [wintypes.HWND, wintypes.LPCWSTR, wintypes.LPCWSTR,
                                      wintypes.LPCWSTR, wintypes.LPCWSTR, ctypes.c_int]
    shell32.ShellExecuteW.restype = ctypes.c_void_p
    user32.MessageBoxW.argtypes = [wintypes.HWND, wintypes.LPCWSTR, wintypes.LPCWSTR, wintypes.UINT]
    user32.MessageBoxW.restype = ctypes.c_int


_declare_api()


# --------------------------------------------------------------------------
# 运行状态提示
# --------------------------------------------------------------------------
# 只在有控制台时输出。pythonw.exe 无窗口运行时 sys.stdout 是 None，
# 此时完全静默；用 start-admin.bat 启动会保留控制台，就能看到这些提示。
def _note(msg: str) -> None:
    if sys.stdout is not None:
        print(msg)


# UIPI 弹窗提醒：每次会话最多弹一次（全局标志），避免反复打扰
_uipi_dialog_shown = False
MB_ICONWARNING = 0x00000030
MB_SETFOREGROUND = 0x00010000
SW_SHOWNORMAL = 1


def _warn_uipi_dialog(hint: str) -> None:
    global _uipi_dialog_shown
    if _uipi_dialog_shown:
        return
    _uipi_dialog_shown = True
    user32.MessageBoxW(
        None,
        hint + "\n\n典型场景：360安全卫士等以管理员权限运行的窗口处于前台。\n"
               "雷蛇等厂商宏能正常是因为它们走内核驱动注入，不受此限制。\n\n"
               "解决办法：用 start-admin.bat 以管理员身份重启本程序，\n"
               "或对 exe 设置「以管理员身份运行」兼容性选项。",
        "鼠标侧键切换 —— 按键被系统拦截", MB_ICONWARNING | MB_SETFOREGROUND)


# --------------------------------------------------------------------------
# 按键合成
# --------------------------------------------------------------------------
def _key_input(vk: int, key_up: bool, use_extended: bool = True,
               raw_scancode: bool = False) -> INPUT:
    """
    构造一个键盘输入事件。

    raw_scancode=False（默认）：wVk 生效、wScan 被系统忽略。这是 AutoHotkey
      SendInput 的默认方式，绝大多数情况下都能触发系统快捷键。
    raw_scancode=True：只发扫描码、wVk 置 0，属于硬件级模拟，作为备选方案。
      （注意：不指定 KEYEVENTF_SCANCODE 时系统只看 wVk，wScan 是无效的。）
    """
    scan = user32.MapVirtualKeyW(vk, MAPVK_VK_TO_VSC) & 0xFF
    flags = 0
    if raw_scancode:
        flags |= KEYEVENTF_SCANCODE
    if (raw_scancode or use_extended) and vk in EXTENDED_KEYS:
        flags |= KEYEVENTF_EXTENDEDKEY
    if key_up:
        flags |= KEYEVENTF_KEYUP

    item = INPUT()
    item.type = INPUT_KEYBOARD
    item.ki = KEYBDINPUT(wVk=0 if raw_scancode else vk, wScan=scan,
                         dwFlags=flags, time=0, dwExtraInfo=EXTRA_INFO)
    return item


def send_ctrl_win_arrow(arrow_vk: int, holds=(VK_LWIN, VK_LCONTROL),
                        use_extended: bool = True, gap_ms: int = 0,
                        raw_scancode: bool = False, split_ms: int = 0) -> bool:
    """发送 Win + Ctrl + 方向键。按下顺序 Win -> Ctrl -> arrow，抬起反向。"""
    seq = []
    for vk in holds:
        seq.append(_key_input(vk, False, use_extended, raw_scancode))
        if gap_ms:
            _flush(seq, holds)
            seq = []
            time.sleep(gap_ms / 1000.0)
    seq.append(_key_input(arrow_vk, False, use_extended, raw_scancode))
    seq.append(_key_input(arrow_vk, True, use_extended, raw_scancode))
    for vk in reversed(holds):
        seq.append(_key_input(vk, True, use_extended, raw_scancode))
        if gap_ms:
            _flush(seq, holds)
            seq = []
            time.sleep(gap_ms / 1000.0)
    if split_ms:
        # 一次只发一个事件，中间留间隔。某些环境下整批发送会被整体拒绝。
        for item in seq:
            if not _flush([item], holds):
                return False
            time.sleep(split_ms / 1000.0)
        return True
    return _flush(seq, holds)


def _flush(seq, holds=()) -> bool:
    if not seq:
        return True
    array = (INPUT * len(seq))(*seq)
    sent = user32.SendInput(len(seq), array, ctypes.sizeof(INPUT))
    if sent != len(seq):
        err = ctypes.get_last_error()
        _note("[警告] SendInput 只送出了 %d/%d 个事件，err=%s —— 强制释放修饰键防止卡键"
              % (sent, len(seq), err))
        if sent == 0:
            # 一个都发不出去，基本就是权限隔离（UIPI）或输入被阻塞
            hint = uipi_check()
            _note("[警告] %s" % (hint if hint else
                                 "SendInput 完全被拒绝：可能是 UIPI 权限隔离，"
                                 "或有程序调用了 BlockInput（远程桌面 / 某些录屏工具）。"
                                 "请以管理员身份运行后重试。"))
            if hint:
                # 后台运行时 _note 是静默的，用户完全感知不到失效原因；
                # 每次会话只弹一次对话框，把原因醒目地告诉用户。
                _warn_uipi_dialog(hint)
        # 兜底：万一修饰键被卡住，用户会发现整个键盘"失灵"，这里强制抬起来
        for vk in (VK_LCONTROL, VK_RCONTROL, VK_LWIN, VK_RWIN, VK_CONTROL):
            user32.keybd_event(vk, 0, KEYEVENTF_KEYUP, EXTRA_INFO)
        return False
    return True


# --------------------------------------------------------------------------
# 主体
# --------------------------------------------------------------------------
class SideKeySwitcher:
    def __init__(self, swap: bool = False, block: bool = True, debounce_ms: int = 80,
                 win_vk: int = VK_LWIN, ctrl_vk: int = VK_LCONTROL,
                 use_extended: bool = True, gap_ms: int = 0,
                 raw_scancode: bool = False, split_ms: int = 0):
        self.swap = swap
        self.block = block
        self.debounce = max(0.0, debounce_ms / 1000.0)
        self.holds = (win_vk, ctrl_vk)
        self.use_extended = use_extended
        self.gap_ms = max(0, gap_ms)
        self.raw_scancode = raw_scancode
        self.split_ms = max(0, split_ms)
        self._last_fire = 0.0
        self._running = False
        self._hook = None
        self._tid = 0
        self._proc = HOOKPROC(self._hook_proc)   # 必须持引用，否则会被 GC 回收

    # ---- 钩子回调：只做判断和投递，绝不做重活 ----
    def _hook_proc(self, n_code, w_param, l_param):
        try:
            if n_code >= HC_ACTION and w_param in (WM_XBUTTONDOWN, WM_XBUTTONDBLCLK,
                                                   WM_XBUTTONUP):
                info = ctypes.cast(ctypes.c_void_p(l_param),
                                   ctypes.POINTER(MSLLHOOKSTRUCT)).contents
                x_button = (info.mouseData >> 16) & 0xFFFF
                if x_button in (XBUTTON1, XBUTTON2):
                    if w_param != WM_XBUTTONUP:
                        arrow = self._pick_arrow(x_button)
                        if self._accept(x_button):
                            ok = user32.PostThreadMessageW(self._tid, WM_APP_TRIGGER, arrow, 0)
                            if not ok:
                                _note("[警告] 投递触发消息失败，err=%s" % ctypes.get_last_error())
                    if self.block:
                        return 1          # 吞掉事件，屏蔽原有的前进 / 后退
        except Exception as exc:              # 回调里绝不能把异常抛出去
            _note("[警告] 钩子回调出错：%r" % (exc,))
        return user32.CallNextHookEx(None, n_code, w_param, l_param)

    def _pick_arrow(self, x_button: int) -> int:
        arrow = VK_LEFT if x_button == XBUTTON1 else VK_RIGHT
        if self.swap:
            arrow = VK_RIGHT if arrow == VK_LEFT else VK_LEFT
        return arrow

    def _accept(self, x_button: int) -> bool:
        """去抖。命中窗口内直接丢弃，避免鼠标连击重复切换。"""
        now = time.monotonic()
        if self.debounce and (now - self._last_fire) < self.debounce:
            return False
        self._last_fire = now
        return True

    # ---- 主循环里真正送按键：离开钩子上下文再发，避免输入流被干扰 ----
    def _fire(self, arrow_vk: int):
        send_ctrl_win_arrow(arrow_vk, self.holds, self.use_extended,
                            self.gap_ms, self.raw_scancode, self.split_ms)

    # ---- 生命周期 ----
    def install(self):
        hmod = kernel32.GetModuleHandleW(None)
        self._hook = user32.SetWindowsHookExW(WH_MOUSE_LL, self._proc, hmod, 0)
        if not self._hook:
            raise OSError("SetWindowsHookExW 失败，错误码 %s（%s）"
                          % (ctypes.get_last_error(),
                             ctypes.FormatError(ctypes.get_last_error())))

    def uninstall(self):
        if self._hook:
            user32.UnhookWindowsHookEx(self._hook)
            self._hook = None

    def stop(self, *_args):
        self._running = False

    def run(self):
        _write_pid_file()
        self._tid = kernel32.GetCurrentThreadId()
        self.install()
        self._running = True

        signal.signal(signal.SIGINT, self.stop)
        try:
            signal.signal(signal.SIGTERM, self.stop)
        except (ValueError, OSError):
            pass

        try:
            msg = wintypes.MSG()
            while self._running:
                # 用阻塞等待代替 sleep 轮询：线程处于消息等待状态时，
                # 系统才能及时回调我们的低级钩子，否则事件会被延迟甚至绕过钩子。
                user32.MsgWaitForMultipleObjectsEx(0, None, 200, QS_ALLINPUT,
                                                   MWMO_INPUTAVAILABLE)
                while user32.PeekMessageW(ctypes.byref(msg), None, 0, 0, PM_REMOVE):
                    if msg.message == WM_APP_TRIGGER:
                        self._fire(msg.wParam)
                    elif msg.message == WM_QUIT:
                        self._running = False
                        break
                    else:
                        user32.TranslateMessage(ctypes.byref(msg))
                        user32.DispatchMessageW(ctypes.byref(msg))
        except KeyboardInterrupt:
            pass
        finally:
            self.uninstall()
            _remove_pid_file()


# --------------------------------------------------------------------------
# 辅助
# --------------------------------------------------------------------------
def _write_pid_file():
    try:
        with open(PID_FILE, "w", encoding="ascii") as fh:
            fh.write(str(os.getpid()))
    except OSError:
        _note("[警告] PID 文件写入失败，--stop 可能不可用：%s" % PID_FILE)


def _remove_pid_file():
    try:
        if os.path.exists(PID_FILE):
            os.remove(PID_FILE)
    except OSError:
        pass


# --------------------------------------------------------------------------
# 控制台脱离（后台化）
# --------------------------------------------------------------------------
# 直接双击 py 或在 cmd 里 python 运行时，进程附着在那个控制台窗口上：
# 窗口一关，Windows 会把附着其上的进程一起杀掉，功能随之失效。
# 所以常驻启动时自动用 pythonw.exe 重启自己 —— pythonw 不创建控制台，
# 进程生命周期不再依附任何窗口。
def _attached_to_console() -> bool:
    return bool(kernel32.GetConsoleWindow())


def _relaunch_detached() -> bool:
    """用 pythonw.exe 重启自身（脱离控制台）。成功返回 True。"""
    pythonw = os.path.join(os.path.dirname(os.path.abspath(sys.executable)),
                           "pythonw.exe")
    if not os.path.exists(pythonw):
        return False
    script = os.path.abspath(__file__)
    subprocess.Popen([pythonw, script] + sys.argv[1:],
                     cwd=os.path.dirname(script),
                     creationflags=DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP,
                     close_fds=True)
    return True


# --------------------------------------------------------------------------
# 提权重启（--elevate）
# --------------------------------------------------------------------------
# UIPI 规则：普通权限进程不能向前台管理员窗口（如 360安全卫士）注入按键。
# 唯一解法是本程序自己也以管理员运行 —— ShellExecuteW 的 runas 动词会触发
# UAC 弹窗，用户确认后以高完整性级别重启自身。雷蛇宏走内核驱动注入所以
# 不需要这一步，用户态脚本绕不开。
def relaunch_elevated() -> bool:
    """通过 UAC 提权重启自身。成功返回 True（调用方应立即退出）。"""
    if getattr(sys, "frozen", False):
        target = sys.executable
        params = " ".join('"%s"' % a for a in sys.argv[1:] if a != "--elevate")
    else:
        target = sys.executable
        script = os.path.abspath(__file__)
        rest = [script] + [a for a in sys.argv[1:] if a != "--elevate"]
        params = " ".join('"%s"' % a for a in rest)
    ret = shell32.ShellExecuteW(None, "runas", target, params, None, SW_SHOWNORMAL)
    # ShellExecuteW 返回值 > 32 表示成功
    return ret is not None and ret > 32


# --------------------------------------------------------------------------
# UIPI（用户界面特权隔离）检测
# --------------------------------------------------------------------------
# 低完整性级别的进程向高完整性级别的窗口模拟输入时，Windows 会直接拒绝
# SendInput —— 返回值是 0，而且 GetLastError 也是 0，非常难查。
# 典型场景：cmd / IDE 以管理员身份运行，而脚本是普通权限。
TOKEN_QUERY = 0x0008
PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
TOKEN_INTEGRITY_LEVEL = 25

INTEGRITY_NAMES = {
    0x0000: "Untrusted(不受信任)",
    0x1000: "Low(低)",
    0x2000: "Medium(中，普通程序默认)",
    0x2100: "Medium Plus(中高)",
    0x3000: "High(高，管理员运行的程序)",
    0x4000: "System(系统)",
    -1: "读取失败",
}


class SID_AND_ATTRIBUTES(ctypes.Structure):
    _fields_ = [("Sid", ctypes.c_void_p), ("Attributes", wintypes.DWORD)]


class TOKEN_MANDATORY_LABEL(ctypes.Structure):
    _fields_ = [("Label", SID_AND_ATTRIBUTES)]


advapi32 = ctypes.WinDLL("advapi32", use_last_error=True)

kernel32.GetCurrentProcess.argtypes = []
kernel32.GetCurrentProcess.restype = wintypes.HANDLE
kernel32.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
kernel32.OpenProcess.restype = wintypes.HANDLE
advapi32.OpenProcessToken.argtypes = [wintypes.HANDLE, wintypes.DWORD,
                                      ctypes.POINTER(wintypes.HANDLE)]
advapi32.OpenProcessToken.restype = wintypes.BOOL
advapi32.GetTokenInformation.argtypes = [wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p,
                                         wintypes.DWORD, ctypes.POINTER(wintypes.DWORD)]
advapi32.GetTokenInformation.restype = wintypes.BOOL
advapi32.GetSidSubAuthorityCount.argtypes = [ctypes.c_void_p]
advapi32.GetSidSubAuthorityCount.restype = ctypes.POINTER(wintypes.BYTE)
advapi32.GetSidSubAuthority.argtypes = [ctypes.c_void_p, wintypes.DWORD]
advapi32.GetSidSubAuthority.restype = ctypes.POINTER(wintypes.DWORD)
user32.GetForegroundWindow.argtypes = []
user32.GetForegroundWindow.restype = wintypes.HWND
user32.GetWindowThreadProcessId.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.DWORD)]
user32.GetWindowThreadProcessId.restype = wintypes.DWORD


def integrity_level(pid: int = None) -> int:
    """返回进程完整性级别的 RID；取不到返回 -1。"""
    try:
        if pid is None:
            proc = kernel32.GetCurrentProcess()
        else:
            proc = kernel32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
            if not proc:
                return -1
        token = wintypes.HANDLE()
        if not advapi32.OpenProcessToken(proc, TOKEN_QUERY, ctypes.byref(token)):
            return -1
        buf = (ctypes.c_char * 256)()
        needed = wintypes.DWORD()
        if not advapi32.GetTokenInformation(token, TOKEN_INTEGRITY_LEVEL, buf, 256,
                                            ctypes.byref(needed)):
            return -1
        label = ctypes.cast(buf, ctypes.POINTER(TOKEN_MANDATORY_LABEL)).contents
        sid = label.Label.Sid
        cnt = advapi32.GetSidSubAuthorityCount(sid)
        if not cnt:
            return -1
        return advapi32.GetSidSubAuthority(sid, cnt[0] - 1)[0]
    except Exception:                                      # noqa: BLE001
        return -1


def describe_level(level: int) -> str:
    return "0x%04X(%s)" % (level, INTEGRITY_NAMES.get(level, "未知"))


def uipi_check() -> str:
    """权限低于前台窗口时返回说明文本，否则返回空字符串。"""
    mine = integrity_level()
    hwnd = user32.GetForegroundWindow()
    if not hwnd:
        return ""
    pid = wintypes.DWORD()
    user32.GetWindowThreadProcessId(hwnd, ctypes.byref(pid))
    if not pid.value:
        return ""
    fg = integrity_level(pid.value)
    if mine < 0 or fg < 0 or mine >= fg:
        return ""
    return ("UIPI 阻止：本进程 %s 低于前台窗口 %s，Windows 拒绝模拟按键。"
            "请以管理员身份运行（用 start-admin.bat），"
            "或让前台的高权限程序不要以管理员运行。"
            % (describe_level(mine), describe_level(fg)))


def already_running() -> bool:
    """用命名互斥体判断是否已经有实例在跑。

    句柄语义：返回 False 时本进程就是首个实例，句柄必须保持打开
    （进程退出时由系统自动释放），否则互斥体会立即销毁、检测失效；
    返回 True 时该句柄是冗余副本，立即关闭。
    """
    handle = kernel32.CreateMutexW(None, False, MUTEX_NAME)
    if not handle:
        return False
    if ctypes.get_last_error() == ERROR_ALREADY_EXISTS:
        kernel32.CloseHandle(handle)
        return True
    return False  # 句柄有意保留：本进程自此成为持有者


SYNCHRONIZE = 0x00100000


def _mutex_exists() -> bool:
    """纯检测：互斥体是否存在。用 OpenMutex，不产生"创建并持有"的副作用。"""
    handle = kernel32.OpenMutexW(SYNCHRONIZE, False, MUTEX_NAME)
    if handle:
        kernel32.CloseHandle(handle)
        return True
    return False


def wait_not_running(timeout: float = 5.0) -> bool:
    """轮询等待互斥体释放（旧实例完全退出）。返回 True 表示已无实例。

    注意不能用 already_running() 来等：它在互斥体不存在时会顺手创建并持有，
    会把等待逻辑自己挡住。
    """
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if not _mutex_exists():
            return True
        time.sleep(0.1)
    return not _mutex_exists()


def cmd_stop() -> int:
    if not os.path.exists(PID_FILE):
        print("没有找到正在运行的实例（缺少 PID 文件）。")
        return 1
    try:
        with open(PID_FILE, "r", encoding="ascii") as fh:
            pid = int(fh.read().strip())
    except (OSError, ValueError):
        print("PID 文件已损坏，已清理。")
        _remove_pid_file()
        return 1

    result = subprocess.run(["taskkill", "/F", "/PID", str(pid)],
                            capture_output=True, creationflags=CREATE_NO_WINDOW)
    if result.returncode == 0:
        print("已停止鼠标侧键切换（PID %d）。" % pid)
        _remove_pid_file()
        return 0
    print("结束进程失败：%s" % result.stderr.decode("gbk", "ignore").strip())
    _remove_pid_file()
    return 1


# 按键合成的几种方式。默认用 rawsplit（实测唯一能穿透过滤器的方式）。
#
# 实测结论：很多环境会按「虚拟键码」过滤合成输入 —— VK_LWIN 单独发送就被拒
# （SendInput 返回 0/1）。改用 KEYEVENTF_SCANCODE 让 wVk 置 0 即可绕过。
# 另外「一批多个事件」也常被拒，必须一次只发一个。两者缺一不可。
# 需要微调间隔时用 --split / --gap，不必再加 preset。
PRESETS = {
    "rawsplit": "【默认】扫描码(wVk=0) + 逐个发送 + 10ms 间隔 —— 实测 6/6 成功",
    "raw":      "只发扫描码，但整批一次发送",
    "split":    "虚拟键码 + 逐个发送，间隔 10ms",
    "default":  "虚拟键码整批发送 —— AutoHotkey Send 的等价方式（常被拒）",
    "no-ext":   "扫描码，但不带扩展键标志",
}


def preset_kwargs(name: str) -> dict:
    kw = dict(win_vk=VK_LWIN, ctrl_vk=VK_LCONTROL, use_extended=True,
              gap_ms=0, raw_scancode=False, split_ms=0)
    if name == "rawsplit":
        kw["raw_scancode"] = True
        kw["split_ms"] = 10
    elif name == "raw":
        kw["raw_scancode"] = True
    elif name == "split":
        kw["split_ms"] = 10
    elif name == "no-ext":
        kw["raw_scancode"] = True
        kw["use_extended"] = False
    elif name != "default":
        raise KeyError(name)
    return kw


def send_preset(name: str, arrow_vk: int = VK_RIGHT) -> bool:
    kw = preset_kwargs(name)
    return send_ctrl_win_arrow(arrow_vk, holds=(kw["win_vk"], kw["ctrl_vk"]),
                               use_extended=kw["use_extended"],
                               gap_ms=kw["gap_ms"],
                               raw_scancode=kw["raw_scancode"],
                               split_ms=kw["split_ms"])


def cmd_test(preset: str = "rawsplit", try_all: bool = True) -> int:
    print("本进程权限级别：%s" % describe_level(integrity_level()))
    hint = uipi_check()
    if hint:
        print("[警告] %s" % hint)
        print()

    # 先试最有把握的，失败就自动往下退。实测 rawsplit（扫描码 + 逐个发送）
    # 是唯一能同时绕开「Win 键过滤」和「整批拒绝」的方式。
    chain = [preset]
    if try_all:
        chain += [p for p in ("raw", "split", "default") if p != preset]

    for name in chain:
        print("--> 方式 %-8s %s" % (name, PRESETS[name]))
        print("    正在发送一次 Ctrl+Win+Right，请观察桌面是否切换 ...")
        ok = send_preset(name)
        print("    结果：%s" % ("发送成功" if ok else "被拒绝（SendInput 返回 0）"))
        if ok:
            print()
            print("桌面如果切换了，就用这个方式常驻：")
            print("    python mouse_side_keys.py --preset %s" % name)
            return 0
        print()
    print("所有方式都被系统拒绝。")
    print("先看窗口第一行打印的权限级别：若不是 High(0x3000)，改用 start-admin.bat 提权。")
    print("提权后仍失败，多半是安全软件拦截了合成输入，把本目录加入白名单再试。")
    return 1


# --------------------------------------------------------------------------
# 入口
# --------------------------------------------------------------------------
def main(argv=None) -> int:
    parser = argparse.ArgumentParser(
        description="鼠标侧键 -> Windows 虚拟桌面切换（默认对调：侧键1 = Ctrl+Win+Right，侧键2 = Ctrl+Win+Left）")
    # 默认方向是用户习惯的对调方向（后退键切右、前进键切左）。
    # 想恢复标准方向（后退切左、前进切右）时用 --no-swap。
    parser.add_argument("--swap", dest="swap", action="store_true", default=True,
                        help="对调两个侧键的方向（当前默认）")
    parser.add_argument("--no-swap", dest="swap", action="store_false",
                        help="恢复标准方向：侧键1 切左、侧键2 切右")
    parser.add_argument("--no-block", action="store_true",
                        help="不吞掉侧键事件，保留原来的前进 / 后退功能")
    parser.add_argument("--debounce", type=int, default=80, metavar="MS",
                        help="连按去抖时间，毫秒；0 表示关闭（默认 80）")
    parser.add_argument("--foreground", action="store_true",
                        help="前台运行不转入后台（调试时用；关闭控制台窗口会停止功能）")
    parser.add_argument("--elevate", action="store_true",
                        help="通过 UAC 以管理员身份重启本程序"
                             "（解决前台是 360 等管理员窗口时按键被 UIPI 拦截的问题）")
    parser.add_argument("--preset", default="rawsplit", choices=sorted(PRESETS),
                        help="按键合成方式；用 --list-presets 查看全部（默认 rawsplit）")
    parser.add_argument("--gap", type=int, default=-1, metavar="MS",
                        help="覆盖 preset 中的键间隔，毫秒")
    parser.add_argument("--split", type=int, default=-1, metavar="MS",
                        help="每个事件单独发一次 SendInput，间隔该毫秒数；"
                             "整批被拒时用它（默认已按 preset 决定）")
    parser.add_argument("--no-raw", action="store_true",
                        help="强制用虚拟键码而不是扫描码（rawsplit 的回退）")
    parser.add_argument("--list-presets", action="store_true",
                        help="列出所有按键合成方式后退出")
    parser.add_argument("--test", action="store_true", help="发送一次 Ctrl+Win+Right 自检后退出")
    parser.add_argument("--stop", action="store_true", help="停止正在运行的实例")
    args = parser.parse_args(argv)

    # 无控制台环境（pythonw / 打包成 --noconsole 的 exe）下 sys.stdout 是 None，
    # 裸 print 会 AttributeError。先尝试挂到父控制台 —— 这样从 cmd 里运行
    # 打包版 exe 的 --stop / --test 时输出依然可见；挂不上（双击运行）
    # 就重定向到 devnull 彻底静默。
    ATTACH_PARENT_PROCESS = 0xFFFFFFFF
    if sys.stdout is None:
        try:
            if kernel32.AttachConsole(ATTACH_PARENT_PROCESS):
                sys.stdout = open("CONOUT$", "w", encoding="gbk", errors="replace")
                sys.stderr = sys.stdout
        except OSError:
            pass
    if sys.stdout is None:
        sys.stdout = open(os.devnull, "w", encoding="utf-8")
    if sys.stderr is None:
        sys.stderr = sys.stdout

    if args.list_presets:
        print("可用的按键合成方式：")
        for name in sorted(PRESETS):
            print("  %-8s %s%s" % (name, PRESETS[name],
                                   "   <- 默认" if name == "rawsplit" else ""))
        return 0

    if args.stop:
        return cmd_stop()

    if os.name != "nt":
        print("本程序只支持 Windows。")
        return 2

    if args.elevate:
        if integrity_level() >= 0x3000:
            print("当前已是管理员权限，无需提权。")
            return 0
        if relaunch_elevated():
            print("已请求管理员权限重启（请在 UAC 弹窗中确认）。")
            return 0
        print("[错误] 提权重启被取消或失败（UAC 弹窗被拒绝？）。")
        return 1

    if args.test:
        return cmd_test(args.preset)

    # 常驻模式下，若进程还附着在控制台窗口上（双击 py / 在 cmd 里 python 运行），
    # 先自动转入后台：用 pythonw 重启自己。否则窗口一关进程就被杀，功能失效。
    # 注意必须在接管逻辑之前执行 —— 本进程不能碰互斥体 / PID 文件，
    # 干净退出后由新的 pythonw 进程走完整流程。
    # 打包成 exe（--noconsole）后本身就是无窗口进程，天然免疫，直接豁免。
    want_detach = (not args.foreground
                   and not getattr(sys, "frozen", False)
                   and _attached_to_console())
    if want_detach:
        if _relaunch_detached():
            print("已转入后台运行（本窗口可以关，不影响功能）。")
            print("停止方式：python mouse_side_keys.py --stop，或双击 stop-admin.bat")
            return 0
        print("[提示] 未找到 pythonw.exe，保持前台运行 —— 关闭本窗口会停止功能。")

    # 关键：先接管上一版残留的进程。否则改了代码却还在跑旧版，
    # 表现就是「怎么改都没反应」。
    if os.path.exists(PID_FILE):
        if sys.stdout is not None:
            print("发现上一个实例残留，正在停止它 ...")
        cmd_stop()
        # taskkill /F 返回不代表目标进程已完全销毁，互斥体释放是异步的。
        # 固定 sleep 在系统繁忙时不够，必须轮询等互斥体真正释放，
        # 否则新实例会误判「仍有实例在运行」而退出 —— 新旧两空。
        if not wait_not_running(timeout=5.0):
            print("旧实例未能退出（超时 5 秒）。请运行 stop-admin.bat 后重试。")
            return 1

    if already_running():
        print("仍有另一个实例在运行（其它目录的副本？）。请先运行 stop-admin.bat。")
        return 1

    kw = preset_kwargs(args.preset)
    if args.gap >= 0:
        kw["gap_ms"] = args.gap
    if args.split >= 0:
        kw["split_ms"] = args.split
    if args.no_raw:
        kw["raw_scancode"] = False

    if sys.stdout is not None:
        d1, d2 = ("Right", "Left") if args.swap else ("Left", "Right")
        print("本进程权限级别：%s" % describe_level(integrity_level()))
        print("鼠标侧键切换已启动。侧键1 -> Ctrl+Win+%s，侧键2 -> Ctrl+Win+%s%s"
              % (d1, d2, "（对调方向）" if args.swap else "（标准方向）"))
        print("按键方式：preset=%s，扫描码=%s，逐个发送间隔=%dms，键间隔=%dms"
              % (args.preset, kw["raw_scancode"], kw["split_ms"], kw["gap_ms"]))
        print("停止方式：运行 stop-admin.bat，或执行 python mouse_side_keys.py --stop")

    hint = uipi_check()
    if hint:
        _note("")
        _note("[警告] %s" % hint)
        _note("        侧键很可能不会有效果，请改用 start-admin.bat。")

    try:
        SideKeySwitcher(swap=args.swap, block=not args.no_block,
                        debounce_ms=args.debounce,
                        win_vk=kw["win_vk"], ctrl_vk=kw["ctrl_vk"],
                        use_extended=kw["use_extended"],
                        gap_ms=kw["gap_ms"],
                        raw_scancode=kw["raw_scancode"],
                        split_ms=kw["split_ms"]).run()
    except OSError as exc:
        _note("[错误] %s" % exc)
        return 3
    return 0


if __name__ == "__main__":
    sys.exit(main())
