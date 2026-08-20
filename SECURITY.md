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

## 披露

协同披露:先出修复,再发 advisory。考虑到单人节奏,合理的缓冲期诉求
双向尊重——需要什么时间线,直说。
