use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::Parser;

mod config;
mod egl;
mod img;
mod output;
mod render;
mod state;

use config::Source;

#[derive(Parser, Debug)]
#[command(version, about = "Minimal Wayland wallpaper daemon for sol")]
struct Cli {
    /// Path to wallpaper.conf. Defaults to `$XDG_CONFIG_HOME/sol/wallpaper.conf`.
    #[arg(long, short = 'c')]
    config: Option<PathBuf>,

    /// Static image path, or `OUTPUT=PATH` for per-output. Repeatable.
    /// Overrides `image` / `output` keys in the config file.
    #[arg(long = "image", short = 'i')]
    image: Vec<String>,

    /// Directory to randomly cycle. Overrides `dir` in the config file.
    #[arg(long, short = 'd')]
    dir: Option<PathBuf>,

    /// Cycle interval in seconds. 0 disables cycling. Overrides config.
    #[arg(long)]
    interval: Option<u64>,

    /// Scaling mode. Overrides config.
    #[arg(long, value_enum)]
    fit: Option<Fit>,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit {
    /// Cover the output; crop overflow.
    Fill,
    /// Fit inside the output; letterbox the rest.
    Fit,
    /// Stretch to the output's exact size (may distort).
    Stretch,
    /// Center at native resolution; clip / letterbox the rest.
    Center,
}

/// Resolved runtime configuration after merging file + CLI.
#[derive(Debug)]
pub struct Runtime {
    pub source: Option<Source>,
    pub interval_secs: u64,
    pub fit: Fit,
    /// Per-output static overrides (NEVER cycled, even when source is a dir).
    pub per_output: HashMap<String, PathBuf>,
}

fn merge(cli: Cli) -> Result<Runtime> {
    // 1. Load file.
    let cfg_path = cli.config.or_else(config::default_path);
    let mut cfg = match cfg_path.as_ref() {
        Some(p) => config::WallpaperConfig::load(p)?,
        None => config::WallpaperConfig::default(),
    };

    // 2. CLI overrides.
    if let Some(dir) = cli.dir {
        cfg.source = Some(Source::Dir(dir));
    } else if !cli.image.is_empty() {
        // Parse `--image`. A single bare path becomes the static source; any
        // `OUTPUT=PATH` entries become per-output overrides.
        let mut bare: Option<PathBuf> = None;
        for entry in cli.image {
            if let Some((out, p)) = entry.split_once('=') {
                cfg.per_output.insert(out.to_string(), PathBuf::from(p));
            } else {
                if bare.is_some() {
                    bail!("multiple bare --image entries; use OUTPUT=PATH for per-output");
                }
                bare = Some(PathBuf::from(entry));
            }
        }
        if let Some(b) = bare {
            // Only override config's source if CLI provided a bare path;
            // `--image OUTPUT=PATH` alone is additive.
            cfg.source = Some(Source::Image(b));
        }
    }
    if let Some(iv) = cli.interval {
        cfg.interval_secs = iv;
    }
    if let Some(f) = cli.fit {
        cfg.fit = Some(f);
    }

    let fit = cfg.fit.unwrap_or(Fit::Fill);
    if cfg.source.is_none() && cfg.per_output.is_empty() {
        bail!(
            "no wallpaper source. Set `dir` or `image` in wallpaper.conf, \
             or pass --dir / --image on the command line."
        );
    }

    Ok(Runtime {
        source: cfg.source,
        interval_secs: cfg.interval_secs,
        fit,
        per_output: cfg.per_output,
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
