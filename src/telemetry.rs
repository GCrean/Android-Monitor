//! Background polling worker + the UI-side rolling state.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::channel::mpsc::UnboundedSender;

use crate::adb::{self, Battery, CpuTimes, Device, MemInfo, NetCounters, Storage};

/// Seconds of history kept (and shown) in the line charts.
pub const HISTORY: usize = 90;

const FAST_SCRIPT: &str = "echo @@STAT; cat /proc/stat; echo @@MEM; cat /proc/meminfo; \
echo @@NET; cat /proc/net/dev; echo @@LOAD; cat /proc/loadavg; echo @@UP; cat /proc/uptime";
const SLOW_SCRIPT: &str = "echo @@BAT; dumpsys battery; echo @@DF; df -k /data; \
echo @@IP; ip -o -4 addr show 2>/dev/null";
const INFO_SCRIPT: &str = "echo @@INFO; getprop ro.product.manufacturer; getprop ro.product.model; \
getprop ro.build.version.release; getprop ro.build.version.sdk";

#[derive(Clone, Debug, Default)]
pub struct DeviceInfo {
    pub manufacturer: String,
    pub model: String,
    pub android: String,
    pub sdk: String,
}

#[derive(Clone, Debug)]
pub struct Sample {
    pub cpu: CpuTimes,
    pub cores: Vec<CpuTimes>,
    pub mem: MemInfo,
    pub net: Option<NetCounters>,
    pub load: Option<(f32, f32, f32)>,
    pub uptime: Option<u64>,
    pub latency: Duration,
    pub warnings: Vec<String>,
    /// Raw text of each @@SECTION returned by the device.
    pub raw: HashMap<String, String>,
}

#[derive(Clone, Debug, Default)]
pub struct Slow {
    pub battery: Option<Battery>,
    pub storage: Option<Storage>,
    pub ip: Option<(String, String)>,
    pub raw: HashMap<String, String>,
}

#[derive(Clone)]
pub enum Msg {
    Devices(Vec<Device>, Option<String>),
    /// Selected device changed - drop history.
    Reset,
    Info(DeviceInfo),
    Sample(Sample),
    Slow(Slow),
    /// Polling failed for the selected device.
    Error(String),
}

/// Shared between UI (writes) and worker (reads).
#[derive(Default)]
pub struct Selection {
    pub serial: Option<String>,
    /// True when the user picked it explicitly (don't auto-switch).
    pub pinned: bool,
}

pub fn spawn_worker(tx: UnboundedSender<Msg>, sel: Arc<Mutex<Selection>>) {
    std::thread::Builder::new()
        .name("adb-poller".into())
        .spawn(move || worker(tx, sel))
        .expect("spawn poller");
}

fn worker(tx: UnboundedSender<Msg>, sel: Arc<Mutex<Selection>>) {
    let send = |m: Msg| tx.unbounded_send(m).is_ok();
    let mut tick: u64 = 0;
    let mut next = Instant::now();
    let mut current: Option<String> = None;
    let mut last_list: Vec<Device> = vec![];

    loop {
        // Fixed-rate ticker (no drift).
        next += Duration::from_secs(1);
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now; // fell behind; don't burst
        }
        tick += 1;

        // --- device discovery (every tick when idle, every 3rd when running)
        let want = sel.lock().unwrap().serial.clone();
        if current.is_none() || tick % 3 == 0 || (want.is_some() && want != current) {
            match adb::list_devices() {
                Ok(list) => {
                    let (pick, changed) = {
                        let mut s = sel.lock().unwrap();
                        let ok = |sn: &String| list.iter().any(|d| &d.serial == sn && d.ready());
                        if let Some(sn) = s.serial.clone() {
                            if !ok(&sn) && !s.pinned {
                                s.serial = None;
                            }
                        }
                        if s.serial.is_none() {
                            s.serial = list.iter().find(|d| d.ready()).map(|d| d.serial.clone());
                        }
                        let pick = s.serial.clone().filter(|sn| ok(sn));
                        (pick.clone(), pick != current)
                    };
                    if list != last_list || changed {
                        if !send(Msg::Devices(list.clone(), pick.clone())) {
                            return;
                        }
                        last_list = list;
                    }
                    if changed {
                        current = pick;
                        if !send(Msg::Reset) {
                            return;
                        }
                        if let Some(sn) = &current {
                            if let Ok(out) = adb::shell(sn, INFO_SCRIPT, Duration::from_secs(5)) {
                                let s = adb::sections(&out);
                                let l: Vec<&str> = s.get("INFO").map(|x| x.lines().collect()).unwrap_or_default();
                                let g = |i: usize| l.get(i).map(|x| x.trim().to_string()).unwrap_or_default();
                                send(Msg::Info(DeviceInfo {
                                    manufacturer: g(0),
                                    model: g(1),
                                    android: g(2),
                                    sdk: g(3),
                                }));
                            }
                        }
                        tick = 0;
                    }
                }
                Err(e) => {
                    if current.is_some() || !last_list.is_empty() {
                        current = None;
                        last_list.clear();
                        send(Msg::Devices(vec![], None));
                        send(Msg::Reset);
                    }
                    if !send(Msg::Error(e)) {
                        return;
                    }
                    continue;
                }
            }
        }

        let Some(serial) = current.clone() else { continue };

        // --- fast poll
        let t0 = Instant::now();
        match adb::shell(&serial, FAST_SCRIPT, Duration::from_millis(2500)) {
            Ok(out) => {
                let s = adb::sections(&out);
                let mut warnings = vec![];
                let stat = s.get("STAT").and_then(|x| adb::parse_stat(x));
                let mem = s.get("MEM").and_then(|x| adb::parse_meminfo(x));
                let net = s.get("NET").and_then(|x| adb::parse_net_dev(x));
                if stat.is_none() {
                    warnings.push("/proc/stat unreadable (CPU unavailable)".to_string());
                }
                if mem.is_none() {
                    warnings.push("/proc/meminfo unreadable (memory unavailable)".to_string());
                }
                if net.is_none() {
                    warnings.push("/proc/net/dev unreadable (network unavailable)".to_string());
                }
                let (cpu, cores) = stat.unwrap_or_default();
                let sample = Sample {
                    cpu,
                    cores,
                    mem: mem.unwrap_or_default(),
                    net,
                    load: s.get("LOAD").and_then(|x| adb::parse_loadavg(x)),
                    uptime: s.get("UP").and_then(|x| adb::parse_uptime(x)),
                    latency: t0.elapsed(),
                    warnings,
                    raw: s,
                };
                if !send(Msg::Sample(sample)) {
                    return;
                }
            }
            Err(e) => {
                if !send(Msg::Error(e)) {
                    return;
                }
                // Force a device re-scan next tick.
                current = None;
                continue;
            }
        }

        // --- slow poll (battery / storage / ip) every 5 s
        if tick % 5 == 1 {
            if let Ok(out) = adb::shell(&serial, SLOW_SCRIPT, Duration::from_secs(4)) {
                let s = adb::sections(&out);
                send(Msg::Slow(Slow {
                    battery: s.get("BAT").and_then(|x| adb::parse_battery(x)),
                    storage: s.get("DF").and_then(|x| adb::parse_df(x)),
                    ip: s.get("IP").and_then(|x| adb::parse_ip(x)),
                    raw: s,
                }));
            }
        }
    }
}

