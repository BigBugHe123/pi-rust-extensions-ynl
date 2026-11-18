# Local build and install preference

When asked to compile or install the plugins in this repository, build and install each crate in `extensions/<name>` as its own executable. Each extension crate has a `src/main.rs` entry point that calls that crate's library `main()`.

- Use the per-extension binaries named `pi-ext-<name>` (with `.exe` on Windows).
- Install one executable and one `package.json` with one `pi.native` entry per extension under `<agentDir>/extensions/<name>`.
- Do not use `crates/pi-rust-ext` for local per-plugin builds or installs; it is the multi-call aggregate binary.
- Do not also add an extension directory under `<agentDir>/extensions` to the `packages` setting. The agent extensions directory is auto-discovered, and loading the same package both ways creates duplicate extensions and commands.

This is the local build/install preference. The repository's existing Marketplace release workflow still uses the aggregate binary unless that workflow is explicitly changed.

## Native extension package manifest

When installing a per-extension executable, write a valid Pi native package manifest. `pi.native` must be an array of entry objects, each with a `command` string; it is not a single object. Resolve the command relative to that extension package directory and do not include shell quotes in the value. For example:

```json
{
  "name": "pi-ext-todo",
  "version": "0.5.16",
  "pi": {
    "native": [
      { "command": "pi-ext-todo.exe" }
    ]
  }
}
```

Pi discovers these manifests under `<agentDir>/extensions/<name>` and starts each native command as a child process. A malformed `pi.native` shape can leave the executable undiscovered, so verify the array structure and command path when installing. Paseo gets slash commands from the running Pi RPC session; restart that session after installing or changing extension manifests.
