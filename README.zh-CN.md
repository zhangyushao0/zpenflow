<div align="center">

# Penflow

**通过一根 USB 线，把 Wacom Movink Pad Pro 14 变成真正的 Windows 数位屏 — 完整压感、倾斜、Windows Ink。**

[![CI](https://github.com/zhangyushao0/zpenflow/actions/workflows/ci.yml/badge.svg)](https://github.com/zhangyushao0/zpenflow/actions/workflows/ci.yml)
[![Release](https://github.com/zhangyushao0/zpenflow/actions/workflows/release.yml/badge.svg)](https://github.com/zhangyushao0/zpenflow/actions/workflows/release.yml)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#许可证)
[![Platform](https://img.shields.io/badge/platform-Windows%2010%2B-0078d4.svg)](#平台支持)

[English](README.md) · [简体中文](README.zh-CN.md)

</div>

---

## Penflow 是什么？

Penflow 通过 USB 把 Windows 桌面投流到 **Wacom Movink Pad Pro 14**，同时把平板上的笔事件以**一等公民 Windows Ink** 的身份回传给 PC — 压感（8192 级）、倾斜、悬浮、Pro Pen 3 的全部三个笔身按键全部保留。端到端延迟约 **26 毫秒**。

Penflow 是 **[Wacom Instant Pen Display Mode](https://community.wacom.com/en-sg/how-to-use-instant-pen-display-mode-movinkpad-tablet/)** 的免费开源替代 — 那是 Wacom 自家（目前 beta、仅支持 Movink Pad 系列）的 PC 连接软件。相比 Wacom 的官方方案，Penflow 的差异在于：

- 通过捆绑的虚拟显示器驱动（VDD）做一块 **正好 2880×1800 的 120 Hz 虚拟显示器**，平板上看到的是原生 1:1 像素、不经任何缩放。Wacom IPD 是 mirror + scale，把源屏拉伸/适配到平板面板，设置里**没有原生分辨率开关**（[Gigazine 2025/12 实测](https://gigazine.net/gsc_news/en/20251206-instant-pen-display-mode/) 提到曲线放大后会变模糊）。整条管线还跑在 120 Hz，是 Wacom 60 Hz 路径的两倍。
- 同一台机器上实测，笔尖到像素的延迟从 Wacom 的 **~60–70 ms** 降到 **~26 ms**。
- **Pro Pen 3 的三个侧键**全部可独立绑定（点击 / 按住 / 鼠标键 / 橡皮切换）。Wacom 的 PC 模式根本不会把侧键事件传给 Windows。
- **完全开源**，可定制到协议层。

> **状态**：pre-v1.0，开发中。当前仅支持 Windows 主机；macOS 主机支持在 [路线图](#路线图) 里。

## 性能

| 指标 | 数值 | 测试条件 |
|---|---|---|
| **端到端延迟**（笔尖→像素） | **~26 毫秒** | RTX 5070 • USB 2.0 OTG • HEVC 50 Mbps • 120 Hz 抓屏 |
| 抓屏 → 编码 | ~6 ms | DXGI Desktop Duplication, NVENC HEVC |
| 传输（USB ADB 隧道） | ~3 ms | reverse-tunnel local-abstract socket |
| 解码 → 显示 | ~10 ms | MediaCodec (Android) async, surface 绑定 |
| 笔事件 → 注入 | ~7 ms | 120 Hz 抓屏循环里的一帧预算 |

延迟用高速摄像机比对笔尖触屏与主机像素更新时刻测得，再用应用内 HUD 独立验证。

### GPU 支持

编码器走 Windows Media Foundation 的硬件 MFT 路径，按 adapter Vendor ID 选择对应厂商的硬件编码器。三家桌面 GPU 厂商在代码里都覆盖到了：

| 厂商 | 编码器 MFT | 状态 |
|---|---|---|
| **NVIDIA** | NVENC | ✅ 日用过（RTX 5070） |
| Intel | Quick Sync (QSV) | 🟡 代码路径已存在；**真机未验证** |
| AMD | AMF | 🟡 代码路径已存在；**真机未验证** |

如果你在 Intel Arc / 核显或 AMD Radeon 上跑出来了（或者跑不起来），欢迎开 Issue 附 `dxdiag` 输出 — 这是关掉这一格的方式。

## 功能

- 🎯 **真 Windows Ink** — 压感（8192 级）、倾斜、悬浮、橡皮。应用看到的是 Wintab/HID 数位板，不是模拟鼠标。
- 🖊️ **Pro Pen 3 三键全部可绑** — Switch 1 / Switch 2 / Switch 3 各自映射成 *点击* / *按住* / *鼠标键* / *橡皮切换*。出厂默认 Krita 友好。
- 🚀 **GPU 直通 HEVC** — DXGI Desktop Duplication 直接把桌面抓到 D3D11 纹理；编码器 MFT 在同一张纹理上跑，全程不过系统内存。
- 🔌 **纯 USB 路径** — 跑在 `adb reverse` 之上，无需 Wi-Fi 设置、无 NAT、无每个网络重新配置。插上、启动、画。
- 🖥️ **120 Hz 虚拟扩展屏** — 捆绑的 `MttVDD` 把平板暴露成一块独立的 120 Hz 扩展桌面，而不是 60 Hz 主屏镜像。整条管线（抓屏 → 编码 → 解码 → 渲染）全程 120 Hz 跑，落笔的流畅度是 Wacom 60 Hz 路径的两倍。

## 为什么选 Penflow？

目前能让 PC 驱动 Movink Pad 当数位屏的两条路，并排对比：

| | Penflow | Wacom Instant Pen Display |
|---|:---:|:---:|
| **方向** | PC → 平板 | PC → 平板 |
| **笔压感 / 倾斜** | ✅ Windows Ink | ✅ Windows Ink |
| **Pro Pen 3 三个侧键** | ✅ 三键全可配 | ❌ IPD 模式下不向 PC 暴露 |
| **平板原生 1:1 像素（无缩放）** | ✅（VDD 直接做 2880×1800） | ❌ 拉伸镜像；IPD 设置里无原生分辨率选项 |
| **刷新率** | **120 Hz** | 60 Hz |
| **延迟（有线）** | **~26 ms** | ~60–70 ms |
| **传输** | USB (ADB) | USB 或 Wi-Fi |
| **开源** | ✅ MIT/Apache | ❌ |
| **价格** | 免费 | 免费（beta） |

> **关于上面的延迟和刷新率数字**：两组都是项目作者在同一台机器（Movink Pad Pro 14 + RTX 5070，USB 直连）上自己测的。Wacom 并未公布 Instant Pen Display Mode 的官方端到端延迟值，所以 60–70 ms 这个数字是**我们自己测的，不是厂商口径**。如果你测出来不一样，欢迎 [开 Issue](https://github.com/zhangyushao0/zpenflow/issues)，我们会更新表格。

## 平台支持

| 主机 (PC) | 状态 |
|---|---|
| Windows 11 (x64) | ✅ 支持，主要目标 |
| Windows 10 22H2 (x64) | ✅ 支持（Mica 自动回退到不透明） |
| Windows on ARM | 🟡 应该能编，没测过 |
| macOS (Apple Silicon) | 🟡 路线图 — 见下 |
| Linux | ❌ v1.x 不计划支持 |

| 平板 (客户端) | 状态 |
|---|---|
| **Wacom Movink Pad Pro 14** | ✅ 参考设备，日用过 |
| Wacom Movink Pad 11 | 🟡 同款 Pro Pen 3 + 同 Android 系统底子；理论可用，未测 |
| 其它 Android 平板 (Android 11+) | 🟡 数位笔通过 Android InputDevice 暴露压感的应该能用；看运气 |
| iPad | ❌ 用 Astropad / Duet — Apple 沙盒挡死了 Penflow 需要的 USB 传输 |

### 路线图

- **v0.x**（现在）：Windows 主机稳定。VDD 自动安装、笔键绑定 UI、托盘 + 任务计划提权。
- **v1.0**：完整 Wintab packet 支持（老应用兼容）、按应用预设、帧节奏调优器、Intel QSV / AMD AMF 路径真机验证。
- **v1.x**：macOS (Apple Silicon) 主机移植。编码器抽象里已经预留了 `videotoolbox.rs` 槽位；抓屏侧切到 `ScreenCaptureKit`，笔注入切到 `IOHIDManager`。Android 客户端和协议保持不变。
- **v2.x**：原生 USB 传输（无需 ADB 依赖） — 参考 `crates/penflow-transport/` 里归档过的 AOA 实现。

## 快速安装 (Windows)

1. 从 [Releases 页面](https://github.com/zhangyushao0/zpenflow/releases) 下载最新的 **Penflow_*.msi**。
2. 运行安装包。安装程序会把 Penflow 注册到程序列表，同时一并装好虚拟显示器驱动（一次 UAC 弹窗）。
3. Movink Pad Pro 14 上：
   - 启用 **开发者选项** → **USB 调试**。
   - 安装 [Penflow Android 客户端](https://github.com/zhangyushao0/zpenflow/releases) 的 APK（每个 release 也会附带）。
4. USB 连接平板。在平板上同意 *允许此电脑进行 USB 调试*。
5. 从开始菜单启动 **Penflow**。Android 端握手成功后，状态徽章会变成 *connected*。

## 从源码构建

### 前置依赖

- Windows 10 22H2+ 或 Windows 11 (x64)
- [Rust stable](https://rustup.rs) ≥ 1.76
- [Node.js](https://nodejs.org) 20.x
- Tauri CLI: `cargo install tauri-cli --version "^2.0" --locked`
- WebView2 运行时（Win11 自带；Win10 由 MSI 自动安装）
- Android Studio Hedgehog+（仅构建 Android 客户端时需要）

### 完整构建

```powershell
git clone https://github.com/zhangyushao0/zpenflow.git
cd zpenflow

# 一次性：从上游 release 下载 MttVDD + devcon.exe。
# MSI bundle 引用这些文件，但二进制本身不入库。
powershell -ExecutionPolicy Bypass -File installer/fetch-vdd.ps1

# 前端依赖（一次性）
cd apps/penflow-gui/ui
npm install
cd ../../..

# 工作空间健康检查
cargo build --workspace
cargo test --workspace

# 构建 MSI
cd apps/penflow-gui
cargo tauri build --bundles msi
# → target/release/bundle/msi/Penflow_<version>_x64_en-US.msi
```

### Dev 模式跑 GUI（不打包）

```powershell
cd apps/penflow-gui
cargo tauri dev
```

## 架构概览

```
zpenflow/
├── crates/
│   ├── penflow-protocol/        协议常量与类型
│   ├── penflow-transport/       Transport trait + ADB 实现
│   ├── penflow-core/            DXGI 抓屏 / MF·NVENC 编码 / WinRT 笔注入
│   └── penflow-server/          tokio 会话编排 + VDD 生命周期
├── apps/penflow-gui/            Tauri 2 + React + Fluent UI 桌面应用
├── android/                     Android 客户端 (Kotlin)
├── installer/                   WiX MSI 片段 + VDD 抓取脚本
└── docs/                        设计与研究笔记
```

详见 [`ARCHITECTURE.md`](ARCHITECTURE.md)（crate 边界与 `Transport` / `EncoderBackend` trait），以及 [`docs/design.md`](docs/design.md)（完整架构论证）。

## 文档

- [`ARCHITECTURE.md`](ARCHITECTURE.md) — 工作空间与 crate 地图、公开 trait。
- [`docs/design.md`](docs/design.md) — 权威设计文档（抓屏管线、协议、错误分类）。
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — 开发环境、lint / test 命令、分支规则。

## 致谢与参考

Penflow 在 *PC → Wacom Movink Pad + Windows Ink* 这一具体生态位上确实是先行者，但工程基础站在前人肩上：

- [**Sunshine**](https://github.com/LizardByte/Sunshine) — DXGI 低延迟抓屏技巧、编码器抽象。
- [**moonlight-android**](https://github.com/moonlight-stream/moonlight-android) — MediaCodec 异常恢复与帧节奏。
- [**scrcpy**](https://github.com/Genymobile/scrcpy) — ADB 隧道与控制协议形态。
- [**OpenTabletDriver**](https://github.com/OpenTabletDriver/OpenTabletDriver) — 笔输入数据模型、绑定语义。
- [**Virtual Display Driver**](https://github.com/VirtualDrivers/Virtual-Display-Driver) — 用于扩展（非镜像）桌面模式的 MttVDD。

每一个的具体借鉴见 [`docs/design.md`](docs/design.md) §3。

## 贡献

欢迎 PR。先读 [`CONTRIBUTING.md`](CONTRIBUTING.md) — 短版：`cargo fmt && cargo clippy -- -D warnings && cargo test` 必须全过，改动尊重设计文档，除非你的改动正好要更新设计文档。

## 许可证

双许可证，[**MIT**](LICENSE-MIT) 或 [**Apache-2.0**](LICENSE-APACHE)，任选其一。

捆绑的 `MttVDD` 驱动版权属其原作者，以 MPL-2.0 发布；`devcon.exe` 版权属 Microsoft，按 WDK 再分发条款携带。*Wacom*、*Movink*、*Pro Pen 3* 是 Wacom Co., Ltd 的商标。Penflow 与 Wacom 公司没有任何关联或背书关系。
