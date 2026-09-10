# Security Policy

English | [简体中文](#安全政策)

## Supported versions

Only the **latest 0.x release** receives security fixes. There are no
backports; upgrading is `npm i -g @liwenkai/deepcode@latest`.

## Reporting a vulnerability

**Do not open a public issue.** Report privately through GitHub:

> https://github.com/liwenka1/deep-code/security/advisories/new

You will get an acknowledgement **within 7 days**. This is a solo-maintained
project: triage and fixes are honest-effort, prioritized by severity, and you
will be kept informed in the advisory thread. There is **no bug bounty**;
credit is given in the published advisory unless you prefer otherwise.

## Scope

deep-code's security model rests on layers documented in the
[README](./README.md#highlights): the execution policy (deny floor, approval
gate, command-identity trust), the OS sandbox (macOS Seatbelt / Linux
Landlock + seccomp), workspace boundaries, and the CI bot's trigger gating.
A report is in scope when it breaks a promise one of those layers makes:

- **Sandbox escape** — writing outside the granted roots or reaching the
  network from a sandboxed command that declared neither.
- **Deny-floor bypass** — a spelling of a hard-refused command
  (`rm -rf /`-class, disk formatting, registry deletion, …) that executes.
- **Approval-gate bypass** — running a gated action without a prompt, or a
  command-identity trust confusion (a consent for `git status` waving
  through `git push`, wrapper/quote/expansion tricks).
- **Unattended command reaching a shell** — a command that runs with no
  prompt (a trusted identity, a remembered session consent, an accept-edits
  file operation) being executed through `sh -c`/`cmd /C` instead of as the
  exact argv the policy parsed, or that argv differing from the words the
  policy judged.
- **Workspace-boundary escape** — path traversal or symlink tricks past the
  granted roots in the built-in file tools or checkpoint restore.
- **Credential exposure** — the tool itself leaking the API key (to
  subprocesses, logs, or the transcript) or defeating the credential-dir
  write denials.
- **Model-requested write grants** (`request_write_root`) — for this one the
  approval panel *is* the boundary, so anything that misleads or skips it
  counts: a grant landing somewhere other than the resolved target the panel
  showed, model-supplied text pushing that target off screen or counterfeiting
  it, a decision resolved on a panel the user never saw, the home/root/
  credential floor bypassed by a channel that restores grants without
  re-checking them (a session record, say), or any mode or config that
  auto-approves the prompt — `yolo` deliberately does not.
- **CI bot privilege escalation** — triggering the bot past
  `allowed-associations`, or injection through issue/comment content that
  executes outside the agent's policy.
- **Installer integrity** — defeating the npm installer's SHA-256
  verification or its platform checks.

Out of scope (not vulnerabilities):

- Model output quality, hallucinations, or the model *attempting* a denied
  action that policy then blocks — the block working is the design.
- Anything that requires `yolo` mode, a root the user *typed*
  (`--add-dir`, `/add-dir`), or a malicious value the user typed into
  config — those are the user's own authority, exercised. A root the
  **model** requested is different, and in scope: see below.
- **Windows filesystem/network confinement**, which does not exist and is
  [documented as such](./README.md#highlights) — reports assuming it are
  answering a promise never made. (Job-object containment and deny-floor
  bypasses on Windows are in scope.)
- Vulnerabilities in DeepSeek's API or other third-party services.

## Invariants

The promises below hold by construction, or by a test that asks the real shell
rather than a hand-maintained list. A change that breaks one must fail the test
named with it.

1. **No shell for an unattended command.** A command that runs with no prompt
   is executed as the argv `parse_unattended` produced, sequenced by deep-code
   for `&&`/`;`; a command that parse cannot read is refused, never handed to a
   shell. Human-approved text keeps shell semantics.
   (`unattended_commands_run_without_a_shell`,
   `execution_authority_follows_who_resolved_the_prompt`)
2. **The unattended grammar means what `sh` means.** Quoting and the two
   sequencing operators are the whole grammar, checked word for word against
   the real shell. (`unattended_parse_matches_sh_word_splitting`)
3. **Every character the shell rewrites is read by a rule.** All 32 ASCII
   punctuation characters are run through the real shell; any that rewrites a
   word must be indirection, a separator, stripped quoting, or the `~` operand
   rule. (`every_punctuation_the_shell_rewrites_is_accounted_for`)
4. **Every word the shell hands the line to is known.** Wrappers and
   interpreters are enumerated by running the real shell, so a session consent
   never collapses `sh -c …` or `time …` to one word.
   (`every_word_the_shell_runs_the_tail_for_is_known`)
5. **A trusted command names no path outside the cwd by spelling.** The
   sandbox leaves reads open, so this spelling is the read fence; `..` counts
   only as a whole path component.
   (`trusted_commands_lose_their_trust_when_an_operand_leaves_the_cwd`)
6. **The rules judge the words the executor runs.** Every rule that reads a
   command's arguments reads the argv `parse_unattended` produced, so requoting
   a word cannot change the verdict while the program receives the same argv —
   enumerated over quoting and escaping spellings rather than sampled, because
   a list of examples is what kept missing the next one.
   (`requoting_a_word_never_changes_the_verdict`)
7. **`network = "never"` refuses every egress path**, counted by exhaustive
   match over the tool kinds. (`never_refuses_every_egress_path`)
8. **A model-requested write root is never auto-approved** by any mode, config
   consent or session memory.

Known residuals — accepted and written down rather than left for the next
review to rediscover:

- `yolo` relies on the OS sandbox alone; the deny floor there is a UX floor.
- Windows has no filesystem or network confinement; an unattended command
  there runs only a real `.exe`/`.com` (cmd builtins and `.cmd`/`.bat`
  wrappers are refused, not routed through `cmd.exe`).
- A symlink inside the workspace resolves outside it; creating one costs a
  prompt, and a repository that ships one is trusted the moment it is opened.
- Credential directories are readable by sandboxed commands: SSH-signed
  commits, `npm` (`~/.npmrc`) and `codesign` (keychains) need them offline,
  so the read fence is the operand spelling above, not the kernel.
- A command a human approved as text keeps every shell feature the human saw.
- On Windows a *program word* carrying two `%` is denied: `cmd.exe` expands
  `%VAR%`, `%VAR:~0,0%` and `%VAR:a=b%` on the command line, so
  `de%PATH:~0,0%l` is `del` by the time anything runs and no rule here can read
  the word. A `%VAR%` *operand* is not denied — it is indirection, so the line
  is never auto-approved and never a bounded edit, a prompt exactly like `$HOME`
  on Unix. The split matters because this floor is mode-blind: denying the
  operand form took `echo %PATH%` away from a human who explicitly approves it,
  in every tier. Residual: a pair split across words (`%VAR:a=b%` accepts
  blanks, and a blank-named variable is settable) is not read as one, and
  `cmd` leaves an *undefined* `%X%` literal, so that spelling is inert until
  something has defined the variable. cmd's other delimiters (`,`, `=`) share
  the root and take a different fix: the deny floor re-reads the line with them
  turned into blanks, so `del,/f/s/q,C:\*` is denied like the blank-spelled
  form. The `sh` equivalents (`$VAR`, `` `…` ``, `$(…)`, globs) are not chased
  by this floor at all — they are never auto-approved, and under `yolo` the
  containment is the OS sandbox, which Windows does not have.
- On Windows a line spelling a backslash immediately before a `"` never runs
  unattended: `CommandLineToArgvW` and `cmd.exe` read that backslash run
  differently, so the parser refuses instead of choosing. The usual casualty is
  a quoted path ending in a separator — `xcopy "src\" "dst\"` costs a prompt
  where `xcopy src\ dst\` runs. (A quoted *absolute* path like
  `cd "C:\Users\me\"` was never in scope: a drive letter leaves the cwd by
  spelling, and `cd` is a cmd builtin, which no unattended command runs.)

## Disclosure

Coordinated: the fix ships first, then the advisory is published. Given the
solo cadence, a reasonable embargo request is always honored the other way
around too — say what timeline you need.

---

# 安全政策

[English](#security-policy) | 简体中文

## 支持版本

只有**最新的 0.x 版本**接收安全修复,不做旧版回迁;升级方式:
`npm i -g @liwenkai/deepcode@latest`。

## 报告漏洞

**请勿开公开 issue。**通过 GitHub 私密通道报告:

> https://github.com/liwenka1/deep-code/security/advisories/new

**7 天内**会收到确认。这是单人维护的项目:分诊与修复按严重程度尽力而为,
进展会在 advisory 线程里同步。**没有漏洞赏金**;除非你不愿意,发布的
advisory 中会署名致谢。

## 范围

deep-code 的安全模型由 [README](./README.zh-CN.md) 中记录的几层构成:
执行策略(deny floor、审批门、命令身份信任)、OS 沙箱(macOS Seatbelt /
Linux Landlock + seccomp)、工作区边界、CI bot 的触发门禁。凡是打破其中
某层承诺的,都在范围内:

- **沙箱逃逸**——沙箱内命令未声明却写出授权根之外或触网。
- **deny floor 绕过**——某种拼写让硬拒命令(`rm -rf /` 级、磁盘格式化、
  注册表删除等)真正执行。
- **审批门绕过**——未经提示执行被门控的动作;命令身份信任混淆(对
  `git status` 的许可放行了 `git push`、包裹/引号/展开花招)。
- **免审命令经过了 shell**——无提示执行的命令(信任表命中、会话记住的身份、
  accept-edits 文件操作)经 `sh -c`/`cmd /C` 执行,而不是按策略解析出的 argv
  直接执行;或该 argv 与策略判定的词不一致。
- **工作区边界逃逸**——内置文件工具或 checkpoint 恢复中的路径穿越、
  symlink 花招。
- **凭据暴露**——工具自身泄露 API key(进子进程、日志、transcript),
  或击穿凭据目录写保护。
- **模型申请的写授权**(`request_write_root`)——这一项的边界**就是**那块
  审批面板,所以任何误导它或跳过它的手段都算:实际授予的目录与面板显示的
  解析结果不一致、模型可控文本把该目录挤出屏幕或伪造出一行、在用户从未
  看到的面板上被结算掉、家目录/文件系统根/凭据地板被某条"恢复授权时不再
  复检"的通道绕过(比如 session record),以及任何档位或配置能自动放行这
  个提示——`yolo` 刻意不能。
- **CI bot 提权**——绕过 `allowed-associations` 触发 bot,或通过
  issue/评论内容注入并在策略之外执行。
- **安装器完整性**——击穿 npm 安装器的 SHA-256 校验或平台检查。

不在范围内(不构成漏洞):

- 模型输出质量、幻觉,或模型*试图*执行被拒动作而策略成功拦截——拦住
  即是设计本身。
- 任何需要 `yolo` 模式、用户**亲手敲的**根(`--add-dir`、`/add-dir`)、或
  用户亲手写入配置的恶意值才成立的攻击——那是用户自己的权限在行使。
  **模型**申请来的根不算,见上面那条,它在范围内。
- **Windows 的文件系统/网络约束**:本就不存在且[已如实写明](./README.zh-CN.md),
  以其存在为前提的报告回应的是一个从未做出的承诺。(Windows 上的
  Job-object 约束和 deny floor 的绕过仍在范围内。)
- DeepSeek API 或其他第三方服务自身的漏洞。

## 不变量

下面这些承诺靠构造成立,或靠一条向真 shell 提问的测试成立,而不靠手工维护的
名单。任何打破其中一条的改动,都必须让括号里的那条测试变红。

1. **免审命令不经 shell。** 无提示执行的命令按 `parse_unattended` 产出的 argv
   直接 execve,`&&`/`;` 由 deep-code 顺序执行;解析不出的命令拒绝执行,绝不
   交给 shell。人工按文本批准的命令保留 shell 语义。
   (`unattended_commands_run_without_a_shell`、
   `execution_authority_follows_who_resolved_the_prompt`)
2. **免审语法与 `sh` 同义。** 引号与两种串接符就是全部语法,逐词对真 shell
   差分。(`unattended_parse_matches_sh_word_splitting`)
3. **shell 会改写的每个字符都有规则读它。** 32 个 ASCII 标点全部交给真 shell
   跑一遍,凡会改写词的,必须属于间接名单、分段符、被剥的引号或 `~` 操作数
   规则之一。(`every_punctuation_the_shell_rewrites_is_accounted_for`)
4. **shell 会把后半行交出去的每个词都在表内。** wrapper 与解释器由真 shell
   枚举,会话同意不会把 `sh -c …` 或 `time …` 塌成一个词。
   (`every_word_the_shell_runs_the_tail_for_is_known`)
5. **可信命令的操作数按拼写不出 cwd。** 沙箱不拦读,这个拼写就是读侧围栏;
   `..` 只按整个路径分量计。
   (`trusted_commands_lose_their_trust_when_an_operand_leaves_the_cwd`)
6. **规则判定的是执行器真正跑的那些词。** 凡是读命令参数的规则,读的都是
   `parse_unattended` 产出的 argv;因此只要程序收到的 argv 不变,换一种引号
   拼法就不能改变裁决。这条按引号与转义拼法**枚举**,不是举例——举例名单正是
   一直漏掉下一种拼法的原因。(`requoting_a_word_never_changes_the_verdict`)
7. **`network = "never"` 拒绝每一条出网路径**,按工具种类穷举 match 计数。
   (`never_refuses_every_egress_path`)
8. **模型申请的写根绝不自动放行**,任何档位、配置同意、会话记忆都不行。

已知残余——写下来接受,而不是留给下一轮 review 重新发现:

- `yolo` 只靠 OS 沙箱兜底,那里的 deny floor 是体验层的地板。
- Windows 没有文件系统与网络约束;免审命令在那里只运行真正的 `.exe`/`.com`
  (cmd 内建与 `.cmd`/`.bat` 包装拒绝执行,不回落到 `cmd.exe`)。
- 工作区内的符号链接会解析到外面;创建它要一次提示,而自带链接的仓库在打开
  的那一刻就已被信任。
- 凭据目录对沙箱内命令可读:SSH 签名的 commit、`npm`(`~/.npmrc`)、`codesign`
  (钥匙串)在离线时也需要它们,所以读侧围栏是上面的操作数拼写,不是内核。
- 人工按文本批准的命令保留人看到的全部 shell 特性。
- Windows 上,**程序词**里带两个 `%` 会被拒:`cmd.exe` 在命令行上展开 `%VAR%`、
  `%VAR:~0,0%`、`%VAR:a=b%`,所以 `de%PATH:~0,0%l` 真正执行时已经是 `del`,这里
  没有任何规则读得懂那个词。`%VAR%` 出现在**操作数**上不拒——它是 indirection,
  所以那行永不自动放行、也永不是有界编辑,和 Unix 上的 `$HOME` 一样只是一次提示。
  这个区分很重要,因为这层楼是模式无关的:拒操作数那一版把 `echo %PATH%` 在所有
  档位从人手里拿走了,连他明确批准也不行。残余:跨词的一对(`%VAR:a=b%` 允许空白,
  带空白的变量名也设得出来)不会被读成一对;而 `cmd` 对**未定义**的 `%X%` 是原样
  保留,所以那种拼法在有人先把变量定义出来之前是惰性的。cmd 的其它分隔符
  (`,`、`=`)同根但另一种修法:deny floor 把它们换成空白后重读一遍,所以
  `del,/f/s/q,C:\*` 和用空白写的一样被拒。`sh` 那侧的对应拼法(`$VAR`、
  `` `…` ``、`$(…)`、通配)这层楼根本不追——它们永不自动放行,而 `yolo` 下的
  约束是 OS 沙箱,Windows 没有那层沙箱。
- Windows 上,一行里只要出现「反斜杠紧挨 `"`」就不会免审运行:
  `CommandLineToArgvW` 与 `cmd.exe` 对那串反斜杠的读法不同,解析器拒绝而不是
  替它选一个。代价最常见的是引号里以分隔符结尾的路径——`xcopy "src\" "dst\"`
  要一次提示,而 `xcopy src\ dst\` 照常跑。(引号里的**绝对**路径如
  `cd "C:\Users\me\"` 本来就不在讨论范围:盘符按拼写就已越界,而 `cd` 是
  cmd 内建,免审命令从不运行它。)

## 披露

协同披露:先出修复,再发 advisory。考虑到单人节奏,合理的缓冲期诉求
双向尊重——需要什么时间线,直说。
