// dsh-launcher: dsh (DeepSeek Harness) 系统托盘管理器。
//
// 架构（决策 1/2/3/4）：
//  - dsh web 进程由 launchd (LaunchAgent) 托管：KeepAlive 崩溃自愈，RunAtLoad 默认关（自启默认关）。
//  - 本应用是纯托盘控制台（无窗口）：状态轮询 + launchctl 控制 + 菜单。
//  - 控制面插件 dsh-control 通过 `dsh web --patch <cordis.patch.yml>` 挂载，
//    暴露 127.0.0.1:3399 状态/优雅停机端点（零侵入 profile）。

use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    path::PathBuf,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    AppHandle, Manager, WebviewUrl, WebviewWindowBuilder,
};

// ── 常量 ──────────────────────────────────────────────────────────────

const WEB_URL: &str = "http://127.0.0.1:3080";
const WEB_PORT: u16 = 3080;
const CONTROL_PORT: u16 = 3399; // dsh-control 插件端口
const LAUNCHD_LABEL: &str = "com.dsh-launcher.web";
const STATUS_POLL_MS: u64 = 2000;

static AUTOSTART: AtomicBool = AtomicBool::new(false);

/// 打开 dsh 界面方式：system=系统浏览器；builtin=托盘内置浏览器。
#[derive(Clone, Copy, PartialEq, Eq)]
enum OpenMode {
    System,
    Builtin,
}

impl OpenMode {
    fn as_str(&self) -> &'static str {
        match self {
            OpenMode::System => "system",
            OpenMode::Builtin => "builtin",
        }
    }
}

fn open_mode_from_str(s: &str) -> OpenMode {
    match s {
        "builtin" => OpenMode::Builtin,
        _ => OpenMode::System,
    }
}

fn config_path() -> PathBuf {
    home_dir().join("Library/Application Support/dsh-launcher/config.json")
}

fn load_open_mode() -> OpenMode {
    let raw = fs::read_to_string(config_path()).unwrap_or_default();
    serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .and_then(|v| v.get("open_mode").and_then(|m| m.as_str()).map(open_mode_from_str))
        .unwrap_or(OpenMode::System)
}

fn save_open_mode(mode: OpenMode) {
    let value = serde_json::json!({ "open_mode": mode.as_str() });
    if let Some(dir) = config_path().parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Err(e) = fs::write(config_path(), serde_json::to_string_pretty(&value).unwrap_or_default()) {
        log_tray(&format!("save_open_mode: 写入失败 {e}"));
    } else {
        log_tray(&format!("save_open_mode: open_mode={}", mode.as_str()));
    }
}

static OPEN_MODE: AtomicBool = AtomicBool::new(false); // false=system, true=builtin

/// 托盘菜单句柄（存进 Tauri state，供后台状态轮询线程更新）。
struct AppMenu(Menu<tauri::Wry>);

// ── 路径工具 ──────────────────────────────────────────────────────────

fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

fn plist_path() -> PathBuf {
    home_dir().join("Library/LaunchAgents").join(format!("{LAUNCHD_LABEL}.plist"))
}

fn log_path() -> PathBuf {
    home_dir().join("Library/Logs/dsh-web.log")
}

fn tray_log_path() -> PathBuf {
    home_dir().join("Library/Logs/dsh-launcher.log")
}

/// 托盘自身日志：追加写入 ~/Library/Logs/dsh-launcher.log（不依赖 stderr，GUI 启动可见）。
fn log_tray(msg: &str) {
    let path = tray_log_path();
    let line = format!("{} [dsh-launcher] {}\n", chrono_now(), msg);
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let mut f = match fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("dsh-launcher: 无法写托盘日志 {}: {e}", path.display());
            return;
        }
    };
    let _ = f.write_all(line.as_bytes());
    let _ = f.flush();
}

/// 当前时间戳，用于日志行。
fn chrono_now() -> String {
    // 避免引入 chrono 依赖：用 date 命令取本地时间。
    if let Ok(out) = Command::new("date").arg("+%Y-%m-%d %H:%M:%S").output() {
        if out.status.success() {
            return String::from_utf8_lossy(&out.stdout).trim().to_string();
        }
    }
    "1970-01-01 00:00:00".to_string()
}

/// 记录 launchctl 命令及其 stdout/stderr（截断），便于排查启动链路。
fn launchctl_logged(args: &[&str]) -> std::io::Result<std::process::Output> {
    log_tray(&format!("launchctl {}", args.join(" ")));
    let out = Command::new("launchctl").args(args).output()?;
    let code = out.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    log_tray(&format!("launchctl {} -> exit {code} stdout={stdout:?} stderr={stderr:?}", args.join(" ")));
    Ok(out)
}

