# Local build and install preference

When asked to compile or install the plugins in this repository, build and install each crate in `extensions/<name>` as its own executable. Each extension crate has a `src/main.rs` entry point that calls that crate's library `main()`.

- Use the per-extension binaries named `pi-ext-<name>` (with `.exe` on Windows).
- Install one executable and one `package.json` with one `pi.native` entry per extension under `<agentDir>/extensions/<name>`.
- Do not use `crates/pi-rust-ext` for local per-plugin builds or installs; it is the multi-call aggregate binary.
- Do not also add an extension directory under `<agentDir>/extensions` to the `packages` setting. The agent extensions directory is auto-discovered, and loading the same package both ways creates duplicate extensions and commands.

This is the local build/install preference. The repository's existing Marketplace release workflow still uses the aggregate binary unless that workflow is explicitly changed.
