//! ghl — minimal ASUS control (TUI + CLI). No daemon, no polling, no async.
//! Talks directly to sysfs (asus-armoury / asus-nb-wmi, platform_profile,
//! asus_custom_fan_curve, leds/asus::kbd_backlight, usb/input, pci, hwmon/rapl/drm),
//! ryzenadj (CPU / iGPU / NPU power & clock limits), and Wayland helpers.

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyCode, KeyEventKind},
    execute, queue,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor},
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::{
    collections::BTreeMap,
    env,
    fmt::{self, Write as _},
    fs::{self, OpenOptions},
    io::{self, BufWriter, Write},
    os::unix::{fs::MetadataExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

const ARMOURY: &str = "/sys/class/firmware-attributes/asus-armoury/attributes";
const LEGACY: &str = "/sys/devices/platform/asus-nb-wmi";
const BUS_WMI: &str = "/sys/bus/platform/devices/asus-nb-wmi";
const PROFILE: &str = "/sys/firmware/acpi/platform_profile";
const CONF: &str = "/etc/ghl.conf";
const UDEV_PERSIST: &str = "/etc/udev/rules.d/99-ghl-persist.rules";
const CPU_BOOST_PATH: &str = "/sys/devices/system/cpu/cpufreq/boost";
const INTEL_NO_TURBO: &str = "/sys/devices/system/cpu/intel_pstate/no_turbo";

// ───────────────────────── sysfs & cached helpers ─────────────────────────

#[inline]
fn rd(p: impl AsRef<Path>) -> Option<String> {
    let mut s = fs::read_to_string(p).ok()?;
    let trimmed_end = s.trim_end().len();
    s.truncate(trimmed_end);
    let leading = s.len() - s.trim_start().len();
    if leading > 0 {
        s.drain(..leading);
    }
    Some(s)
}

#[inline]
fn rd_u32(p: impl AsRef<Path>) -> Option<u32> {
    fs::read_to_string(p).ok()?.trim().parse().ok()
}

#[inline]
fn rd_u64(p: impl AsRef<Path>) -> Option<u64> {
    fs::read_to_string(p).ok()?.trim().parse().ok()
}

#[inline]
fn wr(p: impl AsRef<Path>, v: impl AsRef<[u8]>) -> Result<(), String> {
    let path = p.as_ref();
    fs::write(path, v).map_err(|e| format!("{}: {e}", path.display()))
}

#[inline]
const fn on_off(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

#[inline]
const fn on_off_upper(b: bool) -> &'static str {
    if b { "ON" } else { "OFF" }
}

fn run_quiet(bin: &str, args: &[&str]) -> bool {
    Command::new(bin)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn attr(name: &str) -> Option<PathBuf> {
    for candidate in [
        format!("{ARMOURY}/{name}/current_value"),
        format!("{LEGACY}/{name}"),
        format!("{BUS_WMI}/{name}"),
    ] {
        let p = PathBuf::from(candidate);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn wr_attr_all(name: &str, val: &str) -> bool {
    let mut ok = false;
    for candidate in [
        format!("{ARMOURY}/{name}/current_value"),
        format!("{LEGACY}/{name}"),
        format!("{BUS_WMI}/{name}"),
    ] {
        let p = Path::new(&candidate);
        if p.exists() && wr(p, val).is_ok() {
            ok = true;
        }
    }
    ok
}

fn find_hwmon(names: &[&str]) -> Option<PathBuf> {
    for e in fs::read_dir("/sys/class/hwmon").ok()?.flatten() {
        let p = e.path();
        if let Some(n) = rd(p.join("name")) {
            if names.contains(&n.as_str()) {
                return Some(p);
            }
        }
    }
    None
}

fn hwmon_fan_curve() -> Option<&'static Path> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| find_hwmon(&["asus_custom_fan_curve"]))
        .as_deref()
}

fn hwmon_asus() -> Option<&'static Path> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| find_hwmon(&["asus", "asus_nb_wmi"]))
        .as_deref()
}

fn hwmon_cpu() -> Option<&'static Path> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| find_hwmon(&["k10temp", "coretemp", "zenpower", "acpitz"]))
        .as_deref()
}

// Do not cache sysfs/hidraw discovery: ASUS devices can appear after boot,
// resume, MUX changes, or a delayed driver probe.
fn kbd_led_dir() -> Option<PathBuf> {
    let direct = PathBuf::from("/sys/class/leds/asus::kbd_backlight");
    if direct.exists() {
        return Some(direct);
    }
    for e in fs::read_dir("/sys/class/leds").ok()?.flatten() {
        let name = e.file_name();
        let s = name.to_string_lossy();
        if s.starts_with("asus::kbd_backlight") || s.ends_with("kbd_backlight") {
            return Some(e.path());
        }
    }
    None
}

fn asus_hidraw_devs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/class/hidraw") else {
        return out;
    };
    for e in entries.flatten() {
        let uevent = e.path().join("device/uevent");
        if rd(&uevent).is_some_and(|u| u.to_ascii_uppercase().contains("0B05:")) {
            out.push(PathBuf::from("/dev").join(e.file_name()));
        }
    }
    out
}

fn asus_hidraw_write(pkt: &[u8]) -> bool {
    let mut buf = [0u8; 64];
    let len = pkt.len().min(64);
    buf[..len].copy_from_slice(&pkt[..len]);

    let mut wrote = false;
    for dev in asus_hidraw_devs() {
        if let Ok(mut f) = OpenOptions::new().write(true).open(&dev) {
            if f.write_all(&buf).is_ok() {
                wrote = true;
            }
        }
    }
    wrote
}

fn parse_toggle(s: &str) -> Option<bool> {
    match s.trim() {
        s if s.eq_ignore_ascii_case("on")
            || s == "1"
            || s.eq_ignore_ascii_case("true")
            || s.eq_ignore_ascii_case("enable")
            || s.eq_ignore_ascii_case("enabled") =>
        {
            Some(true)
        }
        s if s.eq_ignore_ascii_case("off")
            || s == "0"
            || s.eq_ignore_ascii_case("false")
            || s.eq_ignore_ascii_case("disable")
            || s.eq_ignore_ascii_case("disabled") =>
        {
            Some(false)
        }
        _ => None,
    }
}

// ───────────────────────── GPU mode ─────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Gpu {
    Igpu,
    Hybrid,
    Dgpu,
}
const GPUS: [Gpu; 3] = [Gpu::Igpu, Gpu::Hybrid, Gpu::Dgpu];

impl Gpu {
    const fn name(self) -> &'static str {
        match self {
            Gpu::Igpu => "igpu",
            Gpu::Hybrid => "hybrid",
            Gpu::Dgpu => "dgpu",
        }
    }
    const fn label(self) -> &'static str {
        match self {
            Gpu::Igpu => "iGPU only (dGPU off)",
            Gpu::Hybrid => "Hybrid",
            Gpu::Dgpu => "dGPU only (MUX)",
        }
    }
    fn parse(s: &str) -> Option<Gpu> {
        GPUS.into_iter().find(|g| g.name().eq_ignore_ascii_case(s))
    }
}

fn gpu_current() -> Option<Gpu> {
    let dis = attr("dgpu_disable").and_then(rd).map(|s| s == "1");
    let mux_dgpu = attr("gpu_mux_mode").and_then(rd).map(|s| s == "0");
    match (mux_dgpu, dis) {
        (Some(true), _) => Some(Gpu::Dgpu),
        (_, Some(true)) => Some(Gpu::Igpu),
        (_, Some(false)) | (Some(false), None) => Some(Gpu::Hybrid),
        _ => None,
    }
}

fn unload_nvidia() {
    if Path::new("/sys/module/nvidia").exists() {
        let _ = run_quiet(
            "modprobe",
            &["-r", "nvidia_drm", "nvidia_modeset", "nvidia_uvm", "nvidia"],
        );
    }
}

fn gpu_set(g: Gpu, cfg: &mut Cfg) -> Result<String, String> {
    let mux = attr("gpu_mux_mode");
    let dis = attr("dgpu_disable");
    let mux_now = mux.as_ref().and_then(rd);
    match g {
        Gpu::Dgpu => {
            let m = mux.ok_or("this model has no MUX switch (gpu_mux_mode missing)")?;
            cfg.gpu_pending = None;
            if let Some(d) = &dis {
                wr(d, "0")?;
            }
            if mux_now.as_deref() != Some("0") {
                wr(&m, "0")?;
                return Ok("MUX set to dGPU — REBOOT required".into());
            }
            if cfg.disable_igpu_dgpu {
                let _ = igpu_in_dgpu_apply(true);
            }
            Ok("dGPU only".into())
        }
        Gpu::Hybrid | Gpu::Igpu => {
            if let (Some(m), Some("0")) = (&mux, mux_now.as_deref()) {
                wr(m, "1")?;
                cfg.gpu_pending = Some(g);
                return Ok(format!(
                    "MUX set to hybrid — REBOOT; '{}' is applied at boot by `ghl apply`",
                    g.name()
                ));
            }
            cfg.gpu_pending = None;
            let d = dis.ok_or("dgpu_disable attribute missing")?;
            if g == Gpu::Igpu {
                unload_nvidia();
                wr(&d, "1")?;
                Ok("iGPU only (dGPU powered off)".into())
            } else {
                wr(&d, "0")?;
                Ok("Hybrid (a rescan/reboot may be needed if dGPU doesn't show up)".into())
            }
        }
    }
}

// ───────────── Disable iGPU (while in dGPU) & Disable NPU (PCI remove) ─────────────

fn pci_unbind_and_remove(bdf: &str) -> bool {
    let dev_dir = PathBuf::from(format!("/sys/bus/pci/devices/{bdf}"));
    if !dev_dir.exists() {
        return true;
    }
    let drv_unbind = dev_dir.join("driver/unbind");
    if drv_unbind.exists() {
        let _ = wr(&drv_unbind, bdf);
    }
    let amdgpu_unbind = Path::new("/sys/bus/pci/drivers/amdgpu/unbind");
    if amdgpu_unbind.exists() {
        let _ = wr(amdgpu_unbind, bdf);
    }
    wr(dev_dir.join("remove"), "1").is_ok()
}

fn pci_rescan() -> Result<(), String> {
    wr("/sys/bus/pci/rescan", "1")
}

fn find_igpu_bdfs() -> Vec<String> {
    let mut out = Vec::new();
    if Path::new("/sys/bus/pci/devices/0000:65:00.0").exists() {
        out.push("0000:65:00.0".to_string());
    }
    let Ok(entries) = fs::read_dir("/sys/bus/pci/devices") else {
        return out;
    };
    for e in entries.flatten() {
        let bdf = e.file_name().to_string_lossy().into_owned();
        if out.contains(&bdf) {
            continue;
        }
        let p = e.path();
        let vendor = rd(p.join("vendor")).unwrap_or_default();
        let cls = rd(p.join("class")).unwrap_or_default();
        if (vendor == "0x1002" || vendor == "0x8086")
            && (cls.starts_with("0x0300") || cls.starts_with("0x0380"))
        {
            if let Ok(drv_link) = fs::read_link(p.join("driver")) {
                let drv = drv_link
                    .file_name()
                    .map(|s| s.to_string_lossy())
                    .unwrap_or_default();
                if matches!(drv.as_ref(), "amdgpu" | "i915" | "xe") {
                    out.push(bdf);
                }
            } else {
                out.push(bdf);
            }
        }
    }
    out
}

