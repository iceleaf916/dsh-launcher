# DSH启动器（dsh-launcher）

dsh（DeepSeek Harness）的系统托盘启动器：通过菜单栏控制 `dsh --profile web` 的启动、停止、重启与热重载，并可在内置浏览器或系统浏览器中打开 dsh 界面。

## 功能

- **启动 / 停止 / 重启 dsh**：菜单栏一键控制 dsh web 服务
- **热重载**：不重启进程，热重载 dsh 配置（需 dsh 支持）
- **打开 dsh 界面**：可在内置浏览器或系统浏览器中打开（菜单“内置浏览器打开”勾选切换）
- **开机自启**：勾选“开机自启 dsh”后，登录时自动启动 dsh（默认关闭）
- **崩溃自愈**：dsh 进程异常退出后会自动拉起（macOS 由 launchd 托管，Linux 由 systemd user service 托管）
- **状态显示**：菜单栏实时显示 dsh 运行状态（运行中 / 已停止）
- **查看日志**：菜单“查看日志”直接打开 dsh 日志文件
- **退出托盘**：退出托盘不影响已运行的 dsh

## 支持平台

| 平台 | 架构 | 安装包 |
|---|---|---|
| macOS | arm64 | `.dmg` |
| Linux (Debian/Ubuntu) | x86_64 | `.deb` |

## 安装

### macOS

1. 从 [GitHub Releases](https://github.com/iceleaf916/dsh-launcher/releases) 下载 `dsh-launcher_<version>_aarch64.dmg`
2. 打开 dmg，将 `dsh-launcher.app` 拖入 `/Applications`
3. 由于当前发布包未进行 Apple 官方签名（未加入 Apple Developer Program），
   首次打开前需要手动移除隔离属性，否则 macOS 会拦截启动：

```bash
# 方式一：先解除 Gatekeeper 隔离再首次启动（推荐）
xattr -dr com.apple.quarantine /Applications/dsh-launcher.app

# 方式二：首次启动时在“访达”中右键点击 dsh-launcher.app → 打开 → 再点“打开”
```

4. 首次启动时，若 dsh 未运行会自动拉起；之后可通过菜单栏控制

> 说明：`xattr -dr` 会移除整个应用的隔离标记。若系统提示“无法验证开发者”，
> 执行该命令后重新打开即可；如仍被拦截，可在“系统设置 → 隐私与安全性”中点击“仍要打开”。

### Linux (Debian/Ubuntu)

```bash
sudo apt install ./dsh-launcher_<version>_amd64.deb
```

安装后可从应用菜单启动 `DSH启动器`，或执行 `dsh-launcher`。

## 使用

启动后菜单栏出现 DSH 鲸鱼图标，点击展开菜单：

| 菜单项 | 作用 |
|---|---|
| 状态行 | 显示 dsh 运行状态 |
| 打开 dsh 界面 | 打开 dsh Web 界面（默认系统浏览器） |
| 内置浏览器打开 | 勾选后改用内置浏览器打开界面 |
| 重启 dsh | 重启 dsh 服务 |
| 热重载 dsh（控制面） | 热重载 dsh 配置 |
| 停止 dsh / 启动 dsh | 停止 / 启动 dsh 服务 |
| 开机自启 dsh | 登录时自动启动 dsh（默认关） |
| 查看日志 | 打开 dsh 日志文件 |
| 退出托盘 | 退出托盘（dsh 继续运行） |

## 依赖

- **dsh**：需已安装且可通过 shell 找到（支持 nvm / Volta / Homebrew / `~/.local/bin` 等常见安装位置）
- **Node.js**：dsh 运行所需，与 dsh 同版本目录解析
- **Linux**：systemd（用户级服务），`libwebkit2gtk-4.1`、`libgtk-3`、`libappindicator3`（deb 已声明依赖）

## 服务与数据位置

### macOS

| 内容 | 路径 |
|---|---|
| 托盘配置 | `~/Library/Application Support/dsh-launcher/config.json` |
| 托盘日志 | `~/Library/Logs/dsh-launcher.log` |
| dsh 日志 | `~/Library/Logs/dsh-web.log` |
| LaunchAgent | `~/Library/LaunchAgents/com.dsh-launcher.web.plist` |

### Linux

| 内容 | 路径 |
|---|---|
| 托盘配置 | `~/.config/dsh-launcher/config.json` |
| 托盘日志 | `~/.local/state/dsh-launcher/dsh-launcher.log` |
| dsh 日志 | `~/.local/state/dsh-launcher/dsh-web.log` |
| systemd unit | `~/.config/systemd/user/dsh-launcher-web.service` |

> 遵循 XDG 规范：`XDG_CONFIG_HOME` / `XDG_STATE_HOME` 被设置时优先使用对应目录。

## 开发

```bash
# 依赖安装
pnpm install

# debug 启动（需在 src-tauri 目录）
cd src-tauri && cargo run

# 正式打包（macOS）
pnpm tauri build --bundles app   # 仅 .app
pnpm tauri build --bundles dmg   # .app + dmg

# 正式打包（Linux x86_64 deb，需在 Linux 环境或 CI 中执行）
pnpm tauri build --bundles deb --target x86_64-unknown-linux-gnu
```

产物路径：

```text
src-tauri/target/release/bundle/macos/dsh-launcher.app
src-tauri/target/release/bundle/dmg/dsh-launcher_<version>_aarch64.dmg
src-tauri/target/x86_64-unknown-linux-gnu/release/bundle/deb/dsh-launcher_<version>_amd64.deb
```

### CI 构建

仓库内置 [release workflow](.github/workflows/release.yml)：

- 手动触发（`workflow_dispatch`）或在 `v*` tag 推送时自动触发
- Linux job：`cargo check` + 构建 x86_64 deb
- macOS job：构建 aarch64 dmg
- 打 tag 时自动创建 GitHub Release 并附上 deb + dmg 产物

## 卸载

### macOS

```bash
launchctl bootout gui/$(id -u)/com.dsh-launcher.web
rm ~/Library/LaunchAgents/com.dsh-launcher.web.plist
rm -rf ~/Library/Application\ Support/dsh-launcher
```

### Linux

```bash
systemctl --user disable --now dsh-launcher-web.service
rm ~/.config/systemd/user/dsh-launcher-web.service
systemctl --user daemon-reload
rm -rf ~/.config/dsh-launcher ~/.local/state/dsh-launcher
sudo apt remove dsh-launcher
```
