# Blurt Windows Fork 协作与开发守则

本文件用于约束在此仓库（`NBananapie/blurt`）进行持续开发、测试与向 `AGIHunt/blurt` 提交 PR 时的仓库规范。

## 1. 分支与提交边界

* **保持 `main` 纯净**：`main` 仅用于快进同步 `upstream/main`（`AGIHunt/blurt`），不得在 `main` 上直接提交自定义修改。
* **日常开发在 `windows` 或特性分支进行**：所有 Windows 适配与功能迭代合入 `windows` 分支；需要向上游贡献时，从最新 `main` 切出新分支并仅 `cherry-pick` 对应功能提交。
* **隔离 Fork 专属提交**：以下内容仅保留在 `windows` 分支，不得混入发往上游的 PR：
  * `apps/desktop/src-tauri/tauri.conf.json` 中清空官方 `updater.endpoints` / `pubkey` 的配置
  * `.github/workflows/*.yml` 中为触发 `windows` 分支 CI 所做的 `branches` 扩展
  * `WINDOWS.md` 与 `AGENTS.md`
* **禁止提交产物与锁文件**：不得提交 `.blurt/`、`/dist/`、模型权重文件以及 `pnpm-lock.yaml`。

## 2. Windows 编码与架构约定

* **隔离平台专有实现**：Windows 原生 API 与钩子逻辑优先收敛在 `#[cfg(windows)]` 块或独立文件（如 `crates/blurt-capture/src/wgc_win.rs`、`apps/desktop/src-tauri/src/dictation/keys_win.rs`）中，减少对 macOS（`skills/blurt/recorder/macos/`）及跨平台公共签名的侵入式修改，以降低后续合入上游更新的冲突面。
* **路径与文本编码**：
  * Windows 下处理 `resource_dir()` 或 `Path::canonicalize()` 返回路径时，注意剥离 `\\?\` 前缀后再传给 Python 或外部工具。
  * PowerShell 脚本、Python 子进程与配置文件读写均显式使用 UTF-8 编码。

## 3. 提交前回归基线

修改对应模块后，提交前应按范围跑通验证（不跳过失败用例或放宽断言）：

* **前端（`web/`）**：`pnpm --dir web typecheck` 与 `pnpm --dir web test` 全部通过。
* **Rust（`crates/blurt-capture/`、`apps/desktop/src-tauri/`）**：在包含 MSVC 工具链与 Windows 10/11 SDK 的环境下执行 `cargo test` 并全部通过。
* **Python（`skills/blurt/scripts/`、`tests/`）**：使用 `uv run --with pytest --with numpy --with pillow pytest` 执行受影响测试集并全部通过。
