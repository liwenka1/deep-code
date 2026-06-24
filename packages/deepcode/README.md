# deepcode

AI coding assistant in your terminal — powered by DeepSeek.

## Install

```sh
npm install -g deepcode
```

The `postinstall` step downloads the prebuilt binary for your platform from
[GitHub Releases](https://github.com/liwenka1/deep-code/releases) and verifies
its SHA-256 checksum. Then run:

```sh
deepcode
```

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