fn find_npu_bdfs() -> Vec<String> {
    let mut out = Vec::new();
    for fixed in ["0000:66:00.1", "0000:66:00.0"] {
        if Path::new(&format!("/sys/bus/pci/devices/{fixed}")).exists() {
            out.push(fixed.to_string());
        }
    }
    let Ok(entries) = fs::read_dir("/sys/bus/pci/devices") else {
        return out;
    };
    for e in entries.flatten() {
        let bdf = e.file_name().to_string_lossy().into_owned();
        if out.contains(&bdf) {
            continue;
        }
        let p = e.path();
        let cls = rd(p.join("class")).unwrap_or_default();
        let drv = fs::read_link(p.join("driver"))
            .ok()
            .and_then(|l| l.file_name().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_default();
        if cls.starts_with("0x1180")
            || cls.starts_with("0x1200")
            || matches!(drv.as_str(), "amdxdna" | "intel_vpu")
        {
            if let Some(prefix) = bdf.strip_suffix(".1") {
                let sib = format!("{prefix}.0");
                let sib_path = PathBuf::from(format!("/sys/bus/pci/devices/{sib}"));
                if sib_path.exists()
                    && !out.contains(&sib)
                    && rd(sib_path.join("class"))
                        .unwrap_or_default()
                        .starts_with("0x1300")
                {
                    out.push(sib);
                }
            }
            out.push(bdf);
        }
    }
    out
}

fn igpu_present() -> bool {
    Path::new("/sys/bus/pci/devices/0000:65:00.0").exists() || !find_igpu_bdfs().is_empty()
}

fn npu_present() -> bool {
    Path::new("/sys/bus/pci/devices/0000:66:00.1").exists() || !find_npu_bdfs().is_empty()
}

fn igpu_in_dgpu_apply(disable: bool) -> Result<String, String> {
    if disable {
        if gpu_current() != Some(Gpu::Dgpu) {
            return Ok("Disable iGPU in dGPU: enabled (will remove iGPU when in dGPU mode)".into());
        }
        let bdfs = find_igpu_bdfs();
        if bdfs.is_empty() {
            return Ok("iGPU already removed".into());
        }
        let removed: Vec<String> = bdfs
            .into_iter()
            .filter(|bdf| pci_unbind_and_remove(bdf))
            .collect();
        if removed.is_empty() {
            Err("failed to unbind/remove iGPU PCI device".into())
        } else {
            Ok(format!("iGPU removed ({})", removed.join(", ")))
        }
    } else {
        pci_rescan()?;
        Ok("Disable iGPU in dGPU: OFF (PCI bus rescanned)".into())
    }
}

fn npu_apply(disable: bool, keep_igpu_off_in_dgpu: bool) -> Result<String, String> {
    if disable {
        let bdfs = find_npu_bdfs();
        if bdfs.is_empty() {
            return Ok("NPU already removed".into());
        }
        let removed: Vec<String> = bdfs
            .into_iter()
            .filter(|bdf| pci_unbind_and_remove(bdf))
            .collect();
        if removed.is_empty() {
            Err("failed to remove NPU PCI device".into())
        } else {
            Ok(format!("NPU removed ({})", removed.join(", ")))
        }
    } else {
        pci_rescan()?;
        if keep_igpu_off_in_dgpu && gpu_current() == Some(Gpu::Dgpu) {
            let _ = igpu_in_dgpu_apply(true);
        }
        Ok("NPU enabled (PCI bus rescanned)".into())
    }
}

// ───────────── RyzenAdj (CPU / iGPU / NPU / SoC Power & Clock Limits) ─────────────

const R_STAPM: usize = 0;
const R_SLOW: usize = 1;
const R_FAST: usize = 2;
const R_TCTL: usize = 3;
const R_CPU_TDC: usize = 4;
const R_CPU_EDC: usize = 5;
const R_IGPU_MHZ: usize = 6;
const R_IGPU_MIN_MHZ: usize = 7;
const R_IGPU_TDC: usize = 8;
const R_IGPU_EDC: usize = 9;
const R_NPU_TDC: usize = 10;
const R_SOC_TDC: usize = 11;
const R_SOC_EDC: usize = 12;
const R_SOC_MHZ: usize = 13;
const R_FCLK_MHZ: usize = 14;
const R_PSAVE: usize = 15;
const R_NUM_VALS: usize = 15;

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
struct RyzenCfg {
    vals: [Option<u32>; R_NUM_VALS],
    power_saving: Option<bool>, // Some(true) => --power-saving, Some(false) => --max-performance
}

struct RyzenFieldMeta {
    key: &'static str,
    flag: &'static str,
    extra_flag: &'static str,
    scale: u32,
    label: &'static str,
    unit: &'static str,
    min: u32,
    max: u32,
    step: u32,
    default_init: u32,
}

#[allow(clippy::too_many_arguments)]
const fn rf(
    key: &'static str,
    flag: &'static str,
    extra_flag: &'static str,
    scale: u32,
    label: &'static str,
    unit: &'static str,
    min: u32,
    max: u32,
    step: u32,
    default_init: u32,
) -> RyzenFieldMeta {
    RyzenFieldMeta {
        key,
        flag,
        extra_flag,
        scale,
        label,
        unit,
        min,
        max,
        step,
        default_init,
    }
}

const RYZEN_FIELDS: [RyzenFieldMeta; 16] = [
    rf("stapm", "--stapm-limit", "", 1000, "CPU Sustained Power (STAPM / SPL)", "W", 5, 120, 2, 28),
    rf("slow", "--slow-limit", "--apu-slow-limit", 1000, "CPU Slow PPT Limit (SPPT)", "W", 5, 120, 2, 35),
    rf("fast", "--fast-limit", "", 1000, "CPU Fast PPT Limit (FPPT)", "W", 5, 140, 2, 45),
    rf("tctl", "--tctl-temp", "--apu-skin-temp", 1, "CPU Temp Limit (Tctl)", "°C", 55, 100, 2, 85),
    rf("cpu-tdc", "--vrm-current", "", 1000, "CPU VRM Current (TDC VDD)", "A", 10, 140, 5, 45),
    rf("cpu-edc", "--vrmmax-current", "", 1000, "CPU VRM Peak Current (EDC VDD)", "A", 15, 180, 5, 65),
    rf("igpu-mhz", "--max-gfxclk", "", 1, "iGPU Max Clock (GFXCLK cap)", "MHz", 400, 3000, 100, 1500),
    rf("igpu-min-mhz", "--min-gfxclk", "", 1, "iGPU Min Clock (GFXCLK min)", "MHz", 200, 2200, 100, 400),
    rf("igpu-tdc", "--vrmgfx-current", "", 1000, "iGPU VRM Current (TDC GFX)", "A", 5, 90, 2, 25),
    rf("igpu-edc", "--vrmgfxmax_current", "", 1000, "iGPU VRM Peak Current (EDC GFX)", "A", 5, 120, 2, 35),
    rf("npu-tdc", "--vrmcvip-current", "", 1000, "NPU / CVIP VRM Current (TDC CVIP)", "A", 1, 50, 1, 8),
    rf("soc-tdc", "--vrmsoc-current", "", 1000, "SoC / Uncore VRM Current (TDC SoC)", "A", 5, 60, 1, 14),
    rf("soc-edc", "--vrmsocmax-current", "", 1000, "SoC / Uncore Peak Current (EDC SoC)", "A", 5, 80, 2, 18),
    rf("soc-mhz", "--max-socclk-frequency", "", 1, "SoC / NPU Bus Max Clock (SOCCLK)", "MHz", 400, 2400, 100, 1200),
    rf("fclk-mhz", "--max-fclk-frequency", "", 1, "Infinity Fabric Max Clock (FCLK)", "MHz", 400, 2400, 100, 1600),
    rf("psave", "", "", 1, "SMU Power Policy", "", 0, 1, 1, 1),
];

const RYZEN_PRESETS: [(&str, RyzenCfg); 4] = [
    (
        "eco",
        RyzenCfg {
            vals: [
                Some(15), Some(18), Some(22), Some(75), Some(25), Some(35),
                Some(1000), None, Some(15), Some(22),
                Some(5), Some(10), Some(14), Some(1000), Some(1200),
            ],
            power_saving: Some(true),
        },
    ),
    (
        "cool",
        RyzenCfg {
            vals: [
                Some(25), Some(28), Some(35), Some(80), Some(40), Some(55),
                Some(1500), None, Some(25), Some(35),
                Some(8), Some(14), Some(18), Some(1200), Some(1600),
            ],
            power_saving: Some(true),
        },
    ),
    (
        "balanced",
        RyzenCfg {
            vals: [
                Some(35), Some(42), Some(50), Some(85), Some(55), Some(75),
                Some(2000), None, Some(35), Some(50),
                Some(12), Some(18), Some(24), None, None,
            ],
            power_saving: None,
        },
    ),
    (
        "max",
        RyzenCfg {
            vals: [
                Some(54), Some(65), Some(80), Some(92), None, None,
                None, None, None, None,
                None, None, None, None, None,
            ],
            power_saving: Some(false),
        },
    ),
];

impl RyzenCfg {
    fn is_active(&self) -> bool {
        *self != Self::default()
    }

    #[inline]
    fn slot(&self, idx: usize) -> Option<u32> {
        self.vals.get(idx).copied().flatten()
    }

    fn step_field(&mut self, idx: usize, dir: i32, fine: bool) {
        if idx == R_PSAVE {
            self.power_saving = match (self.power_saving, dir >= 0) {
                (None, true) | (Some(false), false) => Some(true),
                (Some(true), true) | (None, false) => Some(false),
                (Some(false), true) | (Some(true), false) => None,
            };
            return;
        }
        let meta = &RYZEN_FIELDS[idx];
        let step = if fine { (meta.step / 2).max(1) } else { meta.step };
        if let Some(slot) = self.vals.get_mut(idx) {
            *slot = Some(match *slot {
                Some(v) => (v as i64 + (dir as i64) * (step as i64))
                    .clamp(meta.min as i64, meta.max as i64) as u32,
                None => meta.default_init,
            });
        }
    }

    fn clear_field(&mut self, idx: usize) {
        if idx == R_PSAVE {
            self.power_saving = None;
        } else if let Some(slot) = self.vals.get_mut(idx) {
            *slot = None;
        }
    }

    fn set_by_key(&mut self, key: &str, val: &str) -> Result<(), String> {
        let k = key.trim().to_ascii_lowercase();
        let v = val.trim();
        let v_low = v.to_ascii_lowercase();
        if k == "psave" || k == "power_saving" || k == "policy" {
            self.power_saving = match v_low.as_str() {
                "1" | "on" | "true" | "powersave" | "power-saving" | "eco" => Some(true),
                "0" | "max" | "perf" | "performance" | "max-performance" => Some(false),
                "none" | "default" | "auto" | "-" | "off" => None,
                _ => return Err(format!("invalid psave value '{v}' (use on|max|none)")),
            };
            return Ok(());
        }
        let idx = RYZEN_FIELDS[..R_NUM_VALS]
            .iter()
            .position(|f| f.key == k || f.key.replace('-', "_") == k)
            .ok_or_else(|| format!("unknown ryzen key '{key}'"))?;
        if matches!(v_low.as_str(), "none" | "off" | "default" | "auto" | "-") {
            self.vals[idx] = None;
            return Ok(());
        }
        let meta = &RYZEN_FIELDS[idx];
        let num: u32 = v
            .trim_end_matches(|c: char| c.is_alphabetic())
            .parse()
            .map_err(|_| format!("invalid number '{v}' for {key}"))?;
        self.vals[idx] = Some(num.clamp(meta.min, meta.max));
        Ok(())
    }

    fn summary(&self) -> String {
        if !self.is_active() {
            return "default (no overrides)".into();
        }
        let mut parts = Vec::new();
        let (s, sl, f) = (self.vals[R_STAPM], self.vals[R_SLOW], self.vals[R_FAST]);
        if s.is_some() || sl.is_some() || f.is_some() {
            let fmt_opt = |o: Option<u32>| o.map_or_else(|| "-".into(), |v| v.to_string());
            parts.push(format!("CPU {}/{}/{}W", fmt_opt(s), fmt_opt(sl), fmt_opt(f)));
        }
        if let Some(t) = self.vals[R_TCTL] {
            parts.push(format!("{t}°C"));
        }
        if let Some(tdc) = self.vals[R_CPU_TDC] {
            parts.push(format!("CPU-TDC {tdc}A"));
        }
        if let Some(edc) = self.vals[R_CPU_EDC] {
            parts.push(format!("CPU-EDC {edc}A"));
        }
        if let Some(mhz) = self.vals[R_IGPU_MHZ] {
            parts.push(format!("iGPU≤{mhz}MHz"));
        }
        if let Some(mhz) = self.vals[R_IGPU_MIN_MHZ] {
            parts.push(format!("iGPU≥{mhz}MHz"));
        }
        if let Some(tdc) = self.vals[R_IGPU_TDC] {
            parts.push(format!("iGPU-TDC {tdc}A"));
        }
        if let Some(edc) = self.vals[R_IGPU_EDC] {
            parts.push(format!("iGPU-EDC {edc}A"));
        }
        if let Some(tdc) = self.vals[R_NPU_TDC] {
            parts.push(format!("NPU-TDC {tdc}A"));
        }
        if let Some(tdc) = self.vals[R_SOC_TDC] {
            parts.push(format!("SoC-TDC {tdc}A"));
        }
        if let Some(edc) = self.vals[R_SOC_EDC] {
            parts.push(format!("SoC-EDC {edc}A"));
        }
        if let Some(mhz) = self.vals[R_SOC_MHZ] {
            parts.push(format!("SoC≤{mhz}MHz"));
        }
        if let Some(mhz) = self.vals[R_FCLK_MHZ] {
            parts.push(format!("FCLK≤{mhz}MHz"));
        }
        if let Some(ps) = self.power_saving {
            parts.push(if ps { "psave" } else { "max-perf" }.into());
        }
        parts.join(", ")
    }

    fn to_args(self) -> Vec<String> {
        let mut args = Vec::new();
        for (idx, meta) in RYZEN_FIELDS[..R_NUM_VALS].iter().enumerate() {
            if let Some(v) = self.vals[idx] {
                let scaled = v * meta.scale;
                args.push(format!("{}={scaled}", meta.flag));
                if !meta.extra_flag.is_empty() {
                    args.push(format!("{}={scaled}", meta.extra_flag));
                }
            }
        }
        match self.power_saving {
            Some(true) => args.push("--power-saving".into()),
            Some(false) => args.push("--max-performance".into()),
            None => {}
        }
        args
    }
}

fn ryzenadj_bin() -> &'static str {
    static BIN: OnceLock<&'static str> = OnceLock::new();
    BIN.get_or_init(|| {
        for cand in [
            "/usr/sbin/ryzenadj",
            "/usr/bin/ryzenadj",
            "/usr/local/bin/ryzenadj",
            "/usr/local/sbin/ryzenadj",
        ] {
            if Path::new(cand).exists() {
                return cand;
            }
        }
        "ryzenadj"
    })
}

fn sync_asus_ppt(r: &RyzenCfg) -> bool {
    let mut synced = false;
    if let Some(spl) = r.vals[R_STAPM] {
        if wr_attr_all("ppt_pl1_spl", &spl.clamp(15, 90).to_string()) {
            synced = true;
        }
    }
    if let Some(sppt) = r.vals[R_SLOW] {
        if wr_attr_all("ppt_pl2_sppt", &sppt.clamp(15, 90).to_string()) {
            synced = true;
        }
    }
    if let Some(fppt) = r.vals[R_FAST] {
        let s = fppt.clamp(15, 90).to_string();
        if wr_attr_all("ppt_pl3_fppt", &s) || wr_attr_all("ppt_fppt", &s) {
            synced = true;
        }
    }
    synced
}

fn ryzen_apply(r: &RyzenCfg) -> Result<String, String> {
    let args = r.to_args();
    if args.is_empty() {
        return Ok("ryzenadj: no limits configured".into());
    }
    let ppt_synced = sync_asus_ppt(r);
    let out = Command::new(ryzenadj_bin())
        .args(&args)
        .output()
        .map_err(|e| format!("failed to run {}: {e}", ryzenadj_bin()))?;

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let ok_count = stdout.matches("Sucessfully").count() + stdout.matches("Successfully").count();

    if out.status.success() || ok_count > 0 {
        let suffix = if ppt_synced { " (+ASUS PPT)" } else { "" };
        Ok(format!("ryzenadj applied: {}{suffix}", r.summary()))
    } else {
        let err = stderr
            .lines()
            .find(|l| !l.trim().is_empty())
            .or_else(|| stdout.lines().find(|l| !l.trim().is_empty()))
            .unwrap_or("ryzenadj failed");
        Err(format!("ryzenadj: {}", err.trim()))
    }
}

// ───────────────────────── Nvidia Power Fix ─────────────────────────

fn nv_fix_apply() -> Result<String, String> {
    let mut applied = 0usize;
    let mut details = Vec::new();

    for (name, fallback) in [
        ("nv_tgp", "100"),
        ("nv_dynamic_boost", "15"),
        ("nv_temp_target", "87"),
    ] {
        let max_path = format!("{ARMOURY}/{name}/max_value");
        let val = rd(&max_path)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| fallback.to_string());

        let mut ok = wr_attr_all(name, &val);
        let mut final_val = val.clone();

        if !ok && name == "nv_dynamic_boost" {
            for retry in ["15", "10"] {
                if retry != val && wr_attr_all(name, retry) {
                    ok = true;
                    final_val = retry.to_string();
                    break;
                }
            }
        }

        if ok {
            applied += 1;
            details.push(format!("{name}={final_val}"));
        }
    }

    let _ = run_quiet("systemctl", &["restart", "nvidia-powerd"]);

    if applied > 0 {
        Ok(format!("NVIDIA Power Fix applied ({})", details.join(", ")))
    } else {
        Err("NVIDIA Power attributes not found in sysfs".into())
    }
}

fn nv_fix_restore() {
    for name in ["nv_tgp", "nv_dynamic_boost", "nv_temp_target"] {
        let def_path = format!("{ARMOURY}/{name}/default_value");
        if let Some(def) = rd(&def_path).filter(|s| !s.is_empty()) {
            let _ = wr_attr_all(name, &def);
        }
    }
    let _ = run_quiet("systemctl", &["restart", "nvidia-powerd"]);
}

// ───────────────────────── CPU Boost ─────────────────────────

fn cpu_boost_get() -> Option<bool> {
    if Path::new(CPU_BOOST_PATH).exists() {
        rd(CPU_BOOST_PATH).map(|s| s == "1")
    } else if Path::new(INTEL_NO_TURBO).exists() {
        rd(INTEL_NO_TURBO).map(|s| s == "0")
    } else {
        attr("cpu_core_boost").and_then(rd).map(|s| s != "0")
    }
}

fn cpu_boost_set(on: bool) -> Result<(), String> {
    if Path::new(CPU_BOOST_PATH).exists() {
        wr(CPU_BOOST_PATH, if on { "1" } else { "0" })
    } else if Path::new(INTEL_NO_TURBO).exists() {
        wr(INTEL_NO_TURBO, if on { "0" } else { "1" })
    } else if wr_attr_all("cpu_core_boost", if on { "1" } else { "0" }) {
        Ok(())
    } else {
        Err("CPU boost control interface not found".into())
    }
}

/// Default boost policy: Silent disables boost; Balanced and Turbo enable it.
fn cpu_boost_default_for_mode(mode: Mode) -> bool {
    !matches!(mode, Mode::Silent)
}

/// Resolve boost for one mode: per-mode override `cpu_boost.<mode>=on|off`
/// wins, then the global `cpu_boost=on|off` override, else the default
/// mode-dependent policy. Returns (enabled, overridden).
fn cpu_boost_resolve_for_mode(mode: Mode, cfg: &Cfg) -> (bool, bool) {
    if let Some(b) = cfg.cpu_boost_modes.get(&mode) {
        (*b, true)
    } else if let Some(b) = cfg.cpu_boost {
        (b, true)
    } else {
        (cpu_boost_default_for_mode(mode), false)
    }
}

fn cpu_boost_for_mode(mode: Mode, cfg: &Cfg) -> bool {
    cpu_boost_resolve_for_mode(mode, cfg).0
}

fn cpu_boost_apply_for_mode(mode: Mode, cfg: &Cfg) -> Result<bool, String> {
    let enabled = cpu_boost_for_mode(mode, cfg);
    cpu_boost_set(enabled)?;
    Ok(enabled)
}

/// CLI: `ghl cpuboost <mode> on|off|auto`.
fn cpu_boost_cli_set(cfg: &mut Cfg, m: Mode, v: &str) -> Result<(), String> {
    let current = cfg.mode.or_else(mode_current);
    if v == "auto" {
        cfg.cpu_boost_modes.remove(&m);
        if current == Some(m) {
            let enabled = cpu_boost_apply_for_mode(m, cfg)?;
            println!("cpu boost {} ({} mode policy)", on_off(enabled), m.name());
        } else {
            println!("cpu boost for {} mode set to automatic", m.name());
        }
    } else {
        let on = parse_toggle(v).ok_or("cpuboost: [mode] on|off|auto")?;
        cfg.cpu_boost_modes.insert(m, on);
        if current == Some(m) {
            cpu_boost_set(on)?;
        }
        println!("cpu boost {} for {} mode", on_off(on), m.name());
    }
    cfg.save()?;
    Ok(())
}

// ───────────────────────── Panel Overdrive ─────────────────────────

fn panel_od_get(cfg: &Cfg) -> Option<bool> {
    attr("panel_od")
        .or_else(|| attr("panel_overdrive"))
        .and_then(rd)
        .map(|s| s == "1")
        .or(cfg.panel_od)
}

fn panel_od_set(on: bool) -> Result<String, String> {
    let val = if on { "1" } else { "0" };
    let a = wr_attr_all("panel_od", val);
    let b = wr_attr_all("panel_overdrive", val);
    if a || b {
        Ok(format!("panel overdrive {}", on_off_upper(on)))
    } else {
        Err("panel_od / panel_overdrive attribute not supported on this model".into())
    }
}

// ───────────────────────── Keyboard Brightness ─────────────────────────

const KBD_BRIGHT_LABELS: [&str; 4] = ["Off", "Low", "Med", "High"];

fn kbd_bright_parse(s: &str) -> Option<u8> {
    match s.trim() {
        "0" => Some(0),
        "1" => Some(1),
        "2" => Some(2),
        "3" => Some(3),
        s if s.eq_ignore_ascii_case("off") => Some(0),
        s if s.eq_ignore_ascii_case("low") => Some(1),
        s if s.eq_ignore_ascii_case("med") || s.eq_ignore_ascii_case("medium") => Some(2),
        s if s.eq_ignore_ascii_case("high") || s.eq_ignore_ascii_case("max") => Some(3),
        _ => None,
    }
}

fn kbd_bright_get() -> Option<u8> {
    rd_u32(kbd_led_dir()?.join("brightness")).map(|v| (v as u8).min(3))
}

fn kbd_bright_set(level: u8) -> Result<String, String> {
    let lvl = level.min(3);
    let mut errors = Vec::new();
    let mut via_sysfs = false;
    let mut via_hid = false;

    // LED class writes can succeed without the EC actually lighting the
    // keyboard on some models, so the ASUS HID report is always sent too.
    if let Some(dir) = kbd_led_dir() {
        let p = dir.join("brightness");
        if p.exists() {
            match wr(&p, lvl.to_string()) {
                Ok(()) => via_sysfs = true,
                Err(e) => errors.push(e),
            }
        }
    }

    via_hid = asus_hidraw_write(&[0x5D, 0xBA, 0xC5, 0xC4, lvl]);

    if !via_sysfs && !via_hid {
        if errors.is_empty() {
            Err("keyboard backlight interface not found (LED sysfs and ASUS HID unavailable)".into())
        } else {
            Err(format!("keyboard backlight failed: {}", errors.join("; ")))
        }
    } else {
        let mut method = Vec::new();
        if via_sysfs {
            method.push("sysfs");
        }
        if via_hid {
            method.push("HID");
        }
        Ok(format!(
            "kbd brightness: {} ({lvl}/3) via {}",
            KBD_BRIGHT_LABELS[lvl as usize],
            method.join("+")
        ))
    }
}

// ───────────────────────── Keyboard RGB Picker ─────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Rgb(u8, u8, u8);

impl Default for Rgb {
    fn default() -> Self {
        Rgb(255, 255, 255)
    }
}

impl Rgb {
    fn parse(s: &str) -> Option<Rgb> {
        let s = s.trim();
        let hex = s.strip_prefix('#').unwrap_or(s);
        if hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some(Rgb(r, g, b));
        }
        let mut parts = s.split([',', ' ']).filter(|x| !x.is_empty());
        let r = parts.next()?.parse().ok()?;
        let g = parts.next()?.parse().ok()?;
        let b = parts.next()?.parse().ok()?;
        Some(Rgb(r, g, b))
    }

    fn hex(self) -> String {
        format!("{:02X}{:02X}{:02X}", self.0, self.1, self.2)
    }

    fn label(self) -> String {
        format!(
            "#{:02X}{:02X}{:02X} (R:{} G:{} B:{})",
            self.0, self.1, self.2, self.0, self.1, self.2
        )
    }

    const fn channel(self, idx: usize) -> u8 {
        match idx {
            0 => self.0,
            1 => self.1,
            _ => self.2,
        }
    }

    fn set_channel(&mut self, idx: usize, val: u8) {
        match idx {
            0 => self.0 = val,
            1 => self.1 = val,
            _ => self.2 = val,
        }
    }
}

