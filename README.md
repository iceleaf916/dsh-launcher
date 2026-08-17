# dsh-tray

dsh（DeepSeek Harness）的跨平台托盘管理器。

## 目标

让 `dsh --profile web` 常驻系统托盘，提供菜单控制：

- 状态显示（运行中 / 已停止）
- 打开 Web 界面
- 重启 / 停止 / 启动 dsh
- 开机自启开关
- 退出托盘（不影响 dsh 运行）

## 技术栈

- **UI/壳**：Tauri 2（Rust + 系统托盘 API），跨平台
- **控制面（v2 增强）**：dsh 插件，暴露状态 / 优雅停机 / 热重载入口

## 结构

```
dsh-tray/
├── src-tauri/        # Tauri 2 Rust 端（托盘、进程管理）
├── src/              # 前端（如需要窗口）
├── plugins/          # dsh 控制面插件（可选）
└── README.md
```

## 状态

骨架初始化中。
