# deep-code

English | [简体中文](./README.zh-CN.md)

A DeepSeek-powered terminal coding agent, written in Rust. One small binary: streaming TUI, OS-level sandboxing, role-based sub-agents, and a safety-first execution policy.

## Highlights

**Small, fast, self-contained**

- Single native binary (~4–5 MB) per platform — no Node/Python runtime needed at run time. Prebuilt for macOS (arm64/x64), Linux (x64/arm64, glibc ≥ 2.35), and Windows (x64); `npm i -g` fetches the right one and verifies its SHA-256. musl hosts (Alpine and most slim images) are not supported yet — the installer detects and refuses them with a clear error instead of installing a broken binary.

**Safety-first execution**

- **Four permission tiers** — `default` / `accept_edits` / `auto` / `yolo`, cycled with Shift+Tab. `auto` delegates routine approvals to a cheap Flash classifier with hard floors it can never override: top-risk commands and anything requesting network access always ask a human.
- **OS sandbox (macOS / Linux)** — shell/job commands run inside macOS Seatbelt or Linux Landlock+seccomp: writes confined to the workspace and system temp dirs, **no network by default**. Commands that need egress (installs, `git push`, dev servers) must declare it and go through approval; `[sandbox] network = prompt|always|never` tunes this, and project-level config can only tighten it. If no sandbox backend is available, commands are refused rather than silently run bare.
  **Windows, please read**: only Job Object process-tree containment exists there — **file writes and network are not restricted**, and the `network` setting is a no-op. The deny floor and the approval gate still apply, but "out-of-workspace writes get blocked for you" does not hold on Windows. `deepcode doctor` reports exactly what is enforced on your machine.
