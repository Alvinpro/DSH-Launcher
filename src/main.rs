#![windows_subsystem = "windows"]

use std::{
    env,
    ffi::c_void,
    fs,
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::Deserialize;
use windows::core::{w, PCWSTR};
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WAIT_OBJECT_0, WPARAM},
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        },
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
        LibraryLoader::GetModuleHandleW,
        Threading::{
            GetProcessId, OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
        },
    },
    UI::{
        Shell::{
            ShellExecuteExW, Shell_NotifyIconW, NOTIFYICONDATAW, NIF_ICON, NIF_MESSAGE, NIF_TIP,
            NIM_ADD, NIM_DELETE, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
        },
        WindowsAndMessaging::{
            AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
            DispatchMessageW, GetCursorPos, LoadIconW, MessageBoxW, PeekMessageW,
            RegisterClassW, SetForegroundWindow, TrackPopupMenu, TranslateMessage, MF_STRING, MSG, PM_REMOVE, SW_SHOWNORMAL, TPM_BOTTOMALIGN, TPM_LEFTALIGN,
            TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WINDOW_STYLE, WNDCLASSW, WM_COMMAND, WM_LBUTTONUP,
            WM_QUIT, WM_RBUTTONUP, MB_ICONERROR, MB_OK,
        },
    },
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DEFAULT_PACKAGE: &str = "dsh";
const DEFAULT_URL: &str = "http://127.0.0.1:3080";
const DEFAULT_TIMEOUT_SECS: u64 = 30;
const WM_TRAY: u32 = 0x8000 + 1;
const ID_EXIT: u32 = 1001;
const TRAY_TIP: &str = "DSH 启动器(右键退出服务)";

static CTRLC_FLAG: AtomicBool = AtomicBool::new(false);
static TRAY_EXIT: AtomicBool = AtomicBool::new(false);
static TRAY_HWND: AtomicIsize = AtomicIsize::new(0);
static JOB_HANDLE: AtomicUsize = AtomicUsize::new(0);

macro_rules! log {
    ($($arg:tt)*) => { dsh_log(&format!($($arg)*)) };
}

fn dsh_log(msg: &str) {
    if let Ok(dir) = exe_dir() {
        if let Ok(mut f) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("dsh-launcher.log"))
        {
            let _ = writeln!(f, "[{}] {}", hms_now(), msg);
        }
    }
}

fn hms_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (h, m, s) = (secs / 3600 % 24, secs / 60 % 60, secs % 60);
    format!("{:02}:{:02}:{:02}", h, m, s)
}

fn err_popup(msg: &str) {
    let title: Vec<u16> = "DSH 启动器错误".encode_utf16().chain(std::iter::once(0)).collect();
    let text: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();
    let _ = unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONERROR,
        )
    };
}

#[derive(Deserialize)]
#[serde(default)]
struct Config {
    dsh_path: Option<String>,
    url: Option<String>,
    port: Option<u16>,
    timeout_secs: Option<u64>,
    package: Option<String>,
    #[serde(default = "default_args")]
    args: Vec<String>,
}

fn default_args() -> Vec<String> {
    vec!["web".to_string()]
}

impl Default for Config {
    fn default() -> Self {
        Config {
            dsh_path: None,
            url: None,
            port: None,
            timeout_secs: None,
            package: None,
            args: default_args(),
        }
    }
}

