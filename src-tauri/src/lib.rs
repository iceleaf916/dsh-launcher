// dsh-tray: dsh (DeepSeek Harness) 系统托盘管理器。
//
// 架构（决策 1/2/3/4）：
//  - dsh web 进程由 launchd (LaunchAgent) 托管：KeepAlive 崩溃自愈，RunAtLoad 默认关（自启默认关）。
//  - 本应用是纯托盘控制台（无窗口）：状态轮询 + launchctl 控制 + 菜单。
//  - 控制面插件 dsh-tray-control 通过 `dsh web --patch <cordis.patch.yml>` 挂载，
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
    AppHandle, Manager,
};

// ── 常量 ──────────────────────────────────────────────────────────────

const WEB_URL: &str = "http://127.0.0.1:3080";
const WEB_PORT: u16 = 3080;
const CONTROL_PORT: u16 = 3399; // dsh-tray-control 插件端口
const LAUNCHD_LABEL: &str = "com.dsh-tray.web";
const STATUS_POLL_MS: u64 = 2000;

static AUTOSTART: AtomicBool = AtomicBool::new(false);

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

fn which(name: &str) -> Option<PathBuf> {
    let out = Command::new("sh").arg("-lc").arg(format!("command -v {name}")).output().ok()?;
    if !out.status.success() { return None; }
    let s = String::from_utf8(out.stdout).ok()?;
    let p = PathBuf::from(s.trim());
    if p.as_os_str().is_empty() { None } else { Some(p) }
}

/// 控制面插件目录。
/// - debug 构建（开发）：优先源码目录（编译期路径，release 二进制不含）。
/// - release 构建（发布）：一律从 .app 资源目录解析。
fn control_dir(app: &AppHandle) -> PathBuf {
    #[cfg(debug_assertions)]
    {
        let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins/dsh-tray-control");
        if dev.join("lib/index.js").exists() {
            return dev;
        }
    }
    app.path().resource_dir().unwrap_or_default().join("dsh-tray-control")
}

/// 控制面 patch 路径：动态生成到 ~/Library/Application Support/dsh-tray/，
/// 内容引用实际插件入口（bundle 内或源码目录），解决打包后绝对路径失效问题。
fn control_patch_path(app: &AppHandle) -> PathBuf {
    let dir = home_dir().join("Library/Application Support/dsh-tray");
    let _ = fs::create_dir_all(&dir);
    let patch = dir.join("control.patch.yml");
    let plugin_index = control_dir(app).join("lib/index.js");
    let content = format!(
        "# 由 dsh-tray 自动生成，请勿手改。\n- insert:\n    - id: dsh-tray-control\n      name: '{}'\n",
        plugin_index.to_string_lossy()
    );
    let _ = fs::write(&patch, content);
    patch
}

/// [node, dsh_bin, --profile, web, --patch, <控制面 patch 路径>]
/// node/dsh 一律运行时解析（PATH），解析失败返回错误——不写死任何安装路径。
fn dsh_program(app: &AppHandle) -> Result<Vec<String>, String> {
    let node = which("node").ok_or_else(|| "node 不在 PATH，无法生成 LaunchAgent".to_string())?;
    let dsh = which("dsh").ok_or_else(|| "dsh 不在 PATH，无法生成 LaunchAgent".to_string())?;
    let patch = control_patch_path(app);
    Ok(vec![
        node.to_string_lossy().into_owned(),
        dsh.to_string_lossy().into_owned(),
        "--profile".into(),
        "web".into(),
        "--patch".into(),
        patch.to_string_lossy().into_owned(),
    ])
}

// ── plist 生成（LaunchAgent） ─────────────────────────────────────────

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn plist_xml(app: &AppHandle, run_at_load: bool) -> Result<String, String> {
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
  <true/>
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
        run_at_load = if run_at_load { "true" } else { "false" },
        log = xml_escape(&log_path().to_string_lossy()),
    ))
}