fn kbd_color_get(cfg: &Cfg) -> Option<Rgb> {
    if let Some(dir) = kbd_led_dir() {
        if let Some(s) = rd(dir.join("multi_intensity")) {
            if let Some(rgb) = Rgb::parse(&s) {
                return Some(rgb);
            }
        }
    }
    cfg.kbd_color
}

fn kbd_color_set(c: Rgb) -> Result<String, String> {
    let mut applied = false;

    if let Some(dir) = kbd_led_dir() {
        let mi = dir.join("multi_intensity");
        if mi.exists() && wr(&mi, format!("{} {} {}", c.0, c.1, c.2)).is_ok() {
            applied = true;
        }
        let mode = dir.join("kbd_rgb_mode");
        if mode.exists() {
            let payload = format!("1 0 {} {} {} 0", c.0, c.1, c.2);
            if wr(&mode, &payload).is_ok()
                || wr(&mode, format!("0 0 {} {} {}", c.0, c.1, c.2)).is_ok()
            {
                applied = true;
            }
        }
        let state = dir.join("kbd_rgb_state");
        if state.exists() {
            let _ = wr(&state, "1 1 1 1 0");
        }
    }

    // Apply the HID color report even when a sysfs path succeeded: on many
    // models the EC only honors this packet and ignores multi_intensity.
    let mut hid = false;
    {
        let msg = [
            0x5D, 0xB3, 0x00, 0x00, c.0, c.1, c.2, 0x00, 0x00, 0x00, c.0, c.1, c.2,
        ];
        if asus_hidraw_write(&msg) {
            let _ = asus_hidraw_write(&[0x5D, 0xB5, 0, 0, 0]);
            let _ = asus_hidraw_write(&[0x5D, 0xB4]);
            hid = true;
        }
    }

    if applied || hid {
        let mut method = Vec::new();
        if applied {
            method.push("sysfs");
        }
        if hid {
            method.push("HID");
        }
        Ok(format!("kbd color: {} via {}", c.label(), method.join("+")))
    } else {
        Err("keyboard RGB interface (multi_intensity / kbd_rgb_mode / hidraw) not found".into())
    }
}

// ───────────────────── Webcam & Touchpad (Wayland + Reboot Persistence) ─────────────────────

fn usb_webcam_auth_paths() -> &'static [PathBuf] {
    static PATHS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    PATHS.get_or_init(|| {
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir("/sys/bus/usb/devices") else {
            return out;
        };
        for e in entries.flatten() {
            let p = e.path();
            let name = e.file_name();
            let name_str = name.to_string_lossy();
            if let Some((dev_prefix, _)) = name_str.split_once(':') {
                if rd(p.join("bInterfaceClass")).as_deref() == Some("0e") {
                    let auth =
                        PathBuf::from(format!("/sys/bus/usb/devices/{dev_prefix}/authorized"));
                    if auth.exists() && !out.contains(&auth) {
                        out.push(auth);
                    }
                }
                continue;
            }
            let auth = p.join("authorized");
            if auth.exists() {
                let prod = rd(p.join("product"))
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if (prod.contains("camera") || prod.contains("webcam") || prod.contains("uvc"))
                    && !out.contains(&auth)
                {
                    out.push(auth);
                }
            }
        }
        out
    })
}

fn webcam_get(cfg: &Cfg) -> Option<bool> {
    if let Some(p) = attr("camera_disable") {
        if let Some(v) = rd(p) {
            return Some(v == "0");
        }
    }
    if let Some(p) = attr("camera") {
        if let Some(v) = rd(p) {
            return Some(v != "0");
        }
    }
    if let Some(first) = usb_webcam_auth_paths().first() {
        if let Some(v) = rd(first) {
            return Some(v != "0");
        }
    }
    if Path::new("/sys/module/uvcvideo").exists() {
        return Some(true);
    }
    cfg.webcam
}

fn webcam_set(on: bool) -> Result<String, String> {
    let mut applied = false;

    if wr_attr_all("camera_disable", if on { "0" } else { "1" }) {
        applied = true;
    }
    if wr_attr_all("camera", if on { "1" } else { "0" }) {
        applied = true;
    }

    for auth in usb_webcam_auth_paths() {
        if wr(auth, if on { "1" } else { "0" }).is_ok() {
            applied = true;
        }
    }

    if !applied {
        let ok = if on {
            run_quiet("modprobe", &["uvcvideo"])
        } else {
            run_quiet("modprobe", &["-r", "uvcvideo"])
        };
        if ok {
            applied = true;
        }
    }

    if applied {
        Ok(format!("webcam {}", on_off_upper(on)))
    } else {
        Err("no webcam control interface (camera_disable / USB UVC / uvcvideo) found".into())
    }
}

fn touchpad_inhibit_paths() -> &'static [PathBuf] {
    static PATHS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    PATHS.get_or_init(|| {
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir("/sys/class/input") else {
            return out;
        };
        for e in entries.flatten() {
            let p = e.path();
            let inh = p.join("inhibited");
            if !inh.exists() {
                continue;
            }
            let name = rd(p.join("name")).unwrap_or_default().to_ascii_lowercase();
            let uev = rd(p.join("uevent")).unwrap_or_default();
            if name.contains("touchpad")
                || name.contains("trackpad")
                || uev.contains("ID_INPUT_TOUCHPAD=1")
            {
                out.push(inh);
            }
        }
        out
    })
}

fn session_user_env() -> Option<(String, u32)> {
    static CACHED: Mutex<Option<(String, u32)>> = Mutex::new(None);
    if let Ok(guard) = CACHED.lock() {
        if let Some(ref val) = *guard {
            return Some(val.clone());
        }
    }
    let found = (|| {
        if let Ok(user) = env::var("SUDO_USER") {
            if !user.is_empty() && user != "root" {
                let uid = env::var("SUDO_UID")
                    .ok()
                    .and_then(|u| u.parse::<u32>().ok())
                    .unwrap_or(1000);
                return Some((user, uid));
            }
        }
        for e in fs::read_dir("/run/user").ok()?.flatten() {
            if let Ok(uid) = e.file_name().to_string_lossy().parse::<u32>() {
                if uid >= 1000 {
                    if let Ok(passwd) = fs::read_to_string("/etc/passwd") {
                        for line in passwd.lines() {
                            let mut parts = line.split(':');
                            let name = parts.next().unwrap_or("");
                            let _ = parts.next();
                            if parts.next().and_then(|u| u.parse::<u32>().ok()) == Some(uid) {
                                return Some((name.to_string(), uid));
                            }
                        }
                    }
                }
            }
        }
        None
    })();
    if let Some(ref val) = found {
        if let Ok(mut guard) = CACHED.lock() {
            *guard = Some(val.clone());
        }
    }
    found
}

fn sync_wayland_touchpad(on: bool) -> bool {
    let mut synced = false;
    let Some((ref user, uid)) = session_user_env() else {
        return false;
    };
    let bus_path = format!("/run/user/{uid}/bus");
    let bus_env = format!("DBUS_SESSION_BUS_ADDRESS=unix:path={bus_path}");

    let cosmic_dir = PathBuf::from(format!(
        "/home/{user}/.config/cosmic/com.system76.CosmicSettings.Input/v1"
    ));
    if cosmic_dir.exists() {
        let tp_file = cosmic_dir.join("touchpad");
        let state_str = if on { "Enabled" } else { "Disabled" };
        if let Some(existing) = rd(&tp_file) {
            let updated = if existing.contains("Disabled") {
                existing.replace("Disabled", state_str)
            } else if existing.contains("Enabled") {
                existing.replace("Enabled", state_str)
            } else {
                format!("(state:{state_str})")
            };
            if wr(&tp_file, updated).is_ok() {
                synced = true;
            }
        }
    }

    if !Path::new(&bus_path).exists() {
        return synced;
    }

    let gset_val = if on { "enabled" } else { "disabled" };
    if run_quiet(
        "runuser",
        &[
            "-u",
            user,
            "--",
            "env",
            &bus_env,
            "gsettings",
            "set",
            "org.gnome.desktop.peripherals.touchpad",
            "send-events",
            gset_val,
        ],
    ) {
        synced = true;
    }

    if let Ok(entries) = fs::read_dir("/sys/class/input") {
        for e in entries.flatten() {
            let fname = e.file_name();
            let s = fname.to_string_lossy();
            if !s.starts_with("event") {
                continue;
            }
            let dev_name = rd(e.path().join("device/name"))
                .unwrap_or_default()
                .to_ascii_lowercase();
            if !dev_name.contains("touchpad") && !dev_name.contains("trackpad") {
                continue;
            }
            let obj_path = format!("/org/kde/KWin/InputDevice/{s}");
            let bool_arg = if on {
                "variant:boolean:true"
            } else {
                "variant:boolean:false"
            };
            if run_quiet(
                "runuser",
                &[
                    "-u",
                    user,
                    "--",
                    "env",
                    &bus_env,
                    "dbus-send",
                    "--session",
                    "--dest=org.kde.KWin",
                    &obj_path,
                    "org.freedesktop.DBus.Properties.Set",
                    "string:org.kde.KWin.InputDevice",
                    "string:enabled",
                    bool_arg,
                ],
            ) {
                synced = true;
            }
        }
    }

    synced
}

fn touchpad_get(cfg: &Cfg) -> Option<bool> {
    if let Some(first) = touchpad_inhibit_paths().first() {
        if let Some(v) = rd(first) {
            return Some(v == "0");
        }
    }
    if let Some(p) = attr("touchpad") {
        if let Some(v) = rd(p) {
            return Some(v != "0");
        }
    }
    cfg.touchpad
}

fn touchpad_set(on: bool) -> Result<String, String> {
    let mut applied = false;

    for inh in touchpad_inhibit_paths() {
        if wr(inh, if on { "0" } else { "1" }).is_ok() {
            applied = true;
        }
    }

    if wr_attr_all("touchpad", if on { "1" } else { "0" }) {
        applied = true;
    }

    if sync_wayland_touchpad(on) {
        applied = true;
    }

    if applied {
        Ok(format!("touchpad {}", on_off_upper(on)))
    } else {
        Err("no touchpad control interface found".into())
    }
}

// ───────────────────────── performance mode ─────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Mode {
    Silent,
    Balanced,
    Turbo,
}
const MODES: [Mode; 3] = [Mode::Silent, Mode::Balanced, Mode::Turbo];

impl Mode {
    const fn name(self) -> &'static str {
        match self {
            Mode::Silent => "silent",
            Mode::Balanced => "balanced",
            Mode::Turbo => "turbo",
        }
    }
    fn parse(s: &str) -> Option<Mode> {
        MODES.into_iter().find(|m| m.name().eq_ignore_ascii_case(s))
    }
    const fn profiles(self) -> &'static [&'static str] {
        match self {
            Mode::Silent => &["low-power", "quiet"],
            Mode::Balanced => &["balanced"],
            Mode::Turbo => &["performance"],
        }
    }
}

fn mode_current() -> Option<Mode> {
    match rd(PROFILE)?.as_str() {
        "low-power" | "quiet" => Some(Mode::Silent),
        "balanced" | "balanced-performance" => Some(Mode::Balanced),
        "performance" => Some(Mode::Turbo),
        _ => None,
    }
}

fn mode_set(m: Mode, cfg: &mut Cfg) -> Result<String, String> {
    let choices = rd(format!("{PROFILE}_choices")).ok_or("platform_profile not available")?;
    let p = m
        .profiles()
        .iter()
        .find(|p| choices.split_whitespace().any(|c| c == **p))
        .ok_or("mode not supported by this firmware")?;
    wr(PROFILE, p)?;
    cfg.mode = Some(m);
    let boost_msg = match cpu_boost_apply_for_mode(m, cfg) {
        Ok(enabled) => format!(" + CPU boost {}", on_off(enabled)),
        Err(e) => format!(" + CPU boost unavailable ({e})"),
    };
    let fans = fan_list();
    let fmsg = fan_apply_mode(m, cfg, &fans)?;
    // Re-apply ryzenadj limits if active, since platform_profile resets SMU limits
    let rmsg = if cfg.ryzen.is_active() && ryzen_apply(&cfg.ryzen).is_ok() {
        " + ryzenadj"
    } else {
        ""
    };
    // Re-apply nvidia power fix if active, since platform_profile resets GPU limits
    let nvmsg = if cfg.nv_power_fix {
        match nv_fix_apply() {
            Ok(_) => " + nv_fix",
            Err(_) => " (nv_fix failed)",
        }
    } else {
        ""
    };
    Ok(format!("{}{boost_msg}{fmsg}{rmsg}{nvmsg}", m.name()))
}

// ───────────── Game Mode (auto performance) ─────────────

/// Wine/system processes that are never considered games.
const NOT_GAMES: &[&str] = &[
    "steam",
    "heroic",
    "lutris",
    "steamwebhelper",
    "fossilize_repla",
    "fossilize_replay",
    "legendary",
    "gogdl",
    "nile",
    "comet",
    "wineserver",
    "wineconsole",
    "winemenubuilder",
    "services.exe",
    "winedevice.exe",
    "plugplay.exe",
    "explorer.exe",
    "rpcss.exe",
    "svchost.exe",
    "tabtip.exe",
    "conhost.exe",
    "rundll32.exe",
    "mscorsvw.exe",
    "iexplore.exe",
    "winedbg.exe",
    "start.exe",
    "wineboot.exe",
    "steam.exe",
    "gamemoded",
];

#[derive(Clone, PartialEq, Eq, Debug)]
enum GameRuleKind {
    AnyExe,
    Exe(String),
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct GameRule {
    enabled: bool,
    kind: GameRuleKind,
    value: String,
}

impl GameRule {
    fn parse(s: &str) -> Option<GameRule> {
        let s = s.trim();
        let (enabled, rest) = if let Some(r) = s.strip_prefix("1:") {
            (true, r)
        } else if let Some(r) = s.strip_prefix("0:") {
            (false, r)
        } else if let Some(r) = s.strip_prefix("on:") {
            (true, r)
        } else if let Some(r) = s.strip_prefix("off:") {
            (false, r)
        } else {
            (true, s)
        };
        let rest = rest.trim();
        if rest.eq_ignore_ascii_case("anyexe") {
            Some(GameRule {
                enabled,
                kind: GameRuleKind::AnyExe,
                value: String::new(),
            })
        } else {
            rest.strip_prefix("exe:").map(|name| GameRule {
                enabled,
                kind: GameRuleKind::Exe(name.to_string()),
                value: name.to_string(),
            })
        }
    }

    fn to_conf(&self) -> String {
        let en = if self.enabled { "1" } else { "0" };
        match &self.kind {
            GameRuleKind::AnyExe => format!("{en}:anyexe"),
            GameRuleKind::Exe(name) => format!("{en}:exe:{name}"),
        }
    }

    fn label(&self) -> String {
        let en = if self.enabled { "[x]" } else { "[ ]" };
        match &self.kind {
            GameRuleKind::AnyExe => format!("{en} any .exe / Wine game"),
            GameRuleKind::Exe(name) => format!("{en} exe: {name}"),
        }
    }
}

struct ProcInfo {
    exe: String,
    arg0: String,
    wine_exe: Option<String>,
    is_game_env: bool,
}

fn read_proc(pid: u32) -> Option<ProcInfo> {
    let exe_link = format!("/proc/{pid}/exe");
    let exe = fs::read_link(&exe_link)
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();

    let cmdline = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let mut arg0 = String::new();
    let mut wine_exe = None;

    for (idx, arg_bytes) in cmdline.split(|&b| b == 0).enumerate() {
        if arg_bytes.is_empty() {
            continue;
        }
        let Ok(s) = std::str::from_utf8(arg_bytes) else { continue };
        if idx == 0 {
            let base = s.rsplit(['/', '\\']).next().unwrap_or(s);
            arg0 = base.to_string();
        }
        if proc_ends_with_exe(s) {
            let base = s.rsplit(['/', '\\']).next().unwrap_or(s);
            wine_exe = Some(base.to_string());
        }
    }

    let environ = fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
    let game_env_keys = [
        b"SteamAppId=".as_ref(),
        b"SteamGameId=".as_ref(),
        b"STEAM_COMPAT_DATA_PATH=".as_ref(),
        b"HEROIC_APP_NAME=".as_ref(),
        b"LUTRIS_GAME_UUID=".as_ref(),
    ];
    let is_game_env = environ.split(|&b| b == 0).any(|var| {
        game_env_keys.iter().any(|key| var.starts_with(key))
    });

    Some(ProcInfo { exe, arg0, wine_exe, is_game_env })
}

fn is_zombie(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("State:"))
                .map(|l| l.contains('Z'))
        })
        .unwrap_or(false)
}

fn proc_ends_with_exe(name: &str) -> bool {
    name.len() > 4 && name[name.len() - 4..].eq_ignore_ascii_case(".exe")
}

fn scan_procs(rules: &[GameRule]) -> bool {
    let dir = match fs::read_dir("/proc") {
        Ok(d) => d,
        Err(_) => return false,
    };
    for entry in dir.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let pid: u32 = match name_str.parse() {
            Ok(n) => n,
            Err(_) => continue,
        };
        if is_zombie(pid) {
            continue;
        }
        let Some(info) = read_proc(pid) else { continue };
        for rule in rules {
            if !rule.enabled {
                continue;
            }
            match &rule.kind {
                GameRuleKind::AnyExe => {
                    let is_exe = proc_ends_with_exe(&info.exe)
                        || proc_ends_with_exe(&info.arg0)
                        || info.wine_exe.is_some();
                    let not_game = NOT_GAMES.iter().any(|&ng| {
                        ng.eq_ignore_ascii_case(&info.exe)
                            || ng.eq_ignore_ascii_case(&info.arg0)
                            || info.wine_exe.as_ref().is_some_and(|w| ng.eq_ignore_ascii_case(w))
                    });
                    if (is_exe || info.is_game_env) && !not_game {
                        return true;
                    }
                }
                GameRuleKind::Exe(name) => {
                    if name.eq_ignore_ascii_case(&info.exe)
                        || name.eq_ignore_ascii_case(&info.arg0)
                        || info.wine_exe.as_ref().is_some_and(|w| name.eq_ignore_ascii_case(w))
                    {
                        return true;
                    }
                }
            }
        }
    }
    false
}

