# 插件编译约定

- 编译插件时，每个插件都必须单独构建为一个独立的可执行文件（Windows 下为 `.exe`），不能把多个插件合并进同一个可执行文件。
- 每个插件项目都应在 `src/lib.rs` 中提供 `pub fn main() -> std::process::ExitCode` 作为启动入口。构建该插件可执行文件时，应将此函数作为二进制入口调用。
- 插件通常以独立 Cargo package 放在 `extensions/<插件名>` 下；使用 Cargo 按 package 单独编译，例如：`cargo build --release -p pi-ext-workflows`。生成的可执行文件应能按插件独立分发和安装。
