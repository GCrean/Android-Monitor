//! Thin wrapper around the `adb` CLI plus defensive parsers for /proc files.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[derive(Clone, Debug, PartialEq)]
pub struct Device {
    pub serial: String,
    /// "device", "unauthorized", "offline", ...
    pub state: String,
    pub model: String,
}

impl Device {
    pub fn ready(&self) -> bool {
        self.state == "device"
    }
}

/// Run `adb <args>` with a hard timeout. Returns stdout as lossy UTF-8.
pub fn run(args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut cmd = Command::new("adb");
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot launch adb ({e}) - is it on PATH?"))?;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();

    let (tx, rx) = mpsc::channel();
    let tx2 = tx.clone();
    std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = stdout.read_to_end(&mut b);
        let _ = tx.send((true, b));
    });
    std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = stderr.read_to_end(&mut b);
        let _ = tx2.send((false, b));
    });

    let (mut out, mut err) = (None, None);
    let deadline = std::time::Instant::now() + timeout;
    while out.is_none() || err.is_none() {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok((true, b)) => out = Some(b),
            Ok((false, b)) => err = Some(b),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("adb timed out".into());
            }
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let out = String::from_utf8_lossy(&out.unwrap()).into_owned();
    if !status.success() {
        let err = String::from_utf8_lossy(&err.unwrap()).trim().to_string();
        let detail = if !err.is_empty() { err } else { out.trim().to_string() };
        return Err(if detail.is_empty() { format!("adb exited with {status}") } else { detail });
    }
    Ok(out)
}

pub fn shell(serial: &str, script: &str, timeout: Duration) -> Result<String, String> {
    run(&["-s", serial, "shell", script], timeout)
}

pub fn list_devices() -> Result<Vec<Device>, String> {
    let out = run(&["devices", "-l"], Duration::from_secs(4))?;
    Ok(parse_devices(&out))
}

pub fn parse_devices(out: &str) -> Vec<Device> {
    out.lines()
        .skip_while(|l| !l.starts_with("List of devices"))
        .skip(1)
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let serial = it.next()?.to_string();
            let state = it.next()?.to_string();
            let model = it
                .find_map(|t| t.strip_prefix("model:"))
                .map(|m| m.replace('_', " "))
                .unwrap_or_default();
            Some(Device { serial, state, model })
        })
        .collect()
}

// ---------------------------------------------------------------- parsing --

#[derive(Clone, Debug, Default)]
pub struct CpuTimes {
    pub total: u64,
    pub idle: u64,
}

#[derive(Clone, Debug, Default)]
pub struct MemInfo {
    pub total_kb: u64,
    pub free_kb: u64,
    pub available_kb: u64,
    pub buffers_kb: u64,
    pub cached_kb: u64,
    pub sreclaimable_kb: u64,
    pub swap_total_kb: u64,
    pub swap_free_kb: u64,
}

#[derive(Clone, Debug, Default)]
pub struct NetCounters {
    pub rx: u64,
    pub tx: u64,
    /// Interfaces that carried traffic or are non-trivial, e.g. "wlan0".
    pub ifaces: Vec<String>,
}

/// Aggregate + per-core counters from `/proc/stat`.
pub fn parse_stat(s: &str) -> Option<(CpuTimes, Vec<CpuTimes>)> {
    let mut total = None;
    let mut cores = Vec::new();
    for line in s.lines() {
        let mut it = line.split_whitespace();
        let Some(name) = it.next() else { continue };
        if !name.starts_with("cpu") {
            continue;
        }
        let v: Vec<u64> = it.filter_map(|x| x.parse().ok()).collect();
        if v.len() < 4 {
            continue;
        }
        // user nice system idle iowait irq softirq steal
        let sum: u64 = v.iter().take(8).sum();
        let idle = v[3] + v.get(4).copied().unwrap_or(0);
        let t = CpuTimes { total: sum, idle };
        if name == "cpu" {
            total = Some(t);
        } else {
            cores.push(t);
        }
    }
    total.map(|t| (t, cores))
}