fn running_game_candidates() -> Vec<String> {
    let mut out = Vec::new();
    let Ok(dir) = fs::read_dir("/proc") else { return out };
    for entry in dir.flatten() {
        let name = entry.file_name();
        let Ok(pid) = name.to_string_lossy().parse::<u32>() else { continue };
        if is_zombie(pid) { continue };
        let Some(info) = read_proc(pid) else { continue };
        let not_game = NOT_GAMES.iter().any(|&ng| {
            ng.eq_ignore_ascii_case(&info.exe)
                || ng.eq_ignore_ascii_case(&info.arg0)
                || info.wine_exe.as_ref().is_some_and(|w| ng.eq_ignore_ascii_case(w))
        });
        if not_game { continue };
        if let Some(w) = info.wine_exe {
            if !out.contains(&w) { out.push(w); }
        } else if proc_ends_with_exe(&info.arg0) {
            if !out.contains(&info.arg0) { out.push(info.arg0); }
        } else if proc_ends_with_exe(&info.exe) {
            if !out.contains(&info.exe) { out.push(info.exe); }
        } else if info.is_game_env && !info.arg0.is_empty() && !out.contains(&info.arg0) {
            out.push(info.arg0);
        }
    }
    out.sort();
    out
}

fn run_gamemode_daemon() {
    let restore_path = "/run/ghl-gamemode.state";
    let pid_path = "/run/ghl-gamemode.pid";

    // Avoid two daemons fighting over platform_profile and CPU boost.
    if let Ok(existing) = fs::read_to_string(pid_path) {
        if let Ok(pid) = existing.trim().parse::<u32>() {
            if pid != std::process::id() && Path::new(&format!("/proc/{pid}")).exists() {
                eprintln!("ghl gamemode: daemon already running (pid {pid})");
                return;
            }
        }
    }
    let _ = fs::write(pid_path, std::process::id().to_string());

    let mut last_applied: Option<Mode> = None;
    let mut user_override = false;
    let mut missed_ticks = 0u32;
    let mut restore_mode: Option<Mode> = None;

    loop {
        let cfg = Cfg::load();
        let enabled_rules: Vec<_> = cfg.game_rules.iter().filter(|r| r.enabled).cloned().collect();

        if enabled_rules.is_empty() {
            if last_applied.is_some() || restore_mode.is_some() || user_override {
                if !user_override {
                    if let Some(restore) = restore_mode {
                        let mut cfg2 = Cfg::load();
                        if let Err(e) = mode_set(restore, &mut cfg2) {
                            eprintln!("ghl gamemode: failed to restore {}: {e}", restore.name());
                        }
                    }
                }
                let _ = fs::remove_file(restore_path);
                last_applied = None;
                restore_mode = None;
                user_override = false;
                missed_ticks = 0;
            }
            thread::sleep(Duration::from_secs(3));
            continue;
        }

        let matched = scan_procs(&enabled_rules);
        let current = mode_current();

        if matched {
            missed_ticks = 0;
            if user_override {
                // Respect a manual mode change until the game exits.
            } else if let Some(applied) = last_applied {
                if current != Some(applied) {
                    user_override = true;
                    last_applied = None;
                }
            } else {
                let target = cfg.game_mode.unwrap_or(Mode::Turbo);
                if restore_mode.is_none() {
                    restore_mode = current.filter(|cur| *cur != target);
                    if let Some(original) = restore_mode {
                        let _ = fs::write(restore_path, original.name());
                    }
                }

                let mut cfg2 = Cfg::load();
                match mode_set(target, &mut cfg2) {
                    Ok(_) => last_applied = Some(target),
                    Err(e) => {
                        last_applied = None;
                        eprintln!("ghl gamemode: failed to apply {}: {e}", target.name());
                    }
                }
            }
        } else if last_applied.is_some() || restore_mode.is_some() || user_override {
            missed_ticks += 1;
            // Debounce for 2 ticks (6s) so level loads / process restarts don't flap modes.
            if missed_ticks >= 2 {
                if !user_override {
                    if let Some(restore) = restore_mode {
                        let mut cfg2 = Cfg::load();
                        if let Err(e) = mode_set(restore, &mut cfg2) {
                            eprintln!("ghl gamemode: failed to restore {}: {e}", restore.name());
                        }
                    }
                }
                let _ = fs::remove_file(restore_path);
                last_applied = None;
                restore_mode = None;
                user_override = false;
                missed_ticks = 0;
            }
        }

        thread::sleep(Duration::from_secs(3));
    }
}

// ───────────────────────── fans & CPU/GPU telemetry ─────────────────────────

type Curve = [(u32, u32); 8]; // (°C, percent)

const PRESETS: [(&str, Curve); 3] = [
    (
        "quiet",
        [
            (40, 0),
            (50, 0),
            (60, 15),
            (65, 25),
            (70, 35),
            (75, 45),
            (80, 60),
            (90, 85),
        ],
    ),
    (
        "balanced",
        [
            (40, 0),
            (50, 10),
            (55, 20),
            (60, 30),
            (65, 40),
            (70, 55),
            (75, 70),
            (85, 100),
        ],
    ),
    (
        "aggressive",
        [
            (35, 20),
            (45, 30),
            (55, 45),
            (60, 55),
            (65, 65),
            (70, 80),
            (75, 90),
            (80, 100),
        ],
    ),
];

fn fan_clamp(c: &mut Curve) {
    for (t, p) in c.iter_mut() {
        *t = (*t).min(100);
        *p = (*p).min(100);
    }
}

fn fan_list() -> Vec<(u8, &'static str)> {
    let Some(d) = hwmon_fan_curve() else {
        return vec![];
    };
    [(1u8, "CPU"), (2, "GPU"), (3, "MID")]
        .into_iter()
        .filter(|(n, _)| d.join(format!("pwm{n}_enable")).exists())
        .collect()
}

fn fan_read(n: u8) -> Option<Curve> {
    let d = hwmon_fan_curve()?;
    let mut c: Curve = [(0, 0); 8];
    for (i, p) in c.iter_mut().enumerate() {
        let t = rd_u32(d.join(format!("pwm{n}_auto_point{}_temp", i + 1)))?;
        let w = rd_u32(d.join(format!("pwm{n}_auto_point{}_pwm", i + 1)))?;
        *p = (if t > 255 { t / 1000 } else { t }, (w * 100 + 127) / 255);
    }
    Some(c)
}

fn fan_write(n: u8, c: &Curve) -> Result<(), String> {
    let d = hwmon_fan_curve().ok_or("no custom fan curve interface (kernel < 5.17?)")?;
    let en = d.join(format!("pwm{n}_enable"));
    let millis = rd_u32(d.join(format!("pwm{n}_auto_point1_temp"))).is_some_and(|t| t > 255);
    wr(&en, "2")?;
    for (i, (t, p)) in c.iter().enumerate() {
        let t_val = (*t).min(100) * if millis { 1000 } else { 1 };
        let p_val = (*p).min(100) * 255 / 100;
        wr(
            d.join(format!("pwm{n}_auto_point{}_temp", i + 1)),
            t_val.to_string(),
        )?;
        wr(
            d.join(format!("pwm{n}_auto_point{}_pwm", i + 1)),
            p_val.to_string(),
        )?;
    }
    wr(&en, "1")
}

fn fan_default(n: u8) -> Result<(), String> {
    let d = hwmon_fan_curve().ok_or("no custom fan curve interface")?;
    wr(d.join(format!("pwm{n}_enable")), "2")
}

fn fan_apply_mode(m: Mode, cfg: &Cfg, fans: &[(u8, &str)]) -> Result<&'static str, String> {
    let mut custom = false;
    for (n, _) in fans {
        match cfg.curves.get(&(m, *n)) {
            Some(c) => {
                fan_write(*n, c)?;
                custom = true;
            }
            None => fan_default(*n)?,
        }
    }
    Ok(if custom { " + custom fan curve" } else { "" })
}

fn fan_rpm() -> Vec<u32> {
    let Some(d) = hwmon_asus() else {
        return vec![];
    };
    (1..=3)
        .filter_map(|i| rd_u32(d.join(format!("fan{i}_input"))))
        .collect()
}

#[derive(Default, Clone, Copy)]
struct DevMetrics {
    temp_c: Option<u32>,
    freq_mhz: Option<u32>,
    power_w: Option<f32>,
}

impl DevMetrics {
    fn fmt_line(self, label: &str) -> String {
        let t = self.temp_c.map_or_else(|| "—".into(), |v| format!("{v}°C"));
        let f = self
            .freq_mhz
            .map_or_else(|| "—".into(), |v| format!("{v} MHz"));
        let p = self
            .power_w
            .map_or_else(|| "—".into(), |v| format!("{v:.1} W"));
        format!(" {label:<4}  temp: {t:<6}  freq: {f:<9}  power: {p}")
    }
}

fn cpu_temp() -> Option<u32> {
    hwmon_cpu()
        .and_then(|d| rd_u32(d.join("temp1_input")))
        .or_else(|| hwmon_asus().and_then(|d| rd_u32(d.join("temp1_input"))))
        .map(|t| t / 1000)
}

fn cpu_freq_paths() -> &'static [PathBuf] {
    static PATHS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    PATHS.get_or_init(|| {
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir("/sys/devices/system/cpu") else {
            return out;
        };
        for e in entries.flatten() {
            let name = e.file_name();
            let s = name.to_string_lossy();
            if s.len() > 3 && s.starts_with("cpu") && s[3..].bytes().all(|b| b.is_ascii_digit()) {
                let p = e.path().join("cpufreq/scaling_cur_freq");
                if p.exists() {
                    out.push(p);
                }
            }
        }
        out
    })
}

fn cpu_freq_mhz() -> Option<u32> {
    let mut sum = 0u64;
    let mut count = 0u64;
    for p in cpu_freq_paths() {
        if let Some(khz) = rd_u64(p) {
            sum += khz / 1000;
            count += 1;
        }
    }
    if count > 0 {
        return Some((sum.checked_div(count).unwrap_or(0)) as u32);
    }
    let cpuinfo = fs::read_to_string("/proc/cpuinfo").ok()?;
    for line in cpuinfo.lines() {
        if let Some(rest) = line.strip_prefix("cpu MHz") {
            if let Some((_, val)) = rest.split_once(':') {
                if let Ok(mhz) = val.trim().parse::<f32>() {
                    return Some(mhz as u32);
                }
            }
        }
    }
    None
}

fn rapl_energy_path() -> Option<&'static Path> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        for cand in [
            "/sys/class/powercap/intel-rapl:0/energy_uj",
            "/sys/class/powercap/intel-rapl/intel-rapl:0/energy_uj",
        ] {
            let p = PathBuf::from(cand);
            if p.exists() {
                return Some(p);
            }
        }
        None
    })
    .as_deref()
}

fn rapl_state() -> &'static Mutex<Option<(u64, Instant)>> {
    static LAST: OnceLock<Mutex<Option<(u64, Instant)>>> = OnceLock::new();
    LAST.get_or_init(|| Mutex::new(None))
}

fn prime_rapl() {
    if let Some(rapl) = rapl_energy_path() {
        if let Ok(mut guard) = rapl_state().lock() {
            if guard.is_none() {
                if let Some(e) = rd_u64(rapl) {
                    *guard = Some((e, Instant::now()));
                }
            }
        }
    }
}

#[derive(Default, Clone, Copy)]
struct SmuLive {
    stapm_lim: f32,
    stapm_val: f32,
    fast_lim: f32,
    fast_val: f32,
    slow_lim: f32,
    slow_val: f32,
    vdd_tdc_lim: f32,
    vdd_tdc_val: f32,
    soc_tdc_lim: f32,
    soc_tdc_val: f32,
    vdd_edc_lim: f32,
    vdd_edc_val: f32,
    soc_edc_lim: f32,
    soc_edc_val: f32,
    tctl_lim: f32,
    tctl_val: f32,
}

fn read_smu_live() -> Option<SmuLive> {
    let bytes = fs::read("/sys/kernel/ryzen_smu_drv/pm_table").ok()?;
    if bytes.len() < 18 * 4 {
        return None;
    }
    let f = |idx: usize| -> f32 {
        let off = idx * 4;
        f32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
    };
    let fast_lim = f(2);
    if !(1.0..=500.0).contains(&fast_lim) {
        return None;
    }
    Some(SmuLive {
        stapm_lim: f(0),
        stapm_val: f(1),
        fast_lim,
        fast_val: f(3),
        slow_lim: f(4),
        slow_val: f(5),
        vdd_tdc_lim: f(8),
        vdd_tdc_val: f(9),
        soc_tdc_lim: f(10),
        soc_tdc_val: f(11),
        vdd_edc_lim: f(12),
        vdd_edc_val: f(13),
        soc_edc_lim: f(14),
        soc_edc_val: f(15),
        tctl_lim: f(16),
        tctl_val: f(17),
    })
}

fn cpu_power_w() -> Option<f32> {
    if let Some(smu) = read_smu_live() {
        if smu.fast_val > 0.0 && smu.fast_val < 300.0 {
            return Some(smu.fast_val);
        }
    }
    if let Some(d) = hwmon_cpu() {
        for node in ["power1_average", "power1_input"] {
            if let Some(uw) = rd_u64(d.join(node)) {
                if uw > 0 {
                    return Some(uw as f32 / 1_000_000.0);
                }
            }
        }
    }
    let rapl = rapl_energy_path()?;
    let mut guard = rapl_state().lock().ok()?;
    let e_now = rd_u64(rapl)?;
    let t_now = Instant::now();

    if let Some((e_prev, t_prev)) = *guard {
        let dt = t_now.duration_since(t_prev).as_secs_f32();
        if dt >= 0.012 && e_now >= e_prev {
            *guard = Some((e_now, t_now));
            return Some((e_now - e_prev) as f32 / 1_000_000.0 / dt);
        }
    }
    thread::sleep(Duration::from_millis(15));
    let e2 = rd_u64(rapl)?;
    let t2 = Instant::now();
    let dt = t2.duration_since(t_now).as_secs_f32();
    *guard = Some((e2, t2));
    if dt > 0.0 && e2 >= e_now {
        Some((e2 - e_now) as f32 / 1_000_000.0 / dt)
    } else {
        None
    }
}

fn cpu_metrics() -> DevMetrics {
    DevMetrics {
        temp_c: cpu_temp(),
        freq_mhz: cpu_freq_mhz(),
        power_w: cpu_power_w(),
    }
}

fn nvidia_pci_dev() -> Option<&'static Path> {
    static DEV: OnceLock<Option<PathBuf>> = OnceLock::new();
    DEV.get_or_init(|| {
        for e in fs::read_dir("/sys/bus/pci/devices").ok()?.flatten() {
            let p = e.path();
            if rd(p.join("vendor")).as_deref() == Some("0x10de") {
                let cls = rd(p.join("class")).unwrap_or_default();
                if cls.starts_with("0x0300") || cls.starts_with("0x0302") {
                    return Some(p);
                }
            }
        }
        None
    })
    .as_deref()
}

fn nvidia_pci_awake() -> bool {
    let Some(p) = nvidia_pci_dev() else {
        return false;
    };
    let state = rd(p.join("power_state")).unwrap_or_default();
    state.is_empty() || state == "D0"
}

fn gpu_metrics() -> DevMetrics {
    if gpu_current() != Some(Gpu::Igpu) && nvidia_pci_awake() {
        if let Ok(out) = Command::new("nvidia-smi")
            .args([
                "--query-gpu=clocks.current.graphics,power.draw,temperature.gpu",
                "--format=csv,noheader,nounits",
            ])
            .stderr(Stdio::null())
            .output()
        {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout);
                if let Some(line) = s.lines().next() {
                    let mut parts = line.split(',').map(str::trim);
                    let freq_mhz = parts
                        .next()
                        .and_then(|v| v.parse::<f32>().ok())
                        .map(|v| v as u32);
                    let power_w = parts.next().and_then(|v| v.parse::<f32>().ok());
                    let temp_c = parts
                        .next()
                        .and_then(|v| v.parse::<f32>().ok())
                        .map(|v| v as u32);
                    if freq_mhz.is_some() || power_w.is_some() || temp_c.is_some() {
                        return DevMetrics {
                            temp_c,
                            freq_mhz,
                            power_w,
                        };
                    }
                }
            }
        }
    }

    let mut m = DevMetrics::default();
    let Ok(cards) = fs::read_dir("/sys/class/drm") else {
        return m;
    };
    for c in cards.flatten() {
        let cname = c.file_name();
        let cs = cname.to_string_lossy();
        if !cs.starts_with("card") || cs.contains('-') {
            continue;
        }
        let cpath = c.path();
        let dev = cpath.join("device");
        if m.freq_mhz.is_none() {
            if let Some(mhz) = rd_u32(cpath.join("gt_cur_freq_mhz"))
                .or_else(|| rd_u32(cpath.join("gt/gt0/rps_cur_freq_mhz")))
            {
                m.freq_mhz = Some(mhz);
            } else if let Some(sclk) = rd(dev.join("pp_dpm_sclk")) {
                for line in sclk.lines() {
                    if line.contains('*') {
                        for tok in line.split_whitespace() {
                            if let Some(num) =
                                tok.strip_suffix("Mhz").or_else(|| tok.strip_suffix("MHz"))
                            {
                                if let Ok(v) = num.parse::<u32>() {
                                    m.freq_mhz = Some(v);
                                }
                            }
                        }
                    }
                }
            }
        }

        if let Ok(hwmons) = fs::read_dir(dev.join("hwmon")) {
            for h in hwmons.flatten() {
                let hp = h.path();
                if m.temp_c.is_none() {
                    m.temp_c = rd_u32(hp.join("temp1_input")).map(|v| v / 1000);
                }
                if m.freq_mhz.is_none() {
                    m.freq_mhz = rd_u64(hp.join("freq1_input")).map(|hz| (hz / 1_000_000) as u32);
                }
                if m.power_w.is_none() {
                    m.power_w = rd_u64(hp.join("power1_average"))
                        .or_else(|| rd_u64(hp.join("power1_input")))
                        .map(|uw| uw as f32 / 1_000_000.0);
                }
            }
        }
    }
    m
}

// ───────────────────────── optimal (auto) brightness ─────────────────────────