struct Entry {
    via_cmd: bool,
    program: String,
    args: Vec<String>,
    skip_version: bool,
    slow_start: bool,
}

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let mut cfg = load_config();
    let cli_args: Vec<String> = env::args().skip(1).collect();
    let check = cli_args.iter().any(|a| a == "--check");
    match apply_cli(&mut cfg, &cli_args) {
        Ok(true) => {
            show_help();
            return 0;
        }
        Ok(false) => {}
        Err(msg) => {
            log!("[ERROR] 命令行参数错误: {}", msg);
            err_popup(&msg);
            return 1;
        }
    }
    let url = cfg.url.clone().unwrap_or_else(|| DEFAULT_URL.to_string());
    let port = cfg.port.unwrap_or_else(|| parse_port(&url));
    let timeout = Duration::from_secs(cfg.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
    let pkg = cfg.package.clone().unwrap_or_else(|| DEFAULT_PACKAGE.to_string());

    log!("[INFO] DSH-Launcher v0.1.0");
    log!("[INFO] 目标地址: {}", url);
    log!("[INFO] 探测端口: {}", port);

    if check {
        return cmd_check(&cfg, &pkg);
    }

    if let Err(e) = ctrlc::set_handler(|| {
        CTRLC_FLAG.store(true, Ordering::SeqCst);
        kill_job_if_any();
    }) {
        log!("[WARN] 注册 Ctrl+C 处理器失败: {}", e);
    }

    if http_ok(port) {
        log!("[INFO] 服务已在运行,直接打开浏览器");
        open_browser_watch(&url);
        return 0;
    }
    if tcp_open(port) {
        log!("[WARN] 端口 {} 已被占用但未响应 HTTP,启动 dsh 后需自行确认", port);
    }

    let mut entry = match resolve_entry(&cfg, &pkg) {
        Ok(e) => e,
        Err(msg) => {
            log!("[ERROR] {}", msg);
            err_popup(&format!("无法启动 dsh:\n\n{}", msg));
            return 5;
        }
    };
    entry.args.extend(cfg.args.clone());
    print_entry(&entry);

    let mut child = match spawn_dsh(&entry) {
        Ok(c) => c,
        Err(e) => {
            log!("[ERROR] 启动 dsh 失败: {}", e);
            err_popup(&format!("启动 dsh 失败:\n\n{}", e));
            return 1;
        }
    };

    let job = create_job().ok();
    if let Some(j) = job {
        match assign_to_job(j, &child) {
            Ok(()) => {
                JOB_HANDLE.store(j.0 as usize, Ordering::SeqCst);
                log!("[INFO] 已绑定 JobObject,启动器退出将自动清理 dsh 进程树");
            }
            Err(e) => log!("[WARN] JobObject 绑定失败(进程退出时可能残留): {}", e),
        }
    } else {
        log!("[WARN] JobObject 创建失败(进程退出时可能残留)");
    }

    let wait_timeout = if entry.slow_start {
        timeout.max(Duration::from_secs(60))
    } else {
        timeout
    };
    match wait_ready(port, wait_timeout, &mut child) {
        ProbeOutcome::Ready => {}
        ProbeOutcome::ChildExited(status) => {
            let msg = format!(
                "dsh 进程提前退出: {}\n\n若为参数/缺参错误,请检查 config.json 的 args 字段\n(DSH-Web 示例: [\"web\", \"--port\", \"8899\"])",
                status
            );
            log!("[ERROR] {}", msg);
            kill_job_if_any();
            err_popup(&msg);
            return 4;
        }
        ProbeOutcome::Timeout { ever_tcp } => {
            let msg = format!("等待服务就绪超时({}s)\n\n详情见 dsh-launcher.log", wait_timeout.as_secs());
            log!("[ERROR] {}", msg);
            kill_job_if_any();
            let _ = child.kill();
            err_popup(&msg);
            return if ever_tcp { 2 } else { 3 };
        }
        ProbeOutcome::Interrupted => {
            log!("\n[INFO] 正在关闭 DSH 服务...");
            kill_job_if_any();
            let _ = child.kill();
            return 0;
        }
    }

    log!("[INFO] 服务已就绪");
    let bw = open_browser_watch(&url);
    let mut watching = matches!(&bw, Some(b) if b.fresh);
    if watching {
        log!("[INFO] 关闭浏览器将自动关闭 DSH 服务;或通过托盘图标退出");
    } else {
        log!("[INFO] 浏览器已在前台运行(共享实例),通过托盘图标退出 DSH 服务");
    }

    if let Err(e) = tray_init() {
        log!("[WARN] 托盘图标创建失败(可通过任务管理器结束 dsh): {}", e);
    }

    loop {
        pump_messages();
        if TRAY_EXIT.load(Ordering::SeqCst) {
            log!("[INFO] 托盘:正在关闭 DSH 服务...");
            kill_job_if_any();
            let _ = child.kill();
            return 0;
        }
        if CTRLC_FLAG.load(Ordering::SeqCst) {
            log!("\n[INFO] 正在关闭 DSH 服务...");
            kill_job_if_any();
            let _ = child.kill();
            return 0;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                log!("[ERROR] dsh 进程意外退出: {}", status);
                kill_job_if_any();
                return 4;
            }
            Ok(None) => {}
            Err(e) => log!("[WARN] 读取 dsh 进程状态失败: {}", e),
        }
        if watching {
            if let Some(b) = &bw {
                let rc = unsafe { WaitForSingleObject(b.hprocess, 500) };
                if rc == WAIT_OBJECT_0 {
                    if b.opened_at.elapsed() < Duration::from_secs(3) && any_pre_alive(&b.pre_pids) {
                        log!(
                            "[WARN] 浏览器已存在实例(新进程为握手代理),改为常驻模式;通过托盘图标退出 DSH 服务"
                        );
                        watching = false;
                    } else {
                        log!("[INFO] 浏览器已关闭,正在关闭 DSH 服务...");
                        kill_job_if_any();
                        let _ = child.kill();
                        return 0;
                    }
                }
            }
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn tray_init() -> Result<(), String> {
    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None)
            .map_err(|e| format!("GetModuleHandleW: {}", e))?
            .into();
        let class = w!("DshLauncherTrayWnd");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(tray_wndproc),
            hInstance: hinst,
            lpszClassName: class,
            ..std::mem::zeroed()
        };
        let _ = RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("DSH Launcher"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            hinst,
            None,
        )
        .map_err(|e| format!("CreateWindowExW: {}", e))?;
        TRAY_HWND.store(hwnd.0 as isize, Ordering::SeqCst);

        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = match LoadIconW(hinst, PCWSTR(1 as *const u16)) {
            Ok(icon) => icon,
            Err(_) => LoadIconW(None, PCWSTR(32512 as *const u16)).unwrap_or_default(),
        };
        for (i, c) in TRAY_TIP.encode_utf16().chain(std::iter::once(0)).take(128).enumerate() {
            nid.szTip[i] = c;
        }
        if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
            return Err("Shell_NotifyIconW 失败".to_string());
        }
    }
    Ok(())
}

