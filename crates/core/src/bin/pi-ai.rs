//! `pi-ai` binary — thin wrapper around `pi_core::ai::cli` (the port of
//! `pi-core/ai/src/cli.ts`, the package's `bin` entry). Wires real stdio,
//! the real builtin OAuth flows, and the cwd-relative `auth.json`, then
//! maps the result onto the observable process behavior: errors print
//! `Error: <message>` to stderr and exit 1, mirroring `main().catch`.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

use pi_core::ai::cli::{self, CliIo};

/// Real-console [`CliIo`]: stdin lines, flushed stdout/stderr writes, and
/// `auth.json` in the current working directory.
struct StdioCliIo;

impl CliIo for StdioCliIo {
    fn read_line(&self) -> Option<String> {
        let stdin = std::io::stdin();
        let mut line = String::new();
        let bytes = stdin.lock().read_line(&mut line).ok()?;
        if bytes == 0 {
            return None;
        }
        // node:readline strips the terminator (\n, or \r\n) from the answer.
        if line.ends_with('\n') {
            line.pop();
        }
        if line.ends_with('\r') {
            line.pop();
        }
        Some(line)
    }

    fn write_stdout(&self, text: &str) {
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        let _ = lock.write_all(text.as_bytes());
        let _ = lock.flush();
    }

    fn write_stderr(&self, text: &str) {
        let stderr = std::io::stderr();
        let mut lock = stderr.lock();
        let _ = lock.write_all(text.as_bytes());
        let _ = lock.flush();
    }

    fn auth_file_path(&self) -> PathBuf {
        std::env::current_dir()
            .unwrap_or_default()
            .join(cli::AUTH_FILE)
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    let code = match runtime.block_on(cli::run_cli(&args, Arc::new(StdioCliIo))) {
        Ok(()) => 0,
        Err(message) => {
            eprintln!("Error: {message}");
            1
        }
    };
    std::process::exit(code);
}
