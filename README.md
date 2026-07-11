# deep-code

基于 DeepSeek 的终端 AI 编程助手（Rust 编写）。

## 特性

- **DeepSeek 流式对话**：支持 reasoning 流与工具调用（function calling）。
- **工具审批**：写文件/执行命令等需确认（`y` 批准 / `a` 本会话始终允许 / `n` 拒绝）；shell 按命令程序名在本会话放行，只读工具直接执行。
- **Auto 路由**：按任务在 `deepseek-v4-pro` / `deepseek-v4-flash` 间自动选模型与 reasoning effort；API 限流/故障时自动降级重试。
- **会话与回滚**：会话持久化、`-c` 续接、`-r` 选择恢复；每轮 checkpoint 可 `/restore` 回滚。
- **极简 TUI**：鼠标滚动/划选复制、粘贴折叠、补全菜单、状态行成本与上下文用量。
- **可扩展**：LSP 诊断、MCP、子代理（sub-agents）。

## 安装

```sh
npm i -g @liwenkai/deepcode
```

安装后命令为 `deepcode`（postinstall 会按平台从 GitHub Releases 下载预编译二进制并校验 SHA-256）。更新：

```sh
npm i -g @liwenkai/deepcode@latest
```

## 快速开始

```sh
deepcode            # 启动(新会话)
```

启动后设置 DeepSeek API Key（也可用环境变量 `DEEPSEEK_API_KEY`）：

```
/apikey sk-...
```

## 用法

```
deepcode                 # 新会话
deepcode -c              # 续最近会话
deepcode -r              # 选择历史会话
deepcode doctor [--json] # 环境自检
deepcode serve --http    # 作为 HTTP 服务运行
deepcode eval            # SWE-bench 评测 rollout(见下文)
deepcode session list|resume|delete|export
deepcode mcp list|validate|reload|enable|disable
```

常用 slash 命令：`/help` `/model` `/apikey` `/resume` `/clear` `/sessions` `/checkpoints` `/restore` `/agents` `/copy`（`/help` 查看全部与快捷键）。

## 配置

配置文件：`~/.deep-code/config.toml`（可参考仓库根目录的 `config.example.toml`）。
加载顺序：内置默认 → 全局 → 项目 `.deep-code/config.toml` → 环境变量 → CLI 参数。

常用项：`provider.model`（`pro`/`flash`/`auto`）、`provider.reasoning_effort`（`off`/`low`/`medium`/`high`/`max`）、`cost.currency`、`approval.auto_allow`（预放行的工具前缀）。
> API Key 建议放在环境变量或全局配置；项目级配置中的 `api_key` 会被忽略，以防随仓库泄露。

## 评测(SWE-bench)

内置 SWE-bench rollout 驱动：拉官方数据集，驱动 agent 逐题产出 patch，
写出官方格式的 `predictions.json`。**本地不打分**——patch 产出 ≠ 解决，
真实 resolved 率由官方评测（sb-cli 云端，免本地 Docker）得出。

```sh
# 需已配置 DeepSeek API key（未配置会直接报错，不会空跑）
deepcode eval --sample 2                  # 联调：dev split 先跑 2 题
deepcode eval                             # dev 全量（23 题，约几分钱）
deepcode eval --split test --parallel 4 --timeout 900   # test 全量（300 题）
```

产物在 `eval-out/`（可用 `--out` 改）：`predictions.json`（官方格式）+
`report.json`（含每题耗时、成本、模型与路由来源）。

官方评分：

```sh
pip install sb-cli
sb-cli gen-api-key you@example.com   # 一次性，邮件验证后 export SWEBENCH_API_KEY=...
sb-cli submit swe-bench_lite dev --predictions_path eval-out/predictions.json --run_id my-run
```

网络提示：数据集来自 HuggingFace datasets-server，部分网络需
`HTTPS_PROXY=http://127.0.0.1:<port>` 或用 `DEEP_CODE_HF_BASE` 指向镜像。
参数细节见 `crates/deep-code-eval/README.md`。

## 从源码构建

```sh
git clone https://github.com/liwenka1/deep-code
cd deep-code
cargo build --release -p deep-code-tui
# 产物：target/release/deep-code
```

## 许可证

MIT
