# 添加开机启动
1、按 Win + R，输入 taskschd.msc 回车。
2、右侧点 ‌创建基本任务‌，命名后下一步。
3、触发器选 ‌当用户登录时‌。
4、操作选 ‌启动程序‌，浏览定位到exe文件。
5、**关键**：属性 → 常规 → 勾选 ‌"使用最高权限运行"‌（exe 清单含
   requireAdministrator，默认 Limited 运行级别会在登录时报 740
   ERROR_ELEVATION_REQUIRED，任务静默失败、无任何提示）。
6、笔记本用户：属性 → 条件 → 取消勾选 ‌"只在交流电源时启动"‌，
   否则电池状态下开机不触发。
7、完成后可在任务计划程序库里看到，右键可删除。
- 2026-10-25 实测：只做 1-4 开机无效，手动双击正常（资源管理器会弹
  UAC）；补做第 5 步后开机启动生效。改 RunLevel 需管理员权限，
  普通会话 Set-ScheduledTask 报拒绝访问，在管理员终端跑即可。

# 项目存档记忆（mouse-side-keys，2026-10-23 归档）

> 项目已完结存档，不再修改。本文件为唯一记忆文件：只保留最终结果与修改避坑要点。

## 一、成品概况（最终结果）

- 位置 `mouse-side-keys/`，共 4 个文件：
  - `MouseSideKeys.exe` —— PyInstaller onefile noconsole，8.5MB，免 Python，拷走即用；
    需提权时：右键属性 → 兼容性 → 以管理员身份运行。
  - `mouse_side_keys.py` —— 主程序源码（纯 ctypes，零第三方依赖）。
  - `start-admin.bat` / `stop-admin.bat` —— 仅作提权启动/停止入口。
- 功能：鼠标侧键切换 Windows 虚拟桌面（`WH_MOUSE_LL` 低级钩子 + `SendInput` 注入 Ctrl+Win+←/→）。
- 默认**对调方向**（后退键→右、前进键→左）：`--swap` default=True，`--no-swap` 回标准；
  用户直接跑 py 文件使用。
- 行为特性：控制台启动自动转 pythonw 后台（`--foreground` 保留前台调试）；单实例，
  新实例自动接管旧实例；停止用 `--stop`；无日志文件（刻意设计）。
- 打包命令：`pyinstaller --onefile --noconsole --name MouseSideKeys mouse_side_keys.py`
  （venv：`~/.workbuddy/binaries/python/envs/default`，pyinstaller 6.22.3）。

## 二、修改时需避坑的重点

### A. SendInput 注入（改按键相关代码必读，最易踩）
- 本机存在**两层过滤**，必须同时绕过：
  ① 按虚拟键码过滤 —— VK_LWIN(0x5B) 按虚拟键码单独发即被拒；
  ② 整批拒绝 —— 一次 SendInput 带多个键盘事件整体被拒。
- **唯一可靠组合（现为默认 preset `rawsplit`，勿破坏三要素）**：
  `KEYEVENTF_SCANCODE`（wVk=0）+ 逐个事件发送 + 事件间隔 ≥10ms。
- 「带不带扫描码」等价于「带不带 EXTENDEDKEY」；`keybd_event` 本机 err=5，**禁用**。
- SendInput 返回 0 且 GetLastError=0 不是"没出错"：可能是 UIPI 或上述两层过滤；
  先比对自身完整性级别 vs 前台窗口级别。
- 用户常开**管理员终端** → 脚本必须提权运行，否则静默失败（无任何报错）。

### B. 低级鼠标钩子
- 钩子回调内**禁止直接 SendInput**：必须 `PostThreadMessage` 投递到主循环再发。
- 消息循环必须阻塞式 `MsgWaitForMultipleObjectsEx`，不能 PeekMessage + sleep 轮询。
- 回调的 `WINFUNCTYPE` 引用必须持有防 GC。

### C. 单实例与后台化
- `taskkill /F` 返回 ≠ 进程已销毁：接管旧实例必须**轮询互斥体释放**
  （`_mutex_exists()` 用 OpenMutexW、句柄即开即关），不能固定 sleep。
- 首个实例的 CreateMutex 句柄必须保持打开（关掉互斥体即销毁、检测失效）。
- 自动 detach 必须在碰互斥体/PID 文件**之前**，否则新进程误判"已有实例"而退出。
- pythonw 下 `sys.stdout/stderr` 为 None，裸 print 会崩（main 开头统一重定向 devnull）。

### D. 批处理脚本
- **.bat 必须纯 ASCII**：cmd 永远按 GBK 解析（`chcp 65001` 只管显示），
  中文注释曾炸掉整个脚本并连锁误报 "Python was not found on PATH"。
- Python 改写 .bat 必须 `newline=''` 或二进制，防 CRLF→LF 破坏括号块/goto。
- `enabledelayedexpansion` 下 echo 内容不要含 `!`（会被吞，用 `[X]` 代替）。
- 提权用「中间 bat + `Start-Process -Verb RunAs`」，别在 PowerShell 单引号里嵌套 `\"`。

### E. 重新打包 exe
- frozen 模式跳过自动 detach（noconsole 本身无窗口）；stdout 为 None 时先
  `AttachConsole(ATTACH_PARENT_PROCESS)` 挂父控制台，失败再退 devnull。
- onefile 是**父子双进程**：PID 文件写的是子进程；taskkill 后父进程清理解压目录需数秒。
- 首轮构建失败多为 Defender 实时扫描的瞬时文件锁，重试即可。

### F. 已知问题与关键约束
- exe 双击运行时 `--test`/`--stop` 的输出不可见（AttachConsole 无父控制台），属正常现象。
- 测单实例功能前，先确认机器上没有真实用户的实例在跑（曾因此误判为打包 bug）。
- 回归测试必须断言具体取值（常量、方向），不能只断言"没报错"——
  XBUTTON1/2 曾写反仍"运行正常"，用户只感觉方向不对。
- 排查类开关（preset/参数）必须可自由组合，不能做成互斥枚举
  （`raw` 与 `split` 曾互斥，导致唯一有效组合 `rawsplit` 无法到达）。

### G. UIPI 权限拦截（2026-10-25 补，已修复）
- 现象：360安全卫士等**管理员权限窗口聚焦**时侧键失效；雷蛇宏正常。
  根因：UIPI —— Medium(0x2000) 进程的 SendInput 被前台 High(0x3000) 窗口拒绝
  （返回 0、GetLastError=0）；雷蛇走内核驱动注入（rzudd.sys）天然免疫。
- 修复：① py 加 `--elevate`（shell32.ShellExecuteW "runas" 重启自身，注意
  **ShellExecuteW 在 shell32 不在 user32**，搞错会 AttributeError）；
  ② `_flush` 被 UIPI 拒时每次会话弹一次 MessageBoxW 提醒（后台静默场景用户可感知）；
  ③ exe 用 pyinstaller 加 `--uac-admin` 打包，清单含 requireAdministrator，
  双击自动请求提权，360 聚焦也不再失效。
- 360Safe.exe 实测 0x3000(High)，脚本普通运行 0x2000(Medium)。

## 三、环境
- Windows 中文（GBK ANSI 代码页）；Python `D:\appData\appDevelopTool\Python\python.exe`，PATH 有 `py.exe`。
- 打包须用 `venv Scripts/python.exe -m PyInstaller`（venv 无根级 python.exe，
  Scripts/pyinstaller.exe 在 Git Bash 下静默 exit 1 不可用）。