pub fn parse_meminfo(s: &str) -> Option<MemInfo> {
    let mut m = MemInfo::default();
    let mut have_total = false;
    let mut have_avail = false;
    for line in s.lines() {
        let Some((k, rest)) = line.split_once(':') else { continue };
        let v: u64 = rest
            .split_whitespace()
            .next()
            .and_then(|x| x.parse().ok())
            .unwrap_or(0);
        match k {
            "MemTotal" => {
                m.total_kb = v;
                have_total = true;
            }
            "MemFree" => m.free_kb = v,
            "MemAvailable" => {
                m.available_kb = v;
                have_avail = true;
            }
            "Buffers" => m.buffers_kb = v,
            "Cached" => m.cached_kb = v,
            "SReclaimable" => m.sreclaimable_kb = v,
            "SwapTotal" => m.swap_total_kb = v,
            "SwapFree" => m.swap_free_kb = v,
            _ => {}
        }
    }
    if !have_total || m.total_kb == 0 {
        return None;
    }
    if !have_avail {
        // Very old kernels: estimate.
        m.available_kb = m.free_kb + m.buffers_kb + m.cached_kb;
    }
    Some(m)
}

pub fn parse_net_dev(s: &str) -> Option<NetCounters> {
    let mut n = NetCounters::default();
    let mut any = false;
    for line in s.lines() {
        let Some((name, rest)) = line.split_once(':') else { continue };
        let name = name.trim();
        if name.is_empty() || name == "lo" || name.contains(' ') {
            continue;
        }
        let f: Vec<u64> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
        if f.len() < 9 {
            continue;
        }
        any = true;
        n.rx += f[0];
        n.tx += f[8];
        if f[0] + f[8] > 0 {
            n.ifaces.push(name.to_string());
        }
    }
    any.then_some(n)
}

/// `/proc/loadavg` -> (1m, 5m, 15m)
pub fn parse_loadavg(s: &str) -> Option<(f32, f32, f32)> {
    let mut it = s.split_whitespace().filter_map(|x| x.parse::<f32>().ok());
    Some((it.next()?, it.next()?, it.next()?))
}

pub fn parse_uptime(s: &str) -> Option<u64> {
    s.split_whitespace().next()?.parse::<f64>().ok().map(|x| x as u64)
}

#[derive(Clone, Debug, Default)]
pub struct Battery {
    pub level: Option<u32>,
    pub temp_c: Option<f32>,
    pub status: String,
    pub plugged: bool,
    pub voltage_mv: Option<u32>,
}

pub fn parse_battery(s: &str) -> Option<Battery> {
    let mut b = Battery::default();
    let mut any = false;
    for line in s.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim();
        match k.trim() {
            "level" => {
                b.level = v.parse().ok();
                any |= b.level.is_some();
            }
            "temperature" => b.temp_c = v.parse::<f32>().ok().map(|t| t / 10.0),
            "voltage" => b.voltage_mv = v.parse().ok(),
            "status" => {
                b.status = match v {
                    "2" => "Charging",
                    "3" => "Discharging",
                    "4" => "Not charging",
                    "5" => "Full",
                    _ => "Unknown",
                }
                .into()
            }
            "AC powered" | "USB powered" | "Wireless powered" | "Dock powered" => {
                b.plugged |= v == "true"
            }
            _ => {}
        }
    }
    any.then_some(b)
}

#[derive(Clone, Debug, Default)]
pub struct Storage {
    pub total_kb: u64,
    pub used_kb: u64,
    pub avail_kb: u64,
}

/// Output of `df -k /data` (toybox or busybox).
pub fn parse_df(s: &str) -> Option<Storage> {
    for line in s.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() >= 4 {
            if let (Ok(t), Ok(u), Ok(a)) = (f[1].parse(), f[2].parse(), f[3].parse()) {
                return Some(Storage { total_kb: t, used_kb: u, avail_kb: a });
            }
        }
    }
    None
}

/// First global IPv4 from `ip -o -4 addr` -> ("wlan0", "192.168.1.5")
pub fn parse_ip(s: &str) -> Option<(String, String)> {
    let all: Vec<(String, String)> = s.lines().filter_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        let iface = f.get(1)?;
        if *iface == "lo" {
            return None;
        }
        let i = f.iter().position(|x| *x == "inet")?;
        let addr = f.get(i + 1)?.split('/').next()?;
        Some((iface.to_string(), addr.to_string()))
    }).collect();
    // Prefer Wi-Fi / Ethernet over cellular (rmnet*) interfaces.
    all.iter()
        .find(|(i, _)| i.starts_with("wlan") || i.starts_with("eth"))
        .or_else(|| all.first())
        .cloned()
}