// ------------------------------------------------------------- UI state --

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    /// Nothing yet.
    Starting,
    NoDevice(String),
    Connected,
}

pub struct Telemetry {
    pub status: Status,
    pub devices: Vec<Device>,
    pub selected: Option<String>,
    pub info: DeviceInfo,
    pub slow: Slow,

    pub cpu_hist: VecDeque<f32>,
    pub mem_hist: VecDeque<f32>,
    pub rx_hist: VecDeque<f32>,
    pub tx_hist: VecDeque<f32>,
    pub cores: Vec<f32>,
    pub core_hist: Vec<VecDeque<f32>>,

    pub mem: MemInfo,
    pub load: Option<(f32, f32, f32)>,
    pub uptime: Option<u64>,
    pub latency: Duration,
    pub warnings: Vec<String>,
    pub ticks: u64,
    pub last_arrival: Instant,
    /// Wall-clock time of the newest sample (ms since epoch).
    pub last_unix_ms: u64,
    /// Newest raw sample (cumulative counters etc.).
    pub last: Option<Sample>,
    /// Latest raw device text per source name -> (text, captured unix ms).
    pub raw: BTreeMap<String, (String, u64)>,

    prev_cpu: Option<(CpuTimes, Vec<CpuTimes>)>,
    prev_net: Option<(NetCounters, Instant)>,
}

pub fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Map device section markers to public raw-source names.
fn raw_name(marker: &str) -> Option<&'static str> {
    Some(match marker {
        "STAT" => "stat",
        "MEM" => "meminfo",
        "NET" => "net_dev",
        "LOAD" => "loadavg",
        "UP" => "uptime",
        "BAT" => "battery",
        "DF" => "df",
        "IP" => "ip",
        _ => return None,
    })
}

/// State shared between the GUI, the poller and the MCP server.
#[derive(Clone)]
pub struct Hub {
    pub tel: Arc<Mutex<Telemetry>>,
    pub sel: Arc<Mutex<Selection>>,
}

impl Hub {
    pub fn new() -> Self {
        Self { tel: Arc::new(Mutex::new(Telemetry::new())), sel: Arc::new(Mutex::new(Selection::default())) }
    }
}

fn push(q: &mut VecDeque<f32>, v: f32) {
    if q.len() >= HISTORY + 2 {
        q.pop_front();
    }
    q.push_back(v);
}

fn busy_pct(prev: &CpuTimes, cur: &CpuTimes) -> f32 {
    let dt = cur.total.saturating_sub(prev.total) as f32;
    let di = cur.idle.saturating_sub(prev.idle) as f32;
    if dt <= 0.0 {
        0.0
    } else {
        ((dt - di) / dt * 100.0).clamp(0.0, 100.0)
    }
}

