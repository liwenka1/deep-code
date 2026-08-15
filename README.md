# deep-code

English | [简体中文](./README.zh-CN.md)

A DeepSeek-powered terminal coding agent, written in Rust. One small binary: streaming TUI, OS-level sandboxing, role-based sub-agents, and a safety-first execution policy.

## Highlights

**Small, fast, self-contained**

- Single native binary (~4–5 MB) per platform — no Node/Python runtime needed at run time. Prebuilt for macOS (arm64/x64), Linux (x64/arm64, glibc ≥ 2.35), and Windows (x64).
- `npm i -g` fetches the right binary and verifies its SHA-256. musl hosts (Alpine and most slim images) are not supported yet — the installer detects and refuses them with a clear error instead of installing a broken binary.

**Safety-first execution**

- **Four permission tiers** — `default` / `accept_edits` / `auto` / `yolo`, cycled with Shift+Tab. `auto` delegates routine approvals to a cheap Flash classifier with hard floors it can never override: top-risk commands and anything requesting network access always ask a human.
- **OS sandbox (macOS / Linux)** — shell/job commands run inside macOS Seatbelt or Linux Landlock+seccomp: writes confined to the workspace and system temp dirs, **no network by default**. Commands that need egress (installs, `git push`, dev servers) must declare it and go through approval. `[sandbox] network = prompt|always|never` tunes this, and project-level config can only tighten it. If no sandbox backend is available, commands are refused rather than silently run bare.
  **On Linux, how completely writes are confined depends on your kernel**: Landlock gained the right governing `truncate(2)` in ABI 3 (Linux 6.2) and the one governing device `ioctl(2)` in ABI 5 (Linux 6.10), and a right the kernel cannot express is one it never checks. The two gaps are not the same gap, so they are not reported as one. Below 6.2 (Ubuntu 22.04, Debian 12, RHEL 9) every other write outside the roots is still refused — create, delete, open-for-write — so the residual is destructive (a file outside the roots can be emptied), never disclosing. Below 6.10 (Ubuntu 24.04 and most current distros) the path-write boundary is fully intact; what goes unchecked is `ioctl` on a device node, so the reach is bounded by which devices your user can open rather than by which paths were granted — and the `/dev` nodes this sandbox grants for redirection are granted *without* the ioctl right. `deepcode doctor` names the exact gaps, the approval panel says "partial sandbox" rather than "sandboxed execution", and the model's tool description carries the sentence for the gap you actually have — it is the one issuing the write, so it is the last place a gap may be rounded, in either direction. The network guarantee is unaffected: seccomp denies the `socket`/`connect` syscalls outright, with no per-kernel right to negotiate; `io_uring` is denied under every policy (a syscall filter never sees ring submissions, so leaving it reachable would have made that denial advisory); and the syscalls that hand over another process's memory or an already-open socket — `process_vm_readv`, `pidfd_getfd` and friends — are denied alongside `ptrace`.
  **Windows, please read**: only Job Object process-tree containment exists there — **file writes and network are not restricted**, and the `network` setting is a no-op. The deny floor and the approval gate still apply, but "out-of-workspace writes get blocked for you" does not hold on Windows. `deepcode doctor` reports exactly what is enforced on your machine.