/// Split output on `@@NAME` marker lines.
pub fn sections(s: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let mut cur: Option<String> = None;
    for line in s.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(name) = line.strip_prefix("@@") {
            cur = Some(name.trim().to_string());
            map.entry(name.trim().to_string()).or_insert_with(String::new);
        } else if let Some(c) = &cur {
            let e = map.get_mut(c).unwrap();
            e.push_str(line);
            e.push('\n');
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devices_with_states_and_models() {
        let out = "List of devices attached\nABC123  device usb:1-2 product:p model:Pixel_6_Pro device:raven transport_id:1\nXYZ  unauthorized usb:1-3 transport_id:2\n\n";
        let d = parse_devices(out);
        assert_eq!(d.len(), 2);
        assert!(d[0].ready());
        assert_eq!(d[0].model, "Pixel 6 Pro");
        assert_eq!(d[1].state, "unauthorized");
        assert!(!d[1].ready());
    }

    #[test]
    fn stat_total_and_cores() {
        let s = "cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 50 0 25 400 25 0 0 0 0 0\ncpu1 50 0 25 400 25 0 0 0 0 0\nintr 1\n";
        let (t, cores) = parse_stat(s).unwrap();
        assert_eq!(t.total, 1000);
        assert_eq!(t.idle, 850);
        assert_eq!(cores.len(), 2);
        assert!(parse_stat("garbage").is_none());
    }

    #[test]
    fn meminfo_with_and_without_available() {
        let m = parse_meminfo("MemTotal: 1000 kB\nMemFree: 100 kB\nMemAvailable: 400 kB\nBuffers: 10 kB\nCached: 200 kB\n").unwrap();
        assert_eq!((m.total_kb, m.available_kb), (1000, 400));
        let old = parse_meminfo("MemTotal: 1000 kB\nMemFree: 100 kB\nBuffers: 10 kB\nCached: 200 kB\n").unwrap();
        assert_eq!(old.available_kb, 310);
        assert!(parse_meminfo("nothing here").is_none());
    }

    #[test]
    fn net_dev_skips_loopback_and_sums() {
        let s = "Inter-|   Receive\n face |bytes packets\n    lo: 999 1 0 0 0 0 0 0 999 1 0 0 0 0 0 0\n wlan0: 1000 5 0 0 0 0 0 0 200 3 0 0 0 0 0 0\n rmnet0: 50 1 0 0 0 0 0 0 25 1 0 0 0 0 0 0\n";
        let n = parse_net_dev(s).unwrap();
        assert_eq!((n.rx, n.tx), (1050, 225));
        assert_eq!(n.ifaces, vec!["wlan0", "rmnet0"]);
    }

    #[test]
    fn df_and_ip_and_battery() {
        let s = parse_df("Filesystem 1K-blocks Used Available Use% Mounted on\n/dev/block/dm-1 1000 600 400 60% /data\n").unwrap();
        assert_eq!((s.total_kb, s.used_kb, s.avail_kb), (1000, 600, 400));
        let ip = parse_ip("1: lo    inet 127.0.0.1/8 scope host lo\n15: rmnet1    inet 10.1.2.3/32 scope global rmnet1\n47: wlan0    inet 192.168.0.5/24 scope global wlan0\n").unwrap();
        assert_eq!(ip, ("wlan0".to_string(), "192.168.0.5".to_string()));
        let b = parse_battery("Current Battery Service state:\n  AC powered: true\n  status: 2\n  level: 85\n  temperature: 301\n").unwrap();
        assert_eq!(b.level, Some(85));
        assert_eq!(b.temp_c, Some(30.1));
        assert_eq!(b.status, "Charging");
        assert!(b.plugged);
    }

    #[test]
    fn sections_split_on_markers() {
        let m = sections("@@A\none\ntwo\r\n@@B\nthree\n");
        assert_eq!(m["A"], "one\ntwo\n");
        assert_eq!(m["B"], "three\n");
    }
}
