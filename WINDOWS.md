# Blurt Windows 维护分支说明

本分支（`windows`）是基于上游仓库 [`AGIHunt/blurt`](https://github.com/AGIHunt/blurt) 公开维护的 Windows 桌面端开发与适配分支。

## 1. 仓库与分支结构

* `upstream`：`https://github.com/AGIHunt/blurt.git`（上游官方仓库，只读拉取）
* `origin`：`https://github.com/NBananapie/blurt.git`（本公开 Fork）
* `main`：保持与 `upstream/main` 同步，不直接在该分支上堆砌 Fork 定制改动
* `windows`：Windows 长期维护分支，承载 Windows 特性适配、自用打包配置与 CI 设置

### 向上游提 PR 的方式

从最新的 `main` 切出独立 PR 分支，再从 `windows` 分支 `cherry-pick` 纯功能提交，不要把 Fork 专属配置（如禁用官方 updater、CI 触发分支扩展、`WINDOWS.md`、`AGENTS.md`）带入 PR：

```powershell
git fetch upstream
git checkout -B main upstream/main
git checkout -b pr/<feature-name> main
git cherry-pick <commit-hash>
git push -u origin pr/<feature-name>
```

## 2. 相对上游基线（`v1.0.21`）新增的 Windows 能力

1. **Windows 原生录屏与音频采集**：在 `crates/blurt-capture/src/wgc_win.rs` 中实现基于 `Windows.Graphics.Capture` + Direct3D 11 + Media Foundation（`IMFSinkWriter`）的原生录屏流程，不可用时回退至 `record.py --engine ffmpeg`。
2. **CUDA ASR 与本地 GGUF 润色/翻译**：
   * `skills/blurt/scripts/transcribe.py` 与 `asr_models.json` 新增 `Qwen3AsrCuda`（`qwen3-asr-1.7b-cuda`、`qwen3-asr-0.6b-cuda`）支持。
   * `skills/blurt/scripts/polish.py` 与 `polish_models.json` 新增 GGUF 模型下载与 `llama-server`（含 Vulkan）本地推理支持，并在桌面端提供对应命令与模型设置入口。
3. **Google Gemini（Interactions API）云端协议**：跨 Python（`polish_cloud.py`、`screen_text.py`）、Rust（`control.rs`、`dictation/cloud.rs`）与前端（`cloudPresets.ts`）接入 `gemini` / `custom-gemini` 预设。
4. **Windows 听写快捷键扩展**：`trigger.rs`、`keys_win.rs` 与设置界面新增 `RightCtrl`、`RightShift`、`CtrlWin`、`CapsLock` 及 `custom:<vk>` 自定义按键支持。
5. **Agent 路径与独立打包调整**：`agents.json` 补充 `antigravity` 与 Windows 常见 Agent 路径；`paths.rs` / `agents.py` 剥离 Windows `\\?\` 前缀；`apps/desktop/src-tauri/tauri.conf.json` 资源目录收拢到 `skill/` 下并清空官方 `updater` 配置，防止自编译包被上游更新覆盖。

## 3. 本机开发与验证环境

### 前置依赖

* **Rust 工具链**：`rustup`（`stable-x86_64-pc-windows-msvc`）与 `%USERPROFILE%\.cargo\bin`
* **Visual Studio C++ 工具链与 Windows SDK**：需安装 MSVC C++ 编译工具链以及 **Windows 10/11 SDK**（提供 `kernel32.lib` 等系统库）
* **Node.js & pnpm**：Node `>= 20.19`、`pnpm` `10.22.0`
* **Python 管理工具**：`uv`

### 常用校验命令

前端类型检查与测试（在项目根目录执行）：

```powershell
pnpm --dir web typecheck
pnpm --dir web test
```

Rust 单元测试（需先在当前命令行加载 VS 编译环境，如在 VS Developer Command Prompt 中运行，或先调用 `vcvars64.bat`）：

```powershell
cargo test --manifest-path crates/blurt-capture/Cargo.toml
cargo test --manifest-path apps/desktop/src-tauri/Cargo.toml
```

Python 单元测试：

```powershell
uv run --with pytest --with numpy --with pillow pytest tests/test_polish_cloud.py tests/test_python_version.py tests/test_transcribe_serve_packages.py -v
```

Tauri 桌面端打包（生成 NSIS / MSI 安装包）：

```powershell
pnpm --dir apps/desktop tauri build
```

## 4. 目录与忽略约定

* `.blurt/` 与 `/dist/` 已在 `.gitignore` 中忽略，本地运行数据、模型缓存与迁移脚本包不会进入版本库。
* `pnpm-lock.yaml` 不提交到仓库（与 `.github/workflows/desktop.yml` 保持一致），本地可通过 `.git/info/exclude` 忽略。
