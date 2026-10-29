# Codex Usage Monitor · 本地增强版

基于 [upstream-ray/codex-usage-monitor](https://github.com/upstream-ray/codex-usage-monitor) 的 Windows 任务栏用量监控程序，使用 Rust 和 Win32 原生界面。本仓库增加代理配置和每周定时预热，并改进托盘图标清理和诊断记录。

## 增加了什么

| 能力 | 实现方式 |
| --- | --- |
| 可保存的代理设置 | 托盘菜单提供自定义 HTTP 代理、跟随环境变量和直接连接三种模式；写入 `settings.json`，统一配置额度请求、更新下载和本地 CLI 子进程的代理。 |
| 每周定时预热 | 独立开关，周一至周日各设多个时刻；到点通过当前代理发送一次小型 Codex 请求。执行前落盘去重，错过不补发，失败不自动重复同一时刻。 |
| 托盘退出清理 | 自重启和消息循环退出前显式删除图标；串行处理刷新与清理，退出开始后禁止重新注册，减少旧图标残留。 |
| 跨重启诊断日志 | 日志追加写入并记录 PID、图标添加/删除结果和重启事件，方便排查偶发问题。 |

## 使用

右键托盘或任务栏组件 → **设置 → 代理设置**，选择自定义代理，例如 `http://127.0.0.1:7897`，保存后触发刷新。配置重启后保留，HTTP 代理也支持 HTTPS 请求。

**设置 → Codex 定时预热** 可配置每天 `07:00, 13:00, 19:00` 等时刻并一键应用到整周。默认关闭，按电脑本地时间、仅在程序运行时执行；会消耗真实用量，服务端已有窗口不会被强制重置。模型可留空自动选择，或填写指定型号。

运行 `codex-usage.exe --diagnose` 开启诊断，日志位于 `%TEMP%\codex-usage.log`。

## 构建

环境：Windows x64、Rust stable MSVC 工具链、MSVC Build Tools 和 Windows SDK。

```powershell
cargo test --locked
cargo build --release --locked
```

代理路由、配置保存和原生窗口已有回归测试；托盘偶发残留是否完全消除，仍需实际使用验证。

## 来源与许可

这是上游项目的独立增强分支，保留上游 Git 历史和 [MIT 许可证](LICENSE)。内嵌第三方图标的许可见 [providers/LICENSE](src/icons/providers/LICENSE)。
