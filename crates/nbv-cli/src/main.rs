//! `nvb`: open a Jupyter notebook in an embedded Neovim.

use std::path::PathBuf;
use std::process::{Command, ExitCode};

use clap::Parser;

/// Neovim Notebooks: edit and run Jupyter notebooks in a real Neovim.
#[derive(Parser)]
#[command(name = "nvb", version)]
struct Cli {
    /// The notebook to open.
    notebook: PathBuf,
    /// Start Neovim without your configuration, to tell nvb bugs from plugin conflicts.
    #[arg(long)]
    clean: bool,
    /// The Neovim to embed (default: $NBV_NVIM, then `nvim` on PATH). Must be 0.12 or newer.
    #[arg(long, value_name = "PATH")]
    nvim: Option<PathBuf>,
}

const MIN_NVIM: (u32, u32) = (0, 12);

/// Parses `NVIM v0.12.5` from `nvim --version`.
fn nvim_version(program: &PathBuf) -> Result<(u32, u32), String> {
    let out = Command::new(program)
        .arg("--version")
        .output()
        .map_err(|e| format!("cannot run {}: {e}", program.display()))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.lines().next().unwrap_or("");
    let v = first.strip_prefix("NVIM v").ok_or_else(|| format!("{} is not Neovim: {first}", program.display()))?;
    let mut parts = v.split('.').map(|p| p.parse::<u32>());
    match (parts.next(), parts.next()) {
        (Some(Ok(major)), Some(Ok(minor))) => Ok((major, minor)),
        _ => Err(format!("cannot read the version of {}: {first}", program.display())),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let nvim = cli.nvim.or_else(|| std::env::var_os("NBV_NVIM").map(PathBuf::from)).unwrap_or_else(|| "nvim".into());
    match nvim_version(&nvim) {
        Ok(v) if v >= MIN_NVIM => {}
        Ok((major, minor)) => {
            eprintln!(
                "nvb: Neovim {}.{} or newer is required; {} is {major}.{minor}",
                MIN_NVIM.0,
                MIN_NVIM.1,
                nvim.display()
            );
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("nvb: {e}");
            return ExitCode::FAILURE;
        }
    }
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
    let result = runtime.block_on(nbv_tui::run(nbv_tui::Options { notebook: cli.notebook, clean: cli.clean, nvim }));
    // Kernel and Neovim tasks may still be winding down; do not wait on them.
    runtime.shutdown_timeout(std::time::Duration::from_millis(200));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("nvb: {e}");
            ExitCode::FAILURE
        }
    }
}
