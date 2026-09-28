# 发版

## 人的动作只有一次

```
Actions → Release Prep → 输入新版本号（不带 v）→ Run
```

之后全自动：起草 CHANGELOG → bump 版本 → commit → 打 tag → 5 平台构建与自检 →
建 GitHub Release → 5 平台安装验证 → `npm publish`。

**不需要人工 promote、不需要人工回退、不需要 AI 参与决策。** 一个装不上的版本
根本到不了 npm。

---

## 四个阶段

```
guard ──► build(×5) ──► release ──► verify-install(×4) ──► publish
  │          │             │              │                   │
拒绝非 tag  test+构建   资产+校验和    安装并运行           npm publish
           +执行产物     +自检          真产物
```

| 阶段 | 做什么 | 拦住什么 |
|---|---|---|
| `guard` | 拒绝非 tag 触发 | 误触发的 workflow_dispatch（否则会白跑半小时矩阵再报绿） |
| `build` ×5 | `cargo test` → `cargo build --release` → **执行产物** → 上传 | 编译失败；安全测试静默 skip；**产物起不来**；**glibc floor 被抬高** |
| `release` | tag↔版本↔CHANGELOG 四者一致 → 资产 → `SHA256SUMS` → **校验和自检** → 建 Release | 版本不一致；缺 changelog 节；校验和生成错或漏（它是所有安装的信任根） |
| `verify-install` ×4 | 断言宿主平台 == leg 声称的平台 → `npm pack` → 本地安装 → `deepcode --version` / `--help` | 资产名不匹配；`SHA256SUMS` 与资产不一致；postinstall 失败；启动器回归；`os`/`cpu` 标签不匹配 —— **"全平台装不上"这一整类** |

> `verify-install` 只有 **4** 条 leg，这是刻意的。`install.mjs` 按**宿主机**的
> `platform`-`arch` 选资产，所以在 `macos-latest`（arm64）上放一条叫 `darwin-x64`
> 的 leg，它会下载并运行 **arm64** 资产然后全绿 —— 一条对它名字里的平台什么都没
> 证明的 leg，读起来却像覆盖。所以每条 leg 现在都会先断言宿主平台等于自己声称的
> 平台，不一致就直接红，让这种幽灵 leg 无法悄悄出现。
| `publish` | `npm publish`（幂等） | 只有前四步全绿才跑得到 |

### 三道闸门各自覆盖的真实回归

| 断言 | 它是一次真事故的回归测试 |
|---|---|
| `./deep-code-<target> --version` 能跑 | **glibc 事故**：`ubuntu-latest`(24.04, glibc 2.39) 产出的二进制在 Ubuntu 22.04 / Debian 12 / RHEL 9 / AL2023 上启动即死（`GLIBC_2.3x not found`），而 `npm i -g` 已经报成功了。`linux-x64` leg 构建**并运行**在 `ubuntu-22.04` 上，所以抬高 glibc floor 过不了这一关。 |
| 经启动器 `--version` **恰好**输出 `deepcode <版本>` | **argv[0] 事故**：启动器曾把 `argv[0]`（`deepcode-bin`，谁 PATH 上都没有的名字）传下去，于是新用户读到的第一行提示，让他去运行一个不存在的命令。 |
| `deepcode --help` 退出码为 0 | `--help` 曾落到未知参数分支，打印 `Unknown arguments: --help` 到 stderr 并 exit 2。 |

`--version` / `--help` 都是参数解析的早期分支（`crates/deep-code-tui/src/cli.rs`），
不依赖 TTY 和配置，因此在 CI 里可以直接断言。

---

## 为什么验证在 `npm publish` **之前**

`scripts/verify-install.sh` 安装的是**本地 tarball**，所以它不需要版本已经上 npm；
而它内部的 `install.mjs` 仍然会去**刚建好的 GitHub Release** 拉平台二进制和
`SHA256SUMS`。于是发布前验证等价于发布后验证，但多了一个决定性的差别：

> **装不上的版本根本不会进 npm 的 `latest`，所以没有人需要 promote 或回滚 dist-tag。**

这一点是有代价换来的结论：npm 的 OIDC trusted publishing **只管 `npm publish`**，
workflow 无法执行 `npm dist-tag add`（npm/cli#8547 至今 open）。所以"发完再自动
promote"的方案没有长生命周期 token 就做不成。把验证前移是绕开这个问题，而不是
回答它。

> ⚠️ **前提**：`npm pack` 产出的 tarball 必须与 `npm publish` 上传的一致。今天成立
> ——同一个 `files` 字段，且没有 `prepack` / `prepublishOnly` 钩子。**一旦有人加了
> 这样的钩子，这道闸门对"已发布产物"的覆盖就作废了。**

---

## 失败分支

| 哪一步红 | 后果 | 怎么办 |
|---|---|---|
| `build` / S1 | tag 已存在；GitHub Release 和 npm 都没动 | 修代码 → 发下一个 patch |
| `release` | 同上 | 同上 |
| `verify-install` | GitHub Release 已建；**npm 完全没动，用户零影响** | 产品问题 → 修 + 下一个 patch。**验证脚本自身**的问题 → Actions 里 "Re-run failed jobs"（建 Release 是幂等更新，`publish` 有幂等守卫） |
| `publish` | 极罕见 | Re-run（幂等守卫会跳过已发布的） |

---

## 回退阶梯