fn ab_get() -> Option<bool> {
    attr("screen_auto_brightness")
        .and_then(rd)
        .map(|s| s == "1")
}

fn ab_set(on: bool) -> Result<(), String> {
    let p = attr("screen_auto_brightness").ok_or("screen_auto_brightness not supported")?;
    wr(p, if on { "1" } else { "0" })
}

// ───────────────────── config (/etc/ghl.conf) & reboot persistence ─────────────────────

#[derive(Default)]
struct Cfg {
    mode: Option<Mode>,
    gpu_pending: Option<Gpu>,
    autobright: bool,
    nv_power_fix: bool,
    cpu_boost: Option<bool>,
    cpu_boost_modes: BTreeMap<Mode, bool>,
    panel_od: Option<bool>,
    disable_igpu_dgpu: bool,
    disable_npu: bool,
    kbd_bright: Option<u8>,
    kbd_color: Option<Rgb>,
    webcam: Option<bool>,
    touchpad: Option<bool>,
    ryzen: RyzenCfg,
    curves: BTreeMap<(Mode, u8), Curve>,
    game_rules: Vec<GameRule>,
    game_mode: Option<Mode>, // None = default = Turbo
}

impl Cfg {
    fn load() -> Cfg {
        let mut c = Cfg::default();
        let Some(txt) = rd(CONF) else { return c };
        for l in txt.lines() {
            let Some((k, v)) = l.split_once('=') else {
                continue;
            };
            let k = k.trim();
            let v = v.trim();
            match k {
                "mode" => c.mode = Mode::parse(v),
                "gpu_pending" => c.gpu_pending = Gpu::parse(v),
                "autobright" => c.autobright = v == "1",
                "nv_power_fix" => c.nv_power_fix = v == "1",
                "cpu_boost" => c.cpu_boost = parse_toggle(v),
                "panel_od" => c.panel_od = parse_toggle(v),
                "disable_igpu_dgpu" => c.disable_igpu_dgpu = parse_toggle(v).unwrap_or(false),
                "disable_npu" => c.disable_npu = parse_toggle(v).unwrap_or(false),
                "kbd_bright" => c.kbd_bright = kbd_bright_parse(v),
                "kbd_color" => c.kbd_color = Rgb::parse(v),
                "webcam" => c.webcam = parse_toggle(v),
                "touchpad" => c.touchpad = parse_toggle(v),
                "game_mode" => c.game_mode = Mode::parse(v),
                "game_rule" => {
                    if let Some(r) = GameRule::parse(v) {
                        c.game_rules.push(r);
                    }
                }
                _ => {
                    if let Some(rest) = k.strip_prefix("cpu_boost.") {
                        if let (Some(m), Some(b)) = (Mode::parse(rest), parse_toggle(v)) {
                            c.cpu_boost_modes.insert(m, b);
                        }
                    } else if let Some(rk) = k.strip_prefix("ryzen.") {
                        let _ = c.ryzen.set_by_key(rk, v);
                    } else if let Some(rest) = k.strip_prefix("fan.") {
                        if let Some((m_str, n_str)) = rest.split_once('.') {
                            if let (Some(m), Ok(n)) = (Mode::parse(m_str), n_str.parse::<u8>()) {
                                let pts: Vec<(u32, u32)> = v
                                    .split(',')
                                    .filter_map(|x| {
                                        let (t, p) = x.split_once(':')?;
                                        Some((t.parse().ok()?, p.parse().ok()?))
                                    })
                                    .collect();
                                if pts.len() == 8 {
                                    let mut cv: Curve = [(0, 0); 8];
                                    cv.copy_from_slice(&pts);
                                    fan_clamp(&mut cv);
                                    c.curves.insert((m, n), cv);
                                }
                            }
                        }
                    }
                }
            }
        }
        c
    }

    fn save(&self) -> Result<(), String> {
        let mut s = String::with_capacity(512);
        if let Some(m) = self.mode {
            let _ = writeln!(s, "mode={}", m.name());
        }
        if let Some(g) = self.gpu_pending {
            let _ = writeln!(s, "gpu_pending={}", g.name());
        }
        let _ = writeln!(s, "autobright={}", u8::from(self.autobright));
        let _ = writeln!(s, "nv_power_fix={}", u8::from(self.nv_power_fix));
        let _ = writeln!(s, "disable_igpu_dgpu={}", u8::from(self.disable_igpu_dgpu));
        let _ = writeln!(s, "disable_npu={}", u8::from(self.disable_npu));
        if let Some(b) = self.cpu_boost {
            let _ = writeln!(s, "cpu_boost={}", u8::from(b));
        }
        for (m, b) in &self.cpu_boost_modes {
            let _ = writeln!(s, "cpu_boost.{}={}", m.name(), u8::from(*b));
        }
        if let Some(od) = self.panel_od {
            let _ = writeln!(s, "panel_od={}", u8::from(od));
        }
        if let Some(kb) = self.kbd_bright {
            let _ = writeln!(s, "kbd_bright={kb}");
        }
        if let Some(kc) = self.kbd_color {
            let _ = writeln!(s, "kbd_color={}", kc.hex());
        }
        if let Some(w) = self.webcam {
            let _ = writeln!(s, "webcam={}", u8::from(w));
        }
        if let Some(tp) = self.touchpad {
            let _ = writeln!(s, "touchpad={}", u8::from(tp));
        }
        for (idx, meta) in RYZEN_FIELDS.iter().enumerate() {
            if let Some(val) = self.ryzen.slot(idx) {
                let _ = writeln!(s, "ryzen.{}={val}", meta.key);
            }
        }
        if let Some(ps) = self.ryzen.power_saving {
            let _ = writeln!(
                s,
                "ryzen.psave={}",
                if ps { "on" } else { "max-performance" }
            );
        }
        for ((m, n), c) in &self.curves {
            let _ = write!(s, "fan.{}.{n}=", m.name());
            for (i, (t, p)) in c.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                let _ = write!(s, "{t}:{p}");
            }
            s.push('\n');
        }
        if let Some(gm) = self.game_mode {
            let _ = writeln!(s, "game_mode={}", gm.name());
        }
        for rule in &self.game_rules {
            let _ = writeln!(s, "game_rule={}", rule.to_conf());
        }
        wr(CONF, &s)?;
        self.write_udev_persistence();
        Ok(())
    }

    fn write_udev_persistence(&self) {
        if !Path::new("/etc/udev/rules.d").exists() {
            return;
        }
        let mut rules =
            String::from("# Managed by ghl — persists hardware states across reboots\n");
        if let Some(kb) = self.kbd_bright {
            let _ = writeln!(
                rules,
                "SUBSYSTEM==\"leds\", KERNEL==\"*kbd_backlight*\", ATTR{{brightness}}=\"{kb}\""
            );
        }
        if let Some(kc) = self.kbd_color {
            let _ = writeln!(
                rules,
                "SUBSYSTEM==\"leds\", KERNEL==\"*kbd_backlight*\", ATTR{{multi_intensity}}=\"{} {} {}\"",
                kc.0, kc.1, kc.2
            );
        }
        if let Some(od) = self.panel_od {
            let v = u8::from(od);
            let _ = writeln!(
                rules,
                "SUBSYSTEM==\"platform\", KERNEL==\"asus-nb-wmi\", ATTR{{panel_od}}=\"{v}\""
            );
            let _ = writeln!(
                rules,
                "SUBSYSTEM==\"firmware-attributes\", KERNEL==\"asus-armoury\", RUN+=\"/bin/sh -c 'echo {v} > {ARMOURY}/panel_od/current_value 2>/dev/null; echo {v} > {ARMOURY}/panel_overdrive/current_value 2>/dev/null || true'\""
            );
        }
        if self.disable_igpu_dgpu {
            let _ = writeln!(
                rules,
                "ACTION==\"add\", SUBSYSTEM==\"pci\", KERNEL==\"0000:65:00.0\", RUN+=\"/bin/sh -c 'if [ \\\"$(cat {ARMOURY}/gpu_mux_mode/current_value {LEGACY}/gpu_mux_mode 2>/dev/null | head -n1)\\\" = \\\"0\\\" ]; then echo 0000:65:00.0 > /sys/bus/pci/drivers/amdgpu/unbind 2>/dev/null; echo 1 > /sys/bus/pci/devices/0000:65:00.0/remove 2>/dev/null; fi || true'\""
            );
        }
        if self.disable_npu {
            let _ = writeln!(
                rules,
                "ACTION==\"add\", SUBSYSTEM==\"pci\", KERNEL==\"0000:66:00.1\", RUN+=\"/bin/sh -c 'echo 1 > /sys/bus/pci/devices/0000:66:00.1/remove 2>/dev/null || true'\""
            );
            let _ = writeln!(
                rules,
                "ACTION==\"add\", SUBSYSTEM==\"pci\", KERNEL==\"0000:66:00.0\", RUN+=\"/bin/sh -c 'echo 1 > /sys/bus/pci/devices/0000:66:00.0/remove 2>/dev/null || true'\""
            );
        }
        if let Some(tp) = self.touchpad {
            let inh = if tp { "0" } else { "1" };
            let _ = writeln!(
                rules,
                "SUBSYSTEM==\"input\", KERNEL==\"input*\", ENV{{ID_INPUT_TOUCHPAD}}==\"1\", ATTR{{inhibited}}=\"{inh}\""
            );
        }
        if let Some(false) = self.webcam {
            let _ = writeln!(
                rules,
                "SUBSYSTEM==\"usb\", ATTR{{bInterfaceClass}}==\"0e\", RUN+=\"/bin/sh -c 'echo 0 > /sys$devpath/../authorized 2>/dev/null || true'\""
            );
        }
        let _ = wr(UDEV_PERSIST, rules);
    }
}

fn apply(cfg: &mut Cfg) -> Vec<String> {
    let mut out = Vec::new();
    let mut note = |r: Result<String, String>| match r {
        Ok(s) => out.push(s),
        Err(e) => out.push(format!("error: {e}")),
    };
    note(ab_set(cfg.autobright).map(|_| format!("auto-brightness {}", on_off(cfg.autobright))));
    if let Some(m) = cfg.mode.or_else(mode_current) {
        let (b, overridden) = cpu_boost_resolve_for_mode(m, cfg);
        note(cpu_boost_set(b).map(|_| {
            format!(
                "cpu boost {} ({} mode {})",
                on_off(b),
                m.name(),
                if overridden { "override" } else { "policy" }
            )
        }));
    } else if let Some(b) = cfg.cpu_boost {
        note(cpu_boost_set(b).map(|_| format!("cpu boost override {}", on_off(b))));
    }
    if let Some(od) = cfg.panel_od {
        note(panel_od_set(od));
    }
    if let Some(kb) = cfg.kbd_bright {
        let mut result = Err("keyboard backlight not initialized".to_string());
        for attempt in 0..5 {
            result = kbd_bright_set(kb);
            if result.is_ok() {
                break;
            }
            if attempt < 4 {
                thread::sleep(Duration::from_millis(300));
            }
        }
        note(result);
    }
    if let Some(kc) = cfg.kbd_color {
        let mut result = Err("keyboard RGB not initialized".to_string());
        for attempt in 0..5 {
            result = kbd_color_set(kc);
            if result.is_ok() {
                break;
            }
            if attempt < 4 {
                thread::sleep(Duration::from_millis(300));
            }
        }
        note(result);
    }
    if let Some(w) = cfg.webcam {
        note(webcam_set(w));
    }
    if let Some(tp) = cfg.touchpad {
        note(touchpad_set(tp));
    }
    if let Some(g) = cfg.gpu_pending {
        note(gpu_set(g, cfg));
    }
    if cfg.disable_igpu_dgpu {
        note(igpu_in_dgpu_apply(true));
    }
    if cfg.disable_npu {
        note(npu_apply(true, cfg.disable_igpu_dgpu));
    }
    if let Some(m) = cfg.mode {
        note(mode_set(m, cfg));
    } else {
        if let Some(m) = mode_current() {
            let fans = fan_list();
            note(fan_apply_mode(m, cfg, &fans).map(|s| format!("fans{s}")));
        }
        if cfg.ryzen.is_active() {
            note(ryzen_apply(&cfg.ryzen));
        }
        if cfg.nv_power_fix {
            note(nv_fix_apply());
        }
    }
    let _ = cfg.save();
    out
}

// ───────────────────────── CLI ─────────────────────────

fn status(cfg: &Cfg) {
    prime_rapl();
    let gpu = gpu_metrics();
    let cpu = cpu_metrics();
    println!(
        "gpu mode   : {}",
        gpu_current().map_or("unknown", |g| g.name())
    );
    println!(
        "mode       : {}",
        mode_current().map_or("unknown", |m| m.name())
    );
    let nv_tgp = attr("nv_tgp").and_then(rd);
    let nv_boost = attr("nv_dynamic_boost").and_then(rd);
    let nv_temp = attr("nv_temp_target").and_then(rd);
    let nv_details = match (nv_tgp, nv_boost, nv_temp) {
        (Some(tgp), Some(boost), Some(temp)) => {
            format!(" (TGP: {tgp}W, Boost: +{boost}W, Temp: {temp}°C)")
        }
        (Some(tgp), Some(boost), None) => format!(" (TGP: {tgp}W, Boost: +{boost}W)"),
        _ => String::new(),
    };
    println!(
        "nv-powerfix: {}{}",
        if cfg.nv_power_fix {
            "enabled"
        } else {
            "disabled"
        },
        nv_details
    );
    println!("ryzenadj   : {}", cfg.ryzen.summary());
    println!(
        "igpu-dgpu  : {} (PCI device: {})",
        if cfg.disable_igpu_dgpu {
            "disable in dGPU"
        } else {
            "keep enabled"
        },
        if igpu_present() { "present" } else { "removed" }
    );
    println!(
        "npu        : {} (PCI device: {})",
        if cfg.disable_npu {
            "disabled"
        } else {
            "enabled"
        },
        if npu_present() { "present" } else { "removed" }
    );
    println!(
        "overdrive  : {}",
        panel_od_get(cfg).map_or("unsupported", on_off)
    );
    println!("auto-bright: {}", ab_get().map_or("unsupported", on_off));
    println!(
        "cpu-boost  : {}",
        cpu_boost_get().map_or("unsupported", on_off)
    );
    for m in MODES {
        if let Some(b) = cfg.cpu_boost_modes.get(&m) {
            println!("             {} mode override: {}", m.name(), on_off(*b));
        }
    }
    println!(
        "kbd-bright : {}",
        kbd_bright_get().map_or_else(
            || "unsupported".into(),
            |lvl| format!("{} ({lvl}/3)", KBD_BRIGHT_LABELS[lvl as usize])
        )
    );
    println!(
        "kbd-color  : {}",
        kbd_color_get(cfg).map_or_else(|| "default".into(), |c| c.label())
    );
    println!(
        "webcam     : {}",
        webcam_get(cfg).map_or("unsupported", on_off)
    );
    println!(
        "touchpad   : {}",
        touchpad_get(cfg).map_or("unsupported", on_off)
    );
    println!("cpu stats  :{}", cpu.fmt_line("CPU"));
    println!("gpu stats  :{}", gpu.fmt_line("GPU"));
    println!("fan rpm    : {:?}", fan_rpm());
    for (n, l) in fan_list() {
        if let Some(c) = fan_read(n) {
            let pts: Vec<String> = c.iter().map(|(t, p)| format!("{t}°:{p}%")).collect();
            println!("fan {l:<3}    : {}", pts.join(" "));
        }
    }
}

fn ryzen_status(cfg: &Cfg) {
    println!("ryzenadj binary : {}", ryzenadj_bin());
    println!("summary         : {}", cfg.ryzen.summary());
    if let Some(smu) = read_smu_live() {
        println!(
            "live SMU table  : STAPM {:.1}/{:.0}W  Fast {:.1}/{:.0}W  Slow {:.1}/{:.0}W  VDD {:.1}/{:.0}A (EDC {:.1}/{:.0}A)  SoC {:.1}/{:.0}A (EDC {:.1}/{:.0}A)  Tctl {:.1}/{:.0}°C",
            smu.stapm_val,
            smu.stapm_lim,
            smu.fast_val,
            smu.fast_lim,
            smu.slow_val,
            smu.slow_lim,
            smu.vdd_tdc_val,
            smu.vdd_tdc_lim,
            smu.vdd_edc_val,
            smu.vdd_edc_lim,
            smu.soc_tdc_val,
            smu.soc_tdc_lim,
            smu.soc_edc_val,
            smu.soc_edc_lim,
            smu.tctl_val,
            smu.tctl_lim
        );
    }
    for (idx, meta) in RYZEN_FIELDS.iter().enumerate() {
        let val = if idx == 15 {
            match cfg.ryzen.power_saving {
                Some(true) => "power-saving".to_string(),
                Some(false) => "max-performance".to_string(),
                None => "— (default)".to_string(),
            }
        } else {
            cfg.ryzen.slot(idx).map_or_else(
                || "— (default)".into(),
                |v| format!("{v} {}", meta.unit),
            )
        };
        println!("  {:<14} ({:<34}): {val}", meta.key, meta.label);
    }
}

fn usage() {
    eprintln!(
        "ghl                                                  TUI\n\
         ghl status\n\
         ghl gpu igpu|hybrid|dgpu\n\
         ghl mode silent|balanced|turbo\n\
         ghl ryzen status|apply|reset\n\
         ghl ryzen preset eco|cool|balanced|max\n\
         ghl ryzen cpu <stapm_w> [slow_w] [fast_w] [tctl_c]   (limit CPU power/temp via ryzenadj)\n\
         ghl ryzen igpu <max_mhz> [tdc_a] [edc_a]             (limit iGPU clock/current via ryzenadj)\n\
         ghl ryzen npu <npu_tdc_a> [soc_tdc_a] [soc_max_mhz]  (limit NPU/CVIP & SoC power via ryzenadj)\n\
         ghl ryzen set <key=val>...                           (keys: stapm,slow,fast,tctl,cpu-tdc,cpu-edc,\n\
                                                               igpu-mhz,igpu-min-mhz,igpu-tdc,igpu-edc,\n\
                                                               npu-tdc,soc-tdc,soc-edc,soc-mhz,fclk-mhz,psave)\n\
         ghl disable-igpu on|off                              (unbind amdgpu & remove iGPU when in dGPU mode)\n\
         ghl disable-npu on|off                               (remove NPU PCI devices 0000:66:00.1 / .0)\n\
         ghl fan preset quiet|balanced|aggressive             (current mode, all fans)\n\
         ghl fan default                                      (back to firmware curve)\n\
         ghl kbdbright 0|1|2|3|off|low|med|high\n\
         ghl kbdcolor <RRGGBB | R G B | R,G,B>\n\
         ghl overdrive on|off\n\
         ghl webcam on|off\n\
         ghl touchpad on|off\n\
         ghl autobright on|off\n\
         ghl nvfix on|off|apply\n\
         ghl cpuboost [silent|balanced|turbo] on|off|auto   (per-mode boost, no mode = current)\n\
         ghl apply                                            (restore saved config at boot)\n\
         ghl gamemode status|list|running\n\
         ghl gamemode daemon                                  (run auto-perf loop, use with systemd)\n\
         ghl gamemode add anyexe|<ExeName.exe>\n\
         ghl gamemode toggle|enable|disable <pattern>\n\
         ghl gamemode remove <pattern>\n\
         ghl gamemode mode silent|balanced|turbo\n\
         ghl gamemode service                                 (install & enable systemd service)\n\
         ghl gamemode clear"
    );
}

