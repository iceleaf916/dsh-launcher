// dsh-launcher: dsh (DeepSeek Harness) 系统托盘管理器。
//
// 架构（决策 1/2/3/4）：
//  - dsh web 进程由系统服务托管：
//      macOS  -> launchd (LaunchAgent)：KeepAlive 崩溃自愈，RunAtLoad 默认关（自启默认关）。
//      Linux  -> systemd user service：Restart=always 崩溃自愈，enable 对应登录自启。
//  - 本应用是托盘控制台 + 内置 dsh WebView：状态轮询 + 服务控制 + 菜单。

use std::{
    fs,
    io::Write,
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

use serde::{Deserialize, Serialize};
use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    AppHandle, Manager, WebviewUrl, WebviewWindowBuilder,
};

// ── 常量 ──────────────────────────────────────────────────────────────

const WEB_URL: &str = "http://127.0.0.1:3080";
const WEB_PORT: u16 = 3080;
const STARTUP_UI_WAIT_MS: u64 = 10000;
const DEFAULT_WINDOW_WIDTH: f64 = 1400.0;
const DEFAULT_WINDOW_HEIGHT: f64 = 900.0;
const WINDOW_SIZE_SAVE_DELAY_MS: u64 = 1000;
#[cfg(target_os = "macos")]
const LAUNCHD_LABEL: &str = "com.dsh-launcher.web";
const STATUS_POLL_MS: u64 = 2000;

static AUTOSTART: AtomicBool = AtomicBool::new(false);

/// 托盘菜单句柄（存进 Tauri state，供后台状态轮询线程更新）。
struct AppMenu(Menu<tauri::Wry>);

// ── 路径工具 ──────────────────────────────────────────────────────────

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// 平台配置根目录（不含应用名），也用于 Linux systemd user unit 路径。
#[cfg(target_os = "macos")]
fn config_root() -> PathBuf {
    home_dir().join("Library/Application Support")
}

#[cfg(not(target_os = "macos"))]
fn config_root() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home_dir().join(".config"))
}

/// 平台状态/日志根目录（不含应用名）。
#[cfg(target_os = "macos")]
fn state_root() -> PathBuf {
    home_dir().join("Library/Logs")
}

#[cfg(not(target_os = "macos"))]
fn state_root() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home_dir().join(".local/state"))
}

/// 本应用状态/日志目录：macOS `~/Library/Logs`，
/// Linux `$XDG_STATE_HOME/dsh-launcher`（默认 `~/.local/state/dsh-launcher`）。
fn state_dir() -> PathBuf {
    state_root().join("dsh-launcher")
}

fn config_dir() -> PathBuf {
    config_root().join("dsh-launcher")
}

#[derive(Clone, Copy, Deserialize, Serialize)]
struct WindowSize {
    width: f64,
    height: f64,
}

impl Default for WindowSize {
    fn default() -> Self {
        Self {
            width: DEFAULT_WINDOW_WIDTH,
            height: DEFAULT_WINDOW_HEIGHT,
        }
    }
}

impl WindowSize {
    fn is_valid(self) -> bool {
        self.width.is_finite()
            && self.height.is_finite()
            && self.width >= 320.0
            && self.height >= 240.0
            && self.width <= 10000.0
            && self.height <= 10000.0
    }
}

fn window_size_path() -> PathBuf {
    config_dir().join("window-size.json")
}

fn load_window_size() -> WindowSize {
    fs::read_to_string(window_size_path())
        .ok()
        .and_then(|raw| serde_json::from_str::<WindowSize>(&raw).ok())
        .filter(|size| size.is_valid())
        .unwrap_or_default()
}

fn save_window_size(size: WindowSize) {
    if !size.is_valid() {
        return;
    }
    if let Some(dir) = window_size_path().parent() {
        let _ = fs::create_dir_all(dir);
    }
    match serde_json::to_string_pretty(&size) {
        Ok(raw) => {
            if let Err(e) = fs::write(window_size_path(), raw) {
                log_tray(&format!("save_window_size: 写入失败 {e}"));
            }
        }
        Err(e) => log_tray(&format!("save_window_size: 序列化失败 {e}")),
    }
}

