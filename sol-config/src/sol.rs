//! Parser for the sol config format (Hyprland-ish key=value lines).
//!
//! See `~/.config/sol/sol.conf` for the format we accept. This module produces a
//! [`Config`] matching what the KDL parser would build, so the rest of niri does not need to
//! change.
//!
//! Settings we don't (yet) understand — animation springs, frosted-glass blur, opacity dimming,
//! corner radius, idle timeout — are *parsed and ignored* so a sol config with rich animation
//! tuning still loads cleanly.

use std::path::Path;
use std::str::FromStr;

use miette::miette;
use sol_ipc::{ConfiguredMode, Transform};
use smithay::input::keyboard::xkb::{keysym_from_name, KEYSYM_CASE_INSENSITIVE};
use smithay::input::keyboard::Keysym;
use tracing::warn;

use crate::animations::{Curve, EasingParams, Kind};
use crate::appearance::CornerRadius;
use crate::binds::{Action, Bind, Key, Modifiers, Trigger, WorkspaceReference};
use crate::misc::SpawnAtStartup;
use crate::output::{Mode, Output};
use crate::window_rule::WindowRule;
use crate::Config;

/// Output name we apply `mode` to. Sol's syntax has no per-output selector, so we hardcode the
/// connector niri is currently driving for this user. If you swap monitors with a different
/// name, edit this here or extend the format.
const PRIMARY_OUTPUT_NAME: &str = "DP-2";

pub fn parse_sol(_path: &Path, text: &str) -> miette::Result<Config> {
    let mut config = Config::default();

    // Reasonable master-stack defaults for sol users.
    config.prefer_no_csd = true;
    config.layout.workspace_count = 5;
    config.layout.focus_ring.off = false;

    for (idx, raw_line) in text.lines().enumerate() {
        let lineno = idx + 1;

        // Strip comments and trim. A line is the smallest unit; we don't try to handle
        // multi-line continuations.
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            return Err(miette!(
                "sol config line {lineno}: expected `key = value`, got: {raw_line:?}"
            ));
        };
        let key = key.trim();
        let value = value.trim();

        if let Err(err) = apply_setting(&mut config, key, value, lineno) {
            return Err(err.wrap_err(format!("sol config line {lineno}: failed to apply `{key}`")));
        }
    }

    Ok(config)
}