fn run_ryzen_cli(sub: &[&str], cfg: &mut Cfg) -> Result<(), String> {
    match sub {
        [] | ["status"] => {
            ryzen_status(cfg);
            return Ok(());
        }
        ["apply"] => {
            println!("{}", ryzen_apply(&cfg.ryzen)?);
            return Ok(());
        }
        ["reset" | "clear" | "default" | "off"] => {
            cfg.ryzen = RyzenCfg::default();
            cfg.save()?;
            if let Some(m) = cfg.mode.or_else(mode_current) {
                let _ = mode_set(m, cfg);
            }
            println!("ryzenadj limits cleared (restored firmware defaults)");
            return Ok(());
        }
        ["preset", name] => {
            let (_, preset) = RYZEN_PRESETS
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .ok_or("ryzen preset: eco|cool|balanced|max")?;
            cfg.ryzen = *preset;
        }
        ["cpu", rest @ ..] if !rest.is_empty() && rest.len() <= 4 => {
            if rest[0].eq_ignore_ascii_case("off") || rest[0].eq_ignore_ascii_case("reset") {
                for idx in R_STAPM..=R_CPU_EDC {
                    cfg.ryzen.vals[idx] = None;
                }
            } else {
                let stapm: u32 = rest[0].parse().map_err(|_| "invalid stapm_w")?;
                let slow: u32 = rest
                    .get(1)
                    .map(|s| s.parse().map_err(|_| "invalid slow_w"))
                    .transpose()?
                    .unwrap_or(stapm);
                let fast: u32 = rest
                    .get(2)
                    .map(|s| s.parse().map_err(|_| "invalid fast_w"))
                    .transpose()?
                    .unwrap_or(slow);
                cfg.ryzen.vals[R_STAPM] = Some(stapm.clamp(5, 120));
                cfg.ryzen.vals[R_SLOW] = Some(slow.clamp(5, 120));
                cfg.ryzen.vals[R_FAST] = Some(fast.clamp(5, 140));
                if let Some(t) = rest.get(3) {
                    let tctl: u32 = t.parse().map_err(|_| "invalid tctl_c")?;
                    cfg.ryzen.vals[R_TCTL] = Some(tctl.clamp(55, 100));
                }
            }
        }
        ["igpu", rest @ ..] if !rest.is_empty() && rest.len() <= 3 => {
            if rest[0].eq_ignore_ascii_case("off") || rest[0].eq_ignore_ascii_case("reset") {
                for idx in R_IGPU_MHZ..=R_IGPU_EDC {
                    cfg.ryzen.vals[idx] = None;
                }
            } else {
                let mhz: u32 = rest[0].parse().map_err(|_| "invalid max_mhz")?;
                cfg.ryzen.vals[R_IGPU_MHZ] = Some(mhz.clamp(400, 3000));
                if let Some(tdc_s) = rest.get(1) {
                    let tdc: u32 = tdc_s.parse().map_err(|_| "invalid tdc_a")?;
                    cfg.ryzen.vals[R_IGPU_TDC] = Some(tdc.clamp(5, 90));
                }
                if let Some(edc_s) = rest.get(2) {
                    let edc: u32 = edc_s.parse().map_err(|_| "invalid edc_a")?;
                    cfg.ryzen.vals[R_IGPU_EDC] = Some(edc.clamp(5, 120));
                }
            }
        }
        ["npu", rest @ ..] if !rest.is_empty() && rest.len() <= 3 => {
            if rest[0].eq_ignore_ascii_case("off") || rest[0].eq_ignore_ascii_case("reset") {
                for idx in R_NPU_TDC..=R_SOC_MHZ {
                    cfg.ryzen.vals[idx] = None;
                }
            } else {
                let npu_tdc: u32 = rest[0].parse().map_err(|_| "invalid npu_tdc_a")?;
                cfg.ryzen.vals[R_NPU_TDC] = Some(npu_tdc.clamp(1, 50));
                if let Some(soc_s) = rest.get(1) {
                    let soc_tdc: u32 = soc_s.parse().map_err(|_| "invalid soc_tdc_a")?;
                    cfg.ryzen.vals[R_SOC_TDC] = Some(soc_tdc.clamp(5, 60));
                }
                if let Some(mhz_s) = rest.get(2) {
                    let mhz: u32 = mhz_s.parse().map_err(|_| "invalid soc_max_mhz")?;
                    cfg.ryzen.vals[R_SOC_MHZ] = Some(mhz.clamp(400, 2400));
                }
            }
        }
        ["set", pairs @ ..] if !pairs.is_empty() => {
            for pair in pairs {
                let (k, v) = pair
                    .split_once('=')
                    .ok_or_else(|| format!("expected key=val, got '{pair}'"))?;
                cfg.ryzen.set_by_key(k, v)?;
            }
        }
        pairs if pairs.iter().all(|p| p.contains('=')) => {
            for pair in pairs {
                let (k, v) = pair.split_once('=').unwrap();
                cfg.ryzen.set_by_key(k, v)?;
            }
        }
        _ => {
            return Err(
                "ryzen usage: ghl ryzen [status|apply|reset|preset <name>|cpu ...|igpu ...|npu ...|set key=val...]"
                    .into(),
            );
        }
    }

    cfg.save()?;
    if cfg.ryzen.is_active() {
        println!("{}", ryzen_apply(&cfg.ryzen)?);
    } else {
        println!("ryzenadj limits cleared");
    }
    Ok(())
}

fn run_cli(a: &[String], cfg: &mut Cfg) -> Result<(), String> {
    let s: Vec<&str> = a.iter().map(String::as_str).collect();
    match s[..] {
        ["status"] => status(cfg),
        ["help" | "--help" | "-h"] => usage(),
        ["apply"] => {
            for l in apply(cfg) {
                println!("{l}");
            }
        }
        ["ryzen" | "ryzenadj", ref rest @ ..] => run_ryzen_cli(rest, cfg)?,
        ["gpu", g] => {
            let g = Gpu::parse(g).ok_or("gpu: igpu|hybrid|dgpu")?;
            println!("{}", gpu_set(g, cfg)?);
            cfg.save()?;
        }
        ["mode", m] => {
            let m = Mode::parse(m).ok_or("mode: silent|balanced|turbo")?;
            println!("{}", mode_set(m, cfg)?);
            cfg.save()?;
        }
        ["disable-igpu" | "igpu-dgpu-off", v] => {
            let on = parse_toggle(v).ok_or("disable-igpu: on|off")?;
            cfg.disable_igpu_dgpu = on;
            println!("{}", igpu_in_dgpu_apply(on)?);
            if !on && cfg.disable_npu {
                let _ = npu_apply(true, false);
            }
            cfg.save()?;
        }
        ["disable-npu" | "npu-off", v] => {
            let on = parse_toggle(v).ok_or("disable-npu: on|off")?;
            cfg.disable_npu = on;
            println!("{}", npu_apply(on, cfg.disable_igpu_dgpu)?);
            cfg.save()?;
        }
        ["kbdbright", v] => {
            let lvl = kbd_bright_parse(v).ok_or("kbdbright: 0|1|2|3|off|low|med|high")?;
            cfg.kbd_bright = Some(lvl);
            println!("{}", kbd_bright_set(lvl)?);
            cfg.save()?;
        }
        ["kbdcolor", v] => {
            let rgb = Rgb::parse(v).ok_or("kbdcolor: use hex RRGGBB or R,G,B (0..255)")?;
            cfg.kbd_color = Some(rgb);
            println!("{}", kbd_color_set(rgb)?);
            cfg.save()?;
        }
        ["kbdcolor", r, g, b] => {
            let rgb = Rgb::parse(&format!("{r},{g},{b}"))
                .ok_or("kbdcolor: R G B values must be 0..255")?;
            cfg.kbd_color = Some(rgb);
            println!("{}", kbd_color_set(rgb)?);
            cfg.save()?;
        }
        ["overdrive" | "panelod", v] => {
            let on = parse_toggle(v).ok_or("overdrive: on|off")?;
            cfg.panel_od = Some(on);
            println!("{}", panel_od_set(on)?);
            cfg.save()?;
        }
        ["webcam", v] => {
            let on = parse_toggle(v).ok_or("webcam: on|off")?;
            cfg.webcam = Some(on);
            println!("{}", webcam_set(on)?);
            cfg.save()?;
        }
        ["touchpad", v] => {
            let on = parse_toggle(v).ok_or("touchpad: on|off")?;
            cfg.touchpad = Some(on);
            println!("{}", touchpad_set(on)?);
            cfg.save()?;
        }
        ["autobright", v] => {
            let on = parse_toggle(v).ok_or("autobright: on|off")?;
            cfg.autobright = on;
            ab_set(on)?;
            cfg.save()?;
        }
        ["cpuboost", m_str, v] if Mode::parse(m_str).is_some() => {
            let m = Mode::parse(m_str).ok_or("cpuboost: [mode] on|off|auto")?;
            cpu_boost_cli_set(cfg, m, v)?;
        }
        ["cpuboost", "auto"] => {
            cfg.cpu_boost = None;
            cfg.cpu_boost_modes.clear();
            if let Some(m) = cfg.mode.or_else(mode_current) {
                let enabled = cpu_boost_apply_for_mode(m, &cfg)?;
                println!("cpu boost {} ({} mode policy)", on_off(enabled), m.name());
            } else {
                println!("CPU boost policy set to automatic; choose a mode to apply it");
            }
            cfg.save()?;
        }
        ["cpuboost", v] => {
            let on = parse_toggle(v).ok_or("cpuboost: [mode] on|off|auto")?;
            match cfg.mode.or_else(mode_current) {
                Some(m) => {
                    cfg.cpu_boost_modes.insert(m, on);
                    cpu_boost_set(on)?;
                    println!("cpu boost {} for {} mode", on_off(on), m.name());
                }
                None => {
                    cfg.cpu_boost = Some(on);
                    cpu_boost_set(on)?;
                    println!("cpu boost override {}", on_off(on));
                }
            }
            cfg.save()?;
        }
        ["nvfix" | "nv-fix", "apply"] => {
            println!("{}", nv_fix_apply()?);
        }
        ["nvfix" | "nv-fix", v] => {
            let on = parse_toggle(v).ok_or("nvfix: on|off|apply")?;
            cfg.nv_power_fix = on;
            if on {
                println!("{}", nv_fix_apply()?);
            } else {
                nv_fix_restore();
                println!("NVIDIA Power Fix disabled (defaults restored)");
            }
            cfg.save()?;
        }
        ["fan", "default"] => {
            let m = mode_current().ok_or("unknown current mode")?;
            for (n, _) in fan_list() {
                cfg.curves.remove(&(m, n));
                fan_default(n)?;
            }
            cfg.save()?;
        }
        ["fan", "preset", p] => {
            let m = mode_current().ok_or("unknown current mode")?;
            let (_, c) = PRESETS
                .iter()
                .find(|(n, _)| *n == p)
                .ok_or("preset: quiet|balanced|aggressive")?;
            for (n, _) in fan_list() {
                let mut c = *c;
                fan_clamp(&mut c);
                fan_write(n, &c)?;
                cfg.curves.insert((m, n), c);
            }
            cfg.save()?;
        }
        ["gamemode" | "game-mode", ref rest @ ..] => {
            match rest {
                ["status"] => {
                    let gm = cfg.game_mode.unwrap_or(Mode::Turbo);
                    println!("game_mode target : {}", gm.name());
                    let enabled_count = cfg.game_rules.iter().filter(|r| r.enabled).count();
                    println!("rules ({} total, {} enabled):", cfg.game_rules.len(), enabled_count);
                    for (i, r) in cfg.game_rules.iter().enumerate() {
                        println!("  {i}: {}", r.label());
                    }
                    let state_path = "/run/ghl-gamemode.state";
                    if let Ok(s) = fs::read_to_string(state_path) {
                        println!("active           : YES (restore to: {})", s.trim());
                    } else {
                        println!("active           : no");
                    }
                    let pid_path = "/run/ghl-gamemode.pid";
                    if let Ok(p) = fs::read_to_string(pid_path) {
                        println!("daemon pid       : {}", p.trim());
                    }
                }
                ["daemon"] => {
                    println!("ghl gamemode daemon starting (poll every 3s) ...");
                    run_gamemode_daemon();
                }
                ["add", "anyexe"] => {
                    let rule = GameRule { enabled: true, kind: GameRuleKind::AnyExe, value: String::new() };
                    if !cfg.game_rules.iter().any(|r| matches!(r.kind, GameRuleKind::AnyExe)) {
                        cfg.game_rules.push(rule);
                        cfg.save()?;
                        println!("added rule: any .exe / Wine game");
                    } else {
                        println!("rule already exists");
                    }
                }
                ["add", exe] => {
                    let rule = GameRule {
                        enabled: true,
                        kind: GameRuleKind::Exe(exe.to_string()),
                        value: exe.to_string(),
                    };
                    if !cfg.game_rules.iter().any(|r| r.value.eq_ignore_ascii_case(exe)) {
                        cfg.game_rules.push(rule);
                        cfg.save()?;
                        println!("added rule: exe:{exe}");
                    } else {
                        println!("rule already exists");
                    }
                }
                ["toggle", pat] => {
                    let mut count = 0;
                    for r in &mut cfg.game_rules {
                        if r.value.eq_ignore_ascii_case(pat)
                            || r.to_conf().eq_ignore_ascii_case(pat)
                            || (pat.eq_ignore_ascii_case("anyexe") && matches!(r.kind, GameRuleKind::AnyExe))
                        {
                            r.enabled = !r.enabled;
                            count += 1;
                            println!("rule {}: {}", if r.enabled { "enabled" } else { "disabled" }, r.label());
                        }
                    }
                    if count > 0 {
                        cfg.save()?;
                    } else {
                        println!("no matching rule found for '{pat}'");
                    }
                }
                ["enable", pat] => {
                    let mut count = 0;
                    for r in &mut cfg.game_rules {
                        if r.value.eq_ignore_ascii_case(pat)
                            || r.to_conf().eq_ignore_ascii_case(pat)
                            || (pat.eq_ignore_ascii_case("anyexe") && matches!(r.kind, GameRuleKind::AnyExe))
                        {
                            r.enabled = true;
                            count += 1;
                            println!("rule enabled: {}", r.label());
                        }
                    }
                    if count > 0 {
                        cfg.save()?;
                    } else {
                        println!("no matching rule found for '{pat}'");
                    }
                }
                ["disable", pat] => {
                    let mut count = 0;
                    for r in &mut cfg.game_rules {
                        if r.value.eq_ignore_ascii_case(pat)
                            || r.to_conf().eq_ignore_ascii_case(pat)
                            || (pat.eq_ignore_ascii_case("anyexe") && matches!(r.kind, GameRuleKind::AnyExe))
                        {
                            r.enabled = false;
                            count += 1;
                            println!("rule disabled: {}", r.label());
                        }
                    }
                    if count > 0 {
                        cfg.save()?;
                    } else {
                        println!("no matching rule found for '{pat}'");
                    }
                }
                ["running"] => {
                    let running = running_game_candidates();
                    if running.is_empty() {
                        println!("no active game or .exe processes detected in /proc");
                    } else {
                        println!("detected running game / .exe candidate(s):");
                        for (i, p) in running.iter().enumerate() {
                            let already = cfg.game_rules.iter().any(|r| r.value.eq_ignore_ascii_case(p));
                            let mark = if already { " (already in rules)" } else { "" };
                            println!("  {i}: {p}{mark}");
                        }
                    }
                }
                ["remove", pat] => {
                    let before = cfg.game_rules.len();
                    cfg.game_rules.retain(|r| {
                        !r.to_conf().eq_ignore_ascii_case(pat)
                            && !r.label().eq_ignore_ascii_case(pat)
                            && !r.value.eq_ignore_ascii_case(pat)
                    });
                    let removed = before - cfg.game_rules.len();
                    cfg.save()?;
                    println!("removed {removed} rule(s)");
                }
                ["list"] => {
                    if cfg.game_rules.is_empty() {
                        println!("no game rules configured");
                    } else {
                        for (i, r) in cfg.game_rules.iter().enumerate() {
                            println!("{i}: {} (conf: {})", r.label(), r.to_conf());
                        }
                    }
                }
                ["mode", m] => {
                    let mode = Mode::parse(m).ok_or("game-mode mode: silent|balanced|turbo")?;
                    cfg.game_mode = Some(mode);
                    cfg.save()?;
                    println!("game mode target set to: {}", mode.name());
                }
                ["service" | "install-service"] => {
                    let svc_content = "[Unit]\nDescription=ghl game mode — auto performance switching on game launch\nAfter=multi-user.target\n\n[Service]\nType=simple\nExecStart=/usr/local/bin/ghl gamemode daemon\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=multi-user.target\n";
                    let path = Path::new("/etc/systemd/system/ghl-gamemode.service");
                    wr(path, svc_content)?;
                    let _ = run_quiet("systemctl", &["daemon-reload"]);
                    let _ = run_quiet("systemctl", &["enable", "--now", "ghl-gamemode.service"]);
                    println!("installed & started systemd service: ghl-gamemode.service");
                }
                ["clear"] => {
                    cfg.game_rules.clear();
                    cfg.game_mode = None;
                    cfg.save()?;
                    println!("all game rules cleared");
                }
                _ => {
                    eprintln!(
                        "ghl gamemode status\n\
                         ghl gamemode daemon\n\
                         ghl gamemode add anyexe|<ExeName.exe>\n\
                         ghl gamemode toggle|enable|disable <pattern>\n\
                         ghl gamemode running\n\
                         ghl gamemode remove <pattern>\n\
                         ghl gamemode list\n\
                         ghl gamemode mode silent|balanced|turbo\n\
                         ghl gamemode service\n\
                         ghl gamemode clear"
                    );
                    return Err("bad gamemode arguments".into());
                }
            }
        }
        _ => {
            usage();
            return Err("bad arguments".into());
        }
    }
    Ok(())
}

