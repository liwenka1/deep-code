# Changelog

User-facing changes per release, most recent first. Full commit lists live on
the [GitHub Releases](https://github.com/liwenka1/deep-code/releases) pages.
Entries marked **Security:** change security-relevant behavior.

<!-- next-section -->

## [0.4.7] - 2026-08-23

- The model can now request additional writable roots, and the approval panel
  shows the model's stated reason.
- **Security:** writable-root requests are validated strictly against the
  declared schema so bait keys never reach you, reject targets that overlap
  credential directories (sharing the sandbox's own list), hard-refuse
  control-character names, and bind the displayed target to the granted one —
  the home directory and filesystem root are refused outright.
- Shell output that would be truncated now spills to disk in full: the
  truncation hint names a readable file path, spill loss is measured against
  the budget the result actually retained (closing a silent 12k-20k loss), the
  newest run file is treated as live, orphaned files are removed when the
  stream ends, and run directories idle for a week are cleaned up at startup.
- **Security:** spilled output refuses to follow symlinks and is written with
  `0600` files and `0700` directories.
- Sub-agents can declare `network=true` when they are dispatched; the request
  goes through approval and, once granted, the child runs with egress.
- In `yolo` tier, sandboxed commands now keep network egress always on, instead
  of requiring a per-command declaration that otherwise left them running
  offline with nothing to do.
- When the sandbox fails for lack of network, the model now gets the network
  hint instead of a misleading write hint, and `/tmp` joins the unix write
  roots.
- A sub-agent's auto-rejected result now reports what actually happened instead
  of masquerading as a user rejection.
- The approval panel now pins its header for every tool, sizes to its content,
  draws the resolved target first for write-root requests, allocates frames by
  hand so a short terminal no longer pushes that target off-screen, truncates
  model text by display column, pins the path in the action row, and defaults
  focus to deny — and does not arm an approval it cannot render.
- Control characters are now sanitized across the transcript and every
  approval-panel field — including preview, description, the write-root prompt
  header, and status-line error text — and a single ESC no longer disables
  sanitization for the rest of the panel.
- **Security:** the credential floor now blocks `rename` across the
  intermediate directories of every multi-segment entry (not just `.config`),
  adds the big-three cloud providers' credentials and keychains to the
  protected list, resolves credential paths by their deepest existing
  ancestor, and normalizes paths into one namespace so a macOS firmlink
  spelling can no longer slip past the whole floor — `~/.config` can no longer
  be moved out wholesale.
- **Security:** write grants in a session record are verified by signature
  against the same workspace instead of guessing at danger, so re-resolving a
  grant to a different root no longer redirects it; resume now uses the
  caller's workspace as the primary root, re-runs recorded grants through the
  floor, and shows the same set it enforces.
- **Security:** write resolution now branches on `lstat`, so a dangling
  symlink no longer writes through the grant root, and the macOS Seatbelt
  credential deny is bound to the resolved path.

## [0.4.6] - 2026-08-18

- Code blocks in the TUI transcript are now syntax-highlighted by language, with a per-line cache so only the newly streamed tail is re-highlighted as it arrives.
- The transcript now renders GFM pipe tables, and a run of pipes only becomes a table once the separator row has arrived — partial rows are not misrendered as one.

## [0.4.5] - 2026-08-18

- **Security:** the Linux sandbox now closes the third spelling of unprivileged
  user-namespace creation, adds seccomp stand-ins for `ptrace` and the new
  mount API, and switches `io_uring` to an `ENOSYS` denial — sealing the path
  that let `io_uring` bypass seccomp.
- **Security:** the sandbox capability report the model sees is now
  three-state and no longer over-claims — it reflects what Landlock actually
  enforces rather than asserting protections the kernel never applied.
- Device `ioctl` refusals that the sandbox makes by design are now told to the
  model as such, so it stops chasing `/add-dir`; each gap is worded
  separately, so an `ioctl` gap no longer negates an otherwise intact write
  boundary.

## [0.4.4] - 2026-08-13

- `deep-code eval`: long benchmark runs get a fallback and observability, and
  run artifacts are committed to git.

## [0.4.3] - 2026-08-11

- `/add-dir`: grant an extra writable directory mid-session from the TUI —
  same validation as the launch flag, applied and persisted on the spot.
- Boundary denials (writes refused outside the granted roots) are now their
  own failure class: the first one tells the model exactly why and names
  `/add-dir`, three in one turn stop the turn with the same guidance for you,
  and none of them trigger the Pro escalation reserved for ordinary repeated
  tool failures — a denial the kernel repeats is not something retries fix.

## [0.4.2] - 2026-08-11

- `--add-dir DIR` (repeatable) grants extra writable directories across the
  TUI, headless `-p`, and `serve`: file tools accept absolute paths that land
  inside a granted root (relative paths still resolve against the primary
  workspace; `..` and symlink escapes stay rejected), the OS sandbox adds the
  directory as a write root, and grants persist with the session — `-c`
  restores the same boundary. Credential-dir write denials outrank every
  grant.
- Resuming with `--add-dir` merges the new grant into the session record
  immediately and rebuilds the system prompt to name the effective set.
- CI bot: commit subjects and PR descriptions now describe the resulting
  change instead of echoing the triggering comment; PR body fields are
  length-bounded and sanitized as a whole; a malformed `dc:commit` block no
  longer swallows the text after it.

## [0.4.1] - 2026-08-07

- `deepcode github install` / `deepcode github status`: wire the `/deepcode`
  CI bot into any repository with one command — writes the caller workflow
  and sets the API-key secret through your own `gh` login; `--with-app` walks
  through the optional GitHub App identity; `--print` previews without
  writing.
- The bot pipeline is a reusable workflow (`on: workflow_call`) with trigger
  prefix, permitted commenters, language, model, permission tier, and
  timeouts all configurable from the caller. Machine accounts and unknown
  commenters stay refused regardless of configuration.
- Bot runs execute through headless `-p` instead of `serve` + polling: exit
  codes carry failure detection and timeouts are reaped in-process.
- With a GitHub App configured, bot commits, PRs, and replies carry your
  App's `[bot]` identity, and bot pushes trigger your other workflows (a
  `GITHUB_TOKEN` push never does — meaning without an App, bot PRs get no CI).

## [0.4.0] - 2026-08-05

- Headless one-shot mode: `deepcode -p "..."` runs one full turn without a
  terminal UI — answer on stdout, diagnostics on stderr, stdin attached as
  data below the instruction (`git diff | deepcode -p "write a commit
  message"`). `--output-format text|json|stream-json` (NDJSON sharing the
  serve SSE envelopes), exit codes `0/1/2/124/130`, `--timeout SECS`,
  combinable with `-c` / `--resume <id>`. Approvals that would prompt are
  auto-denied with one stderr line each — the deny floor stays
  non-negotiable. One-shot runs persist a session like any other.
- README is bilingual: English default with a Simplified Chinese edition.

## [0.3.0] - 2026-08-03

- Sub-agents stream live progress into the parent transcript (role, elapsed
  time, step budget), reconnaissance roles pin to the cheap Flash tier, and a
  child's spend — including cache traffic — folds into the parent session's
  totals.
- **Security:** dispatching a write-capable sub-agent is itself an approval
  point: the human authorizes the dispatch, not the child's individual
  writes. Role guidance matches enforcement — only `implementer` writes.
- **Security:** sandbox capability reporting separates "a backend exists"
  from "what it actually confines" — Windows no longer claims to be
  sandboxed; without a usable backend, shell/job commands are refused
  instead of silently running bare; Linux ruleset construction fails closed.
- **Security:** the Windows deny floor recognizes disk-formatting spellings
  (volume-GUID, device-path, `\\?\`), `powershell` as an interpreter, and
  judges recursive deletion by its target; permission tiers are strictly
  monotonic; project-level config can no longer override `base_url`
  (mirroring the `api_key` rule).
- Mid-turn steering: type while the model streams — input queues and is sent
  as a follow-up when the turn ends.
- Running tools show their name and an elapsed clock instead of a frozen
  screen; the approval panel scrolls long previews correctly; the status
  line slims down to tier / model / context.
- Per-turn checkpoints snapshot via copy-on-write clones where the
  filesystem supports it (APFS / Btrfs / XFS) and publish atomically, so a
  crash can't leave a half-copied snapshot visible.
- Costs accumulate per request — multi-tool turns and cancelled turns no
  longer under-count — and session cost persists across resume; the
  compaction summary carry is byte-bounded.
- Shell children run in their own process group and cancellation, timeout,
  and shutdown kill the whole tree — no orphaned dev servers squatting on
  ports; quitting the TUI or switching sessions also kills background jobs.
- The HTTP server drains gracefully on SIGTERM/SIGINT/SIGHUP; npm installs
  fail hard on version-resolution problems instead of silently falling back,
  and re-installs verify the checksum again.
- LSP diagnostics survive spaces and non-ASCII in paths (RFC 3986 percent
  encoding, both directions).

## [0.2.1] - 2026-07-24

- Four permission tiers — `default` / `accept_edits` / `auto` / `yolo` —
  cycled with Shift+Tab and shown in the status line. `auto` delegates
  routine approvals to a Flash classifier with hard floors it can never
  override: top-risk calls and anything requesting network always ask a
  human, and judge errors fail safe to a prompt. Project-level config cannot
  set `auto` or `yolo`.
- Bilingual UI (English / 中文): interface text, runtime errors, approval
  previews, and config warnings all localize; `/lang` switches live and
  persists.
- Sub-agents collapsed to a single blocking `agent` tool call; several calls
  issued in one turn run children concurrently, with results recorded in
  issue order so the transcript (and prefix cache) stays deterministic.
- **Security:** a hardening pass on the shell gate — quote, wrapper, and
  backslash spellings (`r""m`, `r\m`, `env`/`sh -c`/`xargs` wrapping,
  pipe-to-shell), `$HOME`/`$VAR` expansion in accept-edits paths, and
  flag-embedded paths (`--target-directory=/abs`) are all seen through;
  `sed` left the accept-edits allowlist (its `e`/`w` flags can execute or
  write); recursive `rm` is no longer auto-approved.
- **Security:** credential protection when a command is granted network —
  the sandbox denies reads and writes of `~/.deep-code` (plaintext key) and
  writes to `gh` / `docker` / `kube` / `.npmrc` / `.pypirc` / git-credential
  stores, while approved commands regain egress (pushes, installs, builds)
  with filesystem isolation unchanged.
- **Security:** the API key lands on disk as `0600` via a race-free temp
  file; the `serve` token compares in constant time and non-loopback binds
  require one.

## [0.2.0] - 2026-07-20

- Extending deep-code is shell + `SKILL.md`: the MCP subsystem was removed
  (~1,900 lines). A capability is a script or command plus a `SKILL.md`
  whose one-line summary sits in the system prompt and whose body loads on
  demand — subject to the same approval gate and execution policy as
  everything else.
- `apply_patch` matches hunks in three passes (exact → indentation-tolerant
  → punctuation-tolerant) and maps replacements back byte-accurately, so
  CRLF, BOM, and quote variants in untouched content survive edits.
- Web tools gate behind `DEEP_CODE_DISABLE_WEB` for offline or audited
  environments (fail-closed parsing; `/status` shows the switch).
- **Security:** SSRF protection moved to connect time — DNS resolves once,
  the verified IP is pinned for the request, and redirects are followed
  manually hop by hop, closing the DNS-rebinding window.
- **Security:** subprocesses (LSP servers and friends) spawn with
  `DEEPSEEK_API_KEY` and other secrets stripped from their environment;
  checkpoint-restore ids are validated as single path segments (no
  traversal); the desktop-era allow-all CORS layer is gone, and tokenless
  `serve` startup warns explicitly.
- SSE disconnects clean up pending approvals — denied and unblocked instead
  of a dead approval reporting success; background jobs die with the session.
- Workspace snapshots run on the blocking pool, so checkpointing no longer
  stalls the runtime under load.

## [0.1.5] - 2026-07-15

- First tagged release: a DeepSeek-native terminal coding agent in Rust —
  streaming TUI with reasoning display, workspace file/search/shell/web
  tools behind an approval gate with change previews, OS sandboxing (macOS
  Seatbelt, Linux Landlock + seccomp, no network by default), session
  persistence with `-c` / `-r` resume, per-turn checkpoints with `/restore`,
  automatic context compaction, per-request cost tracking, sub-agents, and
  npm distribution (`npm i -g @liwenkai/deepcode`) with SHA-256-verified
  platform binaries.

[0.4.3]: https://github.com/liwenka1/deep-code/compare/v0.4.2...v0.4.3
[0.4.2]: https://github.com/liwenka1/deep-code/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/liwenka1/deep-code/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/liwenka1/deep-code/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/liwenka1/deep-code/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/liwenka1/deep-code/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/liwenka1/deep-code/compare/v0.1.5...v0.2.0
[0.1.5]: https://github.com/liwenka1/deep-code/releases/tag/v0.1.5
[0.4.4]: https://github.com/liwenka1/deep-code/compare/v0.4.3...v0.4.4
[0.4.5]: https://github.com/liwenka1/deep-code/compare/v0.4.4...v0.4.5
[0.4.6]: https://github.com/liwenka1/deep-code/compare/v0.4.5...v0.4.6
[0.4.7]: https://github.com/liwenka1/deep-code/compare/v0.4.6...v0.4.7