fn uid() -> String {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "0".into())
}

fn gui_target() -> String {
    format!("gui/{}", uid())
}

fn service_target() -> String {
    format!("gui/{}/{}", uid(), LAUNCHD_LABEL)
}

// ── dsh 可执行解析（plist 用绝对路径，不依赖 launchd 的 PATH） ────────

/// 从登录 shell 解析可执行文件绝对路径。
/// GUI 启动的 app 环境 PATH 只有系统默认值（无 nvm/Homebrew），
/// 而 launchctl 的 PATH 也不含用户 shell 配置；用 `zsh -lc` 拿到真实 PATH。
fn which_from_login_shell(name: &str) -> Option<PathBuf> {
    for shell in ["zsh", "bash", "sh"] {
        let out = Command::new(shell).arg("-lc").arg(format!("command -v {name}")).output().ok()?;
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                let p = PathBuf::from(&s);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    None
}

/// 常见安装路径兜底（登录 shell 可能因慢/异常失败，或 PATH 不含这些位置）。
fn which_from_common_paths(name: &str) -> Option<PathBuf> {
    let home = home_dir();
    let candidates = [
        home.join(".nvm/versions/node").join("current/bin").join(name),
        home.join(".local/bin").join(name),
        PathBuf::from("/opt/homebrew/bin").join(name),
        PathBuf::from("/usr/local/bin").join(name),
    ];
    // nvm 下可能有多个版本目录：取版本号最大的那个（与 node 同目录）。
    let nvm_root = home.join(".nvm/versions/node");
    if let Ok(entries) = fs::read_dir(&nvm_root) {
        let mut versions: Vec<_> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().join(name).is_file())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        versions.sort();
        if let Some(v) = versions.last() {
            let p = nvm_root.join(v).join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

fn which(name: &str) -> Option<PathBuf> {
    which_from_login_shell(name)
        .or_else(|| which_from_common_paths(name))
}

/// node 与 dsh 必须来自同一目录（nvm 版本一致性）：若 dsh 由常见路径解析出，
/// 则 node 优先取同目录；否则退回登录 shell/常见路径解析 node。
fn node_for_dsh(dsh: &PathBuf) -> Option<PathBuf> {
    let same_dir = dsh.parent().map(|dir| dir.join("node")).filter(|p| p.is_file());
    same_dir.or_else(|| which("node"))
}

/// 控制面插件目录。
/// - debug 构建（开发）：优先源码目录（编译期路径，release 二进制不含）。
/// - release 构建（发布）：一律从 .app 资源目录解析。
fn control_dir(app: &AppHandle) -> PathBuf {
    #[cfg(debug_assertions)]
    {
        let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins/dsh-control");
        if dev.join("lib/index.js").exists() {
            return dev;
        }
    }
    app.path().resource_dir().unwrap_or_default().join("dsh-control")
}

/// 控制面 patch 路径：动态生成到 ~/Library/Application Support/dsh-launcher/，
/// 内容引用实际插件入口（bundle 内或源码目录），解决打包后绝对路径失效问题。
fn control_patch_path(app: &AppHandle) -> PathBuf {
    let dir = home_dir().join("Library/Application Support/dsh-launcher");
    let _ = fs::create_dir_all(&dir);
    let patch = dir.join("control.patch.yml");
    let plugin_index = control_dir(app).join("lib/index.js");
    log_tray(&format!("control_patch_path: plugin_index={} patch={}", plugin_index.display(), patch.display()));
    let content = format!(
        "# 由 dsh-launcher 自动生成，请勿手改。\n- insert:\n    - id: dsh-control\n      name: '{}'\n",
        plugin_index.to_string_lossy()
    );
    if let Err(e) = fs::write(&patch, content) {
        log_tray(&format!("control_patch_path: 写入失败 {e}"));
    }
    patch
}

/// [node, dsh_bin, --profile, web, --patch, <控制面 patch 路径>]
/// node/dsh 一律运行时解析（PATH），解析失败返回错误——不写死任何安装路径。
fn dsh_program(app: &AppHandle) -> Result<Vec<String>, String> {
    let dsh = which("dsh").ok_or_else(|| "dsh 不在 PATH，无法生成 LaunchAgent（已尝试登录 shell 与常见安装路径）".to_string())?;
    let node = node_for_dsh(&dsh).ok_or_else(|| "node 不在 PATH，无法生成 LaunchAgent（已尝试登录 shell 与常见安装路径）".to_string())?;
    let patch = control_patch_path(app);
    let program = vec![
        node.to_string_lossy().into_owned(),
        dsh.to_string_lossy().into_owned(),
        "--profile".into(),
        "web".into(),
        "--patch".into(),
        patch.to_string_lossy().into_owned(),
    ];
    log_tray(&format!("dsh_program: node={} dsh={} patch={}", node.display(), dsh.display(), patch.display()));
    Ok(program)
}

// ── plist 生成（LaunchAgent） ─────────────────────────────────────────

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn plist_xml(app: &AppHandle, run_at_load: bool, keep_alive: bool) -> Result<String, String> {
    let program = dsh_program(app)?;
    let args = program
        .iter()
        .map(|a| format!("    <string>{}</string>", xml_escape(a)))
        .collect::<Vec<_>>()
        .join("
");
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{args}
  </array>
  <key>KeepAlive</key>
  <{keep_alive}/>
  <key>RunAtLoad</key>
  <{run_at_load}/>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
        label = LAUNCHD_LABEL,
        args = args,
        keep_alive = if keep_alive { "true" } else { "false" },
        run_at_load = if run_at_load { "true" } else { "false" },
        log = xml_escape(&log_path().to_string_lossy()),
    ))
}

fn ensure_plist(app: &AppHandle) -> Result<(), String> {
    let path = plist_path();
    log_tray(&format!("ensure_plist: path={} exists={}", path.display(), path.exists()));
    if !path.exists() {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let xml = plist_xml(app, AUTOSTART.load(Ordering::Relaxed), true)?;
        if let Err(e) = fs::write(&path, &xml) {
            log_tray(&format!("ensure_plist: 写入失败 {e}"));
            return Err(e.to_string());
        }
        log_tray(&format!("ensure_plist: 已写入 plist ({})", xml.len()));
    }
    Ok(())
}

fn write_plist(app: &AppHandle, run_at_load: bool) -> Result<(), String> {
    let path = plist_path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let xml = plist_xml(app, run_at_load, true)?;
    if let Err(e) = fs::write(&path, &xml) {
        log_tray(&format!("write_plist: 写入失败 {e}"));
        return Err(e.to_string());
    }
    log_tray(&format!("write_plist: 已写入 plist run_at_load={run_at_load} ({})", xml.len()));
    Ok(())
}

/// 停止时把现有 plist 的 KeepAlive 改为 false（纯文本替换，不重新解析 PATH）。
fn write_plist_keepalive_off() -> Result<(), String> {
    let path = plist_path();
    let xml = fs::read_to_string(&path).map_err(|e| format!("读取 plist 失败: {e}"))?;
    let marker = "<key>KeepAlive</key>";
    let pos = xml.find(marker).ok_or_else(|| "plist 缺少 KeepAlive 字段".to_string())?;
    let rest = &xml[pos + marker.len()..];
    let value_start = rest.find('<').ok_or_else(|| "plist KeepAlive 值缺失".to_string())?;
    let value_end = rest[value_start..].find('>').map(|i| value_start + i + 1)
        .ok_or_else(|| "plist KeepAlive 值格式异常".to_string())?;
    let new_xml = format!("{}{}<false/>{}", &xml[..pos + marker.len()], "", &rest[value_end..]);
    fs::write(&path, new_xml).map_err(|e| format!("写入 plist 失败: {e}"))?;
    log_tray("write_plist_keepalive_off: 已把 KeepAlive 改为 false（文本替换）");
    Ok(())
}

/// 从已存在的 plist 读取 RunAtLoad（自启状态）。
fn read_autostart_from_plist() -> bool {
    fs::read_to_string(plist_path())
        .map(|xml| {
            let marker = "<key>RunAtLoad</key>";
            match xml.find(marker) {
                Some(i) => xml[i + marker.len()..].trim_start().starts_with("<true/>"),
                None => false,
            }
        })
        .unwrap_or(false)
}

// ── launchctl 控制 ────────────────────────────────────────────────────

fn service_is_running() -> bool {
    match launchctl_logged(&["print", &service_target()]) {
        Ok(o) => {
            let running = o.status.success()
                && String::from_utf8_lossy(&o.stdout).contains("state = running");
            log_tray(&format!("service_is_running: {running}"));
            running
        }
        Err(_) => false,
    }
}

/// 启动：卸载（忽略不存在）→ 装载 → 立即启动一次。
fn service_start(app: &AppHandle) {
    let plist = plist_path().to_string_lossy().into_owned();
    log_tray("service_start: begin");
    // 恢复 KeepAlive=true（若之前停止时被禁用）。
    if let Err(e) = write_plist(app, AUTOSTART.load(Ordering::Relaxed)) {
        log_tray(&format!("service_start: 写 KeepAlive=true plist 失败 {e}"));
    }
    let _ = launchctl_logged(&["bootout", &gui_target(), &plist]);
    let _ = launchctl_logged(&["bootstrap", &gui_target(), &plist]);
    let _ = launchctl_logged(&["kickstart", &service_target()]);
    log_tray("service_start: end");
}

/// 停止：SIGTERM 优雅退出（dsh 自带 SIGTERM 处理）。
fn service_stop() {
    log_tray("service_stop: begin");
    // 停止语义（方案 2）：临时禁用 KeepAlive，再 SIGTERM，避免 launchd 立即自愈拉起。
    // 先卸载再装载（让 KeepAlive=false 生效），随后 kill；若进程已退出则忽略错误。
    let plist = plist_path().to_string_lossy().into_owned();
    let _ = launchctl_logged(&["bootout", &gui_target(), &plist]);
    if let Err(e) = write_plist_keepalive_off() {
        log_tray(&format!("service_stop: 写 KeepAlive=false plist 失败 {e}"));
    }
    let _ = launchctl_logged(&["bootstrap", &gui_target(), &plist]);
    // bootstrap 后服务会按 KeepAlive=false 停留在未运行状态，无需再 kill；
    // 若进程仍在（竞态），补一次 SIGTERM 兜底。
    if service_is_running() {
        let _ = launchctl_logged(&["kill", "SIGTERM", &service_target()]);
    }
    log_tray("service_stop: end");
}

/// 重启：kickstart -k（杀旧进程并重新拉起）。
fn service_restart(app: &AppHandle) {
    log_tray("service_restart: begin");
    // 重启语义：恢复 KeepAlive=true。
    if let Err(e) = write_plist(app, AUTOSTART.load(Ordering::Relaxed)) {
        log_tray(&format!("service_restart: 写 KeepAlive=true plist 失败 {e}"));
    }
    let _ = launchctl_logged(&["kickstart", "-k", &service_target()]);
    log_tray("service_restart: end");
}

/// 自启开关：重写 plist 的 RunAtLoad 并重新装载；服务保持运行。
fn set_autostart(app: &AppHandle, enabled: bool) {
    AUTOSTART.store(enabled, Ordering::Relaxed);
    if let Err(e) = write_plist(app, enabled) {
        eprintln!("dsh-launcher: 写入 LaunchAgent 失败: {e}");
        log_tray(&format!("set_autostart: 写入失败 {e}"));
    }
    let plist = plist_path().to_string_lossy().into_owned();
    let _ = launchctl_logged(&["bootout", &gui_target(), &plist]);
    let _ = launchctl_logged(&["bootstrap", &gui_target(), &plist]);
    if service_is_running() {
        let _ = launchctl_logged(&["kickstart", &service_target()]);
    }
}

// ── 状态探测 ──────────────────────────────────────────────────────────

fn port_open(port: u16) -> bool {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    TcpStream::connect_timeout(&addr, Duration::from_millis(400)).is_ok()
}

enum DshStatus { Running, Stopped }

/// 菜单状态行的临时动作消息（显示约 3 秒后被轮询状态覆盖）。
static LAST_ACTION_MSG: Mutex<Option<(String, Instant)>> = Mutex::new(None);

fn show_action_msg(msg: String) {
    if let Ok(mut guard) = LAST_ACTION_MSG.lock() {
        *guard = Some((msg, Instant::now()));
    }
}

/// 向控制面插件发 POST（当前仅 /reload；连接失败视为 dsh 未运行或插件未挂载）。
fn control_post(path: &str) -> Result<String, String> {
    let addr: SocketAddr = format!("127.0.0.1:{CONTROL_PORT}").parse().unwrap();
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(800))
        .map_err(|e| format!("控制面不可达（dsh 未运行或插件未挂载）: {e}"))?;
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{CONTROL_PORT}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status_line = text.lines().next().unwrap_or("").to_string();
    let code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if (200..300).contains(&code) {
        Ok(status_line)
    } else {
        Err(format!("控制面返回 {status_line}"))
    }
}

fn probe_status() -> DshStatus {
    // 控制面端口在则 dsh 一定在；兜底看 web 端口。
    if port_open(CONTROL_PORT) || port_open(WEB_PORT) {
        DshStatus::Running
    } else {
        DshStatus::Stopped
    }
}

// ── 打开浏览器 / 日志 ─────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn open_url(url: &str) { let _ = Command::new("open").arg(url).spawn(); }
#[cfg(target_os = "windows")]
fn open_url(url: &str) {
    let _ = Command::new("cmd").args(["/c", "start", "", url]).spawn();
}
#[cfg(target_os = "linux")]
fn open_url(url: &str) { let _ = Command::new("xdg-open").arg(url).spawn(); }

/// 打开 dsh 界面：按配置走系统浏览器或内置 WebView。
fn open_dsh_ui(app: &AppHandle) {
    if OPEN_MODE.load(Ordering::Relaxed) {
        open_builtin(app);
    } else {
        log_tray("open_dsh_ui: 使用系统浏览器");
        open_url(WEB_URL);
    }
}

/// 内置浏览器：WebviewWindow 加载 dsh web 界面（无系统 chrome，可复用）。
fn open_builtin(app: &AppHandle) {
    log_tray("open_dsh_ui: 使用内置浏览器");
    // 内置窗口需要成为普通前台窗口（可 Cmd+Tab 切换）：从 Accessory 临时切到 Regular。
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    // 已有窗口则聚焦；没有则创建。
    if let Some(win) = app.get_webview_window("dsh-ui") {
        let _ = win.show();
        let _ = win.set_focus();
        log_tray("open_builtin: 复用已有窗口");
        return;
    }
    match WebviewWindowBuilder::new(app, "dsh-ui", WebviewUrl::External(WEB_URL.parse().unwrap()))
        .title("dsh 界面")
        .inner_size(1100.0, 760.0)
        .build()
    {
        Ok(win) => {
            log_tray("open_builtin: 内置窗口已创建");
            // 关闭窗口只隐藏，不让应用退出（纯托盘应用语义）。
            let win2 = win.clone();
            win.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = win2.hide();
                    // 窗口隐藏后恢复纯托盘（Accessory），不再占据 Dock / Cmd+Tab。
                    let app = win2.app_handle();
                    let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);
                }
            });
            let _ = win.show();
            let _ = win.set_focus();
        }
        Err(e) => {
            log_tray(&format!("open_builtin: 创建窗口失败 {e}"));
            open_url(WEB_URL);
        }
    }
}

