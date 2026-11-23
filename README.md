# ghl

A minimal alternative to G-Helper for Linux: control ASUS laptop hardware directly
from sysfs, `ryzenadj`, and Wayland helpers — no daemon, no polling, no async.
Ships a CLI and a TUI.

## Features

- Performance modes: silent / balanced / turbo (`platform_profile`)
- **Per-mode CPU boost**: individual on/off/auto override for each performance mode
- Fan curves per fan and per mode (`asus_custom_fan_curve`), with presets
- ryzenadj: CPU/iGPU/NPU power limits, clocks, temps, fclk, power saving
- GPU mode switching (iGPU / hybrid / dGPU), NVIDIA power fix (TGP, dynamic boost, temp target)
- Keyboard backlight (brightness + RGB), panel overdrive, auto-brightness
- Webcam / touchpad toggles, iGPU-disable in dGPU mode, NPU removal
- Game mode: auto performance switching per running executable

## Build

```sh
cargo build --release
```

## Install

```sh
sudo cp target/release/ghl /usr/local/bin/
```

## Usage

```sh
ghl                      # TUI
ghl mode silent|balanced|turbo
ghl cpuboost [silent|balanced|turbo] on|off|auto   # per-mode CPU boost; no mode = current
ghl fan preset quiet|balanced|aggressive
ghl ryzen status|apply|reset|preset eco|cool|balanced|max
ghl gpu igpu|hybrid|dgpu
ghl overdrive on|off
ghl status
```

### Per-mode CPU boost

Each performance mode has its own CPU boost setting:

```sh
ghl cpuboost silent off      # stop boosting in silent mode
ghl cpuboost balanced on
ghl cpuboost turbo auto      # fall back to the automatic policy
```

Defaults: Silent off, Balanced on, Turbo on. Switching modes applies that mode's
setting. Settings persist in `/etc/ghl.conf` as `cpu_boost.<mode>=0|1`; a legacy
global `cpu_boost=` key is still honored as a fallback override.

## Configuration

Everything is persisted in `/etc/ghl.conf` — performance mode, per-mode CPU boost,
fan curves, ryzenadj limits, RGB, toggles, and game-mode rules.
`ghl apply` restores the saved config at boot.

The board must expose ASUS interfaces (`asus-armoury` / `asus-nb-wmi`).
Writes require root; the CLI re-executes itself via `sudo` when needed.