fn save_window_size_from_physical(width: u32, height: u32, scale_factor: f64) {
    if scale_factor.is_finite() && scale_factor > 0.0 {
        save_window_size(WindowSize {
            width: f64::from(width) / scale_factor,
            height: f64::from(height) / scale_factor,
        });
    }
}

fn log_path() -> PathBuf {
    state_dir().join("dsh-web.log")
}

fn tray_log_path() -> PathBuf {
    state_dir().join("dsh-launcher.log")
}

/// 托盘自身日志：追加写入平台状态目录（不依赖 stderr，GUI 启动可见）。
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

// ── dsh 可执行解析（服务定义用绝对路径，不依赖服务管理器的 PATH） ────

/// 从登录 shell 解析可执行文件绝对路径。
/// GUI 启动的 app 环境 PATH 只有系统默认值（无 nvm/Homebrew），
/// 而 launchctl 的 PATH 也不含用户 shell 配置；用 `zsh -lc` 拿到真实 PATH。
fn which_from_login_shell(name: &str) -> Option<PathBuf> {
    for shell in ["zsh", "bash", "sh"] {
        let out = Command::new(shell)
            .arg("-lc")
            .arg(format!("command -v {name}"))
            .output()
            .ok()?;
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

/// 从登录 shell 读取完整 PATH。
/// launchd 只给 `PATH=/usr/bin:/bin:/usr/sbin:/sbin`，但 dsh 的
/// 子进程（MCP stdio：npx/codegraph/uvx 等）依赖用户完整 PATH；
/// 服务定义必须把这份 PATH 固化进 plist/systemd unit。
/// 失败时回退到当前进程 PATH（GUI 下通常只有系统目录，但优于空值）。
fn login_shell_path() -> Option<String> {
    for shell in ["zsh", "bash", "sh"] {
        let out = Command::new(shell)
            .arg("-lc")
            .arg("printf '%s' \"$PATH\"")
            .output()
            .ok()?;
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    std::env::var("PATH").ok().filter(|p| !p.is_empty())
}

/// 服务子进程环境：完整 PATH + TERM=dumb（与 dsh 内部 bash 工具约定一致）。
/// 只在 plist/systemd unit 里写入这两个键，避免把 GUI 会话的
/// 其他环境变量（SSH_AUTH_SOCK 等）固化到服务里。
fn service_env() -> Vec<(String, String)> {
    let path = login_shell_path()
        .map(|p| {
            log_tray(&format!("service_env: 登录 shell PATH={p}"));
            p
        })
        .unwrap_or_else(|| {
            log_tray("service_env: 登录 shell PATH 解析失败，回退当前进程 PATH");
            std::env::var("PATH").unwrap_or_default()
        });
    vec![
        ("PATH".to_string(), path),
        ("TERM".to_string(), "dumb".to_string()),
    ]
}

/// 常见安装路径兜底（登录 shell 可能因慢/异常失败，或 PATH 不含这些位置）。
fn which_from_common_paths(name: &str) -> Option<PathBuf> {
    let home = home_dir();
    let candidates = [
        home.join(".nvm/versions/node")
            .join("current/bin")
            .join(name),
        home.join(".local/bin").join(name),
        home.join(".volta/bin").join(name),
        PathBuf::from("/opt/homebrew/bin").join(name),
        PathBuf::from("/usr/local/bin").join(name),
        PathBuf::from("/usr/bin").join(name),
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
    which_from_login_shell(name).or_else(|| which_from_common_paths(name))
}

/// node 与 dsh 必须来自同一目录（nvm 版本一致性）：若 dsh 由常见路径解析出，
/// 则 node 优先取同目录；否则退回登录 shell/常见路径解析 node。
fn node_for_dsh(dsh: &PathBuf) -> Option<PathBuf> {
    let same_dir = dsh
        .parent()
        .map(|dir| dir.join("node"))
        .filter(|p| p.is_file());
    same_dir.or_else(|| which("node"))
}

/// [node, dsh_bin, --profile, web, --no-open]
/// node/dsh 一律运行时解析（PATH），解析失败返回错误——不写死任何安装路径。
fn dsh_program(_app: &AppHandle) -> Result<Vec<String>, String> {
    let dsh = which("dsh").ok_or_else(|| {
        "dsh 不在 PATH，无法生成服务定义（已尝试登录 shell 与常见安装路径）".to_string()
    })?;
    let node = node_for_dsh(&dsh).ok_or_else(|| {
        "node 不在 PATH，无法生成服务定义（已尝试登录 shell 与常见安装路径）".to_string()
    })?;
    let program = vec![
        node.to_string_lossy().into_owned(),
        dsh.to_string_lossy().into_owned(),
        "--profile".into(),
        "web".into(),
        "--no-open".into(),
    ];
    log_tray(&format!(
        "dsh_program: node={} dsh={}",
        node.display(),
        dsh.display()
    ));
    Ok(program)
}

#[cfg(target_os = "macos")]
mod macos_service {
    use super::*;

    pub fn plist_path() -> PathBuf {
        home_dir()
            .join("Library/LaunchAgents")
            .join(format!("{LAUNCHD_LABEL}.plist"))
    }

    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    fn plist_xml(app: &AppHandle, run_at_load: bool, keep_alive: bool) -> Result<String, String> {
        let program = dsh_program(app)?;
        let args = program
            .iter()
            .map(|a| format!("    <string>{}</string>", xml_escape(a)))
            .collect::<Vec<_>>()
            .join("\n");
        // dsh 子进程（MCP stdio 等）继承 launchd 环境；launchd 默认 PATH
        // 只有系统目录，必须把用户完整 PATH 与 TERM 固化进 plist。
        let env = service_env()
            .iter()
            .map(|(k, v)| {
                format!(
                    "    <key>{}</key>\n    <string>{}</string>",
                    xml_escape(k),
                    xml_escape(v)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
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
  <key>EnvironmentVariables</key>
  <dict>
{env}
  </dict>
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

    fn launchctl_logged(args: &[&str]) -> std::io::Result<std::process::Output> {
        log_tray(&format!("launchctl {}", args.join(" ")));
        let out = Command::new("launchctl").args(args).output()?;
        let code = out.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        log_tray(&format!(
            "launchctl {} -> exit {code} stdout={stdout:?} stderr={stderr:?}",
            args.join(" ")
        ));
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

    pub fn ensure_definition(app: &AppHandle) -> Result<(), String> {
        let path = plist_path();
        log_tray(&format!(
            "ensure_definition: path={} exists={}",
            path.display(),
            path.exists()
        ));
        if !path.exists() {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            let xml = plist_xml(app, AUTOSTART.load(Ordering::Relaxed), true)?;
            if let Err(e) = fs::write(&path, &xml) {
                log_tray(&format!("ensure_definition: 写入失败 {e}"));
                return Err(e.to_string());
            }
            log_tray(&format!("ensure_definition: 已写入 plist ({})", xml.len()));
        } else if !plist_has_env(&path) {
            // 旧版 plist 没有 EnvironmentVariables：dsh 子进程拿不到用户 PATH，
            // MCP stdio（npx/codegraph/uvx）等会启动失败。自动升级并重载，
            // 保持 KeepAlive/RunAtLoad 现状（用户停止过服务时不被重新拉起）。
            log_tray("ensure_definition: 检测到旧版 plist（缺 EnvironmentVariables），升级");
            let run_at_load = read_autostart();
            let keep_alive = read_keepalive();
            let xml = plist_xml(app, run_at_load, keep_alive)?;
            if let Err(e) = fs::write(&path, &xml) {
                log_tray(&format!("ensure_definition: 升级写入失败 {e}"));
                return Err(e.to_string());
            }
            log_tray("ensure_definition: 旧版 plist 已升级，重新装载");
            let plist = path.to_string_lossy().into_owned();
            let _ = launchctl_logged(&["bootout", &gui_target(), &plist]);
            let _ = launchctl_logged(&["bootstrap", &gui_target(), &plist]);
            if service_is_running() {
                let _ = launchctl_logged(&["kickstart", &service_target()]);
            }
        }
        Ok(())
    }

    /// 旧 plist 升级检测：是否已包含 EnvironmentVariables 键。
    fn plist_has_env(path: &PathBuf) -> bool {
        fs::read_to_string(path)
            .map(|xml| xml.contains("<key>EnvironmentVariables</key>"))
            .unwrap_or(false)
    }

    /// 从已存在的 plist 读取 KeepAlive（保持用户停止/启动语义，升级时不被覆盖）。
    fn read_keepalive() -> bool {
        fs::read_to_string(plist_path())
            .map(|xml| {
                let marker = "<key>KeepAlive</key>";
                match xml.find(marker) {
                    Some(i) => xml[i + marker.len()..].trim_start().starts_with("<true/>"),
                    None => true,
                }
            })
            .unwrap_or(true)
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
        log_tray(&format!(
            "write_plist: 已写入 plist run_at_load={run_at_load} ({})",
            xml.len()
        ));
        Ok(())
    }

    /// 停止时把现有 plist 的 KeepAlive 改为 false（纯文本替换，不重新解析 PATH）。
    fn write_plist_keepalive_off() -> Result<(), String> {
        let path = plist_path();
        let xml = fs::read_to_string(&path).map_err(|e| format!("读取 plist 失败: {e}"))?;
        let marker = "<key>KeepAlive</key>";
        let pos = xml
            .find(marker)
            .ok_or_else(|| "plist 缺少 KeepAlive 字段".to_string())?;
        let rest = &xml[pos + marker.len()..];
        let value_start = rest
            .find('<')
            .ok_or_else(|| "plist KeepAlive 值缺失".to_string())?;
        let value_end = rest[value_start..]
            .find('>')
            .map(|i| value_start + i + 1)
            .ok_or_else(|| "plist KeepAlive 值格式异常".to_string())?;
        let new_xml = format!(
            "{}{}<false/>{}",
            &xml[..pos + marker.len()],
            "",
            &rest[value_end..]
        );
        fs::write(&path, new_xml).map_err(|e| format!("写入 plist 失败: {e}"))?;
        log_tray("write_plist_keepalive_off: 已把 KeepAlive 改为 false（文本替换）");
        Ok(())
    }

    /// 从已存在的 plist 读取 RunAtLoad（自启状态）。
    pub fn read_autostart() -> bool {
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

    pub fn service_is_running() -> bool {
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
    pub fn service_start(app: &AppHandle) {
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
    pub fn service_stop() {
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
    pub fn service_restart(app: &AppHandle) {
        log_tray("service_restart: begin");
        // 重启语义：恢复 KeepAlive=true。
        if let Err(e) = write_plist(app, AUTOSTART.load(Ordering::Relaxed)) {
            log_tray(&format!(
                "service_restart: 写 KeepAlive=true plist 失败 {e}"
            ));
        }
        let _ = launchctl_logged(&["kickstart", "-k", &service_target()]);
        log_tray("service_restart: end");
    }

    /// 自启开关：重写 plist 的 RunAtLoad 并重新装载；服务保持运行。
    pub fn set_autostart(app: &AppHandle, enabled: bool) {
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
}

#[cfg(target_os = "linux")]
mod linux_service {
    use super::*;

    pub const UNIT_NAME: &str = "dsh-launcher-web.service";

    pub fn unit_path() -> PathBuf {
        config_root().join("systemd/user").join(UNIT_NAME)
    }

    fn systemctl_logged(args: &[&str]) -> std::io::Result<std::process::Output> {
        log_tray(&format!("systemctl --user {}", args.join(" ")));
        let out = Command::new("systemctl")
            .arg("--user")
            .args(args)
            .output()?;
        let code = out.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        log_tray(&format!(
            "systemctl --user {} -> exit {code} stdout={stdout:?} stderr={stderr:?}",
            args.join(" ")
        ));
        Ok(out)
    }

    fn unit_content(app: &AppHandle, enabled: bool) -> Result<String, String> {
        let program = dsh_program(app)?;
        let args = program
            .iter()
            .map(|a| format!("      {:?}", a))
            .collect::<Vec<_>>()
            .join("\n");
        // dsh 子进程（MCP stdio 等）继承 systemd user 环境；systemd 默认
        // 不继承登录会话 PATH，必须把用户完整 PATH 与 TERM 固化进 unit。
        let env = service_env()
            .iter()
            .map(|(k, v)| format!("Environment={}={}", k, v))
            .collect::<Vec<_>>()
            .join("\n");
        let log = log_path();
        Ok(format!(
            "# 由 dsh-launcher 自动生成，请勿手改。\n\
             [Unit]\n\
             Description=DSH web service (dsh --profile web)\n\
             After=network-online.target\n\
             Wants=network-online.target\n\n\
             [Service]\n\
             Type=simple\n\
             ExecStart={args}\n\
             {env}\n\
             Restart=always\n\
             RestartSec=2\n\
             StandardOutput=append:{log}\n\
             StandardError=append:{log}\n\n\
             [Install]\n\
             WantedBy=default.target\n",
            args = args,
            env = env,
            log = log.to_string_lossy(),
        ))
    }

    fn write_unit(app: &AppHandle, enabled: bool) -> Result<(), String> {
        let path = unit_path();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let content = unit_content(app, enabled)?;
        fs::write(&path, content).map_err(|e| format!("写入 systemd unit 失败: {e}"))?;
        log_tray(&format!(
            "write_unit: 已写入 {} enabled={enabled}",
            path.display()
        ));
        Ok(())
    }

    pub fn ensure_definition(app: &AppHandle) -> Result<(), String> {
        let path = unit_path();
        log_tray(&format!(
            "ensure_definition: path={} exists={}",
            path.display(),
            path.exists()
        ));
        if !path.exists() {
            write_unit(app, AUTOSTART.load(Ordering::Relaxed))?;
        } else {
            let content = fs::read_to_string(&path).unwrap_or_default();
            if !content.contains("Environment=PATH=") {
                // 旧版 unit 没有 Environment：dsh 子进程拿不到用户 PATH，
                // MCP stdio（npx/codegraph/uvx）等会启动失败。自动升级。
                log_tray("ensure_definition: 检测到旧版 unit（缺 Environment=PATH=），升级");
                write_unit(app, AUTOSTART.load(Ordering::Relaxed))?;
                let _ = systemctl_logged(&["daemon-reload"]);
                // 服务若在运行，重启一次让新环境生效。
                if service_is_running() {
                    let _ = systemctl_logged(&["restart", UNIT_NAME]);
                }
            }
        }
        Ok(())
    }

    /// 服务是否正在运行：`systemctl --user is-active --quiet dsh-launcher-web.service`。
    pub fn service_is_running() -> bool {
        systemctl_logged(&["is-active", "--quiet", UNIT_NAME])
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// 启动：确保 unit 存在 → daemon-reload → enable（持久化自启，幂等）→ start。
    pub fn service_start(app: &AppHandle) {
        log_tray("service_start: begin");
        if let Err(e) = ensure_definition(app) {
            log_tray(&format!("service_start: ensure_definition 失败 {e}"));
        }
        let _ = systemctl_logged(&["daemon-reload"]);
        let _ = systemctl_logged(&["enable", UNIT_NAME]);
        let _ = systemctl_logged(&["start", UNIT_NAME]);
        log_tray("service_start: end");
    }

    /// 停止：stop 后 systemd 不会自动重启（stop 语义与 Restart=always 不冲突）。
    pub fn service_stop() {
        log_tray("service_stop: begin");
        let _ = systemctl_logged(&["stop", UNIT_NAME]);
        log_tray("service_stop: end");
    }

    /// 重启：restart（systemd 负责杀旧进程并重新拉起）。
    pub fn service_restart(app: &AppHandle) {
        log_tray("service_restart: begin");
        if let Err(e) = ensure_definition(app) {
            log_tray(&format!("service_restart: ensure_definition 失败 {e}"));
        }
        let _ = systemctl_logged(&["daemon-reload"]);
        let _ = systemctl_logged(&["restart", UNIT_NAME]);
        log_tray("service_restart: end");
    }

    /// 自启开关：enable/disable 只影响登录自启，不影响当前运行状态。
    pub fn set_autostart(app: &AppHandle, enabled: bool) {
        AUTOSTART.store(enabled, Ordering::Relaxed);
        if let Err(e) = write_unit(app, enabled) {
            eprintln!("dsh-launcher: 写入 systemd unit 失败: {e}");
            log_tray(&format!("set_autostart: 写入失败 {e}"));
        }
        let _ = systemctl_logged(&["daemon-reload"]);
        let _ = systemctl_logged(&[if enabled { "enable" } else { "disable" }, UNIT_NAME]);
    }

    /// 从 systemd 已启用状态读取自启；systemd 不可用或 unit 不存在时按 false 处理。
    pub fn read_autostart() -> bool {
        if unit_path().exists() {
            systemctl_logged(&["is-enabled", UNIT_NAME])
                .map(|o| o.status.success())
                .unwrap_or(false)
        } else {
            false
        }
    }
}

// ── 状态探测 ──────────────────────────────────────────────────────────

fn port_open(port: u16) -> bool {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    TcpStream::connect_timeout(&addr, Duration::from_millis(400)).is_ok()
}

enum DshStatus {
    Running,
    Stopped,
}

/// 菜单状态行的临时动作消息（显示约 3 秒后被轮询状态覆盖）。
static LAST_ACTION_MSG: Mutex<Option<(String, Instant)>> = Mutex::new(None);

fn show_action_msg(msg: String) {
    if let Ok(mut guard) = LAST_ACTION_MSG.lock() {
        *guard = Some((msg, Instant::now()));
    }
}

fn probe_status() -> DshStatus {
    if port_open(WEB_PORT) {
        DshStatus::Running
    } else {
        DshStatus::Stopped
    }
}

// ── 打开浏览器 / 日志 ─────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn open_url(url: &str) {
    let _ = Command::new("open").arg(url).spawn();
}
#[cfg(target_os = "windows")]
fn open_url(url: &str) {
    let _ = Command::new("cmd").args(["/c", "start", "", url]).spawn();
}
#[cfg(target_os = "linux")]
fn open_url(url: &str) {
    let _ = Command::new("xdg-open").arg(url).spawn();
}

/// 用平台默认方式打开本地文件（macOS `open`，Linux `xdg-open`）。
fn open_path(path: &PathBuf) {
    #[cfg(target_os = "macos")]
    let _ = Command::new("open").arg(path).spawn();
    #[cfg(target_os = "linux")]
    let _ = Command::new("xdg-open").arg(path).spawn();
}

/// 用系统默认浏览器打开 http(s) 链接；其他协议（about:/data:/javascript: 等）忽略。
/// 内置浏览器里新窗口/外部链接统一走这里，避免 WebView 内无响应。
fn open_external(url: &str) {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        log_tray(&format!("open_external: {url}"));
        open_url(url);
    }
}

/// 打开 dsh 界面：始终使用托盘内置 WebView。
fn open_dsh_ui(app: &AppHandle) {
    open_builtin(app);
}

/// 内置浏览器：WebviewWindow 加载 dsh web 界面（无系统 chrome，可复用）。
fn open_builtin(app: &AppHandle) {
    log_tray("open_dsh_ui: 使用内置浏览器");
    // 内置窗口需要成为普通前台窗口（可 Cmd+Tab 切换）：从 Accessory 临时切到 Regular。
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    // 已有窗口则聚焦；没有则创建。
    if let Some(win) = app.get_webview_window("dsh-ui") {
        let _ = win.show();
        let _ = win.set_focus();
        log_tray("open_builtin: 复用已有窗口");
        return;
    }
    let window_size = load_window_size();
    let persist_size_after = Instant::now() + Duration::from_millis(WINDOW_SIZE_SAVE_DELAY_MS);
    match WebviewWindowBuilder::new(
        app,
        "dsh-ui",
        WebviewUrl::External(WEB_URL.parse().unwrap()),
    )
    .title("dsh 界面")
    .inner_size(window_size.width, window_size.height)
    // 内置浏览器需要 Tauri IPC：dsh-notification 插件走 Web Notification API，
    // 官方 tauri-plugin-notification 会注入 window.Notification polyfill，
    // polyfill 内部调用 `plugin:notification|notify` 命令。远端页面默认禁止
    // IPC，必须显式声明 remote 能力。
    .initialization_script(
        r#"
        (function () {
          try {
            if (window.__TAURI_INTERNALS__) {
              window.__dsh_builtin_toolkit__ = true;
            }
          } catch (err) {
            console.warn("[dsh-launcher] builtin toolkit init failed:", err);
          }
        })();
        "#,
    )
    // 新窗口请求（window.open / <a target="_blank">）：WebView 默认无响应，
    // 转交系统默认浏览器打开。
    .on_new_window(|url, _features| {
        open_external(url.as_str());
        tauri::webview::NewWindowResponse::Deny
    })
    .build()
    {
        Ok(win) => {
            log_tray("open_builtin: 内置窗口已创建");
            // 关闭窗口只隐藏，不让应用退出（纯托盘应用语义）。
            let win2 = win.clone();
            win.on_window_event(move |event| {
                match event {
                    tauri::WindowEvent::Resized(size) => {
                        if Instant::now() >= persist_size_after {
                            if let Ok(scale_factor) = win2.scale_factor() {
                                save_window_size_from_physical(
                                    size.width,
                                    size.height,
                                    scale_factor,
                                );
                            }
                        }
                    }
                    tauri::WindowEvent::ScaleFactorChanged {
                        scale_factor,
                        new_inner_size,
                        ..
                    } => {
                        if Instant::now() >= persist_size_after {
                            save_window_size_from_physical(
                                new_inner_size.width,
                                new_inner_size.height,
                                *scale_factor,
                            );
                        }
                    }
                    tauri::WindowEvent::CloseRequested { api, .. } => {
                        if let (Ok(size), Ok(scale_factor)) =
                            (win2.inner_size(), win2.scale_factor())
                        {
                            save_window_size_from_physical(size.width, size.height, scale_factor);
                        }
                        api.prevent_close();
                        let _ = win2.hide();
                        // 窗口隐藏后恢复纯托盘（Accessory），不再占据 Dock / Cmd+Tab。
                        #[cfg(target_os = "macos")]
                        let app = win2.app_handle();
                        #[cfg(target_os = "macos")]
                        let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);
                    }
                    _ => {}
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

/// 启动后等待 dsh web 服务就绪，再在 Tauri 主线程弹出内置窗口。
fn spawn_startup_ui(app: AppHandle) {
    thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_millis(STARTUP_UI_WAIT_MS);
        while !port_open(WEB_PORT) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(200));
        }
        if port_open(WEB_PORT) {
            log_tray("startup_ui: dsh 已就绪，准备打开内置窗口");
        } else {
            log_tray("startup_ui: 等待 dsh 超时，仍尝试打开内置窗口");
        }

        let main_app = app.clone();
        if let Err(e) = app.run_on_main_thread(move || open_dsh_ui(&main_app)) {
            log_tray(&format!("startup_ui: 调度内置窗口失败 {e}"));
        }
    });
}

// ── 托盘 ──────────────────────────────────────────────────────────────

fn build_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let status = MenuItem::with_id(app, "status", "dsh: 检测中…", true, None::<&str>)?;
    let open = MenuItem::with_id(app, "open", "打开 dsh 界面", true, None::<&str>)?;
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
    log_tray(&format!("menu: {id}"));
    match id {
        "open" => open_dsh_ui(app),
        "restart" => {
            #[cfg(target_os = "macos")]
            macos_service::service_restart(app);
            #[cfg(target_os = "linux")]
            linux_service::service_restart(app);
        }
        "stop" => {
            #[cfg(target_os = "macos")]
            macos_service::service_stop();
            #[cfg(target_os = "linux")]
            linux_service::service_stop();
        }
        "start" => {
            #[cfg(target_os = "macos")]
            macos_service::service_start(app);
            #[cfg(target_os = "linux")]
            linux_service::service_start(app);
        }
        "autostart" => {
            let next = !AUTOSTART.load(Ordering::Relaxed);
            log_tray(&format!("autostart: 切换为 {next}"));
            #[cfg(target_os = "macos")]
            macos_service::set_autostart(app, next);
            #[cfg(target_os = "linux")]
            linux_service::set_autostart(app, next);
        }
        "log" => {
            if !log_path().exists() {
                let _ = fs::write(&log_path(), "");
            }
            open_path(&log_path());
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
            MenuItemKind::MenuItem(i) => {
                let _ = i.set_text(text);
            }
            MenuItemKind::Check(i) => {
                let _ = i.set_text(text);
            }
            MenuItemKind::Submenu(i) => {
                let _ = i.set_text(text);
            }
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
                (
                    if running {
                        "dsh: 运行中".to_string()
                    } else {
                        "dsh: 已停止".to_string()
                    },
                    true,
                )
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
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            log_tray("=== dsh-launcher 启动 ===");
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            #[cfg(target_os = "macos")]
            AUTOSTART.store(macos_service::read_autostart(), Ordering::Relaxed);
            #[cfg(target_os = "linux")]
            AUTOSTART.store(linux_service::read_autostart(), Ordering::Relaxed);
            #[cfg(target_os = "macos")]
            if let Err(e) = macos_service::ensure_definition(app.handle()) {
                eprintln!("dsh-launcher: {e}");
                log_tray(&format!("setup: 服务定义失败 {e}"));
            }
            #[cfg(target_os = "linux")]
            if let Err(e) = linux_service::ensure_definition(app.handle()) {
                eprintln!("dsh-launcher: {e}");
                log_tray(&format!("setup: 服务定义失败 {e}"));
            }

            // 打开托盘即确保 dsh 运行：web 端口不可达时自动拉起。
            // （已运行则跳过；启动是异步的，状态行先给反馈，轮询会跟进真实状态）
            if !port_open(WEB_PORT) {
                log_tray("setup: 3080 不可达，自动拉起 dsh");
                #[cfg(target_os = "macos")]
                macos_service::service_start(app.handle());
                #[cfg(target_os = "linux")]
                linux_service::service_start(app.handle());
                show_action_msg("dsh 未运行，已自动启动".to_string());
            } else {
                log_tray("setup: dsh 已在运行，跳过自动拉起");
            }

            let menu = build_menu(app.handle())?;
            log_tray("setup: 菜单构建完成");
            app.manage(AppMenu(menu.clone()));
            let _tray = TrayIconBuilder::with_id("main")
                .icon(tauri::image::Image::from_bytes(include_bytes!(
                    "../icons/128x128.png"
                ))?)
                .tooltip("DSH启动器")
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(|app, event| handle_menu(app, event.id.as_ref()))
                .build(app)?;
            log_tray("setup: 托盘构建完成");

            spawn_status_poller(app.handle().clone());
            log_tray("setup: 状态轮询已启动");
            spawn_startup_ui(app.handle().clone());
            log_tray("setup: 已安排启动时打开内置窗口");
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running dsh-launcher");
}
