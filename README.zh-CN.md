# deep-code

[English](./README.md) | 简体中文

基于 DeepSeek 的终端 AI 编程助手,Rust 编写。一个小体积二进制:流式 TUI、OS 级沙箱、角色化子代理、安全优先的执行策略。

## 亮点

**小、快、自足**

- 每平台单一原生二进制(约 4–5 MB),运行时不依赖 Node/Python。预编译覆盖 macOS(arm64/x64)、Linux(x64/arm64,glibc ≥ 2.35)、Windows(x64);`npm i -g` 按平台下载并校验 SHA-256。musl 宿主(Alpine 及多数 slim 镜像)暂不支持——安装器会检测并给出明确报错,而不是装一个跑不起来的二进制。

**安全优先的执行**

- **四档权限**——`default` / `accept_edits` / `auto` / `yolo`,Shift+Tab 循环切换。`auto` 档把日常审批交给低价 Flash 判官,但有它永远越不过的硬底:最高风险命令与任何申请联网的调用,一律问人。
- **OS 沙箱(macOS / Linux)**——shell/job 命令在 macOS Seatbelt 或 Linux Landlock+seccomp 内运行:写入限制在工作区与系统临时目录,**默认不带网络**。需要联网的命令(装依赖、`git push`、dev server)必须声明并转人工审批;`[sandbox] network = prompt|always|never` 可调,项目层配置只许收紧。没有可用沙箱后端时拒绝执行,而非静默裸跑。
  **Windows 请务必知悉**:那里只有 Job Object 进程树收容,**既不限制文件写、也不拦网络**,`network` 设置在该平台是空操作。deny 底板与审批门仍然生效,但"越界写会被替你拒掉"在 Windows 上不成立。`deepcode doctor` 会如实报告本机究竟约束了什么。
