# ClipIt Project Memory

## Purpose

ClipIt is a small, fast Rust LAN file-transfer tool designed around the native
file-manager context menu on Windows and macOS. It must coexist with Synergy 3
and LocalSend by using its own process names, configuration, discovery marker,
and ports.

## Current Stage

- The command-line/daemon MVP and Windows/macOS packaging pipeline are complete.
- Protocol v2 includes sender identity, a persistent trusted-device list, and a
  loopback-only confirmation page for unknown senders.
- One executable provides discovery, receiving, sending, trust management, and
  OS integration commands.
- The next milestone is Windows login startup / macOS LaunchAgent integration
  and lightweight status UI; do not introduce a bundled browser runtime.

## Technical Direction

- Rust stable with Cargo.
- Tokio for asynchronous networking and file I/O.
- Dedicated ClipIt discovery and transfer ports; do not reuse LocalSend or
  Synergy protocols, service names, or configuration locations.
- Bind the transfer service on LAN interfaces, but bind any picker/control UI
  only to loopback.
- Treat all peers and filenames as untrusted input. Prevent path traversal and
  write incomplete data to temporary files before atomic rename.

## Expected Commands

- `cargo build` / `cargo build --release`
- `cargo test`
- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --all-features -- -D warnings`

## Maintenance Guide

- Protocol types and framing belong in a small protocol module.
- LAN discovery belongs in a discovery module.
- File sending/receiving belongs in a transfer module.
- Windows and macOS context-menu integration belongs in platform-specific
  modules and must be idempotent.
- Never commit certificates, pairing secrets, or machine-specific paths.

## Security and Performance Positioning

- ClipIt targets trusted private LANs and intentionally keeps file payloads
  unencrypted to minimize CPU work, protocol overhead, and implementation size.
- Keep BLAKE3 integrity checks, strict path validation, temporary-file writes,
  and loopback-only control UI enabled.
- A future lightweight device allowlist or confirmation mechanism may prevent
  accidental transfers, but transport encryption is not on the roadmap unless
  the product requirements change.
