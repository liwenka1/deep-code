# deep-code

[English](./README.md) | 简体中文

基于 DeepSeek 的终端 AI 编程助手,Rust 编写。一个小体积二进制:流式 TUI、OS 级沙箱、角色化子代理、安全优先的执行策略。

## 亮点

**小、快、自足**

- 每平台单一原生二进制(约 4–6 MB),运行时不依赖 Node/Python。预编译覆盖 macOS(arm64/x64)、Linux(x64/arm64,glibc ≥ 2.35)、Windows(x64)。
- `npm i -g` 按平台下载并校验 SHA-256。musl 宿主(Alpine 及多数 slim 镜像)暂不支持——安装器会检测并给出明确报错,而不是装一个跑不起来的二进制。

**安全优先的执行**

- **四档权限**——`default` / `accept_edits` / `auto` / `yolo`,Shift+Tab 循环切换。`auto` 档放行 `accept_edits` 放行的一切,其余交给低价 Flash 判官,但有它永远越不过的硬底:申请联网的调用到不了判官手里——除非常设同意(`auto_allow`、会话「a」)已覆盖该调用,否则一律问人;`[sandbox] network = "always"` 让沙箱命令不声明也有 egress、也不问,但声明了 `network: true` 的调用与联网原生工具照样问人——写根申请绝不自动放行,最高风险档的调用到不了判官手里——除非 `accept_edits` 那层已经放行(裸的 `mkdir src/x` 在这里和在那里一样不问就跑),否则一律问人。`yolo` 自动批准到达审批门的一切(写根申请与未授权的不可逆命令除外,两者在任何档位都问人),**且沙箱命令 egress 常开**——逐条联网提示的意义在于让人看到 egress 请求,无人在环时它拦不住任何恶意(恶意调用声明 `network: true` 就会被自动批),只会让忘记声明的诚实命令断网空跑。`[sandbox] network = "never"` 在 yolo 下依然绝对——声明联网的命令、联网派遣与 web 工具(`fetch_url`、`web_search`)一律拒绝;写入约束与 deny floor 不受影响。还有一道 yolo 掀不掉的地板:**不可逆的对外命令**(发布包、强推、合并 PR、apply 基础设施、删对象存储)——任何自动通道都不得批准它:yolo 不行,配置 `auto_allow` 不行,判官不行,会话记忆也不行。它们在**每个交互档位都会问人**(批准一次,或拒绝),在没人能回答的地方(无头运行、子代理)自动拒绝,而一旦你用 `[sandbox] allow_irreversible` 授权了该类命令,就永久不再问。
- **OS 沙箱(macOS / Linux)**——shell/job 命令在 macOS Seatbelt 或 Linux Landlock+seccomp 内运行:写入限制在工作区、你授予的根(`--add-dir`、`/add-dir`、批准过的 `request_write_root`)、命令自己的 cwd 与系统临时目录,**默认不带网络**。需要联网的命令(装依赖、`git push`、dev server)必须声明并转人工审批。`[sandbox] network = prompt|always|never` 可调,项目层配置只许收紧。没有可用沙箱后端时拒绝执行,而非静默裸跑。与之相对的、唯一一处刻意的例外是 `[sandbox] mode = "off"`:当边界已经由本进程之外的东西提供(容器、micro-VM、一次性 CI runner)时,它让命令裸跑而不是拒绝;只能由全局层设置,`deepcode doctor` 会报告,状态栏有常驻 `[unsandboxed]` 标记,并且被 eval 侧直接拒绝。
  **Linux 上写入约束有多完整取决于内核**:Landlock 管辖 `truncate(2)` 的权限位从 ABI 3(Linux 6.2)才有,管辖设备 `ioctl(2)` 的从 ABI 5(Linux 6.10)才有,而内核表达不了的权限就是它从不检查的权限。这两个缺口不是同一个缺口,因此也不会被合并成一句话上报。6.2 以下(Ubuntu 22.04、Debian 12、RHEL 9):区外的其余写入照旧被拒——创建、删除、开写——所以残留风险是破坏性的(区外文件可被清空),不涉及泄露。6.10 以下(Ubuntu 24.04 及多数当前发行版):按路径的写入边界完好无损,失管的是设备节点上的 `ioctl`,能触及多少取决于你的用户能打开哪些设备,而不取决于授权了哪些路径——并且本沙箱为重定向而授权的那几个 `/dev` 节点,已经**不再**附带 ioctl 权限位(在真正强制该权限位的内核上,这会让沙箱内的 pty 分配设计性失败——expect/pexpect 一类工具——模型会被明确告知这是有意拒绝,不是能用 `/add-dir` 修的路径问题)。`deepcode doctor` 会列出具体缺口,审批面板显示"部分约束"而非"需沙箱执行",模型的工具描述里写的是**你这台机器实际存在的那个缺口**对应的那句话——真正去写的是它,所以它是最不该把缺口取整的那一层,向上向下都不行。断网保证不受影响:seccomp 直接拒绝 `socket`/`connect`,没有按内核协商的权限位;`io_uring` 在任何策略下一并拒绝(syscall 过滤器看不见 ring 提交,留着它就会把那条拒绝变成"建议");能把别的进程内存或已连好的 socket 直接递过来的那些调用——`process_vm_readv`、`pidfd_getfd` 等——和 `ptrace` 一起拒掉。无特权 user namespace 的三种拼法全部关闭——`unshare`/`setns` 直接拒,`clone(CLONE_NEWUSER)` 按参数拒,`clone3` 回 ENOSYS 让各家 libc 回退到可过滤的 `clone(2)`;实际代价:在沙箱里启动自带浏览器沙箱的工具(Puppeteer/Playwright 的 headless Chromium)需要 `--no-sandbox`——在限制无特权 userns 的发行版上它们本来就需要这个开关。
  **Windows 请务必知悉**:那里只有 Job Object 进程树收容,**既不限制文件写、也不拦网络**,`network` 设置在该平台是空操作。deny 底板与审批门仍然生效,但"越界写会被替你拒掉"在 Windows 上不成立。`deepcode doctor` 会如实报告本机究竟约束了什么。
