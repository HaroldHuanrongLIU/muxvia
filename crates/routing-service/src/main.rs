use std::{env, fs, path::PathBuf};

use clap::Parser;
use muxvia_routing::service::process::{ProcessOptions, run};

#[derive(Parser)]
#[command(name = "muxvia-routing", version)]
struct Args {
    #[arg(long, value_name = "ABSOLUTE_MUXVIA_HOME")]
    home: Option<PathBuf>,
    #[arg(long, hide = true)]
    test_shutdown_file: Option<PathBuf>,
    #[arg(long, hide = true)]
    test_codex_executable: Option<PathBuf>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args = Args::parse();
    let integration = env::var("MUXVIA_INTEGRATION_TEST").as_deref() == Ok("1");
    if !integration && (args.test_shutdown_file.is_some() || args.test_codex_executable.is_some()) {
        eprintln!("test-only Routing Service options require integration invocation");
        std::process::exit(64);
    }
    let home = match args.home {
        Some(path) if path.is_absolute() => path,
        Some(_) => {
            eprintln!("--home must be absolute");
            std::process::exit(64);
        }
        None => match env::var_os("HOME") {
            Some(user_home) => PathBuf::from(user_home).join(".muxvia"),
            None => {
                eprintln!("HOME is required when --home is omitted");
                std::process::exit(64);
            }
        },
    };
    let codex_executable = args
        .test_codex_executable
        .or_else(find_codex_executable)
        .unwrap_or_else(|| PathBuf::from("/usr/bin/codex"));
    let options = ProcessOptions {
        home,
        test_shutdown_file: args.test_shutdown_file,
        codex_executable,
        release: env!("CARGO_PKG_VERSION").to_owned(),
    };
    if let Err(error) = run(options).await {
        eprintln!("{error}");
        std::process::exit(error.exit_code());
    }
}

fn find_codex_executable() -> Option<PathBuf> {
    env::var_os("PATH")
        .and_then(|path| {
            env::split_paths(&path)
                .map(|directory| directory.join("codex"))
                .find(|candidate| candidate.is_file())
        })
        .and_then(|path| fs::canonicalize(path).ok())
}