fn tray_cleanup() {
    unsafe {
        let hwnd = HWND(TRAY_HWND.load(Ordering::SeqCst) as *mut c_void);
        if hwnd.0.is_null() {
            return;
        }
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

unsafe extern "system" fn tray_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_TRAY => {
            let lp = lparam.0 as u32;
            if lp == WM_RBUTTONUP || lp == WM_LBUTTONUP {
                unsafe { show_tray_menu(hwnd) };
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            if wparam.0 as u32 & 0xFFFF == ID_EXIT {
                TRAY_EXIT.store(true, Ordering::SeqCst);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

unsafe fn show_tray_menu(hwnd: HWND) {
    unsafe {
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        if let Ok(menu) = CreatePopupMenu() {
            let _ = AppendMenuW(menu, MF_STRING, ID_EXIT as usize, w!("退出 DSH 服务"));
            let _ = SetForegroundWindow(hwnd);
            let _ = TrackPopupMenu(
                menu,
                TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RIGHTBUTTON,
                pt.x,
                pt.y,
                0,
                hwnd,
                None,
            );
            let _ = DestroyMenu(menu);
        }
    }
}

fn pump_messages() {
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_QUIT {
                TRAY_EXIT.store(true, Ordering::SeqCst);
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn apply_cli(cfg: &mut Config, args: &[String]) -> Result<bool, String> {
    let mut help = false;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "--check" => {}
            "--help" | "-h" => help = true,
            "--url" | "--port" | "--timeout" | "--package" | "--args" | "--dsh-path" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or_else(|| format!("参数 {} 缺少值", a))?;
                match a.as_str() {
                    "--url" => cfg.url = Some(v.clone()),
                    "--port" => {
                        cfg.port = Some(
                            v.parse()
                                .map_err(|_| format!("--port 非法数值: {}", v))?,
                        )
                    }
                    "--timeout" => {
                        cfg.timeout_secs = Some(
                            v.parse()
                                .map_err(|_| format!("--timeout 非法数值: {}", v))?,
                        )
                    }
                    "--package" => cfg.package = Some(v.clone()),
                    "--args" => {
                        cfg.args = v.split_whitespace().map(|s| s.to_string()).collect()
                    }
                    "--dsh-path" => cfg.dsh_path = Some(v.clone()),
                    _ => unreachable!(),
                }
            }
            _ => {
                return Err(format!("未知参数: {}\n\n可用 -h/--help 查看用法", a));
            }
        }
        i += 1;
    }
    Ok(help)
}

fn show_help() {
    let text = "DSH-Launcher v0.1.0 — DeepSeek Harness (dsh) Web 启动器

用法: dsh-launcher.exe [选项]

选项:
  --check           仅验证 dsh 命令解析链,不实际启动
  --url <URL>       服务地址(默认 http://127.0.0.1:3080)
  --port <N>        就绪探测端口(默认取 url 端口)
  --timeout <秒>    就绪等待超时(默认 30)
  --package <名>    npm 包名,npx 拉取用(默认 dsh)
  --args <\"a b c\">  dsh 启动参数(默认 \"web\";显式设置会覆盖 config.json)
  --dsh-path <路径> 显式指定 dsh 入口,跳过解析链
  -h, --help        显示本帮助

配置优先级: 命令行 > config.json > 内置默认
无需 config.json 即可直接运行。";
    let title: Vec<u16> = "DSH 启动器".encode_utf16().chain(std::iter::once(0)).collect();
    let msg: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let _ = unsafe {
        MessageBoxW(None, PCWSTR(msg.as_ptr()), PCWSTR(title.as_ptr()), MB_OK)
    };
}

fn cmd_check(cfg: &Config, pkg: &str) -> i32 {
    match resolve_entry(cfg, pkg) {
        Ok(e) => {
            print_entry(&e);
            log!("[INFO] dsh 命令解析成功,可直接启动");
            0
        }
        Err(msg) => {
            log!("[ERROR] {}", msg);
            5
        }
    }
}

fn print_entry(e: &Entry) {
    if e.via_cmd {
        log!("[INFO] 使用入口: cmd /C \"{}\" {}", e.program, e.args.join(" "));
    } else {
        log!("[INFO] 使用入口: {} {}", e.program, e.args.join(" "));
    }
}

fn resolve_entry(cfg: &Config, pkg: &str) -> Result<Entry, String> {
    if let Some(p) = &cfg.dsh_path {
        let path = PathBuf::from(p);
        if path.is_file() {
            let e = entry_from_file(&path)?;
            if version_ok(&e) {
                log!("[INFO] 命中配置路径: {}", path.display());
                return Ok(e);
            }
            log!("[WARN] 配置路径 {} 版本校验失败,继续解析", path.display());
        } else {
            return Err(format!("配置的 dsh_path 不存在: {}", p));
        }
    }

    if let Ok(dir) = exe_dir() {
        let vendor = dir.join("vendor").join("dsh");
        if vendor.is_dir() {
            for cand in ["dsh.exe", "dsh.cmd", "bin\\dsh.exe", "bin\\dsh.cmd"] {
                let f = vendor.join(cand);
                if f.is_file() {
                    let e = entry_from_file(&f)?;
                    if version_ok(&e) {
                        log!("[INFO] 命中离线 vendor 包: {}", f.display());
                        return Ok(e);
                    }
                }
            }
            let mainjs = vendor.join("main.js");
            if mainjs.is_file() {
                let e = Entry {
                    via_cmd: false,
                    program: "node".to_string(),
                    args: vec![mainjs.to_string_lossy().into_owned()],
                    skip_version: false,
                    slow_start: false,
                };
                if version_ok(&e) {
                    log!("[INFO] 命中离线 vendor 包(node): {}", mainjs.display());
                    return Ok(e);
                }
            }
        }
    }

    if let Some(p) = where_exe(pkg) {
        let e = entry_from_file(&p)?;
        if version_ok(&e) {
            log!("[INFO] 命中 PATH 中的 {}: {}", pkg, p.display());
            return Ok(e);
        }
        log!("[WARN] PATH 中的 {} 版本校验失败,继续解析", p.display());
    }

    if !npm_ok() {
        return Err(format!(
            "未找到 {} 命令,且本机无 npm/node,无法自动安装。\n  排查: 1) 安装 Node.js; 2) 将离线包放入 vendor\\dsh; 3) 或配置 dsh_path",
            pkg
        ));
    }

    let e = Entry {
        via_cmd: true,
        program: "npx".to_string(),
        args: vec!["--yes".to_string(), pkg.to_string()],
        skip_version: true,
        slow_start: true,
    };
    log!("[INFO] PATH 中无 {},改用 npx 自动拉取: npx --yes {}", pkg, pkg);
    log!("[INFO] 如需长期保留,可手动执行: npm install -g {}", pkg);
    Ok(e)
}

fn entry_from_file(p: &Path) -> Result<Entry, String> {
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    if ext.is_empty() {
        for e in ["cmd", "bat", "exe"] {
            let sibling = p.with_extension(e);
            if sibling.is_file() {
                return entry_from_file(&sibling);
            }
        }
    }
    let s = p.to_string_lossy().into_owned();
    match ext.as_str() {
        "cmd" | "bat" => Ok(Entry {
            via_cmd: true,
            program: s,
            args: vec![],
            skip_version: false,
            slow_start: false,
        }),
        "exe" | "" => Ok(Entry {
            via_cmd: false,
            program: s,
            args: vec![],
            skip_version: false,
            slow_start: false,
        }),
        _ => Err(format!("不支持的入口文件类型: {}", p.display())),
    }
}

fn version_ok(e: &Entry) -> bool {
    if e.skip_version {
        return true;
    }
    if e.via_cmd {
        Command::new("cmd.exe")
            .arg("/C")
            .arg(&e.program)
            .args(&e.args)
            .arg("--version")
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    } else {
        Command::new(&e.program)
            .args(&e.args)
            .arg("--version")
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

fn spawn_dsh(e: &Entry) -> std::io::Result<Child> {
    let mut cmd = if e.via_cmd {
        let mut c = Command::new("cmd.exe");
        c.arg("/C").arg(&e.program).args(&e.args);
        c
    } else {
        let mut c = Command::new(&e.program);
        c.args(&e.args);
        c
    };
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd.spawn()
}

fn where_exe(pkg: &str) -> Option<PathBuf> {
    let out = Command::new("where.exe")
        .arg(pkg)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut fallback: Option<PathBuf> = None;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let p = PathBuf::from(line.trim());
        if !p.is_file() {
            continue;
        }
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        match ext.as_str() {
            "cmd" | "bat" | "exe" => return Some(p),
            _ if fallback.is_none() => fallback = Some(p),
            _ => {}
        }
    }
    fallback
}

fn npm_ok() -> bool {
    Command::new("cmd.exe")
        .arg("/C")
        .arg("npm --version")
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn create_job() -> Result<HANDLE, String> {
    unsafe {
        let job = CreateJobObjectW(None, PCWSTR::null())
            .map_err(|e| format!("CreateJobObjectW 失败: {}", e))?;
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .map_err(|e| format!("SetInformationJobObject 失败: {}", e))?;
        Ok(job)
    }
}

fn assign_to_job(job: HANDLE, child: &Child) -> Result<(), String> {
    use std::os::windows::io::AsRawHandle;
    let h = child.as_raw_handle() as *mut c_void;
    unsafe {
        AssignProcessToJobObject(job, HANDLE(h)).map_err(|e| {
            format!("AssignProcessToJobObject 失败(错误码 {}): {}", e.code(), e)
        })
    }
}

fn kill_job_if_any() {
    let h = JOB_HANDLE.load(Ordering::SeqCst);
    if h != 0 {
        let _ = unsafe { TerminateJobObject(HANDLE(h as *mut c_void), 0) };
    }
    tray_cleanup();
}

enum ProbeOutcome {
    Ready,
    ChildExited(std::process::ExitStatus),
    Timeout { ever_tcp: bool },
    Interrupted,
}

fn wait_ready(port: u16, timeout: Duration, child: &mut Child) -> ProbeOutcome {
    let t0 = Instant::now();
    let mut tries = 0u32;
    let mut last_report = Duration::ZERO;
    let mut ever_tcp = false;
    loop {
        if http_ok(port) {
            return ProbeOutcome::Ready;
        }
        if tcp_open(port) {
            ever_tcp = true;
        }
        if CTRLC_FLAG.load(Ordering::SeqCst) {
            return ProbeOutcome::Interrupted;
        }
        match child.try_wait() {
            Ok(Some(status)) => return ProbeOutcome::ChildExited(status),
            Ok(None) => {}
            Err(e) => log!("[WARN] 读取 dsh 进程状态失败: {}", e),
        }
        let elapsed = t0.elapsed();
        if elapsed >= timeout {
            return ProbeOutcome::Timeout { ever_tcp };
        }
        if elapsed - last_report >= Duration::from_secs(5) {
            log!(
                "[INFO] 等待服务就绪... ({}/{})",
                elapsed.as_secs(),
                timeout.as_secs()
            );
            last_report = elapsed;
        }
        let delay = if tries < 5 {
            Duration::from_millis(500)
        } else {
            Duration::from_secs(1)
        };
        tries += 1;
        thread::sleep(delay);
    }
}

fn tcp_open(port: u16) -> bool {
    let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port);
    TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
}

fn http_ok(port: u16) -> bool {
    let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port);
    let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_millis(1000)));
    let req = format!(
        "GET / HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        port
    );
    if s.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 256];
    match s.read(&mut buf) {
        Ok(n) if n > 0 => buf[..n].windows(5).any(|w| w == b"HTTP/"),
        _ => false,
    }
}

struct BrowserInfo {
    hprocess: HANDLE,
    fresh: bool,
    pre_pids: Vec<u32>,
    opened_at: Instant,
}

fn open_browser_watch(url: &str) -> Option<BrowserInfo> {
    let pre_pids = browser_pids();
    let url_w: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    let mut sei: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    sei.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    sei.fMask = SEE_MASK_NOCLOSEPROCESS;
    sei.nShow = SW_SHOWNORMAL.0;
    sei.lpFile = PCWSTR(url_w.as_ptr());
    if unsafe { ShellExecuteExW(&mut sei) }.is_err() || sei.hProcess.0.is_null() {
        log!(
            "[WARN] 打开浏览器失败,请手动访问: {} (启动器将常驻,关闭窗口或按 Ctrl+C 关闭 DSH 服务)",
            url
        );
        return None;
    }
    let pid = unsafe { GetProcessId(sei.hProcess) };
    let fresh = pid != 0 && !pre_pids.contains(&pid);
    log!("[INFO] 已在浏览器打开: {}", url);
    Some(BrowserInfo {
        hprocess: sei.hProcess,
        fresh,
        pre_pids,
        opened_at: Instant::now(),
    })
}

fn any_pre_alive(pids: &[u32]) -> bool {
    for &pid in pids {
        let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) };
        if h.is_ok() {
            return true;
        }
    }
    false
}

