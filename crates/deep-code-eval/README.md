# deep-code-eval

deep-code 的 SWE-bench **rollout 驱动**:加载官方数据集,驱动 agent 逐题产出
patch,写出官方格式的 `predictions.json`。

> **本 crate 不打分。** patch 产出 ≠ 解决;真实 resolved 率必须把
> `predictions.json` 提交官方评测(sb-cli 云端,免本地 Docker)后得出。
> 报告里的"有 patch"只是未评分的 rollout 指标。

## 用法

```bash
# 需先配置 DeepSeek API key(未配置会直接报错,不会在 echo 后端空跑)
export DEEPSEEK_API_KEY="sk-..."

# 联调:dev split(23 题)先跑 2 题
deep-code eval --sample 2

# dev split 全量(默认 split,便宜、适合迭代)
deep-code eval

# test split(300 题,正式对外数字用这个)
deep-code eval --split test --parallel 4 --timeout 900

# 输出目录(默认 eval-out/,含 predictions.json + report.json)
deep-code eval --out my-run
```

## 官方评分(得出真实 resolved 率)

```bash
pip install sb-cli
sb-cli gen-api-key you@example.com        # 一次性,邮件验证
sb-cli submit swe-bench_lite dev --predictions_path eval-out/predictions.json --run_id my-first-run
```

## 参数

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--subset` | `lite` | lite / verified(verified 仅 test split) |
| `--split` | `dev` | dev(23 题,联调)/ test(300 题) |
| `--sample` | 全部 | 按 instance_id 排序后取前 N 题 |
| `--parallel` | `1` | 并发数 |
| `--timeout` | `300` | 单题超时(秒);test 全量建议 900 |
| `--out` | `eval-out` | predictions.json / report.json 输出目录 |
| `--json` / `--markdown` | — | stdout 报告格式 |

## 流程

对每题:

1. 从 HuggingFace datasets-server 拉实例(repo、base_commit、problem_statement)
2. 经 `~/.cache/deep-code/swebench-repos` 的 bare 缓存 checkout 到 base_commit
   (同一仓库多题复用,django 不再反复下载)
3. 启动 runtime,以任务模板包裹 issue 提交(修根因/不改测试/不 commit)
4. 自动批准工具调用;超时先 `cancel_turn` 再收 patch
5. `git add -A`(排除 `.deep-code/` 运行时产物)+ `git diff --cached` 提取
   patch(含新建文件)
6. 记录 patch 与 telemetry(成本 / 模型 / 路由来源 / 级联触发)

## 后续计划

- [ ] 断点续跑(`--resume`)
- [ ] sb-cli 报告回读,把官方 resolved 率合并进 report.json
- [ ] CI 定时评测 + badge
