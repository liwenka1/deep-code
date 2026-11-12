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

`verify-install-canary.yml` 对已发布的包运行当前安装检查，手动触发，不发布资产或修改 registry：

```
Actions → Verify Install (canary) → 输入要验证的发布版本 → Run
```

它用于发版前演练，以及定位具体平台的安装故障。检查统一要求安装成功、
`--help` 正常退出，以及 `--version` 精确输出 `deepcode <版本>`。

工作流先检出当前检查脚本，再将待测发布包检出到 `released/`，以确保实际安装的是
该发布包。两次 checkout 的顺序固定，避免工作区清理删除待测目录。

---

## 本地复现

```bash
# 打包契约检查（PR 级，无网络、秒级）——CI 的 packaging job 跑的就是这个
bash scripts/check-packaging.sh

# 安装验证的前半段（会真的去下载资产，需要网络）
bash scripts/verify-install.sh "$(node -p 'require("./packages/deepcode/package.json").version')"

# 完整本地闸门（fmt / clippy / 测试 / doctest）
./scripts/preflight.sh
```

---

## AI 在这条链路里的位置

**它不参与任何判断。** 每一道闸门都是确定性的、机器判定的：

- `cargo test` / clippy / rustdoc —— 退出码
- 产物执行 —— 退出码 + 版本字符串精确匹配
- 四者版本一致 —— 文本比对
- `SHA256SUMS` —— `sha256sum -c`
- 打包契约（`check-packaging.sh`）—— 资产映射与验证矩阵比对 + tarball 内容比对
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

五个平台均配置了原生 runner 上的产物执行和安装验证；其中 Intel macOS 使用
`macos-15-intel`，安装演练使用相同矩阵。本地契约检查只能验证配置，实际 runner
执行结果仍需以发布工作流或对已发布版本运行安装演练的结果为准。

| 残余 | 影响 | 为什么接受 |
|---|---|---|
| `release` job 里新建的校验和自检，演练覆盖不到 | 首次发版时这一步是第一次运行 | 它是"照搬的已有代码 + 一个自检"，且失败会停在 npm publish 之前 |
| 闸门失败时 GitHub Release 已公开 | 会留下"有 Release、npm 上没有"的孤儿版本 | 用户不受影响（`latest` 没动）。修好代码后发下一个 patch；若是验证脚本自身的问题，"Re-run failed jobs" 可以走通（建 Release 是幂等更新） |
| npm 可能改为默认门禁 install scripts | 若被拦住，`npm i -g deepcode` 会"安装成功"但二进制不存在，**所有新装用户全坏** | 闸门天然覆盖：二进制不存在时 `verify-install` 当场变红；持续关注 npm 的安装脚本策略变化 |
| 逻辑错误、安全边界绕过、UI 异常 | 闸门**不覆盖** | 机器只能判"能不能用"，判不了"用得对不对"。这一类靠 PR 时的 CI 与人工评审 |
| CHANGELOG 内容无人过目 | 文案质量与准确性 | 见上一节；知情选择 |
