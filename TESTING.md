# 测试指南

从仓库根目录执行以下命令，使用 [rust-toolchain.toml](./rust-toolchain.toml) 固定的 Rust 工具链。
脚本需要 Bash；打包及 Node 脚本测试另需 Node.js 和 npm。

## 日常检查

```sh
bash scripts/preflight.sh
bash scripts/check-packaging.sh
node --test scripts/run-service-canary.test.mjs
```

- `preflight.sh`：格式、Clippy、公开及私有 rustdoc、workspace 全部 targets 测试和 doctest。
- `check-packaging.sh`：安装器测试、发布资产与平台矩阵、实际 npm 包内容。
- `run-service-canary.test.mjs`：真实服务检查脚本自身的离线契约测试。

这三项共同覆盖常规本地检查，不调用付费模型或执行 SWE-bench 真实评测。
CI 配置见 [.github/workflows/ci.yml](./.github/workflows/ci.yml)。

## 按模块验证

| 模块 | 命令 | 主要范围 |
| --- | --- | --- |
| Agent | `cargo test --locked -p deep-code-agent --lib` | 模型协议、工具与权限、会话存储、图片、子代理、沙箱和 LSP |
| HTTP runtime | `cargo test --locked -p deep-code-runtime --lib` | 认证、SSE、审批和事件协议 |
| TUI / CLI | `cargo test --locked -p deep-code-tui` | 输入与渲染、命令、会话切换和 headless 行为 |
| Eval | `cargo test --locked -p deep-code-eval --lib` | 逐题保存、断点续跑、归档及报告 |

调试单个场景时可在 `cargo test` 命令后添加测试名过滤条件；确认输出实际运行了目标测试，
避免将匹配零项当作验证通过。具体回归场景以对应模块的测试源码为准。
SWE-bench 的运行、归档和官方评分见 [Eval 使用说明](./crates/deep-code-eval/README.md)。

## 真实服务验证

独立的 [Real services (canary)](./.github/workflows/service-canary.yml) 工作流每周运行，
也可在 Actions 手动触发。它验证真实 DeepSeek SSE、搜索与抓取、子代理，以及本机 rust-analyzer。

配置仓库 secret `DEEPCODE_CANARY_API_KEY` 后才运行真实 API 检查；缺少密钥或手动设置
`live_api=false` 时跳过这些检查，仅运行 LSP。工作流绿色不能代替确认每项是否实际执行。
带密钥的工作流只在定时或手动选定的可信代码上运行，不接收 PR 触发；手动运行前应审查目标版本。

本地检查需要 GNU `timeout`；LSP 另需 `rustup component add rust-analyzer`：

```sh
cargo test --locked -p deep-code-agent --lib --no-run
bash scripts/run-service-canary.sh lsp

# 配置 DEEPSEEK_API_KEY 后按需运行；以下命令会产生真实 API 调用费用。
bash scripts/run-service-canary.sh sse
bash scripts/run-service-canary.sh web
bash scripts/run-service-canary.sh subagent
```

真实服务测试标记为 `#[ignore]`，普通 `cargo test` 不会自动运行。请求、输出和时间预算由
测试及 [canary 脚本](./scripts/run-service-canary.sh) 定义；使用独立低额度密钥并设置账户预算。

## 验收范围

- 模拟客户端和本地子进程测试验证协议与程序行为；真实提供方的图文理解仍需单独验收。
- TUI 自动化测试之外，交互改动还需在真实终端验证输入、粘贴、审批、取消和会话恢复。
- 本机检查通过只代表当前平台；Linux、macOS、Windows 的原生行为需分别验证。
  Linux/macOS preflight 要求沙箱可用；Windows 本地可能跳过缺少权限的符号链接测试，
  CI 通过 `DEEPCODE_REQUIRE_SYMLINKS=1` 要求这些测试实际执行。
- 原生安装与发布验收见 [RELEASING.md](./RELEASING.md)。
  [Verify Install (canary)](./.github/workflows/verify-install-canary.yml) 可检查已发布版本的五平台安装，
  不会发布新版本。
