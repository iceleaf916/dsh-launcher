# DSH启动器（dsh-launcher）

dsh（DeepSeek Harness）的系统托盘启动器：通过菜单栏控制 `dsh --profile web` 的启动、停止、重启与热重载，并可在内置浏览器或系统浏览器中打开 dsh 界面。

## 功能

- **启动 / 停止 / 重启 dsh**：菜单栏一键控制 dsh web 服务
- **热重载**：不重启进程，热重载 dsh 配置（需 dsh 支持）
- **打开 dsh 界面**：可在内置浏览器或系统浏览器中打开（菜单“内置浏览器打开”勾选切换）
- **开机自启**：勾选“开机自启 dsh”后，登录时自动启动 dsh（默认关闭）
- **崩溃自愈**：dsh 进程异常退出后会自动拉起（由系统 launchd 托管）
- **状态显示**：菜单栏实时显示 dsh 运行状态（运行中 / 已停止）
- **查看日志**：菜单“查看日志”直接打开 dsh 日志文件
- **退出托盘**：退出托盘不影响已运行的 dsh

## 安装

1. 从 [GitHub Releases](https://github.com/iceleaf916/dsh-launcher/releases) 下载 `dsh-launcher_<version>_aarch64.dmg`
2. 打开 dmg，将 `dsh-launcher.app` 拖入 `/Applications`
3. 首次启动时，若 dsh 未运行会自动拉起；之后可通过菜单栏控制

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
| 查看日志 | 打开 `~/Library/Logs/dsh-web.log` |
| 退出托盘 | 退出托盘（dsh 继续运行） |

## 依赖

- **dsh**：需已安装且可通过 shell 找到（支持 nvm / Homebrew / `~/.local/bin` 等常见安装位置）
- **Node.js**：dsh 运行所需，与 dsh 同版本目录解析
- **macOS**：当前仅支持 macOS（arm64）

## 开发

```bash
# 依赖安装
pnpm install

# debug 启动（需在 src-tauri 目录）
cd src-tauri && cargo run

# 正式打包
pnpm tauri build --bundles app   # 仅 .app
pnpm tauri build --bundles dmg   # .app + dmg
```

产物路径：

```text
src-tauri/target/release/bundle/macos/dsh-launcher.app
src-tauri/target/release/bundle/dmg/dsh-launcher_<version>_aarch64.dmg
```

## 数据与日志位置

| 内容 | 路径 |
|---|---|
| 托盘配置 | `~/Library/Application Support/dsh-launcher/config.json` |
| 托盘日志 | `~/Library/Logs/dsh-launcher.log` |
| dsh 日志 | `~/Library/Logs/dsh-web.log` |
| LaunchAgent | `~/Library/LaunchAgents/com.dsh-launcher.web.plist` |

## 卸载

彻底移除需要清理以下内容（删除 app 不会自动清理）：

```bash
launchctl bootout gui/$(id -u)/com.dsh-launcher.web
rm ~/Library/LaunchAgents/com.dsh-launcher.web.plist
rm -rf ~/Library/Application\ Support/dsh-launcher
```
