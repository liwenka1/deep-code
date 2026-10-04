# Changelog

User-facing changes per release, most recent first. Full commit lists live on
the [GitHub Releases](https://github.com/liwenka1/deep-code/releases) pages.
Entries marked **Security:** change security-relevant behavior.

<!-- next-section -->

## [0.4.12] - 2026-10-04

- Image understanding arrives in the agent and TUI: an image can be attached in four ways, it is sent only to a model that accepts images, and when it cannot be sent the attachment degrades to a one-line note instead.

## [0.4.11] - 2026-10-03

- A session record now captures whether each tool call was actually put in front of a human, and `/resume` replays that state instead of inferring it: a resumed transcript badges and folds every call exactly as the live session did — a call you answered returns with its `[approved]` / `[denied]` badge, and a call a standing consent resolved without asking folds just as it did live — rather than claiming a badge the record cannot support or un-folding a call that never needed you.
- Thinking and tool calls now fold. A reasoning block, and a run of quiet tool calls, collapse to one dim row; a click expands the run, results and all. A quiet call draws nothing while it runs, so nothing flickers in the bottom-anchored transcript as each call streams in and lands.
- The model catalog — ids, aliases, context windows and per-token prices — moves from Rust literals into an `assets/models.toml` data file, so repricing a model or adding one is a one-file data edit. The user-visible parts: the shipped prices are refreshed, and the Flash model's canonical id is now `deepseek-flash`, the API's current name, with the `deepseek-v4-flash` spelling releases up to 0.4.10 persisted still resolving.

- Thinking and tool calls now fold. A reasoning block, and a run of quiet tool calls, collapse to one dim row — a click on it opens the run, results and all. Only what is worth a reader's attention is hidden: a call that failed or was refused, one that writes, a background job, a sub-agent dispatch, and any call that was put in front of a human stay visible in full, and a call that does not qualify ends the run it landed in rather than being averaged into it. What folds is decided by one whitelist (`folded_entry`); a tool nobody has classified cannot fold, so adding one to the registry can only make the transcript noisier, never silent. A quiet call draws nothing at all while it runs — no command, no output — because anything shown and then swallowed reads as a flicker, and the transcript is bottom-anchored, so each one also drags the rows below it up and down. A long command's progress stays reachable: the status line keeps its clock while it runs, and the run's row opens on one click once it lands.
- A tool call's arguments are no longer painted before the runtime knows which tool they belong to. Providers stream a call's arguments first, and `ToolCallStarted` — which carries the name and the whole argument string — follows only at the end of that stream; the provisional row that drew in that window named the call by its id and vanished the moment the real name arrived.
- `/resume` restores the approval the session actually had. A session record now carries whether each call was put in front of a human, so a resumed transcript badges and folds exactly as the live one did: a call you answered comes back with its `[approved]` / `[denied]` badge, and a call a standing consent (`auto_allow`, a remembered `a`, AcceptEdits, Auto, Yolo) resolved without asking anyone folds exactly as it did live. Sessions recorded before this field still fold, but only the three read-only tools (`read_file`, `list_dir`, `grep_files`) — the only ones the policy can never put in front of a human; everything else stays visible, and nothing claims a badge it cannot support. A call left waiting when a turn was cancelled no longer keeps a `required` badge for a question nobody can answer any more.
- A click on a foldable row no longer folds the wrong block if the transcript scrollback trims between the press and the release.

- The model catalog — ids, aliases, context windows and per-token prices — now lives in a data file compiled into the binary instead of Rust literals, so repricing a model or adding a new one is a one-file data edit. Three things it carries are user-visible: the shipped prices had gone stale and are refreshed; they are now the list (peak) rate, so an off-peak session's estimate is an upper bound rather than an under-count (off-peak hours are billed at half); and the Flash model's canonical id is `deepseek-flash`, the API's current name, with `deepseek-v4-flash` — the id releases up to 0.4.10 persisted — still resolving. Note that resolution happens before the request is built, so a config pinning the old spelling now sends `deepseek-flash` upstream: the official API accepts both names, but a third-party `base_url` that knows only the old one no longer will. The retired `deepseek-chat` / `deepseek-reasoner` names were dropped, as was a write-only capability field. Code that names the Flash model by constant must move from `DEEPSEEK_V4_FLASH` to `DEEPSEEK_FLASH`; the crates are not published, so this reaches source builds only.

## [0.4.10] - 2026-10-01

- A new `/context` command lists the context the model actually holds at that moment, and a new `/compact` command lets you compact history manually at any time; compaction cells now spell out what was folded, what the model now sees, and the before/after token counts.
- `/status` no longer drops the session's cumulative cost and cache after `/resume` (it had collapsed to `last_turn=none`), and its cache line now reports the raw hit/miss token counts.
- When the context is rejected as over-length, the runtime now forces one compaction and retries instead of failing the turn.
- Follow-up questions are now injected at the tool-batch boundary within the same turn, so queued content is visible in the transcript.
- Transcript cells render per-cell from cache, so a long session's per-frame time drops from 42 ms to 3.9 ms.
- Filling the input box to exactly one line no longer jumps the cursor back to the start of the line; the cursor's line is now tracked correctly.

## [0.4.9] - 2026-09-28

- **Security:** Pre-approved commands no longer run through the shell — a new argv tokenizer lets the trust table, `accept_edits`, and session-consent keys cover only commands that can execute shell-less, which are then `execve`d directly from the policy-sliced argv (the program word must be a bare name).
- **Security:** `serve` in no-token mode re-verifies loopback against the resolved address and validates the `Host` header, closing the DNS-rebinding and `localhost`-resolution bypasses; binding to `localhost`/`::1` is refused outright.
- **Security:** opening a malicious repository no longer runs its code outside the sandbox — rust-analyzer no longer runs a repo's build scripts / proc-macros by default, and the LSP path neutralizes a repository's `rustc-wrapper` from `cargo metadata` and strips injectable environment variables; benchmark diff extraction no longer runs a repository's git config outside the sandbox.
- **Security:** `github install` now validates `--ref`, `--lang`, and `--permission-mode` before interpolating them into the generated workflow YAML, and no longer silently drops `--app-private-key` when `--app-id` is missing; the GitHub bot frames thread content as untrusted data to prevent injection, stops fork-PR comments from being pushed to a same-named base branch, and keeps write tokens out of the agent-readable `.git`.
- **Security:** the deny floor now reads brace expansion, glob metacharacters, wrapper/interpreter prefixes, and `%` as shell indirection and judges wrapper words by basename, so `rm{,} -rf /`, `{sudo,ls}`, `--con{fig,fig}`, `rm -r /*`, `rm -r ~`, `%VAR%` line-rewriting, and `/usr/bin/env rm -rf /` no longer bypass it.
- **Security:** trusted-command and session-consent keys now judge the argv the executor actually runs: words and flags that delegate execution no longer collapse to a bare identity, `git diff --no-index` counts as a redirect flag, `--logfile` after `--` no longer rides a trusted `cargo test`, operands must stay within `cwd` by spelling (`git diff /dev/null ~/.ssh/id_rsa`, `cp`, and `mv -t/tmp src` no longer read or edit silently), and the consent key now includes the network declaration so a command approved offline no longer auto-approves its networked variant.
- **Security:** `accept_edits` now only passes edits whose program name is the first word and whose operands stay within `cwd`; `[sandbox] network = "never"` also denies the web tools (so Yolo no longer gets zero-prompt egress), the `auto` tier's network hard gate covers `fetch_url`/`web_search`, `fetch_url` no longer takes a name-based long-term approval, the approval panel for network tools now defaults to Deny, and `sandbox.network` is tightened by tier at the project layer instead of only blocking `always`.
- **Security:** the SSRF guard adds local NAT64 (`64:ff9b:1::/48`), deprecated site-local (`fec0::/10`), and `0.0.0.0/8`, and its IPv6 classifier folds embedded-IPv4 non-mapped encodings; seccomp blocks the x32 ABI range; Seatbelt adds a literal `file-write-unlink` deny on `HOME` itself.
- **Security:** sub-agent guardrails now kill the background-task process group on completion and no longer inherit the environment's egress; three P3s close path leakage, an env-injection vector, and a spill-fd leak on aborted readers; the judge fence escapes angle brackets so model-controlled text can't close `<action>`, and the classifier's task text is fenced.
- **Security:** rustls is pinned to 0.23.45 so RUSTSEC-2026-0285 no longer ships, and the RUSTSEC gate now upgrades two advisories before release.
- **Security:** `snapshot`/`list`/`restore` re-check that the storage directory hasn't been swapped for a symlink before acting, blocking out-of-bounds deletion; spill cleanup and transcript copying no longer follow symlinks.
- Custom endpoints no longer hard-pin Flash, and offline installs keep an already-verified binary; network-granted sub-agents no longer have their own web tools rejected by their approver, the `general-purpose` role alias now works, parallel sub-agent batches only run pre-allowed dispatches concurrently (approval-needed ones go through a serial gate), and sub-agent timeout residue no longer escapes the cleanup sweep.
- Sessions are keyed by filename so a copied fork no longer writes back to the original, and same-millisecond session-id collisions are gone; HTTP 500 is retryable, write denials no longer misread exit 126 / `publickey`, and mixed Chinese/English input is routed correctly.
- Runtime: an in-band Error frame now stops the stream (two frames from a provider no longer make two terminal events), in-turn compaction no longer swallows the current instruction, `config` `auto_allow` no longer bypasses the sub-agent gate, fallback no longer still sends `max` to Flash, human-approval follow-up batches now break the turn boundary in the same turn, cancellation no longer leaves an answerable-but-stuck approval, and a snapshot failure emits a Warning instead of letting the turn run unobserved.
- Headless/TUI: `nohup` survival is restored, Windows Ctrl-C exits gracefully, stdin no longer hangs, the bottom of a very long transcript is reachable, and long-transcript scroll offsets no longer wrap.
- Approval panel: long shell commands move into a scrollable body so the panel no longer freezes; the panel is no longer armed from its first frame (so keystrokes in the input can't decide a freshly shown prompt), modifier keys no longer trigger a safety prompt, commands are no longer folded/truncated into one line, and a second Esc/Ctrl+C no longer clears prompts queued after the first — an approval that arrives after Esc renders as cancelled.
- Chained commands no longer lose output after the first step (the shared spill reader is finished once per call), spawn failures no longer leave `Running` zombie records, real completion records aren't evicted, and SIGTERM/SIGHUP now trigger cleanup with the npm launcher forwarding signals.
- Tools: `grep` results are trimmed to a budget and say so, cancelled searches are no longer recorded as "already searched", and searches in huge workspaces are cancellable; `apply_patch` anchors fuzzy indentation to the file's real indentation (no longer editing lines the patch didn't mention), `read_file` paginates to a budget, writes refuse non-regular files, and `list_dir` caps huge directories and precomputes sort keys.
- Checkpoints: `build`/`dist` are skipped only at the top level, `/restore` is refused mid-turn, resumed compacted sessions no longer attach the checkpoint to the wrong turn (and metadata is pruned), and `before_turn` snapshots no longer full-copy the workspace — large repositories no longer block tens of seconds per turn with Esc dead.
- Model/compaction/pricing/telemetry: `pro`/`flash` short names resolve to their real model ids, compaction reports a no-op when only a stale summary remains, reasoning tokens are no longer double-counted into output, and the prefix stability indicator no longer stays stuck on "changed" and can now reach `Stable`.
- CLI: `/apikey` pasted without a space no longer leaks into the transcript, `--timeout` overflow no longer panics, starting a new session no longer fully re-parses history, `/add-dir` and `--add-dir` share the same canonicalization so the recorded spelling matches on resume, and the command-line surface (including `--help`, `doctor`, and the `session list` age column) is unified in English.
- Config and doctor: misspelled keys and unreadable values are no longer silent (including the env layer; `sandbox.network` still degrades in the permissive direction), and `doctor --json` no longer prints the "missing API key" guidance on a configured machine.
- The npm-installed command no longer self-reports as `deepcode-bin`, and npm downloads cap redirect hops with temp-file cleanup no longer throwing synchronously.
- Unattended auto-denials no longer tell the model "the user denied" when there is no user, and session consent no longer remembers agent dispatch — the panel offers `a` only when it can actually remember.
- Remaining Chinese-only model-facing strings (the interruption placeholder, workspace-overview fallbacks, compaction role labels, and the approval status bar) now have English, and the approval status bar no longer repeats the risk.
- The deny floor's rejections now suggest a workable spelling (`rm -rf build` is no longer a dead end); pipe rules read both sides by position so `cat x | python3 s.py && curl …` and `curl … || bash x.sh` are no longer hard-rejected; env assignments like `NAME+=v`/`NAME[i]=v` are recognized; wrapper/interpreter commands no longer collapse `sh`/`time` into one consent word; shell parsing treats backslash as a path separator on Windows rather than an escape; and Windows `echo`/`printf` are no longer default-trusted while commands that can't run via argv stay prompted instead of failing permanently.

## [0.4.8] - 2026-08-31

- **Security:** `write_file` and `apply_patch` now open with `O_NOFOLLOW`, write resolution is side-effect-free with parent directories created only at execution time, and a symlink met during execution is reported by its cause instead of being followed.
- **Security:** four symlink-following checks across the agent and TUI now refuse to follow; the `.deep-code` writers each require the directory to be a real directory, the `.gitignore` marker is created atomically so a dangling symlink is not written through, `restore` no longer writes through a symlink outside the workspace, and `github install` refuses symlinked directory segments when writing its workflow.
- **Security:** IO and write-failure branches no longer leak absolute host paths into the model's view.
- **Security:** `auto_allow` now matches tools by exact full name across every reference surface, so a shared prefix no longer lets a different tool through.
- Dead `auto_allow` entries are now called out by name at launch, and the warning says whether the tool simply wasn't mounted this time or doesn't exist at all; `/help` spells out the full-name semantics.
- Control characters and zero-width/bidi text are now sanitized across headless stderr, the composer, `/copy`, the status line, the startup and session selectors, the completion menu, and the approval panel and transcript — all through one shared sanitizer, so an ESC in a filename no longer reaches the terminal; the zero-width defense is now the sanitizer's own rather than borrowed from ratatui's undocumented behavior. `github install` sanitizes the repository name it writes.
- The composer's sanitizer now allows newlines, so multiline drafts are no longer collapsed onto one line.
- `restore` now reports what it kept, no longer deletes items it could not restore back, and treats uncapturable paths by the same snapshot-overwrite deletion rule as symlinks; `skills` read failures are no longer silent.
- On Windows, checkpoint cleanup deletes symlinks per-platform so `restore` no longer fails wholesale, and path diagnostics no longer go silent on `Prefix`/`RootDir` paths.
- `grep` now counts skipped files truthfully instead of fabricating refusals or dropping them silently: files over 2 MiB move from a silent skip to an honest count, walker-level skips join the ledger, and the TUI summary line surfaces the skipped count.
- `mkdir` failures now say whether a file is blocking the path or a symlink is, and `read_file` limit errors no longer conflate two distinct causes.
- Launch warnings (including downgrades) are now staged and delivered after startup instead of being silent, buried, or clobbered by `/resume` and `/clear`; `-c`/`-r` warnings now reach the transcript.

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
[0.4.8]: https://github.com/liwenka1/deep-code/compare/v0.4.7...v0.4.8
[0.4.9]: https://github.com/liwenka1/deep-code/compare/v0.4.8...v0.4.9
[0.4.10]: https://github.com/liwenka1/deep-code/compare/v0.4.9...v0.4.10
[0.4.11]: https://github.com/liwenka1/deep-code/compare/v0.4.10...v0.4.11
[0.4.12]: https://github.com/liwenka1/deep-code/compare/v0.4.11...v0.4.12