| 层 | 情况 | 动作 |
|---|---|---|
| 0 | `verify-install` 失败，或还没跑完 | **什么都不用做**，`latest` 没动，用户不受影响 |
| 1 | 已发布，用户装不上 | `npm dist-tag add @liwenkai/deepcode@<上一个好版本> latest` —— 秒级回滚安装路径（需你本人的 npm 登录，OIDC 做不到这件事） |
| 2 | 二进制本身坏 | 上一步**之后**再 `gh release edit v<版本> --draft`。⚠️ 顺序不能反：`install.mjs` 会去拉 `SHA256SUMS`，先撤 Release 会让新装用户撞 404 |
| 3 | 代码层修复 | 发下一个 patch（`release-prep.yml` 一条命令） |
| 4 | `npm unpublish` | **仅**前 72 小时 + **仅**恶意发布。会打断别人的 lockfile，是最后手段 |

---

## 演练（canary）

`verify-install-canary.yml` 对**已发布**的版本跑同一套检查，手动触发、无副作用：

```
Actions → Verify Install (canary) → 输入 0.4.8 → Run
```

**用它做两件事**：

1. **发版前演练**。拿一次真实发版去赌一道从未运行过的闸门是不划算的。而对
   0.4.8 演练是**有意义的**：0.4.8 在 npm 和 GitHub Release 上都真实存在，所以这次
   演练是端到端的。演练通过之后，下一次发版的 `verify-install` 是**已被证明过的
   流程**，而不是第一次运行。
2. **故障诊断**。用户报"装不上"，用他的版本跑一次，变红的那条 leg 直接点名平台。

**为什么它 checkout 两棵树**：主体是发布过的 tag（`released/`，`install.mjs` 要从它
自己的 `package.json` 取版本号拼下载 URL），检查器是**当前分支上**的脚本。这个分离
是必须的 —— v0.4.8 那个 tag 里根本没有 `scripts/verify-install.sh`（本脚本是之后才
加的），从发布树里取脚本会让 4 条 leg 全部以 "No such file" 失败，那是一个和发布
本身毫无关系的红灯。

顺带一条也因此被注意到：`actions/checkout` 会 `git clean` 它的目标目录，所以两次
checkout 的**顺序**不能反 —— 先工作区、后 `released/`。

**故意不加定时任务**：一个因 runner 或网络抖动而变红的定时 job 会训练你去忽略它，
那个代价高于它提前几天报信的价值。

---

## 本地复现

```bash
# 打包契约检查（PR 级，无网络、秒级）——CI 的 packaging job 跑的就是这个
bash scripts/check-packaging.sh

# 安装验证的前半段（会真的去下载资产，需要网络）
bash scripts/verify-install.sh 0.4.8

# 完整本地闸门（fmt / clippy / 测试 / doctest）
./scripts/preflight.sh
```

---

## AI 在这条链路里的位置

**它不参与任何判断。** 每一道闸门都是确定性的、机器判定的：

- `cargo test` / clippy / rustdoc / MSRV —— 退出码
- 产物执行 —— 退出码 + 版本字符串精确匹配
- 四者版本一致 —— 文本比对
- `SHA256SUMS` —— `sha256sum -c`
- 打包契约（`check-packaging.sh`）—— 键值对文本比对 + tarball 内容比对
- 安装验证 —— 宿主平台断言 + 退出码 + `deepcode <版本>` 精确匹配

唯一一处 AI 在链路里：**`release-prep.yml` 用模型起草 CHANGELOG**，且无人过目
（这是仓库里写明了的知情选择）。它是**写作**，不是验证，所以不制造"审不完"的问题。
但它有两个必须知道的后果：

1. 起草是 **fail-closed** 的 —— 缺 `DEEPSEEK_API_KEY` 或起草失败，发版停在打 tag
   **之前**。所以 API 故障 = 发版等 API。它不会产生"版本推了但 changelog 是坏的"
   这种中间态。
2. `release.yml` 要求 `CHANGELOG.md` 里有该版本节，所以本节内容会**未经人眼**直接
   发布。事后修正路径是 main 上补一个 docs commit + 编辑 GitHub Release 正文；tag
   树里的那份不可改。

---

## 已知的残余（诚实清单）

| 残余 | 影响 | 为什么接受 |
|---|---|---|
| **`deep-code-x86_64-apple-darwin` 从未在 CI 里被执行过** | 它是唯一一个"没被执行就发出去"的产物。若它启动即死，只有 Intel Mac 用户会先发现 | 没有 x86_64 macOS runner 可用（`macos-13` 稀缺且在退役，见 `release.yml` 的 matrix 注释），arm64 runner 上执行需 Rosetta 而 runner 没有。S1 与 `verify-install` 都**显式出声**声明这一点，不静默；`verify-install` 的每条 leg 还断言宿主平台，杜绝"名字覆盖了、实际没有" |
| `release` job 里新建的校验和自检，演练覆盖不到 | 首次发版时这一步是第一次运行 | 它是"照搬的已有代码 + 一个自检"，且失败会停在 npm publish 之前 |
| 闸门失败时 GitHub Release 已公开 | 会留下"有 Release、npm 上没有"的孤儿版本 | 用户不受影响（`latest` 没动）。修好代码后发下一个 patch；若是验证脚本自身的问题，"Re-run failed jobs" 可以走通（建 Release 是幂等更新） |
| Windows leg 上 `npm i -g` 后 `deepcode` 能否被找到，未经实证 | 若全局 bin 目录不在 Git Bash 的 PATH 上，`windows-latest` leg 会假红并挡住 publish | **这正是 canary 存在的理由**：先对 0.4.8 dispatch 一次，所有平台的真假红灯都会在真发版之前暴露 |
| 逻辑错误、安全边界绕过、UI 异常 | 闸门**不覆盖** | 机器只能判"能不能用"，判不了"用得对不对"。这一类靠 PR 时的 CI 与人工评审 |
| CHANGELOG 内容无人过目 | 文案质量与准确性 | 见上一节；知情选择 |