fn browser_pids() -> Vec<u32> {
    const NAMES: [&str; 11] = [
        "msedge.exe", "chrome.exe", "firefox.exe", "opera.exe", "brave.exe", "vivaldi.exe",
        "360se.exe", "360chrome.exe", "qqbrowser.exe", "sogouexplorer.exe", "iexplore.exe",
    ];
    let mut pids = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return pids;
        };
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut next = Process32FirstW(snap, &mut entry);
        while next.is_ok() {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase();
            if NAMES.contains(&name.as_str()) {
                pids.push(entry.th32ProcessID);
            }
            next = Process32NextW(snap, &mut entry);
        }
        let _ = CloseHandle(snap);
    }
    pids
}

fn load_config() -> Config {
    let Ok(dir) = exe_dir() else {
        return Config::default();
    };
    let p = dir.join("config.json");
    match fs::read_to_string(&p) {
        Ok(s) => match serde_json::from_str(&s) {
            Ok(c) => c,
            Err(e) => {
                log!("[WARN] config.json 解析失败,使用默认配置: {}", e);
                Config::default()
            }
        },
        Err(_) => Config::default(),
    }
}

fn exe_dir() -> Result<PathBuf, ()> {
    env::current_exe()
        .map(|p| p.parent().map(|x| x.to_path_buf()).unwrap_or_default())
        .map_err(|_| ())
}