fn apply_setting(config: &mut Config, key: &str, value: &str, lineno: usize) -> miette::Result<()> {
    match key {
        "remap" => apply_remap(config, value, lineno)?,

        "gaps_in" => {
            config.layout.gaps = parse_f64(value, lineno, "gaps_in")?;
        }
        // niri only has a single gaps value; sol's outer gap is folded in implicitly via struts
        // for v1.
        "gaps_out" => {}

        "border_width" => {
            let w = parse_f64(value, lineno, "border_width")?;
            config.layout.focus_ring.width = w;
            config.layout.focus_ring.off = w == 0.0;
        }

        "border_color" => {
            let s = if value.starts_with('#') {
                value.to_string()
            } else {
                format!("#{value}")
            };
            config.layout.focus_ring.active_color = s
                .parse()
                .map_err(|e| miette!("border_color {value:?}: {e}"))?;
        }

        "mode" => {
            let mode = ConfiguredMode::from_str(value)
                .map_err(|e| miette!("mode {value:?}: {e}"))?;
            config.outputs.0.push(Output {
                off: false,
                name: PRIMARY_OUTPUT_NAME.into(),
                scale: None,
                transform: Transform::Normal,
                position: None,
                mode: Some(Mode {
                    custom: false,
                    mode,
                }),
                modeline: None,
                variable_refresh_rate: None,
                focus_at_startup: false,
                background_color: None,
                backdrop_color: None,
                hot_corners: None,
                layout: None,
            });
        }

        "keyboard_repeat_rate" => {
            config.input.keyboard.repeat_rate = value
                .parse::<u8>()
                .map_err(|e| miette!("keyboard_repeat_rate {value:?}: {e}"))?;
        }
        "keyboard_repeat_delay" => {
            config.input.keyboard.repeat_delay = value
                .parse::<u16>()
                .map_err(|e| miette!("keyboard_repeat_delay {value:?}: {e}"))?;
        }

        "exec-once" => {
            let argv =
                shell_split(value).ok_or_else(|| miette!("exec-once: malformed quoting"))?;
            if argv.is_empty() {
                return Err(miette!("exec-once: empty command"));
            }
            config.spawn_at_startup.push(SpawnAtStartup { command: argv });
        }

        "bind" => apply_bind(config, value, lineno)?,

        "corner_radius" => {
            // Global rounded-corner radius applied to every window. We synthesize a
            // matchless `WindowRule` (rules with empty `matches` apply to everything)
            // that sets both `geometry_corner_radius` and `clip_to_geometry = true`.
            // The rest of the render path already supports rounded clipping + a
            // matching focus-ring curve via the existing `BorderRenderElement` /
            // `ClippedSurfaceRenderElement` shaders; this just plumbs a global value
            // in as if the user had written one giant catch-all window-rule.
            //
            // Per-window window-rules can still override this because window-rules
            // overlay last-wins, and our synthesized rule lives at index 0.
            let radius = parse_f64(value, lineno, "corner_radius")? as f32;
            if !radius.is_finite() || radius < 0. {
                return Err(miette!(
                    "line {lineno}: corner_radius {value:?}: must be a non-negative number"
                ));
            }
            config.window_rules.insert(
                0,
                WindowRule {
                    geometry_corner_radius: Some(CornerRadius::from(radius)),
                    clip_to_geometry: Some(true),
                    ..WindowRule::default()
                },
            );
        }

        "inactive_alpha" => {
            let a = parse_f64(value, lineno, "inactive_alpha")? as f32;
            if !a.is_finite() || !(0. ..=1.).contains(&a) {
                return Err(miette!(
                    "line {lineno}: inactive_alpha {value:?}: must be in [0.0, 1.0]"
                ));
            }
            config.inactive_alpha = Some(a);
        }

        "inactive_blur" => {
            config.inactive_blur = parse_on_off(value, lineno, "inactive_blur")?;
        }

        "wallpaper_daemon" => {
            config.wallpaper_daemon = parse_on_off(value, lineno, "wallpaper_daemon")?;
        }

        "inactive_blur_passes" => {
            config.blur.passes = value.parse::<u8>().map_err(|e| {
                miette!("line {lineno}: inactive_blur_passes {value:?}: {e}")
            })?;
        }

        "inactive_blur_radius" => {
            // Map sol.conf's `inactive_blur_radius` to the existing global blur's
            // Kawase `offset` parameter — the value that controls how blurry the
            // sample looks per pass. Naming kept user-facing for sol.conf
            // familiarity.
            let r = parse_f64(value, lineno, "inactive_blur_radius")?;
            if !r.is_finite() || r < 0. {
                return Err(miette!(
                    "line {lineno}: inactive_blur_radius {value:?}: must be a non-negative number"
                ));
            }
            config.blur.offset = r;
        }

        // Tile-movement spring (master↔stack swap, stack vertical reorder).
        // Tile slides from its old slot to the new one with spring physics.
        // `on` enables the animation, `off` makes slots snap instantly.
        "tile_movement" => {
            let enabled = parse_on_off(value, lineno, "tile_movement")?;
            config.animations.tile_movement.0.off = !enabled;
        }
        "tile_movement_damping_ratio" => {
            let v = parse_f64(value, lineno, "tile_movement_damping_ratio")?;
            if !(0.1..=10.).contains(&v) {
                return Err(miette!(
                    "line {lineno}: tile_movement_damping_ratio {value:?}: must be between 0.1 and 10"
                ));
            }
            set_tile_movement_spring(&mut config.animations.tile_movement.0, |p| {
                p.damping_ratio = v
            });
        }
        "tile_movement_stiffness" => {
            let v = value.parse::<u32>().map_err(|e| {
                miette!("line {lineno}: tile_movement_stiffness {value:?}: {e}")
            })?;
            if v == 0 {
                return Err(miette!(
                    "line {lineno}: tile_movement_stiffness {value:?}: must be >= 1"
                ));
            }
            set_tile_movement_spring(&mut config.animations.tile_movement.0, |p| p.stiffness = v);
        }
        "tile_movement_epsilon" => {
            let v = parse_f64(value, lineno, "tile_movement_epsilon")?;
            if !(0.00001..=0.1).contains(&v) {
                return Err(miette!(
                    "line {lineno}: tile_movement_epsilon {value:?}: must be between 0.00001 and 0.1"
                ));
            }
            set_tile_movement_spring(&mut config.animations.tile_movement.0, |p| p.epsilon = v);
        }

        // Bezier-easing tuning for tile movement. The default is a
        // Hyprland-style overshoot curve (see `wind_curve` in
        // animations.rs); these knobs let users retune duration + curve
        // without dropping back to a spring.
        "tile_movement_duration_ms" => {
            let v = value
                .parse::<u32>()
                .map_err(|e| miette!("line {lineno}: tile_movement_duration_ms {value:?}: {e}"))?;
            set_easing(&mut config.animations.tile_movement.0, |p| p.duration_ms = v);
        }
        "tile_movement_curve" => {
            let curve = parse_curve(value, lineno)?;
            set_easing(&mut config.animations.tile_movement.0, |p| p.curve = curve);
        }

        // Window open animation (fade-in + zoom-in 0.5 → 1.0).
        // Driven by an easing curve over `window_open_duration_ms`.
        "window_open" => {
            let enabled = parse_on_off(value, lineno, "window_open")?;
            config.animations.window_open.anim.off = !enabled;
        }
        "window_open_duration_ms" => {
            let v = value
                .parse::<u32>()
                .map_err(|e| miette!("line {lineno}: window_open_duration_ms {value:?}: {e}"))?;
            set_easing(&mut config.animations.window_open.anim, |p| p.duration_ms = v);
        }
        "window_open_curve" => {
            let curve = parse_curve(value, lineno)?;
            set_easing(&mut config.animations.window_open.anim, |p| p.curve = curve);
        }

        // Window close animation (fade-out + zoom-out 1.0 → 0.8).
        "window_close" => {
            let enabled = parse_on_off(value, lineno, "window_close")?;
            config.animations.window_close.anim.off = !enabled;
        }
        "window_close_duration_ms" => {
            let v = value
                .parse::<u32>()
                .map_err(|e| miette!("line {lineno}: window_close_duration_ms {value:?}: {e}"))?;
            set_easing(&mut config.animations.window_close.anim, |p| p.duration_ms = v);
        }
        "window_close_curve" => {
            let curve = parse_curve(value, lineno)?;
            set_easing(&mut config.animations.window_close.anim, |p| p.curve = curve);
        }

        // ──── Parse-and-ignore (not yet implemented in niri's master-stack rework) ────
        "idle_timeout" => {}

        other => {
            warn!("sol config line {lineno}: unknown key `{other}` (ignored)");
        }
    }
    Ok(())
}