// ───────────────────────── TUI ─────────────────────────

const MAIN_ROWS: usize = 13;

fn line(out: &mut impl Write, y: &mut u16, s: impl fmt::Display, sel: bool) -> io::Result<()> {
    queue!(out, MoveTo(0, *y))?;
    if sel {
        queue!(out, SetAttribute(Attribute::Reverse))?;
    }
    queue!(
        out,
        Print(s),
        SetAttribute(Attribute::Reset),
        Clear(ClearType::UntilNewLine)
    )?;
    *y += 1;
    Ok(())
}

enum Screen {
    Main,
    Performance,
    Rgb,
    Ryzen,
    GameMode,
}

struct App {
    cfg: Cfg,
    screen: Screen,
    row: usize,
    gpu_sel: usize,
    rgb_ch: usize,
    rgb_edit: Rgb,
    hex_buf: String,
    ryzen_row: usize,
    ryzen_edit: RyzenCfg,
    fans: Vec<(u8, &'static str)>,
    fan_i: usize,
    pt: usize,
    curve: Curve,
    msg: String,
    gamemode_sel: usize,
    // Cached hardware snapshot
    gpu_cur: Option<Gpu>,
    mode_cur: Option<Mode>,
    od_cur: Option<bool>,
    ab_cur: Option<bool>,
    boost_cur: Option<bool>,
    igpu_alive: bool,
    npu_alive: bool,
    kbd_bright_cur: Option<u8>,
    kbd_color_cur: Option<Rgb>,
    webcam_cur: Option<bool>,
    touchpad_cur: Option<bool>,
    rpm_cur: Vec<u32>,
    cpu_stat: DevMetrics,
    gpu_stat: DevMetrics,
}

impl App {
    fn new(cfg: Cfg) -> App {
        prime_rapl();
        let fans = fan_list();
        let rgb_init = kbd_color_get(&cfg).unwrap_or_default();
        let ryzen_init = cfg.ryzen;

        let mut a = App {
            cfg,
            screen: Screen::Main,
            row: 0,
            gpu_sel: 1,
            rgb_ch: 0,
            rgb_edit: rgb_init,
            hex_buf: String::new(),
            ryzen_row: 0,
            ryzen_edit: ryzen_init,
            fans,
            fan_i: 0,
            pt: 0,
            curve: PRESETS[1].1,
            msg: String::new(),
            gamemode_sel: 0,
            gpu_cur: None,
            mode_cur: None,
            od_cur: None,
            ab_cur: None,
            boost_cur: None,
            igpu_alive: true,
            npu_alive: true,
            kbd_bright_cur: None,
            kbd_color_cur: None,
            webcam_cur: None,
            touchpad_cur: None,
            rpm_cur: Vec::new(),
            cpu_stat: DevMetrics::default(),
            gpu_stat: DevMetrics::default(),
        };
        a.refresh();
        if let Some(g) = a.gpu_cur {
            a.gpu_sel = GPUS.iter().position(|x| *x == g).unwrap_or(1);
        }
        if let Some(c) = a.kbd_color_cur {
            a.rgb_edit = c;
        }
        a.load_curve();
        a
    }

    fn refresh_state(&mut self) {
        self.gpu_cur = gpu_current();
        self.mode_cur = mode_current();
        self.od_cur = panel_od_get(&self.cfg);
        self.ab_cur = ab_get();
        self.boost_cur = cpu_boost_get();
        self.igpu_alive = igpu_present();
        self.npu_alive = npu_present();
        self.kbd_bright_cur = kbd_bright_get().or(self.cfg.kbd_bright);
        self.kbd_color_cur = kbd_color_get(&self.cfg);
        self.webcam_cur = webcam_get(&self.cfg);
        self.touchpad_cur = touchpad_get(&self.cfg);
    }

    fn refresh(&mut self) {
        self.refresh_state();
        self.rpm_cur = fan_rpm();
        self.gpu_stat = gpu_metrics();
        self.cpu_stat = cpu_metrics();
    }

    fn load_curve(&mut self) {
        let Some((n, _)) = self.fans.get(self.fan_i).copied() else {
            return;
        };
        let m = self.mode_cur.unwrap_or(Mode::Balanced);
        if let Some(c) = self
            .cfg
            .curves
            .get(&(m, n))
            .copied()
            .or_else(|| fan_read(n))
        {
            self.curve = c;
        }
    }

    fn report(&mut self, r: Result<String, String>) {
        self.msg = match r {
            Ok(s) => s,
            Err(e) => format!("ERROR: {e}"),
        };
        self.refresh_state();
    }

