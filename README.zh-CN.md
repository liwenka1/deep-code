# deep-code

[English](./README.md) | 简体中文

基于 DeepSeek 的终端 AI 编程助手,Rust 编写。一个小体积二进制:流式 TUI、OS 级沙箱、角色化子代理、安全优先的执行策略。

## 亮点

**小、快、自足**

- 每平台单一原生二进制(约 4–5 MB),运行时不依赖 Node/Python。预编译覆盖 macOS(arm64/x64)、Linux(x64/arm64,glibc ≥ 2.35)、Windows(x64)。
- `npm i -g` 按平台下载并校验 SHA-256。musl 宿主(Alpine 及多数 slim 镜像)暂不支持——安装器会检测并给出明确报错,而不是装一个跑不起来的二进制。

**安全优先的执行**

- **四档权限**——`default` / `accept_edits` / `auto` / `yolo`,Shift+Tab 循环切换。`auto` 档把日常审批交给低价 Flash 判官,但有它永远越不过的硬底:最高风险命令与任何申请联网的调用,一律问人。
- **OS 沙箱(macOS / Linux)**——shell/job 命令在 macOS Seatbelt 或 Linux Landlock+seccomp 内运行:写入限制在工作区与系统临时目录,**默认不带网络**。需要联网的命令(装依赖、`git push`、dev server)必须声明并转人工审批。`[sandbox] network = prompt|always|never` 可调,项目层配置只许收紧。没有可用沙箱后端时拒绝执行,而非静默裸跑。
  **Linux 上写入约束有多完整取决于内核**:Landlock 管辖 `truncate(2)` 的权限位从 ABI 3(Linux 6.2)才有,管辖设备 `ioctl(2)` 的从 ABI 5(Linux 6.10)才有,而内核表达不了的权限就是它从不检查的权限。在更老的内核上(Ubuntu 22.04、Debian 12、RHEL 9),工作区外的其余写入照旧被拒——创建、删除、开写——所以残留风险是破坏性的(区外文件可被清空),不涉及泄露。`deepcode doctor` 会列出具体缺口,审批面板在这类主机上显示"部分约束"而非"需沙箱执行"。断网保证不受影响:seccomp 直接拒绝 `socket`/`connect` 系统调用,没有按内核协商的权限位。
  **Windows 请务必知悉**:那里只有 Job Object 进程树收容,**既不限制文件写、也不拦网络**,`network` 设置在该平台是空操作。deny 底板与审批门仍然生效,但"越界写会被替你拒掉"在 Windows 上不成立。`deepcode doctor` 会如实报告本机究竟约束了什么。