fn parse_f64(s: &str, lineno: usize, name: &str) -> miette::Result<f64> {
    s.parse::<f64>()
        .map_err(|e| miette!("line {lineno}: {name} = {s:?}: {e}"))
}

/// Mutate the tile-movement spring params, switching `Animation.kind` to
/// `Kind::Spring` if it wasn't already. Sol's flat config only exposes
/// spring tuning for tile motion; if a KDL config layer had set an easing
/// curve, this coerces back to a spring with the new override applied on
/// top of the default spring params.
fn set_tile_movement_spring(
    anim: &mut crate::animations::Animation,
    f: impl FnOnce(&mut crate::animations::SpringParams),
) {
    use crate::animations::SpringParams;
    let mut params = match anim.kind {
        Kind::Spring(p) => p,
        _ => SpringParams {
            damping_ratio: 1.,
            stiffness: 600,
            epsilon: 0.0001,
        },
    };
    f(&mut params);
    anim.kind = Kind::Spring(params);
}

/// Mutate the easing params of an animation. Switches `Animation.kind`
/// to `Kind::Easing` if it wasn't already; the user can mix spring + easing
/// at the per-property level in KDL, but the flat sol.conf format only
/// drives easing for open/close.
fn set_easing(
    anim: &mut crate::animations::Animation,
    f: impl FnOnce(&mut EasingParams),
) {
    let mut params = match anim.kind {
        Kind::Easing(p) => p,
        _ => EasingParams {
            duration_ms: 150,
            curve: Curve::EaseOutQuad,
        },
    };
    f(&mut params);
    anim.kind = Kind::Easing(params);
}

/// Parse an easing curve name into [`Curve`]. Names match niri's KDL
/// config (kebab-case), case-insensitive. `cubic-bezier(x1,y1,x2,y2)` is
/// supported as an inline form.
fn parse_curve(s: &str, lineno: usize) -> miette::Result<Curve> {
    let lower = s.to_ascii_lowercase();
    let trimmed = lower.trim();
    match trimmed {
        "linear" => Ok(Curve::Linear),
        "ease-out-quad" | "ease_out_quad" => Ok(Curve::EaseOutQuad),
        "ease-out-cubic" | "ease_out_cubic" => Ok(Curve::EaseOutCubic),
        "ease-out-expo" | "ease_out_expo" => Ok(Curve::EaseOutExpo),
        s if s.starts_with("cubic-bezier(") && s.ends_with(')') => {
            let inner = &s["cubic-bezier(".len()..s.len() - 1];
            let mut nums = inner.split(',').map(|p| p.trim().parse::<f64>());
            let x1 = nums.next().and_then(|r| r.ok());
            let y1 = nums.next().and_then(|r| r.ok());
            let x2 = nums.next().and_then(|r| r.ok());
            let y2 = nums.next().and_then(|r| r.ok());
            match (x1, y1, x2, y2) {
                (Some(x1), Some(y1), Some(x2), Some(y2)) => {
                    Ok(Curve::CubicBezier(x1, y1, x2, y2))
                }
                _ => Err(miette!(
                    "line {lineno}: cubic-bezier expects four comma-separated numbers"
                )),
            }
        }
        _ => Err(miette!(
            "line {lineno}: unknown curve {s:?}: expected one of \
             linear, ease-out-quad, ease-out-cubic, ease-out-expo, \
             cubic-bezier(x1,y1,x2,y2)"
        )),
    }
}

