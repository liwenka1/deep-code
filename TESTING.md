# 测试与真实服务验证

## 评测恢复的离线回归

`cargo test --locked -p deep-code-eval --lib` 使用本地模拟任务和临时文件验证
逐题保存、并发完成顺序、中断恢复、输出目录锁、配置校验和写入失败。
这些测试不会调用付费模型,也不会运行 SWE-bench 真实评测。

## 常规验证

PR 的 CI 使用固定 Rust 工具链检查格式、Clippy、workspace 测试和打包契约。本地可运行
`bash scripts/preflight.sh`；canary runner 自身的契约测试不访问网络或模型：

```sh
node --test scripts/run-service-canary.test.mjs
```

## 工具工作流回归

常规 workspace 测试覆盖有序图文内容、图片快照身份与原生 tool 消息、逐请求视觉路由和省图记录、
压缩后图片来源保留、超长 UTF-8 文本的字节游标续读、输出落盘完整性、标准输入及 EOF/取消、
输入权限门，以及 TUI/HTTP/headless 的内容与执行状态投影。这些测试使用临时工作区、真实本地
子进程和模拟模型客户端，不调用付费 API；协议序列化通过不代表已验证真实提供方的图片理解。

可归档的工具详情预览另有 1 MiB 的全会话累计保留预算，优先保留近期预览；执行事实与完整详情来源引用
独立保留。没有来源准备记录的详情保留原文，避免恢复和再次保存丢失唯一副本。新结果的预览
成本与归档摘要在后台一次准备，会话通过增量队列执行保留，不在每次结果完成时重新序列化历史。
长轮次回归在同一个用户回合执行 600 次小编辑，验证保存与恢复全部结果、旧 diff 通过
`read_file` 分页读取完整来源，以及重复保存和重启不会恢复已归档的预览；另有无来源详情恢复、
增量处理的序列化次数和历史插入后索引更新的回归。

会话超过 8 MiB 时采用小索引和不可变的 1 MiB 数据块保存，恢复时完整重建正文与工具参数；
索引先验证宿主签名，数据块逐个校验摘要，重建后再验证会话签名。单个索引仍限 8 MiB，
逻辑正文读取上限为 128 MiB，列表中的候选窗口也限制累计保留体积。600 次、每次 15,000
字符的 `write_file` 回归检查全部调用参数和完成结果均可恢复；缺块、损坏、链接和写入失败
另有拒绝或保留旧快照的检查。保留或搬运会话时需要一并保留 `sessions/chunks/`。

数据块引用列表也包含在签名中。保存与删除按所有有效索引回收无引用块，fork 共享块共同保留；
读写和回收通过目录级文件锁协调，读取期间不会回收其快照。无法确认索引时保守跳过回收并记录
诊断，清理失败不冒充保存失败。连续 20 次约 10 MiB 前部更新的回归检查块占用收敛，另验证
fork、并发读写、发布失败及等待锁时的取消行为。

评测归档会复制会话实际引用的图片、详情与完整输出，重定位来源并重新签名；删除原临时工作区后
仍可读取。缺失来源、越界路径和导出失败会显式报告。长命令详情降级仍保留执行事实；输出截断
计数区分内联省略字节与环形缓冲淘汰字节，并覆盖非 ASCII 及非法 UTF-8 输出。
完整工具输出来源由运行时写入 `content_storage`，导出只依据结构化来源元数据；普通正文中的
续读示例不构成文件引用。

## 真实服务 canary

`.github/workflows/service-canary.yml` 每周一 04:17 UTC 跑一次，也可从 Actions 的
**Real services (canary)** 手动触发。它独立于每次 PR 的离线测试，不发布版本、评论或上传会话。

## 覆盖与结果

- SSE：真实 DeepSeek 请求、认证、流式解析和结束事件。
- Search & Fetch：一次搜索（最多一个结果）和一次 `https://example.com/` 抓取。
- 子代理：临时工作区中的单函数只读调查和结构化报告。
- LSP：固定 Rust 工具链附带的 rust-analyzer，对临时错误文件返回诊断。

配置仓库 secret **`DEEPCODE_CANARY_API_KEY`** 后才执行前三项。建议使用独立、低额度的
DeepSeek key；它仅在检查密钥是否存在和实际调用服务的步骤中进入环境，构建和 LSP 步骤没有密钥。
没有 key 时 live API job 会显示 **skipped**，summary 会明确写出未验证 provider；
workflow 整体为绿色不能替代查看各项是否实际执行。手动输入 `live_api=false` 可只跑 LSP。

工作流只在定时或手动触发的可信代码上运行，不接收 PR 触发；有写权限的维护者应只对已审查的
revision 手动运行带密钥的检查。不要在公共 PR 中运行该工作流或扩大 token 权限。

## 预算与截止时间

| 检查 | 请求/输出约束 | Rust 检查时限 | 外层进程时限 |
| --- | --- | --- | --- |
| SSE | Flash、关闭思考、一次请求、最多 32 输出 tokens | 60 秒 | 90 秒 |
| Search & Fetch | 一个结果、一次 fetch；服务瞬时错误沿用单次内置重试 | 90 秒 | 120 秒 |
| 子代理 | Flash、关闭思考、最多 4 个模型请求、每次最多 512 输出 tokens、流式重试关闭 | 120 秒 | 150 秒 |
| rust-analyzer | 本机临时文件，不调用模型 | 现有 LSP 冷启动轮询预算 | 90 秒 |

编译单独限时 15 分钟，每个 job 总计最多 20 分钟。canary 本身不做失败重跑；频率和输出上限
控制调用规模，不代表账户的货币硬额度，账户侧仍应设置预算。网络故障、服务契约变化或额度耗尽
都会让实际执行的检查失败，应查看具体 step，而不是忽略红灯。

## 本地验证

先安装仓库固定的 Rust 工具链。LSP 额外需要 `rustup component add rust-analyzer`；
脚本的外层截止时间使用 GNU `timeout`，工作流运行在 Ubuntu。

```sh
cargo test --locked -p deep-code-agent --lib --no-run
bash scripts/run-service-canary.sh lsp
# 自行设置 DEEPSEEK_API_KEY 后再按需运行；这些命令会产生真实服务调用费用。
bash scripts/run-service-canary.sh sse
bash scripts/run-service-canary.sh web
bash scripts/run-service-canary.sh subagent
```

脚本在执行前按完整名称确认 ignored 测试确实存在，避免测试改名后 `--exact` 匹配零项却返回成功。
测试仍保留 `#[ignore]`，常规 `cargo test` 不会自动消耗 API 额度。