- **deny 底板**——毁灭性命令在任何档位都硬拒、不可加白:系统根上的 `rm -rf`、磁盘格式化(`format C:`、卷 GUID/设备路径/`\\?\` 拼法、`diskpart`)、注册表删除等——包括它们的 Windows 形态。
- **模型可申请写授权**——任务确实需要授权根之外的目录时,模型可调用 `request_write_root` 并附一句理由;审批面板显示解析后的目录与该理由(明确标注为模型的说法),由你决定——符号链接拼写在你判断之前就被解析,批准落地时还会按同一解析结果复核(提示期间被偷换的路径直接拒绝),而覆盖家目录、文件系统根,或与凭据目录重叠(`~/.ssh`、AWS/GCP/Azure 三家、`~/.gnupg`、`~/Library/Keychains`、其他 agent 的 token 存放处等,以及存放 API key 的 deep-code 自身 `~/.deep-code`)的申请,在问到你之前就被驳回。这道驳回在进程内,所以三个平台都成立——而且它是这道地板自己的功劳,不是沙箱的:macOS 上 Seatbelt 确实按同一份清单拒绝授写(按解析后的路径,所以被软链的 `~/.aws` 也盖得住),但 Linux 上这道地板是两者中**唯一**的一道——Landlock 只能表达白名单,没法在已授权的根里面再挖一个 deny(见上面那条内核说明),Windows 上则根本没有文件系统约束。一处值得知道的边界:这道地板只拒绝把边界**扩**到那些路径上,它不会收窄任何东西——所以在家目录里起的会话,那些路径从一开始就是可写的。批准即时生效——无需重启——且授权随会话保存(与 `--add-dir` 同款),`-c` 恢复时会再过一遍同一道地板(session 文件就在工作区内,模型本身写得到它)。拒绝不授予任何权限,并会告诉模型别再申请同一路径,但它仍然可以再问;真正被保证的是没有任何通道能自动放行:`yolo` 不行,`auto` 分类器不行,配置 auto-allow 不行,会话记忆也不行。该面板上 `Enter` 默认落在**拒绝**。
- **不外溢的信任**——shell 的"始终允许"按命令 identity(程序 + 子命令)匹配:信任了 `git push` 不会连带放行 `git status`。会改变*执行什么/写到哪*的 flag(`--config`、`--exec-path`、`--output`、`--target-dir` 等)会击穿信任匹配、重新弹审批;按拼写指向工作目录之外的操作数同样击穿(`git diff /dev/null ~/.ssh/id_rsa` 会问,`git diff main..HEAD` 不会)——沙箱不拦读,这个拼写就是读侧的围栏。shell 元字符(`$`、反引号、重定向、花括号展开、glob 通配、子 shell)一律转审批,不做乐观解析——信任门比对的是写出来的文本,凡是 shell 会先行重写的构造一律排除在自动放行之外,不去猜;哪些字符属于这一类由一条对真 shell 的测试判定,不靠记性。免审执行的命令(信任表命中、会话记住的身份、accept-edits 的文件操作)按信任门解析出的词直接 execve,中间没有 shell(`&&` 与 `;` 由 deep-code 自己顺序执行);只有人工按文本批准的命令才带 shell 语义,因为人看到的就是那段文本。

**带真护栏的子代理**

- 用一次阻塞式 `agent` 调用把调查或实现委托给子代理;同一轮发多个调用即并行。
  六个角色——`general` / `explore` / `plan` / `review` / `verifier` 严格只读;**唯有 `implementer` 可写**,且派遣它本身就是一个审批点:人批准这次派遣,子代理的工作区写入随后免打扰进行(在写操作本会弹窗的档位上)。
  网络同样以派遣为界:子代理**默认完全无网**,除非派遣时声明 `network: true`——这本身也是一个审批点(提示写明后果:子代理读到的任何内容都可能被发往外部主机)。获授的子代理得到 `fetch_url`/`web_search`,其白名单命令带 egress 运行;命令白名单本身绝不因此变宽。子代理同时**继承本会话的权限档位**,这让会话级的策略读数(egress、沙箱开关)保持一致——但继承的是模式,不是能力。子代理自己的受门调用由**无人值守策略**裁决,而那个函数根本不读档位,所以白名单之外的 shell 命令(`git push`、`npm run e2e`)**在任何档位下都被拒,yolo 也一样**。出路有且只有两条,都是你在派遣时做的决定:`allow_commands` 指定它可以运行哪些命令身份(`["git push"]` 会额外弹一次审批并把命令名列出来),`network: true` 授予 egress。两者都没有时,拒绝文案让它把需求写进报告,由**父会话**——那里才有人、才有档位——去执行。而不可逆命令对子代理仍然无解:`allow_commands` 也掀不动那道地板,那种活由父会话做。子代理内部被拒的请求会说真话——策略自动拒绝、无人看过——并指明出路(在授权内干活,或写进报告请父会话重派),而不是冒充"用户拒绝了"。把工具调用预算用尽的子代理,会把**部分报告**随失败一起交回,父会话据此接着干,而不是从零重派;被**取消**的子代理只报告"已取消"——取消是用户自己的决定,而父会话此刻必然还在场,由它决定下一步。
- 侦察角色(`explore` / `review` / `verifier`)固定跑低价 flash 档——扇出的 token 烧在最便宜的地方;子代理进度实时流入父会话:`[explore] +41s step 7/50: grep_files`,长时间运行的子代理不再像卡死。
- 子代理的 token 花费折入父会话的成本统计。

**可信赖的会话**

- 持久化 + `-c` 续接 + `-r` 选择恢复;每轮开始前快照,`/restore` 回滚,文件系统支持时用写时复制克隆(APFS / Btrfs / XFS)。
- 上下文自动压缩,摘要携带有界;成本按请求逐次记账(含缓存命中/未命中与节省、子代理花费),`/status` 查看,币种可选。
- 超出内联窗口的 shell 输出**全量**落盘(单流 64MB 封顶、保留开头)并在结果中给出文件路径——长构建日志的开头(第一个报错所在)不再蒸发,随时可 grep 回读。文件位于 workspace 的 `.deep-code/spill/`,checkpoint 与默认代码搜索都会跳过它,小输出永不落盘,闲置一周的 run 目录会在下次启动时清理。

**一个诚实的 TUI**

- 流式回复带 DeepSeek reasoning、鼠标滚动/划选复制、粘贴折叠、补全菜单。代码块按语言语法高亮(内置 75 种语法,按终端能力用 24-bit 或 256 色);管道表格排成真正的列——列宽感知 CJK、单元格原地换行、窗口装不下时退化为纯文本。流式中可继续输入——排队并在本回合结束后作为追问自动发出(mid-turn steering)。
- 审批面板带真实变更预览;运行中的工具显示自己的耗时钟(`agent … · 47s`),不再像冻屏;状态行保持极简(档位、生效模型、上下文占用)。
- 双语界面(English / 中文),`/lang` 热切换。端到端优雅停机:SIGTERM/SIGINT 有界收尾,进程组整树击杀——不会留下占着端口的孤儿 dev server。

**模型路由**

- `auto` 按任务在 `deepseek-v4-pro` / `deepseek-flash` 间选择模型与 reasoning effort;限流或上游故障时自动降级重试。可用 `/model` 或 `provider.model` 固定。

**图片理解**

- **粘贴、拖入文件、在文件菜单里打 `@`、或 `/image <路径>`,四种方式都能附加图片。** `Ctrl+V` 读取系统剪贴板,能附加它承载的任何图片——截图、Finder 里复制的文件、从别的应用复制的图片——存到 `<workspace>/.deep-code/images/`,按内容寻址,同一张图贴两次只落一个文件。**终端自己的粘贴键**也有两条路能附加图片,走哪条取决于剪贴板上**有没有文本**:Finder 复制的文件**有**(就是文件名),所以"整段粘贴恰好是一个我们打不开的图片名"会让我们回头去剪贴板取文件本体;截图或从应用里复制的图片**只有像素**,终端没有任何东西可发——而那些在这种情况下仍会转发一个**空粘贴事件**的终端,等于在告诉我们"有人按了粘贴",这是第二条路。终端若在这种情况下保持静默,就只有 `Ctrl+V` 能用,所以想要确定就用它。拖入文件一如既往可用,终端本来就是把拖入的文件以路径形式交出来。输入区在图片所在位置显示 `[图片 #N PNG]` chip,`/image` 不带参数会列出已附加的图片及其位置。
- **只有 `deepseek-flash` 接受图片**,所以带图的回合会被路由到它——不管难度关键词、上下文压力或级联升级本来会怎么选,否则那个请求会被 API 拒绝。固定的模型如果按目录声明不支持图片,**两种入口的行为刻意不同**:在 TUI 里回合会在**开始之前**停下并保留草稿,图片还在你眼前,一键就能修(`/model flash`,或把图拿掉);而 `-p`/headless 没有草稿可留,图片会变成该回合文本里的一句 `[图片未发送 / image not sent, …]`,请求照常发出——**在那里拒绝会把会话卡死**:一个回合的图片会被记进 transcript,之后每个回合都会重新 derive 出来,不支持图片的固定模型于是会拒绝之后**每一个**请求,纯文本的也一样。同一套机制也是"会话中途切到这种模型仍可用"的原因:历史里已有的图片会以句子的形式送出,而不是把会话变成死路。配置里的 `[vision] detail` 决定图片的处理方式(`low` 先降到 512×512;`original`/`auto` 原样发出)。
- **发不出去的图片不会连累整条消息。** 回合记录之后被移走或删掉的文件、名不副实的图片(格式按文件自身字节判定,不看扩展名)、或超出大小与张数上限的图片,都会变成该回合文本里的一句说明并点名文件——模型会知道自己缺了什么,而不是被静默地少给。粘贴进来的图片存在 `<workspace>/.deep-code/images/`(已在 git 忽略范围内),**不会自动清理**——想清掉就直接删这个目录。

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

常用 slash 命令:`/help` `/model` `/apikey` `/lang` `/resume` `/clear` `/sessions` `/checkpoints` `/restore` `/agents` `/copy` `/add-dir` `/image`(`/help` 查看全部与快捷键)。

### 跨仓库联调(`--add-dir`)

SDK 仓库 + 宿主应用这类"一个项目拆多个仓库"的场景,在启动时把兄弟仓库授权进来:

```sh
cd ~/code/my-sdk
deepcode --add-dir ../host-app
```

- **两层同时放行**:文件工具接受落在授权目录内的**绝对路径**(相对路径永远相对主工作区;`..` 与符号链接逃逸照旧拒绝),OS 沙箱同步把该目录加入可写根——对 shell 命令,凭据目录的写保护(`~/.ssh` 等)仍然压在所有授权之上。内建文件工具的边界就是授权根本身,所以只授权你真愿意交出去的目录树。
- **授权随会话保存**:记录在会话里,`deepcode -c` 恢复原有边界;续会话时再加 `--add-dir` 会并入并保存。该记录就在工作区内,是模型写得到的文件——所以里面的授权带一个以 `~/.deep-code/session-key` 为密钥的 HMAC,验不过的授权在恢复时会被丢弃并告警,而不是照单恢复——签给**别的** workspace 的授权同样丢弃,所以把记录整份拷进另一个 checkout 也带不过去;记录里的根如果现在解析到了别处(比如在已获批的目录上盖了一个符号链接),也是丢弃而不是悄悄改向。(该密钥在会话实际拿到的那些根之外,macOS 上沙箱化 shell 也读不到。它有三条可达路径,每条都是本页别处已写明的选择的后果:**你自己**授的、包含它的根——在 `$HOME` 里起的会话,或 `--add-dir ~`——因为这道地板刻意不管你亲手敲的命令行;**Linux** 上的沙箱化 shell,那里 Landlock 表达不了读拒绝,与上面那条 Linux 凭据可读是同一个缺口;以及 **Windows** 上的任何 shell,那里根本没有文件系统约束。这个签名在三种情况下换来的都一样:伪造授权得先拿到密钥,而不是只要写那个模型本来就写得到的 session 文件。)中途想加目录,在 TUI 里执行 `/add-dir DIR`——校验一致、当场生效并落盘;或 `deepcode -c --add-dir DIR` 重启;模型自己撞到边界时也会用 `request_write_root` 申请,批准那次提示就是同一种授权,同样即时生效、同样落盘。
- **授权跟随本次运行**:同一次 `deepcode` 运行内授权跟人走——`/clear` 开启的新对话继承当前授权(启动横幅与转录始终列出生效集合)。
- **决定权只在人**:配置文件不提供此项——授权要么是启动参数,要么是键盘敲出的 `/add-dir`,要么是你对模型 `request_write_root` 申请的当场批准。恶意仓库无法借项目配置自授权,申请通道也没有任何自动放行的口子:任何权限档位都不放行(含 `yolo`),配置 auto-allow 与会话记忆对它一律无效,其审批面板也刻意不提供"本会话允许"。
- **检查点不覆盖附加目录**:`/restore` 只回滚主工作区,并会提示附加目录未回滚(它们通常本身就是 git 仓库,用各自的 git 回滚)。
- **撞边界不烧 token**:被边界拒绝的写入是重试不可能成功的一类错误,因此单独归类处理——第一次拒绝就把这一点连同 `request_write_root` 告诉模型,一轮内撞满三次直接中止本轮并向你给出同样的指引,且全程不触发普通工具连续失败才有的 Pro 升级。

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

常用项:`provider.model`(`pro`/`flash`/`auto`)、`provider.reasoning_effort`(`off`/`low`/`medium`/`high`/`max`)、`cost.currency`、`approval.auto_allow`(预放行的工具全名,精确匹配、不是前缀)。

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
