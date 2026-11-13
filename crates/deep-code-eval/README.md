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

# 输出目录(默认 eval-out/,含 checkpoint.json、predictions.json 和 report.json)
deep-code eval --sample 2 --out my-run

# 恢复同一批题;与首次运行保持相同 subset/split/sample、模型和超时配置
deep-code eval --sample 2 --out my-run --resume
```

## 逐题保存与断点续跑

每题结束后,runner 会先将结果原子写入 `checkpoint.json`,再原子刷新
`predictions.json` 和 `report.json`。中断后使用相同 `--out DIR` 加 `--resume`
继续;已保存的题不会再次调用模型。未保存的题会从头开始,运行中的模型状态不恢复。
如果进程在模型已调用但结果尚未保存时退出,恢复该题仍可能产生重复调用费用。
两个导出文件分别原子替换,不构成跨文件事务;恢复时会从 checkpoint 重建它们。
评测未完成时,报告的 `total` 是选题总数,`results` 只包含已保存的题。

保存的 **所有终态** 都会跳过,包括空 patch、超时和错误。`--resume` 只补未保存题,
不自动重试失败题;需要重新评测时使用新的输出目录,避免不知情地重复收费。

恢复会核对题集内容和有效配置;请保留原来的 `--subset`、`--split`、`--sample`、
`--timeout` 以及模型等配置。API key 不写入 checkpoint,换 key 不会阻止恢复;
`--parallel` 和报告输出格式可以调整。配置不匹配、checkpoint 损坏或缺失时会明确报错,
不会默默另开一轮。没有 `--resume` 时,已有 checkpoint 或报告的目录不会被覆盖;
同一输出目录也不能由两个评测进程同时写入。

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
| `--out` | `eval-out` | checkpoint.json / predictions.json / report.json 输出目录 |
| `--resume` | 关闭 | 从同一 `--out` 的 checkpoint 恢复,跳过所有已保存题 |
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
6. 记录 patch 与 telemetry(成本 / 模型 / 路由来源 / 级联触发),逐题保存 checkpoint 和报告

## 后续计划

- [x] 逐题保存与断点续跑(`--resume`)
- [ ] sb-cli 报告回读,把官方 resolved 率合并进 report.json
- [ ] CI 定时评测 + badge
