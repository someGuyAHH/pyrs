use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    name = "pyrs",
    version,
    about = "Bundle a Python project into one file and run it on any machine that has Python"
)]
pub struct Cli {
    /// Suppress pyrs status messages (the program's own output is untouched)
    #[arg(short, long, global = true)]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Build a project directory into a .pyrs bundle
    Build(BuildArgs),
    /// Unpack a bundle and run it
    Run(RunArgs),
    /// Show what a bundle contains
    Inspect(InspectArgs),
}

#[derive(Args)]
pub struct BuildArgs {
    /// Project directory (must contain a pyproject.toml)
    #[arg(default_value = ".")]
    pub dir: PathBuf,

    /// Output file [default: ./<name>-<version>.pyrs]
    #[arg(short, long)]
    pub output: Option<PathBuf>,

    /// Console script or package to run, when the project has more than one
    #[arg(long)]
    pub entry: Option<String>,

    /// Python to build with; it must have pip [env: PYRS_PYTHON]
    #[arg(long, value_name = "PATH_OR_NAME")]
    pub python: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum EnvKind {
    /// Fresh virtual environment in a temporary workspace (default)
    Temp,
    /// The interpreter's own environment; nothing is written to its site-packages
    Global,
    /// An existing virtual environment, given with --venv
    Venv,
}

#[derive(Args)]
pub struct RunArgs {
    /// The .pyrs bundle to run
    pub bundle: PathBuf,

    /// Which environment to run in [default: temp, or venv when --venv is given]
    #[arg(long, value_enum)]
    pub env: Option<EnvKind>,

    /// Use this existing virtual environment
    #[arg(long, value_name = "DIR")]
    pub venv: Option<PathBuf>,

    /// Python to use for temp/global runs [env: PYRS_PYTHON]
    #[arg(long, value_name = "PATH_OR_NAME", conflicts_with = "venv")]
    pub python: Option<String>,

    /// Keep the prepared environment in the cache and reuse it on later runs (temp/global)
    #[arg(long)]
    pub cache: bool,

    /// With --venv: run even if the project isn't installed there, instead of installing it
    #[arg(long)]
    pub no_install: bool,

    /// Arguments passed to the program, after `--`
    #[arg(last = true)]
    pub args: Vec<OsString>,
}

#[derive(Args)]
pub struct InspectArgs {
    /// The .pyrs bundle to inspect
    pub bundle: PathBuf,
}