- **deny 底板**——毁灭性命令在任何档位都硬拒、不可加白:系统根上的 `rm -rf`、磁盘格式化(`format C:`、卷 GUID/设备路径/`\\?\` 拼法、`diskpart`)、注册表删除等——包括它们的 Windows 形态。
- **不外溢的信任**——shell 的"始终允许"按命令 identity(程序 + 子命令)匹配:信任了 `git push` 不会连带放行 `git status`。会改变*执行什么/写到哪*的 flag(`--config`、`--exec-path`、`--output`、`--target-dir` 等)会击穿信任匹配、重新弹审批。shell 元字符(`$`、反引号、重定向)一律转审批,不做乐观解析。

**带真护栏的子代理**

- 用一次阻塞式 `agent` 调用把调查或实现委托给子代理;同一轮发多个调用即并行。
  六个角色——`general` / `explore` / `plan` / `review` / `verifier` 严格只读;**唯有 `implementer` 可写**,且派遣它本身就是一个审批点:人批准这次派遣,子代理的工作区写入随后免打扰进行(在写操作本会弹窗的档位上)。
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
deepcode --add-dir DIR   # 额外授权一个可写目录(可重复;-p/serve 同样可用)
deepcode -p "..."        # 无头单发:整轮跑完,答案打到 stdout(见下)
deepcode --help          # 命令一览(--version 查版本)
deepcode doctor [--json] # 环境自检
deepcode serve --http    # 作为 HTTP 服务运行
deepcode eval            # SWE-bench 评测 rollout(见下文)
deepcode session list|resume|delete|export
```

常用 slash 命令:`/help` `/model` `/apikey` `/lang` `/resume` `/clear` `/sessions` `/checkpoints` `/restore` `/agents` `/copy` `/add-dir`(`/help` 查看全部与快捷键)。

### 跨仓库联调(`--add-dir`)

SDK 仓库 + 宿主应用这类"一个项目拆多个仓库"的场景,在启动时把兄弟仓库授权进来:

```sh
cd ~/code/my-sdk
deepcode --add-dir ../host-app
```

- **两层同时放行**:文件工具接受落在授权目录内的**绝对路径**(相对路径永远相对主工作区;`..` 与符号链接逃逸照旧拒绝),OS 沙箱同步把该目录加入可写根——对 shell 命令,凭据目录的写保护(`~/.ssh` 等)仍然压在所有授权之上。内建文件工具的边界就是授权根本身,所以只授权你真愿意交出去的目录树。
- **授权随会话保存**:记录在会话里,`deepcode -c` 恢复原有边界;续会话时再加 `--add-dir` 会并入并保存。中途想加目录,在 TUI 里执行 `/add-dir DIR`——校验一致、当场生效并落盘;或 `deepcode -c --add-dir DIR` 重启。
- **授权跟随本次运行**:同一次 `deepcode` 运行内授权跟人走——`/clear` 开启的新对话继承当前授权(启动横幅与转录始终列出生效集合)。
- **只能是人的显式动作**:配置文件不提供此项——授权要么是启动参数,要么是键盘敲出的 `/add-dir` 命令,不给恶意仓库借项目配置自授权的机会(模型无法调用 slash 命令)。
- **检查点不覆盖附加目录**:`/restore` 只回滚主工作区,并会提示附加目录未回滚(它们通常本身就是 git 仓库,用各自的 git 回滚)。
- **撞边界不烧 token**:被边界拒绝的写入是重试不可能成功的一类错误,因此单独归类处理——第一次拒绝就把这一点连同 `/add-dir` 告诉模型,一轮内撞满三次直接中止本轮并向你给出同样的指引,且全程不触发普通工具连续失败才有的 Pro 升级。

### 无头单发(`-p`)

```sh
deepcode -p "总结这个仓库的结构"                 # 答案 → stdout,诊断一律 → stderr
git diff | deepcode -p "写一条 commit message"   # stdin 作为数据,拼在指令下方
deepcode -c -p "继续:把测试补上"                 # 续最近会话,再单发一轮
deepcode -p "修掉这个 lint" --permission-mode accept_edits
deepcode -p "..." --output-format json           # 单个 {"result","reasoning","cost",...} 对象
deepcode -p "..." --output-format stream-json    # NDJSON 逐事件,与 serve 的 SSE 同一套 envelope
```

- **审批姿态与 CI bot 相同**:会弹窗的调用一律自动拒绝、绝不挂起,每次拒绝在 stderr 打一行。放行能力用既有开关:`--permission-mode accept_edits|auto|yolo`、`approval.auto_allow`、`DEEP_CODE_APPROVAL_AUTO_ALLOW`——`auto` 档的 Flash 判官不需要人在场,无头下照常工作。deny 底板照旧不可越过。
- **退出码**:`0` 完成 / `1` 出错 / `2` 用法错误 / `124` 超时(`--timeout SECS`)/ `130` Ctrl-C。
- 单发同样落会话:stderr 会给出 id,随时 `deepcode -c` 进 TUI 接着这条线聊。

## 在 CI 里用(GitHub Actions)

给任意仓库装一个 `/deepcode` 机器人:评论 → 改代码 → 开草稿 PR → 回帖。一条命令:

```sh
cd your-repo
deepcode github install          # 写 workflow + 用你的 gh 凭据设好 secret
deepcode github status           # 查看接入状态
```

`--print` 先预览不落盘,`--with-app` 额外引导配一个 GitHub App(下面说)。装完提交那个文件、推上去就能用了。不需要托管服务,也没有任何东西要你长期运维。

手写也行,内容就是这些:

```yaml
on:
  issue_comment: { types: [created] }
jobs:
  deepcode:
    permissions: { contents: write, pull-requests: write, issues: write }
    uses: liwenka1/deep-code/.github/workflows/deepcode-bot.yml@main
    secrets:
      deepseek-api-key: ${{ secrets.DEEPSEEK_API_KEY }}
```

**可选的 bot 身份**:不配就以 `github-actions[bot]` 干活,一切正常。配一个自己的 GitHub App(`--with-app` 会一步步带你走)能多拿两样——提交计入贡献者、评论带 `[bot]` 徽章;以及 bot 推的分支**会正常触发你其他 workflow**,而 `GITHUB_TOKEN` 的推送永远不触发(GitHub 防环规则),也就是说不配 App 时 bot 开的 PR 上没有 CI 检查。

触发前缀、允许触发的人、语言、模型、权限档等全部可调,说明见 [`deepcode-bot.yml`](./.github/workflows/deepcode-bot.yml) 顶部。想自己拼流程(PR review、issue 分类、定时任务)不必用这套管线,两行就够:`npm i -g @liwenkai/deepcode` 然后 `deepcode -p --output-format json`。

> **放宽触发权限前请想清楚**:默认只有仓库 Owner 能触发(`allowed-associations`)。放开它等于把"在你的 CI 里、挨着你的 secrets 跑 shell"交给能评论的人。安全靠三条腿——可信触发者、CLI 的 deny 底板、PR 从不自动合并——拆一条另外两条撑不住。

## 配置

配置文件:`~/.deep-code/config.toml`(可参考仓库根目录的 `config.example.toml`)。
加载顺序:内置默认 → 全局 → 项目 `.deep-code/config.toml` → 环境变量 → CLI 参数。

常用项:`provider.model`(`pro`/`flash`/`auto`)、`provider.reasoning_effort`(`off`/`low`/`medium`/`high`/`max`)、`cost.currency`、`approval.auto_allow`(预放行的工具前缀)。

> API Key 建议放在环境变量或全局配置;项目级配置中的 `api_key` 会被忽略,以防随仓库泄露。

环境变量 `DEEP_CODE_DISABLE_WEB`:设为 `1`/`true`/`on` 即可关闭联网工具(`web_search`/`fetch_url`),用于断网或审计场景;默认开启。`/status` 会显示当前 `web=on|off`。

在 macOS / Linux 上,shell/job 命令在 OS 沙箱内运行且**默认不带网络**(详见[亮点](#亮点))。`[sandbox] network = prompt|always|never` 可调,项目层只许收紧。**Windows 上没有沙箱约束**,因此本段的断网与写入限制均不生效(`network` 设置是空操作),声明联网仍会转审批、deny 底板仍然生效;跑 `deepcode doctor` 看本机实况。

## 扩展能力(skills + shell)

deep-code **不内置 MCP**。它本来就有 shell,所以扩展能力的方式是**写个脚本/命令 + 一份 `SKILL.md`**:一行摘要注入系统提示、模型按需读取 SKILL.md 正文,再通过 `shell` 工具调用你的脚本。

- **发现**:把带 `name`/`description` frontmatter 的 `SKILL.md` 放进 skills 目录(全局 `~/.deep-code/skills/<name>/` 或项目 `<workspace>/.deep-code/skills/<name>/`);其一行摘要常驻提示,正文只在相关时才读入上下文。
- **执行**:能力就是普通命令(`psql`、`curl`、你自己的脚本……),经 `shell` 工具运行,同样受审批门与执行策略约束。
- **为什么不做 MCP**:对有 shell 的 agent,shell 就是通用工具协议。一份几十 token 的 SKILL.md 摘要按需加载,比把整套工具 schema 常驻每一轮请求更省上下文。用管道(`| head` 等)裁剪结果、只把关键片段带回上下文。确需现成的 MCP 生态 server 时,用支持 MCP 的宿主即可——deep-code 保持精简。

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