- **Deny floor** — catastrophic commands are hard-refused at any tier and cannot be allow-listed: `rm -rf` on system roots, disk formatting (`format C:`, volume-GUID/device-path/`\\?\` spellings, `diskpart`), registry deletion, and friends — including their Windows spellings.
- **Trust that doesn't leak** — "always allow" for shell matches on command identity (program + subcommand): trusting `git push` does not wave through `git status`, and flags that change *what executes or where it writes* (`--config`, `--exec-path`, `--output`, `--target-dir`, …) break the trust match and re-prompt. Shell metacharacters (`$`, backticks, redirection) route to approval instead of being parsed optimistically.

**Sub-agents with real guardrails**

- Delegate investigation or implementation to child agents via one blocking `agent` tool call; issue several calls in one turn to run children in parallel. Six roles — `general` / `explore` / `plan` / `review` / `verifier` are strictly read-only; **only `implementer` can write**, and dispatching one is itself an approval point: the human authorizes the dispatch, the child's workspace writes then proceed unattended (on the tiers where writes would prompt).
- Reconnaissance roles (`explore` / `review` / `verifier`) are pinned to the cheap flash tier — fan-out burns tokens where it's cheapest — while children stream live progress into the parent transcript: `[explore] +41s step 7/50: grep_files`, so a long-running child never looks hung.
- Child token spend is folded into the parent session's cost tracking.

**Sessions you can trust**

- Persistence + `-c` resume + `-r` picker; per-turn checkpoints with `/restore` rollback, snapshotted via copy-on-write clones where the filesystem supports it (APFS / Btrfs / XFS).
- Automatic context compaction with a bounded summary carry; costs are tracked per request (including cache hit/miss savings and sub-agent spend), shown in `/status`, in your currency of choice.

**A TUI that stays honest**

- Streaming responses with DeepSeek reasoning, mouse scroll/select, paste folding, completion menus. Type while the model streams — your input queues and is sent as a follow-up when the turn ends (mid-turn steering).
- Approval panel shows a real change preview; running tools display their own elapsed clock (`agent … · 47s`) instead of a frozen screen; the status line stays minimal (tier, effective model, context usage).
- Bilingual UI (English / 中文), hot-switchable with `/lang`. Graceful shutdown end to end: SIGTERM/SIGINT drain properly, and process groups are killed as a tree — no orphaned dev servers squatting on ports.

**Model routing**

- `auto` picks between `deepseek-v4-pro` and `deepseek-v4-flash` (and the reasoning effort) per task, and degrades with retry on rate limits or upstream failures. Pin with `/model` or `provider.model`.

## Install

```sh
npm i -g @liwenkai/deepcode
```

The command is `deepcode` (postinstall downloads the platform binary from GitHub Releases and verifies SHA-256). To update:

```sh
npm i -g @liwenkai/deepcode@latest
```

## Quick start

```sh
deepcode            # start a new session
```

Set your DeepSeek API key on first run (or use the `DEEPSEEK_API_KEY` environment variable):

```
/apikey sk-...
```

## Usage

```
deepcode                 # new session
deepcode -c              # continue the latest session
deepcode -r              # pick a session to resume
deepcode --new           # explicitly new session
deepcode --help          # command overview (--version for the version)
deepcode doctor [--json] # environment self-check
deepcode serve --http    # run as an HTTP server
deepcode eval            # SWE-bench rollout (see below)
deepcode session list|resume|delete|export
```

Common slash commands: `/help` `/model` `/apikey` `/lang` `/resume` `/clear` `/sessions` `/checkpoints` `/restore` `/agents` `/copy` (`/help` lists everything plus keybindings).

## Configuration

Config file: `~/.deep-code/config.toml` (see `config.example.toml` in the repo root).
Load order: built-in defaults → global → project `.deep-code/config.toml` → environment variables → CLI flags.

Common keys: `provider.model` (`pro`/`flash`/`auto`), `provider.reasoning_effort` (`off`/`low`/`medium`/`high`/`max`), `cost.currency`, `approval.auto_allow` (pre-approved tool prefixes).

> Keep the API key in an environment variable or the global config; `api_key` in project-level config is ignored, so it can't leak with the repo.

`DEEP_CODE_DISABLE_WEB`: set to anything non-empty other than `0`/`false`/`off`/`no` (case-insensitive; empty counts as unset) to disable the web tools (`web_search`/`fetch_url`) for offline or audited environments; on by default. `/status` shows the current `web=on|off`.

On macOS / Linux, shell/job commands run inside the OS sandbox with **no network by default** (including listening on ports): commands that need egress go to approval, and "remember for this session" stops re-asking for the same command shape; `[sandbox] network = prompt|always|never` tunes it, project config can only tighten. **On Windows there is no sandbox confinement**, so the no-network and write-confinement guarantees in this section do not apply there (the `network` setting is a no-op); declared network still routes to approval and the deny floor still applies. Run `deepcode doctor` for the ground truth on your machine.

## Extending (skills + shell)

deep-code **does not ship MCP**. It already has a shell, so the way to add a capability is **a script/command + a `SKILL.md`**: a one-line summary lives in the system prompt, the model reads the SKILL.md body on demand, then calls your script through the `shell` tool.

- **Discovery**: drop a `SKILL.md` with `name`/`description` frontmatter into a skills directory (global `~/.deep-code/skills/<name>/` or project `<workspace>/.deep-code/skills/<name>/`); the one-line summary is always in the prompt, the body only enters context when relevant.
- **Execution**: a capability is just a normal command (`psql`, `curl`, your own script, …) run through the `shell` tool — subject to the same approval gate and execution policy as everything else.
- **Why no MCP**: for an agent that has a shell, the shell *is* the universal tool protocol. A few dozen tokens of SKILL.md summary loaded on demand beats keeping a full tool schema resident in every request, and you can pipe (`| head`, …) to trim results so only the relevant slice enters context. If you need an existing MCP-ecosystem server, use a host that speaks MCP — deep-code stays lean.

## Eval (SWE-bench)

Built-in SWE-bench rollout driver: pulls the official dataset, drives the agent through each task to produce patches, and writes an official-format `predictions.json`. **No local scoring** — a produced patch ≠ resolved; the real resolved rate comes from the official evaluation (sb-cli in the cloud, no local Docker).

```sh
# Requires a configured DeepSeek API key (errors out instead of dry-running)
deepcode eval --sample 2                  # shakedown: 2 tasks from the dev split
deepcode eval                             # full dev split (23 tasks, a few cents)
deepcode eval --split test --parallel 4 --timeout 900   # full test split (300 tasks)
```

Artifacts land in `eval-out/` (change with `--out`): `predictions.json` (official format) + `report.json` (per-task time, cost, model and routing source).

Official scoring:

```sh
pip install sb-cli
sb-cli gen-api-key you@example.com   # one-time; after email verification export SWEBENCH_API_KEY=...
sb-cli submit swe-bench_lite dev --predictions_path eval-out/predictions.json --run_id my-run
```

Network note: the dataset comes from the HuggingFace datasets-server; some networks need `HTTPS_PROXY=http://127.0.0.1:<port>` or `DEEP_CODE_HF_BASE` pointed at a mirror. Parameter details in `crates/deep-code-eval/README.md`.

## Build from source

```sh
git clone https://github.com/liwenka1/deep-code
cd deep-code
cargo build --release -p deep-code-tui
# artifact: target/release/deep-code
```

## License

MIT