    fn draw(&self, out: &mut impl Write) -> io::Result<()> {
        let mut y = 0u16;
        line(out, &mut y, " ghl — ASUS control", false)?;
        line(out, &mut y, "", false)?;
        match self.screen {
            Screen::Main => {
                let g = GPUS[self.gpu_sel];
                let pending = if Some(g) != self.gpu_cur {
                    "  (Enter to apply)"
                } else {
                    ""
                };
                line(
                    out,
                    &mut y,
                    format!(
                        " GPU                           ◂ {} ▸  now: {}{}",
                        g.label(),
                        self.gpu_cur.map_or("?", |x| x.name()),
                        pending
                    ),
                    self.row == 0,
                )?;

                let ms: Vec<String> = MODES
                    .iter()
                    .map(|m| {
                        if Some(*m) == self.mode_cur {
                            format!("[{}]", m.name())
                        } else {
                            format!(" {} ", m.name())
                        }
                    })
                    .collect();
                line(
                    out,
                    &mut y,
                    format!(" Mode                          {}", ms.join(" ")),
                    self.row == 1,
                )?;

                line(
                    out,
                    &mut y,
                    format!(
                        " RyzenAdj Power (CPU/iGPU/NPU) {} (Enter: configure)",
                        self.cfg.ryzen.summary()
                    ),
                    self.row == 2,
                )?;

                let kb_str: String = (0..=3u8)
                    .map(|lvl| {
                        let lbl = KBD_BRIGHT_LABELS[lvl as usize];
                        if self.kbd_bright_cur == Some(lvl) {
                            format!("[{lvl}:{lbl}]")
                        } else {
                            format!(" {lvl}:{lbl} ")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                line(
                    out,
                    &mut y,
                    format!(" Kbd Brightness                {kb_str}"),
                    self.row == 3,
                )?;

                let cur_c = self.kbd_color_cur.unwrap_or(self.rgb_edit);
                line(
                    out,
                    &mut y,
                    format!(
                        " Kbd Color (RGB Picker)        {} (Enter: open)",
                        cur_c.label()
                    ),
                    self.row == 4,
                )?;

                line(
                    out,
                    &mut y,
                    format!(
                        " Panel Overdrive               {}",
                        self.od_cur.map_or("unsupported", on_off_upper)
                    ),
                    self.row == 5,
                )?;

                line(
                    out,
                    &mut y,
                    format!(
                        " Webcam                        {}",
                        self.webcam_cur.map_or("unsupported", on_off_upper)
                    ),
                    self.row == 6,
                )?;

                line(
                    out,
                    &mut y,
                    format!(
                        " Touchpad (Wayland/sysfs)      {}",
                        self.touchpad_cur.map_or("unsupported", on_off_upper)
                    ),
                    self.row == 7,
                )?;

                line(
                    out,
                    &mut y,
                    format!(
                        " Auto-bright                   {}",
                        self.ab_cur.map_or("unsupported", on_off_upper)
                    ),
                    self.row == 8,
                )?;

                let fix_chk = if self.cfg.nv_power_fix { "[x]" } else { "[ ]" };
                line(
                    out,
                    &mut y,
                    format!(" NVIDIA POWER FIX (EXP)        {fix_chk}"),
                    self.row == 9,
                )?;

                let igpu_chk = if self.cfg.disable_igpu_dgpu {
                    "[x]"
                } else {
                    "[ ]"
                };
                let igpu_state = if !self.igpu_alive {
                    "removed"
                } else if self.gpu_cur == Some(Gpu::Dgpu) {
                    "present"
                } else {
                    "active (not in dGPU mode)"
                };
                line(
                    out,
                    &mut y,
                    format!(" Disable iGPU while in dGPU    {igpu_chk}  ({igpu_state})"),
                    self.row == 10,
                )?;

                let npu_chk = if self.cfg.disable_npu { "[x]" } else { "[ ]" };
                let npu_state = if self.npu_alive { "present" } else { "removed" };
                line(
                    out,
                    &mut y,
                    format!(" Disable NPU                   {npu_chk}  ({npu_state})"),
                    self.row == 11,
                )?;

                let gm_chk = if self.cfg.game_rules.is_empty() { "[ ]" } else { "[x]" };
                let gm_target = self.cfg.game_mode.unwrap_or(Mode::Turbo).name();
                let gm_rules = self.cfg.game_rules.len();
                line(
                    out,
                    &mut y,
                    format!(
                        " Game Mode (auto perf)         {gm_chk} → {gm_target}  ({gm_rules} rules, Enter: manage)"
                    ),
                    self.row == 12,
                )?;

                line(out, &mut y, "", false)?;
                line(out, &mut y, self.cpu_stat.fmt_line("CPU"), false)?;
                line(out, &mut y, self.gpu_stat.fmt_line("GPU"), false)?;
                line(
                    out,
                    &mut y,
                    format!(" Fans  rpm: {:?}", self.rpm_cur),
                    false,
                )?;
                line(out, &mut y, "", false)?;
                line(out, &mut y, format!(" {}", self.msg), false)?;
                line(out, &mut y, "", false)?;
                line(
                    out,
                    &mut y,
                    " ↑↓ select  ←→ change  Enter apply/open  r refresh  q quit",
                    false,
                )?;
            }
            Screen::Performance => {
                let m = self.mode_cur.unwrap_or(Mode::Balanced);
                let modes: Vec<String> = MODES.iter().map(|mode| {
                    if *mode == m { format!("[{}]", mode.name()) } else { format!(" {} ", mode.name()) }
                }).collect();
                let fan_name = self.fans.get(self.fan_i).map_or("unsupported", |f| f.1);
                line(out, &mut y, format!(" Performance mode: {}   (←→ change mode)", modes.join(" ")), self.pt == 0)?;
                let boost = cpu_boost_get().map_or("unsupported", on_off_upper);
                let policy = if self.cfg.cpu_boost_modes.is_empty() && self.cfg.cpu_boost.is_none() {
                    "automatic by mode".to_string()
                } else {
                    MODES
                        .iter()
                        .map(|mode| {
                            let b = self
                                .cfg
                                .cpu_boost_modes
                                .get(mode)
                                .copied()
                                .or(self.cfg.cpu_boost)
                                .unwrap_or_else(|| cpu_boost_default_for_mode(*mode));
                            format!("{}:{}", mode.name(), on_off(b))
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                };
                line(out, &mut y, format!(" CPU boost: {boost} ({policy})   [b] toggle / [a] auto"), self.pt == 1)?;
                line(out, &mut y, format!(" Fan curve: {fan_name}"), false)?;
                for (i, (t, p)) in self.curve.iter().enumerate() {
                    let bar = "█".repeat((*p / 5) as usize);
                    line(out, &mut y, format!("  {:>3}°C  {:>3}%  {}", t, p, bar), self.pt == i + 1)?;
                }
                line(out, &mut y, "", false)?;
                line(out, &mut y, format!(" {}", self.msg), false)?;
                line(out, &mut y, "", false)?;
                line(out, &mut y, " ↑↓ select  ←→ mode/boost/fan  b boost toggle  a automatic  [ ] temp  1/2/3 presets  Enter save+apply  d default  Tab fan  Esc back", false)?;
            }
            Screen::Rgb => {
                let c = self.rgb_edit;
                queue!(
                    out,
                    MoveTo(0, y),
                    Print(format!(
                        " Keyboard RGB Picker — #{:02X}{:02X}{:02X}  ",
                        c.0, c.1, c.2
                    )),
                    SetBackgroundColor(Color::Rgb {
                        r: c.0,
                        g: c.1,
                        b: c.2
                    }),
                    Print("      "),
                    ResetColor,
                    Clear(ClearType::UntilNewLine)
                )?;
                y += 1;
                line(out, &mut y, "", false)?;

                for (idx, name) in [(0, "R (Red)  "), (1, "G (Green)"), (2, "B (Blue) ")] {
                    let v = c.channel(idx);
                    let bar = "█".repeat((v as usize * 32) / 255);
                    line(
                        out,
                        &mut y,
                        format!("  {name}  {:>3}  {:<32}", v, bar),
                        self.rgb_ch == idx,
                    )?;
                }

                line(out, &mut y, "", false)?;
                let hex_prompt = if self.hex_buf.is_empty() {
                    "type 0-9/A-F to enter hex directly".to_string()
                } else {
                    format!("#{:_<6}", self.hex_buf)
                };
                line(out, &mut y, format!("  Hex input: {hex_prompt}"), false)?;
                line(out, &mut y, "", false)?;
                line(out, &mut y, format!(" {}", self.msg), false)?;
                line(out, &mut y, "", false)?;
                line(
                    out,
                    &mut y,
                    " ↑↓ channel  ←→ ±5  [ ] ±1  -/+ ±15  0-9/a-f hex  Enter apply+save  Esc back",
                    false,
                )?;
            }
            Screen::Ryzen => {
                line(
                    out,
                    &mut y,
                    " RyzenAdj Power & Clock Limits — CPU / iGPU / NPU / SoC",
                    false,
                )?;
                if let Some(smu) = read_smu_live() {
                    line(
                        out,
                        &mut y,
                        format!(
                            " Live SMU: STAPM {:.1}/{:.0}W  Slow {:.1}/{:.0}W  Fast {:.1}/{:.0}W  VDD {:.1}/{:.0}A  SoC {:.1}/{:.0}A  Tctl {:.0}/{:.0}°C",
                            smu.stapm_val,
                            smu.stapm_lim,
                            smu.slow_val,
                            smu.slow_lim,
                            smu.fast_val,
                            smu.fast_lim,
                            smu.vdd_tdc_val,
                            smu.vdd_tdc_lim,
                            smu.soc_tdc_val,
                            smu.soc_tdc_lim,
                            smu.tctl_val,
                            smu.tctl_lim
                        ),
                        false,
                    )?;
                } else {
                    line(out, &mut y, "", false)?;
                }
                for (idx, meta) in RYZEN_FIELDS.iter().enumerate() {
                    if idx == 0 {
                        line(out, &mut y, " ── CPU Power & Thermal ──", false)?;
                    } else if idx == 6 {
                        line(out, &mut y, " ── iGPU (Radeon GFX) Power & Clock ──", false)?;
                    } else if idx == 10 {
                        line(
                            out,
                            &mut y,
                            " ── NPU (XDNA CVIP) & SoC / Uncore Power ──",
                            false,
                        )?;
                    }
                    let val_str = if idx == 15 {
                        match self.ryzen_edit.power_saving {
                            Some(true) => "power-saving (--power-saving)".to_string(),
                            Some(false) => "max-perf (--max-performance)".to_string(),
                            None => "— (firmware default)".to_string(),
                        }
                    } else if let Some(v) = self.ryzen_edit.slot(idx) {
                        let span = (meta.max - meta.min).max(1);
                        let filled =
                            ((v.saturating_sub(meta.min) as usize) * 16 / (span as usize)).min(16);
                        let bar = "█".repeat(filled);
                        format!("{:>4} {:<3}  {:<16}", v, meta.unit, bar)
                    } else {
                        "— (default)".to_string()
                    };
                    line(
                        out,
                        &mut y,
                        format!("  {:<36} {}", meta.label, val_str),
                        self.ryzen_row == idx,
                    )?;
                }
                line(out, &mut y, "", false)?;
                line(out, &mut y, format!(" {}", self.msg), false)?;
                line(
                    out,
                    &mut y,
                    " ↑↓ select  ←→ adjust  [ ] fine  0/Bksp clear item  1:Eco 2:Cool 3:Bal 4:Max  d clear all  Enter apply+save  Esc back",
                    false,
                )?;
            }
            Screen::GameMode => {
                let gm_target = self.cfg.game_mode.unwrap_or(Mode::Turbo);
                let ms: Vec<String> = MODES
                    .iter()
                    .map(|m| {
                        if *m == gm_target {
                            format!("[{}]", m.name())
                        } else {
                            format!(" {} ", m.name())
                        }
                    })
                    .collect();
                line(
                    out,
                    &mut y,
                    format!(" Game Mode target:  {}  (← → to change)", ms.join(" ")),
                    false,
                )?;
                line(out, &mut y, "", false)?;
                if self.cfg.game_rules.is_empty() {
                    line(out, &mut y, " No rules configured. Press 'a' to add 'any .exe / Wine game'.", false)?;
                } else {
                    for (i, rule) in self.cfg.game_rules.iter().enumerate() {
                        line(
                            out,
                            &mut y,
                            format!("  {i}: {}", rule.label()),
                            self.gamemode_sel == i,
                        )?;
                    }
                }
                line(out, &mut y, "", false)?;
                line(out, &mut y, format!(" {}", self.msg), false)?;
                line(out, &mut y, "", false)?;
                line(
                    out,
                    &mut y,
                    " ↑↓ select  Space toggle on/off  ← → target mode  Del remove  a anyexe  r add running  Esc back",
                    false,
                )?;
            }
        }
        queue!(out, Clear(ClearType::FromCursorDown))?;
        out.flush()
    }

    fn key(&mut self, k: KeyCode) -> bool {
        match self.screen {
            Screen::Main => match k {
                KeyCode::Char('q') | KeyCode::Esc => return false,
                KeyCode::Char('r') => {
                    self.refresh();
                    self.msg = "metrics & status refreshed".into();
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.row = (self.row + MAIN_ROWS - 1) % MAIN_ROWS;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.row = (self.row + 1) % MAIN_ROWS;
                }
                KeyCode::Left
                | KeyCode::Right
                | KeyCode::Char('h')
                | KeyCode::Char('l')
                | KeyCode::Char(' ')
                | KeyCode::Enter => {
                    let dir: i32 = if matches!(k, KeyCode::Left | KeyCode::Char('h')) {
                        -1
                    } else {
                        1
                    };
                    match self.row {
                        0 => {
                            if matches!(k, KeyCode::Enter) {
                                let r = gpu_set(GPUS[self.gpu_sel], &mut self.cfg)
                                    .and_then(|msg| self.cfg.save().map(|_| msg));
                                self.report(r);
                            } else {
                                self.gpu_sel = (self.gpu_sel as i32 + dir).rem_euclid(3) as usize;
                            }
                        }
                        1 => {
                            if matches!(k, KeyCode::Enter | KeyCode::Char(' ')) {
                                self.pt = 0;
                                self.load_curve();
                                self.msg.clear();
                                self.screen = Screen::Performance;
                            } else {
                                let cur = self.mode_cur.map_or(1, |m| m as usize);
                                let mode = MODES[(cur as i32 + dir).rem_euclid(3) as usize];
                                let r = mode_set(mode, &mut self.cfg)
                                    .and_then(|msg| self.cfg.save().map(|_| msg));
                                self.report(r);
                            }
                        }
                        2 => {
                            self.ryzen_edit = self.cfg.ryzen;
                            self.msg.clear();
                            self.screen = Screen::Ryzen;
                        }
                        3 => {
                            let cur = self.kbd_bright_cur.unwrap_or(1) as i32;
                            let next = (cur + dir).rem_euclid(4) as u8;
                            self.cfg.kbd_bright = Some(next);
                            let r = kbd_bright_set(next).and_then(|msg| self.cfg.save().map(|_| msg));
                            self.report(r);
                        }
                        4 => {
                            if let Some(c) = self.kbd_color_cur { self.rgb_edit = c; }
                            self.hex_buf.clear();
                            self.msg.clear();
                            self.screen = Screen::Rgb;
                        }
                        5 => {
                            let next = !self.od_cur.unwrap_or(false);
                            self.cfg.panel_od = Some(next);
                            let r = panel_od_set(next).and_then(|msg| self.cfg.save().map(|_| msg));
                            self.report(r);
                        }
                        6 => {
                            let next = !self.webcam_cur.unwrap_or(true);
                            self.cfg.webcam = Some(next);
                            let r = webcam_set(next).and_then(|msg| self.cfg.save().map(|_| msg));
                            self.report(r);
                        }
                        7 => {
                            let next = !self.touchpad_cur.unwrap_or(true);
                            self.cfg.touchpad = Some(next);
                            let r = touchpad_set(next).and_then(|msg| self.cfg.save().map(|_| msg));
                            self.report(r);
                        }
                        8 => {
                            let on = !self.ab_cur.unwrap_or(false);
                            self.cfg.autobright = on;
                            let r = ab_set(on).and_then(|_| self.cfg.save())
                                .map(|_| format!("auto-brightness {}", on_off(on)));
                            self.report(r);
                        }
                        9 => {
                            self.cfg.nv_power_fix = !self.cfg.nv_power_fix;
                            let result = if self.cfg.nv_power_fix { nv_fix_apply() } else {
                                nv_fix_restore(); Ok("NVIDIA Power Fix disabled (defaults restored)".into())
                            };
                            self.report(result.and_then(|msg| self.cfg.save().map(|_| msg)));
                        }
                        10 => {
                            let next = !self.cfg.disable_igpu_dgpu;
                            self.cfg.disable_igpu_dgpu = next;
                            let r = igpu_in_dgpu_apply(next).and_then(|msg| {
                                if !next && self.cfg.disable_npu { let _ = npu_apply(true, false); }
                                self.cfg.save().map(|_| msg)
                            });
                            self.report(r);
                        }
                        11 => {
                            let next = !self.cfg.disable_npu;
                            self.cfg.disable_npu = next;
                            let r = npu_apply(next, self.cfg.disable_igpu_dgpu)
                                .and_then(|msg| self.cfg.save().map(|_| msg));
                            self.report(r);
                        }
                        12 => {
                            if matches!(k, KeyCode::Enter | KeyCode::Char(' ')) {
                                self.gamemode_sel = 0;
                                self.msg.clear();
                                self.screen = Screen::GameMode;
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            },
            Screen::Performance => {
                match k {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.msg.clear();
                        self.screen = Screen::Main;
                    }
                    KeyCode::Up | KeyCode::Char('k') => self.pt = (self.pt + 9) % 10,
                    KeyCode::Down | KeyCode::Char('j') => self.pt = (self.pt + 1) % 10,
                    KeyCode::Left | KeyCode::Char('h') | KeyCode::Right | KeyCode::Char('l') => {
                        let dir = if matches!(k, KeyCode::Left | KeyCode::Char('h')) { -1 } else { 1 };
                        if self.pt == 0 {
                            let cur = self.mode_cur.unwrap_or(Mode::Balanced) as i32;
                            let next = MODES[(cur + dir).rem_euclid(MODES.len() as i32) as usize];
                            let r = mode_set(next, &mut self.cfg).and_then(|msg| self.cfg.save().map(|_| msg));
                            self.report(r);
                            self.load_curve();
                        } else if self.pt == 1 {
                            let m = self.mode_cur.unwrap_or(Mode::Balanced);
                            let next = dir > 0;
                            self.cfg.cpu_boost_modes.insert(m, next);
                            let r = cpu_boost_set(next).and_then(|_| self.cfg.save())
                                .map(|_| format!("CPU boost {} ({} mode)", on_off(next), m.name()));
                            self.report(r);
                        } else {
                            let i = self.pt - 2;
                            self.curve[i].1 = if dir > 0 { (self.curve[i].1 + 5).min(100) } else { self.curve[i].1.saturating_sub(5) };
                        }
                    }
                    KeyCode::Char('b') | KeyCode::Char(' ') | KeyCode::Enter if self.pt == 1 => {
                        let m = self.mode_cur.unwrap_or(Mode::Balanced);
                        let next = !cpu_boost_get().unwrap_or(true);
                        self.cfg.cpu_boost_modes.insert(m, next);
                        let r = cpu_boost_set(next).and_then(|_| self.cfg.save()).map(|_| format!("CPU boost {} ({} mode)", on_off(next), m.name()));
                        self.report(r);
                    }
                    KeyCode::Char('a') => {
                        let m = self.mode_cur.unwrap_or(Mode::Balanced);
                        self.cfg.cpu_boost_modes.remove(&m);
                        self.cfg.cpu_boost = None;
                        let r = cpu_boost_apply_for_mode(m, &self.cfg)
                            .and_then(|enabled| self.cfg.save().map(|_| format!("CPU boost {} (automatic mode policy)", on_off(enabled))));
                        self.report(r);
                    }
                    KeyCode::Char(']') if self.pt > 1 => {
                        let i = self.pt - 2;
                        self.curve[i].0 = (self.curve[i].0 + 2).min(100);
                    }
                    KeyCode::Char('[') if self.pt > 1 => {
                        let i = self.pt - 2;
                        self.curve[i].0 = self.curve[i].0.saturating_sub(2);
                    }
                    KeyCode::Char(c @ '1'..='3') if self.pt > 1 => self.curve = PRESETS[c as usize - '1' as usize].1,
                    KeyCode::Tab if self.fans.len() > 1 => {
                        self.fan_i = (self.fan_i + 1) % self.fans.len();
                        self.load_curve();
                    }
                    KeyCode::Enter if self.pt > 1 => {
                        if let Some((n, _)) = self.fans.get(self.fan_i).copied() {
                            let m = self.mode_cur.unwrap_or(Mode::Balanced);
                            let r = fan_write(n, &self.curve).and_then(|_| {
                                self.cfg.curves.insert((m, n), self.curve);
                                self.cfg.save()
                            });
                            self.report(r.map(|_| "curve applied & saved".into()));
                        } else {
                            self.msg = "no custom fan curve interface".into();
                        }
                    }
                    KeyCode::Char('d') if self.pt > 1 => {
                        if let Some((n, _)) = self.fans.get(self.fan_i).copied() {
                            let m = self.mode_cur.unwrap_or(Mode::Balanced);
                            self.cfg.curves.remove(&(m, n));
                            let r = fan_default(n).and_then(|_| self.cfg.save());
                            self.report(r.map(|_| "firmware default curve restored".into()));
                            if let Some(c) = fan_read(n) { self.curve = c; }
                        }
                    }
                    _ => {}
                }
            }
            Screen::Rgb => match k {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.hex_buf.clear();
                    self.msg.clear();
                    self.screen = Screen::Main;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.rgb_ch = (self.rgb_ch + 2) % 3;
                }
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                    self.rgb_ch = (self.rgb_ch + 1) % 3;
                }
                KeyCode::Left => {
                    let v = self.rgb_edit.channel(self.rgb_ch).saturating_sub(5);
                    self.rgb_edit.set_channel(self.rgb_ch, v);
                }
                KeyCode::Right => {
                    let v = self.rgb_edit.channel(self.rgb_ch).saturating_add(5);
                    self.rgb_edit.set_channel(self.rgb_ch, v);
                }
                KeyCode::Char('[') => {
                    let v = self.rgb_edit.channel(self.rgb_ch).saturating_sub(1);
                    self.rgb_edit.set_channel(self.rgb_ch, v);
                }
                KeyCode::Char(']') => {
                    let v = self.rgb_edit.channel(self.rgb_ch).saturating_add(1);
                    self.rgb_edit.set_channel(self.rgb_ch, v);
                }
                KeyCode::Char('-') => {
                    let v = self.rgb_edit.channel(self.rgb_ch).saturating_sub(15);
                    self.rgb_edit.set_channel(self.rgb_ch, v);
                }
                KeyCode::Char('+' | '=') => {
                    let v = self.rgb_edit.channel(self.rgb_ch).saturating_add(15);
                    self.rgb_edit.set_channel(self.rgb_ch, v);
                }
                KeyCode::Backspace => {
                    self.hex_buf.pop();
                }
                KeyCode::Char(c) if c.is_ascii_hexdigit() => {
                    self.hex_buf.push(c.to_ascii_uppercase());
                    if self.hex_buf.len() == 6 {
                        if let Some(parsed) = Rgb::parse(&self.hex_buf) {
                            self.rgb_edit = parsed;
                        }
                        self.hex_buf.clear();
                    }
                }
                KeyCode::Enter => {
                    let rgb = self.rgb_edit;
                    self.cfg.kbd_color = Some(rgb);
                    let r = kbd_color_set(rgb).and_then(|s| self.cfg.save().map(|_| s));
                    self.report(r);
                }
                _ => {}
            },
            Screen::Ryzen => match k {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.msg.clear();
                    self.screen = Screen::Main;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.ryzen_row = (self.ryzen_row + RYZEN_FIELDS.len() - 1) % RYZEN_FIELDS.len();
                }
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                    self.ryzen_row = (self.ryzen_row + 1) % RYZEN_FIELDS.len();
                }
                KeyCode::Left | KeyCode::Char('h') => {
                    self.ryzen_edit.step_field(self.ryzen_row, -1, false);
                }
                KeyCode::Right | KeyCode::Char('l') => {
                    self.ryzen_edit.step_field(self.ryzen_row, 1, false);
                }
                KeyCode::Char('[') => {
                    self.ryzen_edit.step_field(self.ryzen_row, -1, true);
                }
                KeyCode::Char(']') => {
                    self.ryzen_edit.step_field(self.ryzen_row, 1, true);
                }
                KeyCode::Backspace | KeyCode::Delete | KeyCode::Char('0') => {
                    self.ryzen_edit.clear_field(self.ryzen_row);
                }
                KeyCode::Char(c @ '1'..='4') => {
                    let (name, preset) = RYZEN_PRESETS[c as usize - '1' as usize];
                    self.ryzen_edit = preset;
                    self.msg = format!("loaded '{name}' preset (press Enter to apply & save)");
                }
                KeyCode::Char('d' | 'c') => {
                    self.ryzen_edit = RyzenCfg::default();
                    self.cfg.ryzen = RyzenCfg::default();
                    let _ = self.cfg.save();
                    if let Some(m) = self.cfg.mode.or_else(mode_current) {
                        let _ = mode_set(m, &mut self.cfg);
                    }
                    self.report(Ok("cleared all ryzenadj overrides & restored mode defaults".into()));
                }
                KeyCode::Enter => {
                    self.cfg.ryzen = self.ryzen_edit;
                    let r = if self.cfg.ryzen.is_active() {
                        ryzen_apply(&self.cfg.ryzen).and_then(|msg| self.cfg.save().map(|_| msg))
                    } else {
                        self.cfg
                            .save()
                            .map(|_| "ryzenadj overrides cleared & saved".into())
                    };
                    self.report(r);
                }
                _ => {}
            },
            Screen::GameMode => match k {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.msg.clear();
                    self.screen = Screen::Main;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if !self.cfg.game_rules.is_empty() {
                        let len = self.cfg.game_rules.len();
                        self.gamemode_sel = (self.gamemode_sel + len - 1) % len;
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if !self.cfg.game_rules.is_empty() {
                        let len = self.cfg.game_rules.len();
                        self.gamemode_sel = (self.gamemode_sel + 1) % len;
                    }
                }
                KeyCode::Left | KeyCode::Char('h') => {
                    let cur = self.cfg.game_mode.unwrap_or(Mode::Turbo) as usize;
                    let m = MODES[(cur + 2) % 3];
                    self.cfg.game_mode = Some(m);
                    let r = self.cfg.save().map(|_| format!("game mode target: {}", m.name()));
                    self.report(r);
                }
                KeyCode::Right | KeyCode::Char('l') => {
                    let cur = self.cfg.game_mode.unwrap_or(Mode::Turbo) as usize;
                    let m = MODES[(cur + 1) % 3];
                    self.cfg.game_mode = Some(m);
                    let r = self.cfg.save().map(|_| format!("game mode target: {}", m.name()));
                    self.report(r);
                }
                KeyCode::Delete | KeyCode::Backspace => {
                    if !self.cfg.game_rules.is_empty() {
                        let idx = self.gamemode_sel;
                        let removed = self.cfg.game_rules.remove(idx);
                        if self.gamemode_sel >= self.cfg.game_rules.len() && !self.cfg.game_rules.is_empty() {
                            self.gamemode_sel = self.cfg.game_rules.len() - 1;
                        }
                        let r = self.cfg.save().map(|_| format!("removed: {}", removed.label()));
                        self.report(r);
                    }
                }
                KeyCode::Char(' ') => {
                    if !self.cfg.game_rules.is_empty() {
                        let idx = self.gamemode_sel;
                        self.cfg.game_rules[idx].enabled = !self.cfg.game_rules[idx].enabled;
                        let state = if self.cfg.game_rules[idx].enabled { "enabled" } else { "disabled" };
                        let r = self.cfg.save().map(|_| format!("rule {state}: {}", self.cfg.game_rules[idx].label()));
                        self.report(r);
                    }
                }
                KeyCode::Char('a') => {
                    let rule = GameRule { enabled: true, kind: GameRuleKind::AnyExe, value: String::new() };
                    if !self.cfg.game_rules.iter().any(|r| matches!(r.kind, GameRuleKind::AnyExe)) {
                        self.cfg.game_rules.push(rule);
                        let r = self.cfg.save().map(|_| "added: any .exe / Wine game".into());
                        self.report(r);
                    } else {
                        self.msg = "anyexe rule already exists".into();
                    }
                }
                KeyCode::Char('r') => {
                    let candidates = running_game_candidates();
                    let mut added = 0;
                    for cand in candidates {
                        let rule = GameRule {
                            enabled: true,
                            kind: GameRuleKind::Exe(cand.clone()),
                            value: cand,
                        };
                        if !self.cfg.game_rules.iter().any(|r| r.value.eq_ignore_ascii_case(&rule.value)) {
                            self.cfg.game_rules.push(rule);
                            added += 1;
                        }
                    }
                    if added > 0 {
                        let r = self.cfg.save().map(|_| format!("added {added} running game(s)"));
                        self.report(r);
                    } else {
                        self.msg = "no new running game processes detected in /proc".into();
                    }
                }
                _ => {}
            },
        }
        true
    }
}

struct TermGuard;

impl TermGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, Hide)?;
        Ok(TermGuard)
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

fn tui(cfg: Cfg) -> io::Result<()> {
    let _guard = TermGuard::enter()?;
    let mut out = BufWriter::new(io::stdout());
    let mut app = App::new(cfg);
    loop {
        app.draw(&mut out)?;
        if let Event::Key(k) = event::read()? {
            if k.kind == KeyEventKind::Press && !app.key(k.code) {
                return Ok(());
            }
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let read_only = matches!(
        args.first().map(String::as_str),
        Some("status" | "help" | "--help" | "-h")
    ) || (matches!(args.first().map(String::as_str), Some("ryzen" | "ryzenadj"))
        && matches!(args.get(1).map(String::as_str), None | Some("status")))
    || (matches!(
            args.first().map(String::as_str),
            Some("gamemode" | "game-mode")
        ) && matches!(args.get(1).map(String::as_str), Some("status" | "list" | "running")));
    let euid = fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(1);
    if euid != 0 && !read_only {
        let exe = env::current_exe().unwrap_or_else(|_| PathBuf::from("ghl"));
        let e = Command::new("sudo").arg(exe).args(&args).exec();
        eprintln!("need root (sudo failed: {e})");
        std::process::exit(1);
    }
    if euid == 0 {
        if let Ok(exe) = env::current_exe() {
            let sys_bin = Path::new("/usr/local/bin/ghl");
            if exe != sys_bin && Path::new("/usr/local/bin").exists() {
                let _ = fs::copy(&exe, sys_bin);
            }
            if let Some((ref user, _)) = session_user_env() {
                let user_bin = PathBuf::from(format!("/home/{user}/.local/bin/ghl"));
                if user_bin.exists() && exe != user_bin {
                    let _ = fs::copy(&exe, &user_bin);
                }
            }
        }
    }
    let mut cfg = Cfg::load();
    if args.is_empty() {
        if let Err(e) = tui(cfg) {
            eprintln!("{e}");
            std::process::exit(1);
        }
    } else if let Err(e) = run_cli(&args, &mut cfg) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