impl Telemetry {
    pub fn new() -> Self {
        Self {
            status: Status::Starting,
            devices: vec![],
            selected: None,
            info: DeviceInfo::default(),
            slow: Slow::default(),
            cpu_hist: VecDeque::new(),
            mem_hist: VecDeque::new(),
            rx_hist: VecDeque::new(),
            tx_hist: VecDeque::new(),
            cores: vec![],
            core_hist: vec![],
            mem: MemInfo::default(),
            load: None,
            uptime: None,
            latency: Duration::ZERO,
            warnings: vec![],
            ticks: 0,
            last_arrival: Instant::now(),
            last_unix_ms: 0,
            last: None,
            raw: BTreeMap::new(),
            prev_cpu: None,
            prev_net: None,
        }
    }

    fn clear(&mut self) {
        let devices = std::mem::take(&mut self.devices);
        let selected = self.selected.take();
        *self = Self::new();
        self.devices = devices;
        self.selected = selected;
    }

    pub fn apply(&mut self, msg: Msg) {
        match msg {
            Msg::Devices(list, sel) => {
                self.devices = list;
                self.selected = sel;
                if self.selected.is_none() {
                    let why = if self.devices.is_empty() {
                        "Waiting for an ADB device. Connect it over USB and enable USB debugging.".to_string()
                    } else if let Some(d) = self.devices.iter().find(|d| !d.ready()) {
                        format!("Device {} is '{}'. Unlock it and accept the USB debugging prompt.", d.serial, d.state)
                    } else {
                        "No usable device.".to_string()
                    };
                    self.status = Status::NoDevice(why);
                }
            }
            Msg::Reset => {
                self.clear();
                if self.selected.is_none() {
                    self.status = Status::NoDevice("Waiting for an ADB device. Connect it over USB and enable USB debugging.".into());
                }
            }
            Msg::Info(i) => self.info = i,
            Msg::Slow(s) => {
                let ms = unix_ms();
                for (k, v) in &s.raw {
                    if let Some(n) = raw_name(k) {
                        self.raw.insert(n.to_string(), (v.clone(), ms));
                    }
                }
                // keep last known good values when one source fails
                self.slow.battery = s.battery.or(self.slow.battery.take());
                self.slow.storage = s.storage.or(self.slow.storage.take());
                self.slow.ip = s.ip.or(self.slow.ip.take());
            }
            Msg::Error(e) => {
                self.status = Status::NoDevice(format!("ADB error: {e}"));
            }
            Msg::Sample(s) => {
                self.status = Status::Connected;
                let now = Instant::now();
                let ms = unix_ms();
                self.last_unix_ms = ms;
                for (k, v) in &s.raw {
                    if let Some(n) = raw_name(k) {
                        self.raw.insert(n.to_string(), (v.clone(), ms));
                    }
                }
                self.last = Some(s.clone());

                if let Some((pt, pc)) = &self.prev_cpu {
                    push(&mut self.cpu_hist, busy_pct(pt, &s.cpu));
                    self.cores = s
                        .cores
                        .iter()
                        .zip(pc.iter())
                        .map(|(c, p)| busy_pct(p, c))
                        .collect();
                    if self.core_hist.len() != self.cores.len() {
                        self.core_hist = vec![VecDeque::new(); self.cores.len()];
                    }
                    for (h, v) in self.core_hist.iter_mut().zip(&self.cores) {
                        push(h, *v);
                    }
                } else {
                    // first sample is only a baseline for deltas
                    self.cores = vec![0.0; s.cores.len()];
                }
                if s.cpu.total > 0 {
                    self.prev_cpu = Some((s.cpu.clone(), s.cores.clone()));
                }

                if let Some(n) = &s.net {
                    if let Some((p, t)) = &self.prev_net {
                        let dt = now.duration_since(*t).as_secs_f32().max(0.2);
                        let rx = n.rx.saturating_sub(p.rx) as f32 / dt;
                        let tx = n.tx.saturating_sub(p.tx) as f32 / dt;
                        push(&mut self.rx_hist, rx);
                        push(&mut self.tx_hist, tx);
                    }
                    self.prev_net = Some((n.clone(), now));
                }

                if s.mem.total_kb > 0 {
                    let used = s.mem.total_kb.saturating_sub(s.mem.available_kb);
                    push(&mut self.mem_hist, used as f32 / s.mem.total_kb as f32 * 100.0);
                }
                self.mem = s.mem;
                self.load = s.load;
                self.uptime = s.uptime;
                self.latency = s.latency;
                self.warnings = s.warnings;
                self.last_arrival = now;
                self.ticks += 1;
            }
        }
    }

    pub fn cpu_now(&self) -> f32 {
        self.cpu_hist.back().copied().unwrap_or(0.0)
    }
    pub fn rx_now(&self) -> f32 {
        self.rx_hist.back().copied().unwrap_or(0.0)
    }
    pub fn tx_now(&self) -> f32 {
        self.tx_hist.back().copied().unwrap_or(0.0)
    }
    pub fn mem_used_pct(&self) -> f32 {
        self.mem_hist.back().copied().unwrap_or(0.0)
    }
}
