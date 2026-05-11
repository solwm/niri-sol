//! Parse `~/.config/sol/wallpaper.conf`.
//!
//! Format mirrors sol.conf (Hyprland-ish `key = value`, `#` comments). Keys:
//!
//! ```text
//! dir      = /path/to/directory      # cycle random images from here
//! image    = /path/to/static.png     # OR a single static image
//! interval = 30                      # seconds between cycles; 0 = no cycling
//! fit      = fill                    # fill | fit | stretch | center
//! output   = eDP-1, /path/to/img.png # per-output override (repeatable)
//! ```
//!
//! `dir` and `image` are mutually exclusive; if both are set, `dir` wins.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::Fit;

#[derive(Debug, Default, Clone)]
pub struct WallpaperConfig {
    pub source: Option<Source>,
    pub interval_secs: u64,
    pub fit: Option<Fit>,
    pub per_output: HashMap<String, PathBuf>,
}

#[derive(Debug, Clone)]
pub enum Source {
    Image(PathBuf),
    Dir(PathBuf),
}

impl WallpaperConfig {
    /// Read+parse the file at `path`. Missing-file returns `Ok(default)`.
    pub fn load(path: &Path) -> Result<Self> {
        let body = match std::fs::read_to_string(path) {
            Ok(b) => b,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(err) => {
                return Err(err).with_context(|| format!("read {}", path.display()));
            }
        };
        Self::parse(&body).with_context(|| format!("parse {}", path.display()))
    }

    pub fn parse(body: &str) -> Result<Self> {
        let mut cfg = Self::default();
        for (lineno, raw) in body.lines().enumerate() {
            let lineno = lineno + 1;
            let line = raw.split_once('#').map(|(a, _)| a).unwrap_or(raw).trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                bail!("line {lineno}: missing `=` separator");
            };
            let key = key.trim();
            let value = value.trim();
            match key {
                "dir" => {
                    if value.is_empty() {
                        bail!("line {lineno}: dir: empty value");
                    }
                    cfg.source = Some(Source::Dir(expand_path(value)));
                }
                "image" => {
                    if value.is_empty() {
                        bail!("line {lineno}: image: empty value");
                    }
                    // Only set if `dir` hasn't already won.
                    if !matches!(cfg.source, Some(Source::Dir(_))) {
                        cfg.source = Some(Source::Image(expand_path(value)));
                    }
                }
                "interval" => {
                    cfg.interval_secs = value
                        .parse::<u64>()
                        .with_context(|| format!("line {lineno}: interval: not a u64"))?;
                }
                "fit" => {
                    cfg.fit = Some(parse_fit(value).with_context(|| {
                        format!("line {lineno}: fit: unknown mode `{value}`")
                    })?);
                }
                "output" => {
                    let (name, path) = value.split_once(',').with_context(|| {
                        format!("line {lineno}: output: expected `NAME, PATH`")
                    })?;
                    let name = name.trim();
                    let path = path.trim();
                    if name.is_empty() || path.is_empty() {
                        bail!("line {lineno}: output: empty name or path");
                    }
                    cfg.per_output.insert(name.to_string(), expand_path(path));
                }
                other => {
                    bail!("line {lineno}: unknown key `{other}`");
                }
            }
        }
        Ok(cfg)
    }
}

fn parse_fit(s: &str) -> Result<Fit> {
    Ok(match s {
        "fill" => Fit::Fill,
        "fit" => Fit::Fit,
        "stretch" => Fit::Stretch,
        "center" => Fit::Center,
        _ => bail!("expected one of: fill, fit, stretch, center"),
    })
}

/// Expand a leading `~/` to `$HOME/`. (Bare `~` also works.)
fn expand_path(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            let mut p = PathBuf::from(home);
            p.push(rest);
            return p;
        }
    } else if s == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(s)
}

/// Default config-file location: `$XDG_CONFIG_HOME/sol/wallpaper.conf`,
/// falling back to `$HOME/.config/sol/wallpaper.conf`.
pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| {
                let mut p = PathBuf::from(h);
                p.push(".config");
                p
            })
        })?;
    let mut p = base;
    p.push("sol");
    p.push("wallpaper.conf");
    Some(p)
}
