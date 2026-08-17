// dsh-tray: dsh (DeepSeek Harness) 系统托盘管理器。
//
// 架构（决策 1/2/3/4）：
//  - dsh web 进程由 launchd (LaunchAgent) 托管：KeepAlive 崩溃自愈，RunAtLoad 默认关（自启默认关）。
//  - 本应用是纯托盘控制台（无窗口）：状态轮询 + launchctl 控制 + 菜单。
//  - 控制面插件 dsh-tray-control 通过 `dsh web --patch <cordis.patch.yml>` 挂载，
//    暴露 127.0.0.1:3399 状态/优雅停机端点（零侵入 profile）。

use std::{
    fs,
    net::{SocketAddr, TcpStream},
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
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

/// [node, dsh_bin, --profile, web, --patch, <控制面 patch 路径>]
fn dsh_program() -> Vec<String> {
    let node = which("node").unwrap_or_else(|| {
        PathBuf::from("/Users/iceleaf/.nvm/versions/node/v24.18.0/bin/node")
    });
    let dsh = which("dsh").unwrap_or_else(|| {
        PathBuf::from("/Users/iceleaf/.nvm/versions/node/v24.18.0/bin/dsh")
    });
    let patch = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../plugins/dsh-tray-control/cordis.patch.yml");
    vec![
        node.to_string_lossy().into_owned(),
        dsh.to_string_lossy().into_owned(),
        "--profile".into(),
        "web".into(),
        "--patch".into(),
        patch.to_string_lossy().into_owned(),
    ]
}

// ── plist 生成（LaunchAgent） ─────────────────────────────────────────

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn plist_xml(run_at_load: bool) -> String {
    let program = dsh_program();
    let args = program
        .iter()
        .map(|a| format!("    <string>{}</string>", xml_escape(a)))
        .collect::<Vec<_>>()
        .join("
");
    format!(
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
    )
}

fn ensure_plist() -> std::io::Result<()> {
    let path = plist_path();
    if !path.exists() {
        if let Some(dir) = path.parent() { fs::create_dir_all(dir)?; }
        fs::write(&path, plist_xml(AUTOSTART.load(Ordering::Relaxed)))?;
    }
    Ok(())
}

fn write_plist(run_at_load: bool) -> std::io::Result<()> {
    let path = plist_path();
    if let Some(dir) = path.parent() { fs::create_dir_all(dir)?; }
    fs::write(&path, plist_xml(run_at_load))
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
fn set_autostart(enabled: bool) {
    AUTOSTART.store(enabled, Ordering::Relaxed);
    let _ = write_plist(enabled);
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
        "restart" => { let _ = ensure_plist(); service_restart(); }
        "stop" => service_stop(),
        "start" => { let _ = ensure_plist(); service_start(); }
        "autostart" => {
            let next = !AUTOSTART.load(Ordering::Relaxed);
            set_autostart(next);
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
        let text = match probe_status() {
            DshStatus::Running => "dsh: 运行中",
            DshStatus::Stopped => "dsh: 已停止",
        };
        if let Some(tray) = app.tray_by_id("main") {
            let _ = tray.set_tooltip(Some(text));
        }
        set_menu_text(&app.state::<AppMenu>().0, "status", text);
        thread::sleep(Duration::from_millis(STATUS_POLL_MS));
    });
}

pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            AUTOSTART.store(read_autostart_from_plist(), Ordering::Relaxed);
            let _ = ensure_plist();

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
