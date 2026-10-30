# deepcode

AI coding assistant in your terminal — powered by DeepSeek.

## Install

```sh
npm install -g @liwenkai/deepcode
```

(The installed command is `deepcode`.)

The `postinstall` step downloads the prebuilt binary for your platform from
[GitHub Releases](https://github.com/liwenka1/deep-code/releases) and verifies
its SHA-256 checksum. Then run:

```sh
deepcode
```

## Use it from your code

`deepcode -p "<instruction>"` runs one full turn without a terminal and prints the answer on
stdout — everything diagnostic goes to stderr, so it pipes cleanly. `--output-format json` turns
stdout into exactly one object to branch on:

| field | type | notes |
| --- | --- | --- |
| `status` | string | `finished` / `cancelled` / `timeout` / `error` — branch on this |
| `result` | string | the answer; empty unless `status` is `finished` |
| `reasoning` | string | the turn's reasoning text, empty when the model emitted none |
| `error` | string (optional) | present exactly when `status` is not `finished` |
| `session_id` | string (optional) | persist it to continue this thread |
| `denied_approvals` | number | tool calls auto-denied because nobody was there to approve |
| `duration_ms` | number | wall clock for the turn |
| `cost` | object (optional) | `{ usd, cny }` |
| `usage` | object (optional) | token counts |

- **Fields are added, never renamed or repurposed**; nothing documented here changes meaning
  without a bump of this contract's version, which is `1`.
- **Decide by `status`, not the exit code**: a cancelled turn exits `1`, not `124`.
- **Never use `-c` in a wrapper** — it means "the latest session in this workspace" and races.
  Start with `--new` (the default), keep `session_id`, continue with `--resume <id>`.
- **`denied_approvals > 0` is not a bad answer**: a tool was refused because no human was there
  to approve it, and a retry fails the same way.
- **Close stdin when spawning**: an inherited idle pipe costs 2 s on macOS/Linux and hangs on
  Windows, and `--timeout` does not cover that read.

Exit codes: `0` finished / `1` error / `2` usage / `124` timeout / `130` Ctrl-C.

Full contract, flags and approval posture: [README](https://github.com/liwenka1/deep-code#driving-it-from-another-program)
(`deepcode --help` lists the flags for the exact version you installed).

## Supported platforms

| OS | Arch |
|----|------|
| macOS | arm64, x64 |
| Linux | arm64, x64 |
| Windows | x64 (arm64 via x64 emulation) |

## Notes

- Requires Node.js >= 18 (for the installer only; the CLI itself is a native binary).
- If your network blocks GitHub, grab a binary from the
  [releases page](https://github.com/liwenka1/deep-code/releases) manually, or
  build from source: clone `liwenka1/deep-code` and run
  `cargo build --release -p deep-code-tui`.

## License

MIT