fn ensure_plist(app: &AppHandle) -> Result<(), String> {
    let path = plist_path();
    if !path.exists() {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let xml = plist_xml(app, AUTOSTART.load(Ordering::Relaxed))?;
        fs::write(&path, xml).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn write_plist(app: &AppHandle, run_at_load: bool) -> Result<(), String> {
    let path = plist_path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let xml = plist_xml(app, run_at_load)?;
    fs::write(&path, xml).map_err(|e| e.to_string())
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

fn launchctl(args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new("launchctl").args(args).output()
}

fn service_is_running() -> bool {
    match launchctl(&["print", &service_target()]) {
        Ok(o) => o.status.success()
            && String::from_utf8_lossy(&o.stdout).contains("state = running"),
        Err(_) => false,
    }
}

/// 启动：卸载（忽略不存在）→ 装载 → 立即启动一次。
fn service_start() {
    let plist = plist_path().to_string_lossy().into_owned();
    let _ = launchctl(&["bootout", &gui_target(), &plist]);
    let _ = launchctl(&["bootstrap", &gui_target(), &plist]);
    let _ = launchctl(&["kickstart", &service_target()]);
}

/// 停止：SIGTERM 优雅退出（dsh 自带 SIGTERM 处理）。
fn service_stop() {
    let _ = launchctl(&["kill", "SIGTERM", &service_target()]);
}

/// 重启：kickstart -k（杀旧进程并重新拉起）。
fn service_restart() {
    let _ = launchctl(&["kickstart", "-k", &service_target()]);
}

/// 自启开关：重写 plist 的 RunAtLoad 并重新装载；服务保持运行。
fn set_autostart(app: &AppHandle, enabled: bool) {
    AUTOSTART.store(enabled, Ordering::Relaxed);
    if let Err(e) = write_plist(app, enabled) {
        eprintln!("dsh-tray: 写入 LaunchAgent 失败: {e}");
    }
    let plist = plist_path().to_string_lossy().into_owned();
    let _ = launchctl(&["bootout", &gui_target(), &plist]);
    let _ = launchctl(&["bootstrap", &gui_target(), &plist]);
    if service_is_running() {
        let _ = launchctl(&["kickstart", &service_target()]);
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

// ── 托盘 ──────────────────────────────────────────────────────────────

fn build_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let status = MenuItem::with_id(app, "status", "dsh: 检测中…", true, None::<&str>)?;
    let open = MenuItem::with_id(app, "open", "打开 Web 界面", true, None::<&str>)?;
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
    match id {
        "open" => open_url(WEB_URL),
        "restart" => {
            if let Err(e) = ensure_plist(app) { eprintln!("dsh-tray: {e}"); }
            service_restart();
        }
        "reload" => match control_post("/reload") {
            Ok(_) => show_action_msg("热重载：已触发".to_string()),
            Err(e) => show_action_msg(format!("热重载失败：{e}")),
        },
        "stop" => service_stop(),
        "start" => {
            if let Err(e) = ensure_plist(app) { eprintln!("dsh-tray: {e}"); }
            service_start();
        }
        "autostart" => {
            let next = !AUTOSTART.load(Ordering::Relaxed);
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
        let text = match msg {
            Some((text, _)) => text,
            None => match probe_status() {
                DshStatus::Running => "dsh: 运行中".to_string(),
                DshStatus::Stopped => "dsh: 已停止".to_string(),
            },
        };
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
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            AUTOSTART.store(read_autostart_from_plist(), Ordering::Relaxed);
            if let Err(e) = ensure_plist(app.handle()) {
                eprintln!("dsh-tray: {e}");
            }

            let menu = build_menu(app.handle())?;
            app.manage(AppMenu(menu.clone()));
            let _tray = TrayIconBuilder::with_id("main")
                .icon(tauri::image::Image::from_bytes(include_bytes!("../icons/128x128.png"))?)
                .tooltip("dsh-tray")
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(|app, event| handle_menu(app, event.id.as_ref()))
                .build(app)?;

            spawn_status_poller(app.handle().clone());
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running dsh-tray");
}