- **Deny floor** — catastrophic commands are hard-refused at any tier and cannot be allow-listed: `rm -rf` on system roots, disk formatting (`format C:`, volume-GUID/device-path/`\\?\` spellings, `diskpart`), registry deletion, and friends — including their Windows spellings.
- **Trust that doesn't leak** — "always allow" for shell matches on command identity (program + subcommand): trusting `git push` does not wave through `git status`. Flags that change *what executes or where it writes* (`--config`, `--exec-path`, `--output`, `--target-dir`, …) break the trust match and re-prompt. Shell metacharacters (`$`, backticks, redirection) route to approval instead of being parsed optimistically.

**Sub-agents with real guardrails**

- Delegate investigation or implementation to child agents via one blocking `agent` tool call; issue several calls in one turn to run children in parallel.
  Six roles — `general` / `explore` / `plan` / `review` / `verifier` are strictly read-only; **only `implementer` can write**, and dispatching one is itself an approval point: the human authorizes the dispatch, the child's workspace writes then proceed unattended (on the tiers where writes would prompt).
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

- `auto` picks between `deepseek-v4-pro` and `deepseek-v4-flash` (and the reasoning effort) per task, and falls back with retry on rate limits or upstream failures. Pin with `/model` or `provider.model`.

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
deepcode --add-dir DIR   # grant an extra writable directory (repeatable; works with -p/serve too)
deepcode -p "..."        # headless one-shot: run a full turn, print the answer (see below)
deepcode --help          # command overview (--version prints the version)
deepcode doctor [--json] # environment self-check
deepcode serve --http    # run as an HTTP server
deepcode eval            # SWE-bench rollout (see below)
deepcode session list|resume|delete|export
```

Common slash commands: `/help` `/model` `/apikey` `/lang` `/resume` `/clear` `/sessions` `/checkpoints` `/restore` `/agents` `/copy` `/add-dir` (`/help` lists everything plus keybindings).

### Working across sibling repos (`--add-dir`)

For "one project, several repos" setups (an SDK repo plus its host app), grant the sibling at launch:

```sh
cd ~/code/my-sdk
deepcode --add-dir ../host-app
```

- **Both layers open together**: the file tools accept **absolute paths** that land inside a granted directory (relative paths always resolve against the primary workspace; `..` and symlink escapes stay rejected), and the OS sandbox adds the directory as a write root — for shell commands, the credential-dir write denials (`~/.ssh` and friends) still outrank every grant. The built-in file tools are bounded by the granted roots themselves, so only grant trees you mean to hand over.
- **Grants persist with the session**: they are stored in the session record, so `deepcode -c` restores the same boundary; `--add-dir` on a resume merges in and is saved. To add a directory mid-task, run `/add-dir DIR` in the TUI — same validation, applied and persisted on the spot — or restart with `deepcode -c --add-dir DIR`.
- **Grants follow the run**: within one `deepcode` run they stay with you — a new conversation started by `/clear` inherits the current grants (the startup banner and transcript always name the effective set).
- **Human action only**: there is deliberately no config key for this — granting an extra writable tree is either the launch flag or the `/add-dir` command typed at the keyboard, never something a malicious repo can self-grant through project config (the model cannot invoke slash commands).
- **Checkpoints cover the primary workspace only**: `/restore` rolls back just the workspace and says so when extra roots are granted (they are usually git repos themselves — roll back there with git).
- **Denials don't burn tokens**: a write refused by the boundary is a failure no retry can fix, so it is handled as its own class — the first denial tells the model exactly that (naming `/add-dir`), three in one turn stop the turn with the same guidance for you, and none of them trigger the Pro escalation that ordinary repeated tool failures do.

### Headless one-shot (`-p`)

```sh
deepcode -p "summarize this repo"                # answer → stdout, all diagnostics → stderr
git diff | deepcode -p "write a commit message"  # stdin is attached as data below the instruction
deepcode -c -p "continue: add the tests"         # resume the latest session, then one shot
deepcode -p "fix this lint" --permission-mode accept_edits
deepcode -p "..." --output-format json           # one {"result","reasoning","cost",...} object
deepcode -p "..." --output-format stream-json    # NDJSON, same envelopes as the serve SSE
```

- **Same approval posture as the CI bot**: a call that would prompt is auto-denied — never parked — with one stderr line per denial. Capability comes from the existing knobs: `--permission-mode accept_edits|auto|yolo`, `approval.auto_allow`, `DEEP_CODE_APPROVAL_AUTO_ALLOW` — the `auto` tier's Flash judge needs no human present, so it works headless. The deny floor stays non-negotiable.
- **Exit codes**: `0` finished / `1` error / `2` usage / `124` timeout (`--timeout SECS`) / `130` Ctrl-C.
- One-shot runs persist a session too: stderr prints the id, and `deepcode -c` picks the thread up interactively any time.

## Use it in CI (GitHub Actions)

Give any repository a `/deepcode` bot — comment → edit code → open a draft PR → reply in the thread — with one command:

```sh
cd your-repo
deepcode github install          # writes the workflow, sets the secret via your own gh login
deepcode github status           # show what is wired up
```

`--print` previews without writing; `--with-app` also walks you through a GitHub App (below). Commit the file, push, done. No hosted service, nothing for anyone to keep running.

By hand it is just this:

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

**Optional bot identity.** Without an App the bot works fine, acting as `github-actions[bot]`. Configuring your own GitHub App (`--with-app` walks you through it) buys two things: commits count toward contributors and comments carry a `[bot]` badge; and the bot's pushes trigger your other workflows, which a `GITHUB_TOKEN` push never does — meaning without an App, bot PRs get no CI checks.

Trigger prefix, who may trigger it, language, model and permission tier are all configurable — see the header of [`deepcode-bot.yml`](./.github/workflows/deepcode-bot.yml). For your own pipeline (PR review, issue triage, scheduled maintenance) you don't need this workflow at all; two lines do it: `npm i -g @liwenkai/deepcode` then `deepcode -p --output-format json`.

> **Think before widening who can trigger it.** Only the repository owner can by default (`allowed-associations`). Opening that up hands anyone who can comment the ability to run shell in your CI, next to your secrets. Safety rests on three legs — trusted triggers, the CLI's deny floor, and PRs never auto-merging — and removing one is not carried by the other two.

## Configuration

Config file: `~/.deep-code/config.toml` (see `config.example.toml` in the repo root).
Load order: built-in defaults → global → project `.deep-code/config.toml` → environment variables → CLI flags.

Common keys: `provider.model` (`pro`/`flash`/`auto`), `provider.reasoning_effort` (`off`/`low`/`medium`/`high`/`max`), `cost.currency`, `approval.auto_allow` (pre-approved tool prefixes).

> Keep the API key in an environment variable or the global config; `api_key` in project-level config is ignored, so it can't leak with the repo.

`DEEP_CODE_DISABLE_WEB`: set to `1`/`true`/`on` to disable web tools (`web_search`/`fetch_url`) for offline or audited environments. Enabled by default; `/status` shows current `web=on|off`.

On macOS / Linux, shell/job commands run inside the OS sandbox with **no network by default** (see [Highlights](#highlights) for details). `[sandbox] network = prompt|always|never` tunes this, project config can only tighten. **On Windows there is no sandbox confinement** — the no-network and write-confinement guarantees do not apply (`network` is a no-op); declared network still routes to approval and the deny floor still applies. Run `deepcode doctor` for the ground truth on your machine.

## Extending (skills + shell)

deep-code **does not ship MCP**. It already has a shell, so the way to add a capability is **a script/command + a `SKILL.md`**: a one-line summary lives in the system prompt, the model reads the SKILL.md body on demand, then calls your script through the `shell` tool.

- **Discovery**: drop a `SKILL.md` with `name`/`description` frontmatter into a skills directory (global `~/.deep-code/skills/<name>/` or project `<workspace>/.deep-code/skills/<name>/`); the one-line summary is always in the prompt, the body only enters context when relevant.
- **Execution**: a capability is just a normal command (`psql`, `curl`, your own script, …) run through the `shell` tool — subject to the same approval gate and execution policy as everything else.
- **Why no MCP**: for an agent that has a shell, the shell *is* the universal tool protocol. A few dozen tokens of SKILL.md summary loaded on demand beats keeping a full tool schema resident in every request. Pipe (`| head`, …) to trim results so only the relevant slice enters context. If you need an existing MCP-ecosystem server, use a host that speaks MCP — deep-code stays lean.

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
