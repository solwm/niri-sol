//! Parse `~/.config/sol/wallpaper.conf`.
//!
//! Format mirrors sol.conf (Hyprland-ish `key = value`, `#` comments).
//! Single key currently supported:
//!
//! ```text
//! shader = /path/to/wallpaper.wgsl   # WGSL fragment shader; default is built-in
//! ```
//!
//! Missing or unset → daemon uses the baked-in color-blob shader.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

#[derive(Debug, Default, Clone)]
pub struct WallpaperConfig {
    pub shader_path: Option<PathBuf>,
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
                "shader" => {
                    if value.is_empty() {
                        bail!("line {lineno}: shader: empty value");
                    }
                    cfg.shader_path = Some(expand_path(value));
                }
                other => {
                    tracing::warn!("line {lineno}: unknown key `{other}` (ignored)");
                }
            }
        }
        Ok(cfg)
    }
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
