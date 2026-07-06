# deep-code-eval

deep-code 的基准评测驱动。支持加载 SWE-bench 等数据集，驱动 Agent 逐个修复 issue，输出评测报告。

## 用法

```bash
# 需先设置 DeepSeek API Key
export DEEPSEEK_API_KEY="sk-..."

# 快速调试（跑 5 个实例）
deep-code eval --sample 5

# 并发跑 50 个
deep-code eval --sample 50 --parallel 4

# 全量 Lite 评测
deep-code eval

# 输出 JSON 报告（可用于后续 Docker 验证）
deep-code eval --sample 10 --json > results.json

# 生成 Markdown 报告（可直接贴 README）
deep-code eval --sample 10 --markdown
```

## 参数

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--subset` | `lite` | 数据集子集（lite / verified） |
| `--sample` | 全部 | 限制评测实例数 |
| `--parallel` | `1` | 并发数 |
| `--json` | — | JSON 格式输出 |
| `--markdown` | — | Markdown 表格输出 |
| `--timeout` | `300` | 单实例超时（秒） |

## 数据集

当前支持 **SWE-bench Lite**（300 个实例），从 HuggingFace datasets-server 实时加载。需要网络能访问 `huggingface.co` 和 `github.com`。

## 评测流程

对每个实例：

1. 从 HuggingFace 拉取实例数据（repo、base_commit、problem_statement）
2. 在临时目录 git init → fetch 指定 commit → checkout
3. 启动 deep-code runtime，将 issue 描述提交给 agent
4. 自动批准所有工具调用
5. 等待 agent 完成修复（或超时）
6. git diff 提取 patch
7. 记录结果（resolved / unresolved / timeout / error）

## 输出示例

```
═══════════════════════════════════════════════
  Benchmark:    swe-bench / lite
  Started at:   17:27:12Z
  Duration:     53.8s
───────────────────────────────────────────────
  Total:        1
  ✅ Resolved:  0
  ❌ Unresolved: 1
  ⏱️  Timeouts:  0
  💥 Errors:    0
───────────────────────────────────────────────
  Resolve rate: 0.0%
═══════════════════════════════════════════════

  ❌ astropy__astropy-12907  (53386ms, patch=0b)
```

## 后续计划

- [ ] 缓存/断点续跑（`--resume`）
- [ ] `--dry-run` 成本估算
- [ ] 支持 verified / full 子集
- [ ] CI 定时评测 + badge
- [ ] 输出 patch 文件供 SWE-bench Docker 验证