// ── 托盘 ──────────────────────────────────────────────────────────────

fn build_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let status = MenuItem::with_id(app, "status", "dsh: 检测中…", true, None::<&str>)?;
    let open = MenuItem::with_id(app, "open", "打开 dsh 界面", true, None::<&str>)?;
    let builtin = CheckMenuItem::with_id(
        app,
        "open-builtin",
        "内置浏览器打开",
        true,
        OPEN_MODE.load(Ordering::Relaxed),
        None::<&str>,
    )?;
    let restart = MenuItem::with_id(app, "restart", "重启 dsh", true, None::<&str>)?;
    let reload = MenuItem::with_id(app, "reload", "热重载 dsh（控制面）", true, None::<&str>)?;
    let stop = MenuItem::with_id(app, "stop", "停止 dsh", true, None::<&str>)?;
    let start = MenuItem::with_id(app, "start", "启动 dsh", true, None::<&str>)?;
    let autostart = CheckMenuItem::with_id(
        app,
        "autostart",
        "开机自启 dsh",
        true,
        AUTOSTART.load(Ordering::Relaxed),
        None::<&str>,
    )?;
    let log = MenuItem::with_id(app, "log", "查看日志", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出托盘（dsh 继续运行）", true, None::<&str>)?;
    Menu::with_items(
        app,
        &[
            &status,
            &open,
            &builtin,
            &PredefinedMenuItem::separator(app)?,
            &restart,
            &reload,
            &stop,
            &start,
            &PredefinedMenuItem::separator(app)?,
            &autostart,
            &log,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )
}

fn handle_menu(app: &AppHandle, id: &str) {
    log_tray(&format!("menu: {id}"));
    match id {
        "open" => open_dsh_ui(app),
        "open-builtin" => {
            let next = !OPEN_MODE.load(Ordering::Relaxed);
            OPEN_MODE.store(next, Ordering::Relaxed);
            save_open_mode(if next { OpenMode::Builtin } else { OpenMode::System });
            log_tray(&format!("open-builtin: 切换为 {}", if next { "内置" } else { "系统" }));
        }
        "restart" => {
            if let Err(e) = ensure_plist(app) {
                eprintln!("dsh-launcher: {e}");
                log_tray(&format!("restart: ensure_plist 失败 {e}"));
            }
            service_restart(app);
        }
        "reload" => match control_post("/reload") {
            Ok(_) => {
                log_tray("reload: 已触发");
                show_action_msg("热重载：已触发".to_string());
            }
            Err(e) => {
                log_tray(&format!("reload: 失败 {e}"));
                show_action_msg(format!("热重载失败：{e}"));
            }
        },
        "stop" => service_stop(),
        "start" => {
            if let Err(e) = ensure_plist(app) {
                eprintln!("dsh-launcher: {e}");
                log_tray(&format!("start: ensure_plist 失败 {e}"));
            }
            service_start(app);
        }
        "autostart" => {
            let next = !AUTOSTART.load(Ordering::Relaxed);
            log_tray(&format!("autostart: 切换为 {next}"));
            set_autostart(app, next);
        }
        "log" => {
            if !log_path().exists() {
                let _ = fs::write(&log_path(), "");
            }
            #[cfg(target_os = "macos")]
            let _ = Command::new("open").arg(&log_path()).spawn();
        }
        "quit" => app.exit(0),
        _ => {}
    }
}

/// 更新菜单项文本（MenuItemKind 枚举按变体转发）。
fn set_menu_text(menu: &Menu<tauri::Wry>, id: &str, text: &str) {
    use tauri::menu::MenuItemKind;
    if let Some(item) = menu.get(id) {
        match item {
            MenuItemKind::MenuItem(i) => { let _ = i.set_text(text); }
            MenuItemKind::Check(i) => { let _ = i.set_text(text); }
            MenuItemKind::Submenu(i) => { let _ = i.set_text(text); }
            MenuItemKind::Predefined(_) | MenuItemKind::Icon(_) => {}
        }
    }
}

/// 后台状态轮询：更新托盘 tooltip 与菜单状态行。
fn spawn_status_poller(app: AppHandle) {
    thread::spawn(move || loop {
        let msg = LAST_ACTION_MSG
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(3));
        let (text, status_changed) = match msg {
            Some((text, _)) => (text, false),
            None => {
                let running = match probe_status() {
                    DshStatus::Running => true,
                    DshStatus::Stopped => false,
                };
                (if running { "dsh: 运行中".to_string() } else { "dsh: 已停止".to_string() }, true)
            }
        };
        if status_changed {
            log_tray(&format!("status: {}", text));
        }
        if let Some(tray) = app.tray_by_id("main") {
            let _ = tray.set_tooltip(Some(&text));
        }
        set_menu_text(&app.state::<AppMenu>().0, "status", &text);
        thread::sleep(Duration::from_millis(STATUS_POLL_MS));
    });
}

pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            log_tray("=== dsh-launcher 启动 ===");
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            AUTOSTART.store(read_autostart_from_plist(), Ordering::Relaxed);
            OPEN_MODE.store(
                match load_open_mode() {
                    OpenMode::Builtin => true,
                    OpenMode::System => false,
                },
                Ordering::Relaxed,
            );
            if let Err(e) = ensure_plist(app.handle()) {
                eprintln!("dsh-launcher: {e}");
                log_tray(&format!("setup: ensure_plist 失败 {e}"));
            }

            // 打开托盘即确保 dsh 运行：web 端口与控制面端口均不可达时自动拉起。
            // （已运行则跳过；启动是异步的，状态行先给反馈，轮询会跟进真实状态）
            if !port_open(WEB_PORT) && !port_open(CONTROL_PORT) {
                log_tray("setup: 3080/3399 均不可达，自动拉起 dsh");
                service_start(app.handle());
                show_action_msg("dsh 未运行，已自动启动".to_string());
            } else {
                log_tray("setup: dsh 已在运行，跳过自动拉起");
            }

            let menu = build_menu(app.handle())?;
            log_tray("setup: 菜单构建完成");
            app.manage(AppMenu(menu.clone()));
            let _tray = TrayIconBuilder::with_id("main")
                .icon(tauri::image::Image::from_bytes(include_bytes!("../icons/128x128.png"))?)
                .tooltip("DSH启动器")
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(|app, event| handle_menu(app, event.id.as_ref()))
                .build(app)?;
            log_tray("setup: 托盘构建完成");

            spawn_status_poller(app.handle().clone());
            log_tray("setup: 状态轮询已启动");
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running dsh-launcher");
}
