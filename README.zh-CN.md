<div align="center">

# Penflow

**通过一根 USB 线,把 Android 平板变成真正的 Windows 数位屏 — 完整压感、倾斜、Windows Ink。**

[![CI](https://github.com/zhangyushaow/zpenflow/actions/workflows/ci.yml/badge.svg)](https://github.com/zhangyushaow/zpenflow/actions/workflows/ci.yml)
[![Release](https://github.com/zhangyushaow/zpenflow/actions/workflows/release.yml/badge.svg)](https://github.com/zhangyushaow/zpenflow/actions/workflows/release.yml)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#许可证)
[![Platform](https://img.shields.io/badge/platform-Windows%2010%2B-0078d4.svg)](#平台支持)

[English](README.md) · [简体中文](README.zh-CN.md)

</div>

---

## Penflow 是什么?

Penflow 通过 USB 把 Windows 桌面投流到 Android 平板, 同时把平板上的笔事件以**一等公民 Windows Ink** 的身份回传给 PC — 压感、倾斜、悬浮、三个笔身按键、掌侧防误触全部保留。结果就是: 一台 Wacom Movink Pad (4000 RMB 左右) 摇身一变成为完整的 14" 数位屏, 直接给 Krita / Photoshop / Clip Studio / Blender 用, **端到端延迟约 26 毫秒**。

可以理解成 Wacom *EasyCanvas*、Astropad、Duet Display 的开源、跨厂商版本 — 但面向 Android, 源码全公开, 配置可以细到每个笔身按键。

> **状态**: pre-v1.0, 开发中。当前仅支持 Windows 主机; macOS 主机支持在 [路线图](#路线图) 里。

## 性能

| 指标 | 数值 | 测试条件 |
|---|---|---|
| **端到端延迟** (笔尖→像素) | **~26 毫秒** | RTX 3060+ • USB 2.0 OTG • HEVC 50 Mbps • 120 Hz |
| 抓屏 → 编码 | ~6 ms | DXGI Desktop Duplication, NVENC HEVC |
| 传输 (USB ADB 隧道) | ~3 ms | reverse-tunnel local-abstract socket |
| 解码 → 显示 | ~10 ms | MediaCodec (Android) async, surface 绑定 |
| 笔事件 → 注入 | ~7 ms | 120 Hz 抓屏循环里的一帧预算 |

延迟是用高速摄像机比对笔尖触屏与主机像素更新时刻测的, 之后用应用内的 HUD 独立验证过。

## 功能

- 🎯 **真 Windows Ink** — 压感、倾斜、悬浮、橡皮、三个笔身按键。应用看到的是 Wintab/HID 数位板, 不是模拟鼠标。
- 🚀 **GPU 直通 HEVC** — DXGI Desktop Duplication 直接把桌面抓到 D3D11 纹理, NVENC / AMF / QSV 编码, 全程不过系统内存。
- 🔌 **纯 USB 路径** — 跑在 `adb reverse` 之上, 无需 Wi-Fi 设置、无 NAT、无每个网络重新配置。插上、启动、画。
- 🖥️ **可选虚拟显示器驱动** — 扩展而非镜像桌面, 物理屏不会被平板分辨率绑架。MSI 自动安装捆绑的 `MttVDD`。
- 🎨 **每个按键独立绑定** — 笔身 1 / 笔身 2 / 第三键各可映射成 *点击* / *按住* / *鼠标键* / *橡皮切换*。出厂默认 Krita 友好。
- 🔐 **一次 UAC** — 设置里勾选"以管理员运行"时, Penflow 注册一个 Highest 等级的任务计划程序; 之后每次启动 (含开机自启) 都**静默**, 不再弹 UAC。
- 🪟 **原生 Win11 风格** — Mica 背景, Fluent UI v9 控件, 系统托盘后台服务。关窗不会切断流。

## 为什么选 Penflow?

| | Penflow | scrcpy | Spacedesk | EasyCanvas (Wacom) | Astropad / Duet |
|---|:---:|:---:|:---:|:---:|:---:|
| **方向** | PC → 平板 (PC 主控,平板画) | 平板 → PC | PC → 平板 | PC → 平板 | PC/Mac → iPad |
| **笔压感 / 倾斜** | ✅ Windows Ink | ❌ 仅触摸 | ❌ 无笔 | ✅ | ✅ |
| **三个笔身按键** | ✅ 全可配置 | n/a | n/a | 部分 | ✅ |
| **延迟** | ~26 ms | ~50–80 ms | ~80–120 ms | ~30 ms | ~25 ms |
| **传输** | USB (ADB) | USB / Wi-Fi | Wi-Fi | USB-C DP-Alt + USB | USB / Wi-Fi |
| **Android 客户端** | ✅ | ✅ | ✅ | ✅ (仅 Wacom Movink Pad) | ❌ 仅 iPad |
| **开源** | ✅ MIT/Apache | ✅ | ❌ | ❌ | ❌ |
| **按应用键位绑定** | ✅ | n/a | n/a | 部分 | 部分 |
| **价格** | 免费 | 免费 | 免费 / 付费 | 随硬件赠送 | $80+/年订阅 |

最接近的对手是 Wacom 自家的 *EasyCanvas*, 但它闭源、硬件锁定 Movink Pad 系列、不能脚本化或自定义按键。Penflow 在同样的硬件 (以及任何带数位笔的 Android 平板) 上跑同样的工作负载, 但源码在你这边。

## 平台支持

| 主机 (PC) | 状态 |
|---|---|
| Windows 11 (x64) | ✅ 支持, 主要目标 |
| Windows 10 22H2 (x64) | ✅ 支持 (Mica 自动回退到不透明) |
| Windows on ARM | 🟡 应该能编, 没测过 |
| macOS (Apple Silicon) | 🟡 路线图 — 见下 |
| Linux | ❌ v1.x 不计划支持 |

| 平板 (客户端) | 状态 |
|---|---|
| **Wacom Movink Pad Pro 14** | ✅ 参考设备, 日用过 |
| 其它 Android 平板 (Android 11+) | 🟡 数位笔通过 Android InputDevice 暴露压感的应该能用; 看运气 |
| iPad | ❌ 用 Astropad / Duet — Apple 沙盒挡死了 Penflow 需要的 USB 传输 |

### 路线图

- **v0.x** (现在): Windows 主机稳定。VDD 自动安装、笔键绑定 UI、托盘 + 任务计划提权。
- **v1.0**: 完整 Wintab packet 支持 (老应用兼容)、按应用预设、帧节奏调优器。
- **v1.x**: macOS (Apple Silicon) 主机移植。编码器抽象里已经预留了 `videotoolbox.rs` 槽位; 抓屏侧切到 `ScreenCaptureKit`, 笔注入切到 `IOHIDManager`。Android 客户端和协议保持不变。
- **v2.x**: 原生 USB 传输 (无需 ADB 依赖) — 参考 `crates/penflow-transport/` 里归档过的 AOA 实现。

## 快速安装 (Windows)

1. 从 [Releases 页面](https://github.com/zhangyushaow/zpenflow/releases) 下载最新的 **Penflow_*.msi**。
2. 运行安装包。安装程序会把 Penflow 注册到程序列表, 同时一并装好虚拟显示器驱动 (一次 UAC 弹窗)。
3. Android 平板上:
   - 启用 **开发者选项** → **USB 调试**。
   - 安装 [Penflow Android 客户端](https://github.com/zhangyushaow/zpenflow/releases) 的 APK (每个 release 也会附带)。
4. USB 连接平板。在平板上同意 *允许此电脑进行 USB 调试*。
5. 从开始菜单启动 **Penflow**。Android 端握手成功后, 状态徽章会变成 *connected*。

## 从源码构建

### 前置依赖

- Windows 10 22H2+ 或 Windows 11 (x64)
- [Rust stable](https://rustup.rs) ≥ 1.75
- [Node.js](https://nodejs.org) 20.x
- Tauri CLI: `cargo install tauri-cli --version "^2.0" --locked`
- WebView2 运行时 (Win11 自带; Win10 由 MSI 自动安装)
- Android Studio Hedgehog+ (仅构建 Android 客户端时需要)

### 完整构建

```powershell
git clone https://github.com/zhangyushaow/zpenflow.git
cd zpenflow

# 一次性: 从上游 release 下载 MttVDD + devcon.exe。
# MSI bundle 引用这些文件, 但二进制本身不入库。
powershell -ExecutionPolicy Bypass -File installer/fetch-vdd.ps1

# 前端依赖 (一次性)
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

### Dev 模式跑 GUI (不打包)

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

详见 [`ARCHITECTURE.md`](ARCHITECTURE.md) (crate 边界与 `Transport` / `EncoderBackend` trait), 以及 [`docs/design.md`](docs/design.md) (完整架构论证)。

## 文档

- [`ARCHITECTURE.md`](ARCHITECTURE.md) — 工作空间与 crate 地图、公开 trait。
- [`docs/design.md`](docs/design.md) — 权威设计文档 (抓屏管线、协议、错误分类)。
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — 开发环境、lint / test 命令、分支规则。

## 致谢与参考

Penflow 在 *PC → Android 数位屏 + Windows Ink* 这一具体生态位上确实是先行者, 但工程基础站在前人肩上:

- [**Sunshine**](https://github.com/LizardByte/Sunshine) — DXGI 低延迟抓屏技巧、编码器抽象。
- [**moonlight-android**](https://github.com/moonlight-stream/moonlight-android) — MediaCodec 异常恢复与帧节奏。
- [**scrcpy**](https://github.com/Genymobile/scrcpy) — ADB 隧道与控制协议形态。
- [**OpenTabletDriver**](https://github.com/OpenTabletDriver/OpenTabletDriver) — 笔输入数据模型、绑定语义。
- [**Virtual Display Driver**](https://github.com/VirtualDrivers/Virtual-Display-Driver) — 用于扩展(非镜像)桌面模式的 MttVDD。

每一个的具体借鉴见 [`docs/design.md`](docs/design.md) §3。

## 贡献

欢迎 PR。先读 [`CONTRIBUTING.md`](CONTRIBUTING.md) — 短版: `cargo fmt && cargo clippy -- -D warnings && cargo test` 必须全过, 改动尊重设计文档, 除非你的改动正好要更新设计文档。

## 许可证

双许可证, [**MIT**](LICENSE-MIT) 或 [**Apache-2.0**](LICENSE-APACHE), 任选其一。

捆绑的 `MttVDD` 驱动版权属其原作者, 以 MPL-2.0 发布; `devcon.exe` 版权属 Microsoft, 按 WDK 再分发条款携带。
