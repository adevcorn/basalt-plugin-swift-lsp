# basalt-plugin-swift-lsp

Basalt plugin: Swift language-server bridge backed by `sourcekit-lsp`.

It currently provides:

- workspace activation for SwiftPM and Xcode-style roots
- best-effort `textDocument/publishDiagnostics`
- `textDocument/hover`
- semantic tokens and symbol relations for Swift files

## Installation

Download the latest `.wasm` from [Releases](https://github.com/adevcorn/basalt-plugin-swift-lsp/releases) and place it in `~/.config/basalt/plugins/`.

Or install via the Basalt plugin registry.

## Building from source

```bash
rustup target add wasm32-unknown-unknown
cargo build --target wasm32-unknown-unknown --release
cp target/wasm32-unknown-unknown/release/swift_lsp.wasm ~/.config/basalt/plugins/swift-lsp.wasm
```

`sourcekit-lsp` is launched via `/usr/bin/xcrun sourcekit-lsp`, so it depends on the active Xcode toolchain on macOS.