fn parse_port(url: &str) -> u16 {
    url.split('/')
        .nth(2)
        .and_then(|h| h.rsplit(':').next())
        .and_then(|p| p.parse().ok())
        .unwrap_or(3080)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_port_default() {
        assert_eq!(parse_port("http://127.0.0.1:8080"), 8080);
        assert_eq!(parse_port("http://127.0.0.1:9090/web"), 9090);
        assert_eq!(parse_port("http://127.0.0.1"), 3080);
    }

    fn cli(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn default_args_is_web() {
        assert_eq!(Config::default().args, vec!["web"]);
    }

    #[test]
    fn apply_cli_overrides() {
        let mut c = Config::default();
        apply_cli(&mut c, &cli(&["--port", "8899", "--args", "web --port 8899", "--timeout", "60", "--url", "http://127.0.0.1:9000"])).unwrap();
        assert_eq!(c.port, Some(8899));
        assert_eq!(c.args, vec!["web", "--port", "8899"]);
        assert_eq!(c.timeout_secs, Some(60));
        assert_eq!(c.url.as_deref(), Some("http://127.0.0.1:9000"));
    }

    #[test]
    fn apply_cli_unknown_flag() {
        let mut c = Config::default();
        assert!(apply_cli(&mut c, &cli(&["--nope"])).is_err());
    }

    #[test]
    fn apply_cli_missing_value() {
        let mut c = Config::default();
        assert!(apply_cli(&mut c, &cli(&["--port"])).is_err());
        assert!(apply_cli(&mut c, &cli(&["--port", "abc"])).is_err());
    }

    #[test]
    fn apply_cli_ignores_check_and_help() {
        let mut c = Config::default();
        assert!(!apply_cli(&mut c, &cli(&["--check"])).unwrap());
        assert!(apply_cli(&mut c, &cli(&["-h"])).unwrap());
        assert!(apply_cli(&mut c, &cli(&["--help"])).unwrap());
    }
}
