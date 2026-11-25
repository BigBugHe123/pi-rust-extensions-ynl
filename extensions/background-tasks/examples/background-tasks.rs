//! Runs the extension on stdin and stdout, and `background-tasks --launch`, for the tests that
//! do not build the `pi-rust-ext` dispatcher.

fn main() -> std::process::ExitCode {
    pi_ext_background_tasks::main()
}