/// Parse a Hyprland-style on/off value (also accepts true/false, 1/0, yes/no).
/// Anything else is rejected with a clear error.
fn parse_on_off(s: &str, lineno: usize, name: &str) -> miette::Result<bool> {
    match s.to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Ok(true),
        "off" | "false" | "no" | "0" => Ok(false),
        _ => Err(miette!(
            "line {lineno}: {name} {s:?}: expected on/off (true/false, yes/no, 1/0)"
        )),
    }
}

/// Sol's `remap = FROM, TO` is a directional one-key remap. We translate the common
/// (CapsLock, Escape) case into the corresponding xkb option. Other remaps are not
/// representable as a stock xkb option and are warned about.
fn apply_remap(config: &mut Config, value: &str, lineno: usize) -> miette::Result<()> {
    let mut parts = value.split(',').map(str::trim);
    let (Some(from), Some(to)) = (parts.next(), parts.next()) else {
        return Err(miette!(
            "remap: expected `FROM_KEY, TO_KEY`, got: {value:?}"
        ));
    };
    if parts.next().is_some() {
        return Err(miette!("remap: too many comma-separated values"));
    }

    let xkb_option = match (
        from.to_ascii_lowercase().as_str(),
        to.to_ascii_lowercase().as_str(),
    ) {
        ("capslock", "escape") => "caps:escape",
        _ => {
            warn!(
                "sol config line {lineno}: remap {from:?} -> {to:?} has no direct xkb option; \
                 ignored. Add it manually via xkb options if needed."
            );
            return Ok(());
        }
    };

    let opts = config
        .input
        .keyboard
        .xkb
        .options
        .as_deref()
        .unwrap_or("");
    let merged = if opts.is_empty() {
        xkb_option.to_string()
    } else {
        format!("{opts},{xkb_option}")
    };
    config.input.keyboard.xkb.options = Some(merged);
    Ok(())
}

/// `bind = MODS, KEY, ACTION[, ARGS]` — modifiers `+`-joined, the rest comma-separated.
fn apply_bind(config: &mut Config, value: &str, lineno: usize) -> miette::Result<()> {
    let mut parts = value.splitn(4, ',').map(str::trim);
    let (Some(mods), Some(key), Some(action_name)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(miette!(
            "bind: expected `MODS, KEY, ACTION[, ARGS]`, got: {value:?}"
        ));
    };
    let rest = parts.next().unwrap_or("").trim();

    let modifiers = parse_modifiers(mods, lineno)?;
    let trigger = parse_trigger(key, lineno)?;
    let action = parse_action(action_name, rest, lineno)?;

    config.binds.0.push(Bind {
        key: Key { trigger, modifiers },
        action,
        repeat: true,
        cooldown: None,
        allow_when_locked: false,
        allow_inhibiting: true,
        hotkey_overlay_title: None,
    });
    Ok(())
}

fn parse_modifiers(s: &str, lineno: usize) -> miette::Result<Modifiers> {
    let mut m = Modifiers::empty();
    for token in s.split('+').map(str::trim).filter(|s| !s.is_empty()) {
        match token.to_ascii_uppercase().as_str() {
            "CTRL" | "CONTROL" => m |= Modifiers::CTRL,
            "ALT" | "MOD1" => m |= Modifiers::ALT,
            "SHIFT" => m |= Modifiers::SHIFT,
            "SUPER" | "META" | "MOD" | "WIN" => m |= Modifiers::SUPER,
            other => {
                return Err(miette!("line {lineno}: unknown modifier `{other}`"));
            }
        }
    }
    Ok(m)
}

fn parse_trigger(key: &str, lineno: usize) -> miette::Result<Trigger> {
    let keysym = keysym_from_name(key, KEYSYM_CASE_INSENSITIVE);
    if keysym == Keysym::NoSymbol {
        return Err(miette!("line {lineno}: unknown key `{key}`"));
    }
    Ok(Trigger::Keysym(keysym))
}