- **deny 底板**——毁灭性命令在任何档位都硬拒、不可加白:系统根上的 `rm -rf`、磁盘格式化(`format C:`、卷 GUID/设备路径/`\\?\` 拼法、`diskpart`)、注册表删除等——包括它们的 Windows 形态。
- **不外溢的信任**——shell 的"始终允许"按命令 identity(程序 + 子命令)匹配:信任了 `git push` 不会连带放行 `git status`;会改变*执行什么/写到哪*的 flag(`--config`、`--exec-path`、`--output`、`--target-dir` 等)会击穿信任匹配、重新弹审批。shell 元字符(`$`、反引号、重定向)一律转审批,不做乐观解析。

**带真护栏的子代理**

- 用一次阻塞式 `agent` 调用把调查或实现委托给子代理;同一轮发多个调用即并行。六个角色——`general` / `explore` / `plan` / `review` / `verifier` 严格只读;**唯有 `implementer` 可写**,且派遣它本身就是一个审批点:人批准这次派遣,子代理的工作区写入随后免打扰进行(在写操作本会弹窗的档位上)。
- 侦察角色(`explore` / `review` / `verifier`)固定跑低价 flash 档——扇出的 token 烧在最便宜的地方;子代理进度实时流入父会话:`[explore] +41s step 7/50: grep_files`,长时间运行的子代理不再像卡死。
- 子代理的 token 花费折入父会话的成本统计。

**可信赖的会话**

- 持久化 + `-c` 续接 + `-r` 选择恢复;每轮开始前快照,`/restore` 回滚,文件系统支持时用写时复制克隆(APFS / Btrfs / XFS)。
- 上下文自动压缩,摘要携带有界;成本按请求逐次记账(含缓存命中/未命中与节省、子代理花费),`/status` 查看,币种可选。

**一个诚实的 TUI**

- 流式回复带 DeepSeek reasoning、鼠标滚动/划选复制、粘贴折叠、补全菜单。流式中可继续输入——排队并在本回合结束后作为追问自动发出(mid-turn steering)。
- 审批面板带真实变更预览;运行中的工具显示自己的耗时钟(`agent … · 47s`),不再像冻屏;状态行保持极简(档位、生效模型、上下文占用)。
- 双语界面(English / 中文),`/lang` 热切换。端到端优雅停机:SIGTERM/SIGINT 有界收尾,进程组整树击杀——不会留下占着端口的孤儿 dev server。

**模型路由**

- `auto` 按任务在 `deepseek-v4-pro` / `deepseek-v4-flash` 间选择模型与 reasoning effort;限流或上游故障时自动降级重试。可用 `/model` 或 `provider.model` 固定。

## 安装

```sh
npm i -g @liwenkai/deepcode
```

安装后命令为 `deepcode`(postinstall 会按平台从 GitHub Releases 下载预编译二进制并校验 SHA-256)。更新:

```sh
npm i -g @liwenkai/deepcode@latest
```

## 快速开始

```sh
deepcode            # 启动(新会话)
```

启动后设置 DeepSeek API Key(也可用环境变量 `DEEPSEEK_API_KEY`):

```
/apikey sk-...
```

## 用法

```
deepcode                 # 新会话
deepcode -c              # 续最近会话
deepcode -r              # 选择历史会话
deepcode --new           # 显式新会话
deepcode --help          # 命令一览(--version 查版本)
deepcode doctor [--json] # 环境自检
deepcode serve --http    # 作为 HTTP 服务运行
deepcode eval            # SWE-bench 评测 rollout(见下文)
deepcode session list|resume|delete|export
```

常用 slash 命令:`/help` `/model` `/apikey` `/lang` `/resume` `/clear` `/sessions` `/checkpoints` `/restore` `/agents` `/copy`(`/help` 查看全部与快捷键)。

## 配置

配置文件:`~/.deep-code/config.toml`(可参考仓库根目录的 `config.example.toml`)。
加载顺序:内置默认 → 全局 → 项目 `.deep-code/config.toml` → 环境变量 → CLI 参数。

常用项:`provider.model`(`pro`/`flash`/`auto`)、`provider.reasoning_effort`(`off`/`low`/`medium`/`high`/`max`)、`cost.currency`、`approval.auto_allow`(预放行的工具前缀)。

> API Key 建议放在环境变量或全局配置;项目级配置中的 `api_key` 会被忽略,以防随仓库泄露。

环境变量 `DEEP_CODE_DISABLE_WEB`:设为**非空**且非 `0`/`false`/`off`/`no` 的值(大小写不敏感;空值视为未设置)即可关闭联网工具(`web_search`/`fetch_url`),用于断网或审计场景;默认开启。`/status` 会显示当前 `web=on|off`。

在 macOS / Linux 上,shell/job 命令在 OS 沙箱内运行且**默认不带网络**(含端口监听):需要联网的命令会转人工审批,"本会话记住"后同形态命令不再问;`[sandbox] network = prompt|always|never` 可调,项目层只许收紧。**Windows 上没有沙箱约束**,因此本段的断网与写入限制均不生效(`network` 设置是空操作),声明联网仍会转审批、deny 底板仍然生效;跑 `deepcode doctor` 看本机实况。

## 扩展能力(skills + shell)

deep-code **不内置 MCP**。它本来就有 shell,所以扩展能力的方式是**写个脚本/命令 + 一份 `SKILL.md`**:一行摘要注入系统提示、模型按需读取 SKILL.md 正文,再通过 `shell` 工具调用你的脚本。

- **发现**:把带 `name`/`description` frontmatter 的 `SKILL.md` 放进 skills 目录(全局 `~/.deep-code/skills/<name>/` 或项目 `<workspace>/.deep-code/skills/<name>/`);其一行摘要常驻提示,正文只在相关时才读入上下文。
- **执行**:能力就是普通命令(`psql`、`curl`、你自己的脚本……),经 `shell` 工具运行,同样受审批门与执行策略约束。
- **为什么不做 MCP**:对有 shell 的 agent,shell 就是通用工具协议。一份几十 token 的 SKILL.md 摘要按需加载,比把整套工具 schema 常驻每一轮请求更省上下文,还能用管道(`| head` 等)裁剪结果、只把关键片段带回上下文。确需现成的 MCP 生态 server 时,用支持 MCP 的宿主即可——deep-code 保持精简。

## 评测(SWE-bench)

内置 SWE-bench rollout 驱动:拉官方数据集,驱动 agent 逐题产出 patch,写出官方格式的 `predictions.json`。**本地不打分**——patch 产出 ≠ 解决,真实 resolved 率由官方评测(sb-cli 云端,免本地 Docker)得出。

```sh
# 需已配置 DeepSeek API key(未配置会直接报错,不会空跑)
deepcode eval --sample 2                  # 联调:dev split 先跑 2 题
deepcode eval                             # dev 全量(23 题,约几分钱)
deepcode eval --split test --parallel 4 --timeout 900   # test 全量(300 题)
```

产物在 `eval-out/`(可用 `--out` 改):`predictions.json`(官方格式)+ `report.json`(含每题耗时、成本、模型与路由来源)。

官方评分:

```sh
pip install sb-cli
sb-cli gen-api-key you@example.com   # 一次性,邮件验证后 export SWEBENCH_API_KEY=...
sb-cli submit swe-bench_lite dev --predictions_path eval-out/predictions.json --run_id my-run
```

网络提示:数据集来自 HuggingFace datasets-server,部分网络需 `HTTPS_PROXY=http://127.0.0.1:<port>` 或用 `DEEP_CODE_HF_BASE` 指向镜像。参数细节见 `crates/deep-code-eval/README.md`。

## 从源码构建

```sh
git clone https://github.com/liwenka1/deep-code
cd deep-code
cargo build --release -p deep-code-tui
# 产物:target/release/deep-code
```

## 许可证

MIT
