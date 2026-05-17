# sol

> [!WARNING]
> **Work in progress.** Personal fork that I daily-drive on my own
> hardware. APIs change, defaults change, scope wanders. The `main`
> branch is "today's experiment", not a release — treat it accordingly.

sol is a fork of [niri](https://github.com/YaLTeR/niri). The renderer,
input stack, protocols, and most of the backend come from niri unchanged;
the layout engine has been replaced and the animation system rebuilt
around springs and crossfades.

## How sol differs from niri

### Layout

|  | niri | sol |
|---|---|---|
| Arrangement | Scrollable horizontal strip of columns | Master-stack: one master pane + a vertical stack column |
| New windows | Pushed onto the end of the strip; no resize | Land in the stack; master is untouched |
| Workspaces | Dynamic, arranged vertically, always one empty | Static *N* workspaces per monitor |
| Fullscreen | Resizes the column to fill the screen | Full-area takeover that auto-exits when a new window is focused |

The master pane is resizable via an `ALT+R` modal (`h`/`l` to step,
`Esc` to commit). Master ↔ stack swap keeps the demoted column in its
original slot instead of reshuffling.

### Animations

|  | niri | sol |
|---|---|---|
| Tile motion | Easing curves | Spring-based slide, with crossfade between source/dest |
| Default curve | niri defaults | Hyprland-style overshoot — `cubic-bezier(0.05, 0.9, 0.1, 1.05)` |
| Workspace switch | Vertical slide | Crossfade + zoom (outgoing fades & shrinks, incoming fades in & grows) |
| Window close | Element removed | Snapshot is rendered and sprung out; surviving tiles spring into the freed slot |
| Focus ring during motion | Stays opaque | Fades out while the tile slides |

### Other touches

- **sol-wallpaper** — a standalone wgpu shader daemon that owns the
  background instead of niri's static-image path.
- **Frosted-glass blur + inactive-alpha** for unfocused windows,
  routed through a manual-blend offscreen so fractional alpha composes
  cleanly even on NVIDIA.
- **`sol.conf`** with a hand-written parser, preferred over `config.kdl`.
  Tuning lives in `~/.config/sol/sol.conf`.
- **Disabled by default**: niri's hot-corner overview trigger and the
  hotkey-overlay popup that pops on startup.
- **Screen-sharing** works end-to-end through `xdg-desktop-portal-hyprland`
  + PipeWire on NVIDIA — see `~/.config/xdg-desktop-portal/sol-portals.conf`.

## Building

Same toolchain as niri:

```
cargo build --release
./target/release/sol --session
```

## Credit

Everything below the layout and animation layers is niri's work by
[Ivan Molodetskikh](https://github.com/YaLTeR) and contributors. Sol
rebases on niri periodically; if you're after a stable scrollable-tiling
compositor, **use niri** — sol diverges from upstream in ways that get
tested nowhere but on my desk.