fn parse_action(name: &str, rest: &str, lineno: usize) -> miette::Result<Action> {
    Ok(match name {
        "exec" => {
            let argv = shell_split(rest)
                .ok_or_else(|| miette!("line {lineno}: exec: malformed quoting"))?;
            if argv.is_empty() {
                return Err(miette!("line {lineno}: exec: empty command"));
            }
            Action::Spawn(argv)
        }
        "focus_dir" => match rest {
            "left" => Action::FocusColumnLeft,
            "right" => Action::FocusColumnRight,
            "up" => Action::FocusWindowUp,
            "down" => Action::FocusWindowDown,
            _ => {
                return Err(miette!(
                    "line {lineno}: focus_dir requires left/right/up/down, got `{rest}`"
                ))
            }
        },
        "move_dir" => match rest {
            "left" => Action::MoveColumnLeft,
            "right" => Action::MoveColumnRight,
            "up" => Action::MoveWindowUp,
            "down" => Action::MoveWindowDown,
            _ => {
                return Err(miette!(
                    "line {lineno}: move_dir requires left/right/up/down, got `{rest}`"
                ))
            }
        },
        "toggle_zoom" => Action::MaximizeWindowToEdges,
        "toggle_fullscreen" => Action::FullscreenWindow,
        "close_window" => Action::CloseWindow,
        "workspace" => {
            let n: u8 = rest
                .parse()
                .map_err(|e| miette!("line {lineno}: workspace index `{rest}`: {e}"))?;
            Action::FocusWorkspace(WorkspaceReference::Index(n))
        }
        "move_to_workspace" => {
            let n: u8 = rest
                .parse()
                .map_err(|e| miette!("line {lineno}: move_to_workspace index `{rest}`: {e}"))?;
            Action::MoveWindowToWorkspace(WorkspaceReference::Index(n), false)
        }
        // Sol-only: enters the modal resize mode (h/l adjusts master,
        // Escape exits). All keys are intercepted while the mode is on.
        "resize_mode" => Action::EnterResizeMode,

        other => {
            return Err(miette!("line {lineno}: unknown action `{other}`"));
        }
    })
}

/// Minimal POSIX-shell-ish word splitter. Supports `"..."` quoted strings with `\\` escapes
/// inside them; no globbing, no env expansion. Returns None if quoting is unbalanced.
fn shell_split(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut had_token = false;
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\\' if in_quote => {
                let next = chars.next()?;
                cur.push(next);
            }
            '"' => {
                in_quote = !in_quote;
                had_token = true;
            }
            ' ' | '\t' if !in_quote => {
                if had_token {
                    out.push(std::mem::take(&mut cur));
                    had_token = false;
                }
            }
            _ => {
                cur.push(c);
                had_token = true;
            }
        }
    }
    if in_quote {
        return None;
    }
    if had_token {
        out.push(cur);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_split_basics() {
        assert_eq!(shell_split("foo bar"), Some(vec!["foo".into(), "bar".into()]));
        assert_eq!(
            shell_split("sh -c \"echo hi\""),
            Some(vec!["sh".into(), "-c".into(), "echo hi".into()])
        );
        assert_eq!(
            shell_split("rofi -show drun"),
            Some(vec!["rofi".into(), "-show".into(), "drun".into()])
        );
        assert_eq!(shell_split("\"unbalanced"), None);
    }

    #[test]
    fn parse_minimal() {
        let text = r#"
            # comment
            gaps_in = 12
            border_width = 2
            border_color = ffff00
            keyboard_repeat_rate = 100
            keyboard_repeat_delay = 200
            exec-once = waybar
            bind = ALT, Return, exec, soltty -e zsh
            bind = ALT, H, focus_dir, left
            bind = CTRL+ALT, Y, move_to_workspace, 1
            remap = CapsLock, Escape
        "#;
        let config = parse_sol(Path::new("sol.conf"), text).expect("parse");
        assert_eq!(config.layout.gaps, 12.0);
        assert_eq!(config.layout.focus_ring.width, 2.0);
        assert!(!config.layout.focus_ring.off);
        assert_eq!(config.input.keyboard.repeat_rate, 100);
        assert_eq!(config.input.keyboard.repeat_delay, 200);
        assert_eq!(config.spawn_at_startup.len(), 1);
        assert_eq!(config.binds.0.len(), 3);
        assert_eq!(
            config.input.keyboard.xkb.options.as_deref(),
            Some("caps:escape")
        );
    }
}
