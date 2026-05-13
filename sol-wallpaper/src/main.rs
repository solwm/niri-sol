use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

mod config;
mod gpu;
mod output;
mod state;

#[derive(Parser, Debug)]
#[command(version, about = "Shader-driven wallpaper daemon for sol")]
struct Cli {
    /// Path to wallpaper.conf. Defaults to `$XDG_CONFIG_HOME/sol/wallpaper.conf`.
    #[arg(long, short = 'c')]
    config: Option<PathBuf>,

    /// Override the WGSL shader path. Overrides `shader` in the config file.
    #[arg(long, short = 's')]
    shader: Option<PathBuf>,
}

/// Resolved runtime configuration after merging file + CLI.
#[derive(Debug)]
pub struct Runtime {
    /// Path to a user-supplied WGSL fragment shader. `None` falls back
    /// to the baked-in default color-blob shader.
    pub shader_path: Option<PathBuf>,
}

fn merge(cli: Cli) -> Result<Runtime> {
    let cfg_path = cli.config.or_else(config::default_path);
    let mut cfg = match cfg_path.as_ref() {
        Some(p) => config::WallpaperConfig::load(p)?,
        None => config::WallpaperConfig::default(),
    };

    if let Some(s) = cli.shader {
        cfg.shader_path = Some(s);
    }

    Ok(Runtime {
        shader_path: cfg.shader_path,
    })
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let runtime = merge(cli)?;
    state::run(runtime)
}
