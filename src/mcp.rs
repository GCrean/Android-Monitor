//! Minimal MCP (Model Context Protocol) server: JSON-RPC 2.0, newline-delimited,
//! over stdio or TCP. Exposes live snapshots, history and raw device data.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::MutexGuard;
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::adb;
use crate::telemetry::{unix_ms, Hub, Status, Telemetry, HISTORY};

const LATEST: &str = "2025-06-18";
const SUPPORTED: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
const RAW_SOURCES: [&str; 8] = ["stat", "meminfo", "net_dev", "loadavg", "uptime", "battery", "df", "ip"];

type RpcError = (i64, String);

/// When set, tools that change the device (install, uninstall, launch, permissions...) are refused.
pub static READ_ONLY: AtomicBool = AtomicBool::new(false);

fn tel(hub: &Hub) -> MutexGuard<'_, Telemetry> {
    hub.tel.lock().unwrap_or_else(|e| e.into_inner())
}

fn r2(x: f32) -> f64 {
    ((x as f64) * 100.0).round() / 100.0
}

// ------------------------------------------------------------- transports --

/// Blocking: serve one client on stdin/stdout until EOF. Nothing but protocol goes to stdout.
pub fn serve_stdio(hub: &Hub) {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(resp) = handle_message(hub, &line) {
            let mut out = stdout.lock();
            if writeln!(out, "{resp}").is_err() || out.flush().is_err() {
                break;
            }
        }
    }
}

/// Bind and serve TCP clients on background threads. Returns the bound address.
pub fn serve_tcp(hub: Hub, addr: &str) -> std::io::Result<SocketAddr> {
    let listener = TcpListener::bind(addr)?;
    let local = listener.local_addr()?;
    std::thread::Builder::new().name("mcp-tcp".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let hub = hub.clone();
            std::thread::spawn(move || serve_conn(hub, stream));
        }
    })?;
    Ok(local)
}

fn serve_conn(hub: Hub, stream: TcpStream) {
    let Ok(read_half) = stream.try_clone() else { return };
    let mut w = stream;
    for line in BufReader::new(read_half).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(resp) = handle_message(&hub, &line) {
            if writeln!(w, "{resp}").is_err() || w.flush().is_err() {
                break;
            }
        }
    }
}

// --------------------------------------------------------------- protocol --

/// Handle one line of input; returns the line to send back (None for notifications).
pub fn handle_message(hub: &Hub, line: &str) -> Option<String> {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return Some(error_obj(Value::Null, -32700, &format!("parse error: {e}")).to_string()),
    };
    match v {
        Value::Array(items) => {
            let out: Vec<Value> = items.iter().filter_map(|m| handle_value(hub, m)).collect();
            (!out.is_empty()).then(|| Value::Array(out).to_string())
        }
        other => handle_value(hub, &other).map(|v| v.to_string()),
    }
}

fn error_obj(id: Value, code: i64, msg: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg}})
}

fn handle_value(hub: &Hub, msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned();
    let Some(method) = msg.get("method").and_then(Value::as_str) else {
        if msg.get("result").is_some() || msg.get("error").is_some() {
            return None; // a response to something we never sent
        }
        return Some(error_obj(id.unwrap_or(Value::Null), -32600, "invalid request"));
    };
    let id = id?; // no id => notification, never answered
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    Some(match dispatch(hub, method, &params) {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, m)) => error_obj(id, code, &m),
    })
}

fn dispatch(hub: &Hub, method: &str, p: &Value) -> Result<Value, RpcError> {
    match method {
        "initialize" => {
            let asked = p.get("protocolVersion").and_then(Value::as_str).unwrap_or(LATEST);
            let version = if SUPPORTED.contains(&asked) { asked } else { LATEST };
            Ok(json!({
                "protocolVersion": version,
                "capabilities": {"tools": {"listChanged": false}, "resources": {"listChanged": false, "subscribe": false}},
                "serverInfo": {"name": "android-monitor", "version": env!("CARGO_PKG_VERSION")},
                "instructions": "Live telemetry for an Android device polled over adb once per second. \
                    get_snapshot = current computed stats; get_history = 1 Hz time series; \
                    get_raw = unparsed /proc and dumpsys text plus cumulative counters."
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tool_defs()})),
        "tools/call" => {
            let name = p.get("name").and_then(Value::as_str).ok_or((-32602, "missing tool name".to_string()))?;
            let args = p.get("arguments").cloned().unwrap_or_else(|| json!({}));
            call_tool(hub, name, &args)
        }
        "resources/list" => Ok(json!({"resources": resource_defs()})),
        "resources/templates/list" => Ok(json!({"resourceTemplates": []})),
        "resources/read" => {
            let uri = p.get("uri").and_then(Value::as_str).ok_or((-32602, "missing uri".to_string()))?;
            read_resource(hub, uri)
        }
        _ => Err((-32601, format!("method not found: {method}"))),
    }
}

// ------------------------------------------------------------------ tools --

fn tool_defs() -> Value {
    json!([
        {
            "name": "list_devices",
            "description": "List ADB devices seen by the monitor and which one is being polled.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "select_device",
            "description": "Choose which ADB device to poll (by serial), or 'auto' to follow the first ready device. History resets on change.",
            "inputSchema": {"type": "object", "properties": {"serial": {"type": "string"}}, "required": ["serial"]}
        },
        {
            "name": "get_snapshot",
            "description": "Current computed stats: CPU (total + per core), memory breakdown, network rates, battery, storage, load, uptime, device info and polling status.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "get_history",
            "description": "Rolling 1 Hz time series (up to ~90 s, oldest first / newest last). Timestamp of sample i = end_unix_ms - (samples-1-i)*1000.",
            "inputSchema": {"type": "object", "properties": {
                "metric": {"type": "string", "enum": ["all", "cpu", "memory", "network", "cores"], "default": "all"},
                "seconds": {"type": "integer", "minimum": 1, "description": "Only the most recent N samples"}
            }}
        },
        {
            "name": "list_apps",
            "description": "List installed packages on the selected device with their APK paths.",
            "inputSchema": {"type": "object", "properties": {
                "type": {"type": "string", "enum": ["user", "system", "all"], "default": "user"},
                "filter": {"type": "string", "description": "Case-insensitive substring of the package name"}
            }}
        },
        {
            "name": "install_app",
            "description": "Install an APK from a path on the machine running android-monitor (adb install -r).",
            "inputSchema": {"type": "object", "properties": {
                "apk_path": {"type": "string"},
                "grant_all_permissions": {"type": "boolean", "default": false, "description": "adb install -g"},
                "allow_downgrade": {"type": "boolean", "default": false}
            }, "required": ["apk_path"]}
        },
        {
            "name": "uninstall_app",
            "description": "Uninstall a package.",
            "inputSchema": {"type": "object", "properties": {
                "package": {"type": "string"},
                "keep_data": {"type": "boolean", "default": false, "description": "adb uninstall -k"}
            }, "required": ["package"]}
        },
        {
            "name": "launch_app",
            "description": "Launch an app by package (its launcher activity), or a specific activity.",
            "inputSchema": {"type": "object", "properties": {
                "package": {"type": "string"},
                "activity": {"type": "string", "description": "Optional activity class, e.g. .MainActivity"}
            }, "required": ["package"]}
        },
        {
            "name": "stop_app",
            "description": "Force-stop an app.",
            "inputSchema": {"type": "object", "properties": {"package": {"type": "string"}}, "required": ["package"]}
        },
        {
            "name": "list_app_permissions",
            "description": "Requested and runtime permissions of a package with granted state.",
            "inputSchema": {"type": "object", "properties": {"package": {"type": "string"}}, "required": ["package"]}
        },
        {
            "name": "grant_permissions",
            "description": "Grant one or more runtime permissions (pm grant), e.g. android.permission.CAMERA.",
            "inputSchema": {"type": "object", "properties": {
                "package": {"type": "string"},
                "permissions": {"type": "array", "items": {"type": "string"}, "minItems": 1}
            }, "required": ["package", "permissions"]}
        },
        {
            "name": "revoke_permissions",
            "description": "Revoke one or more runtime permissions (pm revoke).",
            "inputSchema": {"type": "object", "properties": {
                "package": {"type": "string"},
                "permissions": {"type": "array", "items": {"type": "string"}, "minItems": 1}
            }, "required": ["package", "permissions"]}
        },
        {
            "name": "get_raw",
            "description": "Latest unparsed device output (/proc/stat, /proc/meminfo, /proc/net/dev, /proc/loadavg, /proc/uptime, dumpsys battery, df, ip addr) and parsed cumulative counters.",
            "inputSchema": {"type": "object", "properties": {
                "source": {"type": "string", "enum": ["all", "counters", "stat", "meminfo", "net_dev", "loadavg", "uptime", "battery", "df", "ip"], "default": "all"}
            }}
        }
    ])
}

fn call_tool(hub: &Hub, name: &str, args: &Value) -> Result<Value, RpcError> {
    let result = match name {
        "list_devices" => Ok(devices(&tel(hub))),
        "get_snapshot" => Ok(snapshot(&tel(hub))),
        "get_history" => {
            let metric = args.get("metric").and_then(Value::as_str).unwrap_or("all");
            let secs = args.get("seconds").and_then(Value::as_u64).map(|s| s as usize);
            history(&tel(hub), metric, secs)
        }
        "get_raw" => raw(&tel(hub), args.get("source").and_then(Value::as_str).unwrap_or("all")),
        "select_device" => select_device(hub, args.get("serial").and_then(Value::as_str)),
        "list_apps" | "list_app_permissions" => apps::read_tool(hub, name, args),
        "install_app" | "uninstall_app" | "launch_app" | "stop_app" | "grant_permissions" | "revoke_permissions" => {
            if READ_ONLY.load(Ordering::Relaxed) {
                Err("server is running read-only (--mcp-readonly); device-changing tools are disabled".to_string())
            } else {
                apps::write_tool(hub, name, args)
            }
        }
        _ => return Err((-32602, format!("unknown tool: {name}"))),
    };
    Ok(match result {
        Ok(v) => json!({
            "content": [{"type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default()}],
            "structuredContent": v,
            "isError": false
        }),
        Err(e) => json!({"content": [{"type": "text", "text": e}], "isError": true}),
    })
}

fn select_device(hub: &Hub, serial: Option<&str>) -> Result<Value, String> {
    let serial = serial.ok_or("'serial' is required")?;
    if serial == "auto" {
        let mut s = hub.sel.lock().unwrap_or_else(|e| e.into_inner());
        s.serial = None;
        s.pinned = false;
        return Ok(json!({"selected": "auto"}));
    }
    {
        let t = tel(hub);
        match t.devices.iter().find(|d| d.serial == serial) {
            None => return Err(format!("device '{serial}' not found; call list_devices")),
            Some(d) if !d.ready() => return Err(format!("device '{serial}' is '{}' (not ready)", d.state)),
            _ => {}
        }
    }
    let mut s = hub.sel.lock().unwrap_or_else(|e| e.into_inner());
    s.serial = Some(serial.to_string());
    s.pinned = true;
    Ok(json!({"selected": serial, "note": "takes effect within ~1 s; history resets"}))
}

// ------------------------------------------------------------------- data --

fn devices(t: &Telemetry) -> Value {
    json!({
        "selected": t.selected,
        "devices": t.devices.iter().map(|d| json!({
            "serial": d.serial, "state": d.state, "model": d.model, "ready": d.ready()
        })).collect::<Vec<_>>()
    })
}

fn snapshot(t: &Telemetry) -> Value {
    let (status, detail) = match &t.status {
        Status::Connected => ("connected", Value::Null),
        Status::Starting => ("starting", Value::Null),
        Status::NoDevice(m) => ("disconnected", json!(m)),
    };
    let m = &t.mem;
    let cache = m.available_kb.saturating_sub(m.free_kb);
    let used = m.total_kb.saturating_sub(m.free_kb + cache);
    let battery = t.slow.battery.as_ref().map(|b| {
        json!({
            "level_percent": b.level, "temperature_c": b.temp_c, "status": b.status,
            "plugged": b.plugged, "voltage_mv": b.voltage_mv
        })
    });
    let storage = t.slow.storage.as_ref().map(|s| {
        json!({
            "mount": "/data", "total_kb": s.total_kb, "used_kb": s.used_kb, "available_kb": s.avail_kb,
            "used_percent": r2(s.used_kb as f32 / s.total_kb.max(1) as f32 * 100.0)
        })
    });
    json!({
        "timestamp_unix_ms": unix_ms(),
        "status": status,
        "status_detail": detail,
        "device": {
            "serial": t.selected, "manufacturer": t.info.manufacturer, "model": t.info.model,
            "android_version": t.info.android, "api_level": t.info.sdk
        },
        "sample_unix_ms": t.last_unix_ms,
        "sample_age_ms": if t.last_unix_ms == 0 { Value::Null } else { json!(t.last_arrival.elapsed().as_millis() as u64) },
        "poll_latency_ms": t.latency.as_millis() as u64,
        "samples_collected": t.ticks,
        "cpu": {
            "total_percent": r2(t.cpu_now()),
            "core_count": t.cores.len(),
            "cores_percent": t.cores.iter().map(|v| r2(*v)).collect::<Vec<_>>()
        },
        "memory": {
            "total_kb": m.total_kb, "free_kb": m.free_kb, "available_kb": m.available_kb,
            "used_kb": used, "cached_reclaimable_kb": cache, "buffers_kb": m.buffers_kb,
            "page_cache_kb": m.cached_kb, "sreclaimable_kb": m.sreclaimable_kb,
            "swap_total_kb": m.swap_total_kb, "swap_used_kb": m.swap_total_kb.saturating_sub(m.swap_free_kb),
            "used_percent": r2(t.mem_used_pct())
        },
        "network": {
            "rx_bytes_per_s": r2(t.rx_now()), "tx_bytes_per_s": r2(t.tx_now()),
            "active_interfaces": t.last.as_ref().and_then(|s| s.net.as_ref()).map(|n| n.ifaces.clone()),
            "ip": t.slow.ip.as_ref().map(|(i, a)| json!({"interface": i, "address": a}))
        },
        "load_avg": t.load.map(|l| json!([l.0, l.1, l.2])),
        "uptime_s": t.uptime,
        "battery": battery,
        "storage": storage,
        "warnings": t.warnings
    })
}

fn tail(q: &std::collections::VecDeque<f32>, n: usize) -> Vec<f64> {
    q.iter().skip(q.len().saturating_sub(n)).map(|v| r2(*v)).collect()
}

fn history(t: &Telemetry, metric: &str, secs: Option<usize>) -> Result<Value, String> {
    let n = secs.unwrap_or(HISTORY + 2).max(1);
    let mut series = Map::new();
    let all = metric == "all";
    if all || metric == "cpu" {
        series.insert("cpu_percent".into(), json!(tail(&t.cpu_hist, n)));
    }
    if all || metric == "memory" {
        series.insert("memory_used_percent".into(), json!(tail(&t.mem_hist, n)));
    }
    if all || metric == "network" {
        series.insert("network_rx_bytes_per_s".into(), json!(tail(&t.rx_hist, n)));
        series.insert("network_tx_bytes_per_s".into(), json!(tail(&t.tx_hist, n)));
    }
    if all || metric == "cores" {
        series.insert("core_percent".into(), json!(t.core_hist.iter().map(|h| tail(h, n)).collect::<Vec<_>>()));
    }
    if series.is_empty() {
        return Err(format!("unknown metric '{metric}' (use all, cpu, memory, network, cores)"));
    }
    Ok(json!({
        "interval_seconds": 1,
        "order": "oldest_first",
        "end_unix_ms": t.last_unix_ms,
        "samples": t.cpu_hist.len().min(n),
        "device": t.selected,
        "series": series
    }))
}

fn counters(t: &Telemetry) -> Value {
    let Some(s) = &t.last else { return Value::Null };
    json!({
        "cpu_jiffies": {"total": s.cpu.total, "idle": s.cpu.idle},
        "core_jiffies": s.cores.iter().map(|c| json!({"total": c.total, "idle": c.idle})).collect::<Vec<_>>(),
        "network": s.net.as_ref().map(|n| json!({"rx_bytes_total": n.rx, "tx_bytes_total": n.tx, "interfaces": n.ifaces})),
        "meminfo_kb": {
            "MemTotal": s.mem.total_kb, "MemFree": s.mem.free_kb, "MemAvailable": s.mem.available_kb,
            "Buffers": s.mem.buffers_kb, "Cached": s.mem.cached_kb, "SReclaimable": s.mem.sreclaimable_kb,
            "SwapTotal": s.mem.swap_total_kb, "SwapFree": s.mem.swap_free_kb
        },
        "poll_latency_ms": s.latency.as_millis() as u64
    })
}

fn raw_entry(t: &Telemetry, name: &str) -> Value {
    match t.raw.get(name) {
        Some((text, ms)) => json!({"captured_unix_ms": ms, "text": text}),
        None => Value::Null,
    }
}

fn raw(t: &Telemetry, source: &str) -> Result<Value, String> {
    match source {
        "counters" => Ok(json!({"counters": counters(t)})),
        "all" => {
            let mut m = Map::new();
            for s in RAW_SOURCES {
                m.insert(s.into(), raw_entry(t, s));
            }
            Ok(json!({"device": t.selected, "sources": m, "counters": counters(t)}))
        }
        s if RAW_SOURCES.contains(&s) => Ok(json!({"device": t.selected, "source": s, "data": raw_entry(t, s)})),
        s => Err(format!("unknown source '{s}'")),
    }
}

// -------------------------------------------------------------- resources --

fn resource_defs() -> Value {
    let mut v = vec![
        json!({"uri": "android-monitor://snapshot", "name": "Snapshot", "description": "Current computed stats", "mimeType": "application/json"}),
        json!({"uri": "android-monitor://history", "name": "History", "description": "Rolling 1 Hz series", "mimeType": "application/json"}),
        json!({"uri": "android-monitor://devices", "name": "Devices", "description": "ADB devices", "mimeType": "application/json"}),
        json!({"uri": "android-monitor://raw/counters", "name": "Raw counters", "description": "Cumulative counters", "mimeType": "application/json"}),
    ];
    for s in RAW_SOURCES {
        v.push(json!({"uri": format!("android-monitor://raw/{s}"), "name": format!("Raw {s}"),
            "description": "Latest unparsed device output", "mimeType": "text/plain"}));
    }
    json!(v)
}

fn read_resource(hub: &Hub, uri: &str) -> Result<Value, RpcError> {
    let t = tel(hub);
    let content = |mime: &str, text: String| json!({"contents": [{"uri": uri, "mimeType": mime, "text": text}]});
    let pretty = |v: Value| serde_json::to_string_pretty(&v).unwrap_or_default();
    match uri.strip_prefix("android-monitor://") {
        Some("snapshot") => Ok(content("application/json", pretty(snapshot(&t)))),
        Some("devices") => Ok(content("application/json", pretty(devices(&t)))),
        Some("history") => Ok(content("application/json", pretty(history(&t, "all", None).unwrap_or(Value::Null)))),
        Some("raw/counters") => Ok(content("application/json", pretty(counters(&t)))),
        Some(r) if r.starts_with("raw/") && RAW_SOURCES.contains(&&r[4..]) => {
            let text = t.raw.get(&r[4..]).map(|(s, _)| s.clone()).unwrap_or_default();
            Ok(content("text/plain", text))
        }
        _ => Err((-32002, format!("resource not found: {uri}"))),
    }
}

// --------------------------------------------------------------- app tools --

mod apps {
    use super::*;

    fn serial(hub: &Hub) -> Result<String, String> {
        let t = tel(hub);
        match (&t.selected, &t.status) {
            (Some(s), Status::Connected) => Ok(s.clone()),
            _ => Err("no connected device (call list_devices / select_device)".into()),
        }
    }

    fn checked<'a>(v: Option<&'a str>, what: &str, extra: &[char]) -> Result<&'a str, String> {
        let s = v.ok_or_else(|| format!("'{what}' is required"))?;
        let ok = !s.is_empty()
            && s.len() <= 256
            && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || extra.contains(&c));
        if ok { Ok(s) } else { Err(format!("invalid {what}: '{s}'")) }
    }

    /// Package / permission names: letters, digits, `_` and `.` only.
    /// Strict allow-list because these end up in a device shell command line.
    pub(super) fn ident<'a>(v: Option<&'a str>, what: &str) -> Result<&'a str, String> {
        checked(v, what, &[])
    }

    /// Activity class names may contain `$` (inner classes); it is returned single-quoted so the
    /// device shell cannot expand it.
    pub(super) fn activity(v: Option<&str>) -> Result<String, String> {
        checked(v, "activity", &['$']).map(|s| format!("'{s}'"))
    }

    fn perms(args: &Value) -> Result<Vec<&str>, String> {
        let arr = args.get("permissions").and_then(Value::as_array).ok_or("'permissions' (array) is required")?;
        if arr.is_empty() {
            return Err("'permissions' is empty".into());
        }
        arr.iter().map(|p| ident(p.as_str(), "permission")).collect()
    }

    fn sh(serial: &str, cmd: &str, secs: u64) -> Result<String, String> {
        adb::shell(serial, cmd, Duration::from_secs(secs))
    }

    pub fn read_tool(hub: &Hub, name: &str, args: &Value) -> Result<Value, String> {
        let serial = serial(hub)?;
        match name {
            "list_apps" => {
                let flag = match args.get("type").and_then(Value::as_str).unwrap_or("user") {
                    "user" => "-3",
                    "system" => "-s",
                    "all" => "",
                    o => return Err(format!("unknown type '{o}' (user, system, all)")),
                };
                let filter = args.get("filter").and_then(Value::as_str).unwrap_or("").to_lowercase();
                let out = sh(&serial, &format!("pm list packages -f {flag}"), 20)?;
                let mut apps: Vec<Value> = out
                    .lines()
                    .filter_map(|l| l.trim().strip_prefix("package:"))
                    .filter_map(|l| l.rsplit_once('=').map(|(path, pkg)| (pkg.trim(), path)))
                    .filter(|(pkg, _)| filter.is_empty() || pkg.to_lowercase().contains(&filter))
                    .map(|(pkg, path)| json!({"package": pkg, "apk_path": path}))
                    .collect();
                apps.sort_by(|a, b| a["package"].as_str().cmp(&b["package"].as_str()));
                Ok(json!({"count": apps.len(), "apps": apps}))
            }
            _ => {
                let pkg = ident(args.get("package").and_then(Value::as_str), "package")?;
                let out = sh(&serial, &format!("dumpsys package {pkg}"), 20)?;
                if !out.contains(&format!("Package [{pkg}]")) {
                    return Err(format!("package '{pkg}' not found"));
                }
                let mut requested = vec![];
                let mut in_req = false;
                let mut state = std::collections::BTreeMap::new();
                for line in out.lines() {
                    let t = line.trim();
                    if t.ends_with("requested permissions:") {
                        in_req = true;
                        continue;
                    }
                    if in_req {
                        if line.starts_with("    ") && !t.ends_with(':') && !t.contains(' ') {
                            requested.push(t.trim_end_matches(':').to_string());
                            continue;
                        }
                        in_req = false;
                    }
                    if let Some((perm, rest)) = t.split_once(": granted=") {
                        if perm.contains('.') && !perm.contains(' ') {
                            state.insert(perm.to_string(), rest.starts_with("true"));
                        }
                    }
                }
                Ok(json!({
                    "package": pkg,
                    "requested": requested,
                    "granted_state": state,
                    "note": "granted_state lists install-time and runtime permissions the system reports"
                }))
            }
        }
    }

    pub fn write_tool(hub: &Hub, name: &str, args: &Value) -> Result<Value, String> {
        let serial = serial(hub)?;
        match name {
            "install_app" => {
                let path = args.get("apk_path").and_then(Value::as_str).ok_or("'apk_path' is required")?;
                let p = std::path::Path::new(path);
                if !p.is_file() || p.extension().map(|e| !e.eq_ignore_ascii_case("apk")).unwrap_or(true) {
                    return Err(format!("'{path}' is not an existing .apk file on this machine"));
                }
                let mut a = vec!["-s", serial.as_str(), "install", "-r"];
                if args.get("grant_all_permissions").and_then(Value::as_bool) == Some(true) {
                    a.push("-g");
                }
                if args.get("allow_downgrade").and_then(Value::as_bool) == Some(true) {
                    a.push("-d");
                }
                a.push(path);
                let out = adb::run(&a, Duration::from_secs(180))?;
                if out.contains("Success") {
                    Ok(json!({"installed": true, "output": out.trim()}))
                } else {
                    Err(format!("install failed: {}", out.trim()))
                }
            }
            "uninstall_app" => {
                let pkg = ident(args.get("package").and_then(Value::as_str), "package")?;
                let mut a = vec!["-s", serial.as_str(), "uninstall"];
                if args.get("keep_data").and_then(Value::as_bool) == Some(true) {
                    a.push("-k");
                }
                a.push(pkg);
                let out = adb::run(&a, Duration::from_secs(60))?;
                if out.contains("Success") {
                    Ok(json!({"uninstalled": pkg, "output": out.trim()}))
                } else {
                    Err(format!("uninstall failed: {}", out.trim()))
                }
            }
            "launch_app" => {
                let pkg = ident(args.get("package").and_then(Value::as_str), "package")?;
                let out = match args.get("activity").and_then(Value::as_str) {
                    Some(act) => {
                        let act = activity(Some(act))?;
                        sh(&serial, &format!("am start -n {pkg}/{act}"), 15)?
                    }
                    None => sh(&serial, &format!("monkey -p {pkg} -c android.intent.category.LAUNCHER 1"), 15)?,
                };
                let started = if args.get("activity").is_some() {
                    out.contains("Starting:") && !out.contains("Error")
                } else {
                    out.contains("Events injected: 1")
                };
                if started {
                    Ok(json!({"launched": pkg}))
                } else {
                    let tail: Vec<&str> = out.lines().filter(|l| l.contains("**") || l.contains("Error")).collect();
                    Err(format!("could not launch '{pkg}': {}", if tail.is_empty() { out.trim().to_string() } else { tail.join(" ") }))
                }
            }
            "stop_app" => {
                let pkg = ident(args.get("package").and_then(Value::as_str), "package")?;
                sh(&serial, &format!("am force-stop {pkg}"), 10)?;
                Ok(json!({"stopped": pkg}))
            }
            _ => {
                let pkg = ident(args.get("package").and_then(Value::as_str), "package")?;
                let verb = if name == "grant_permissions" { "grant" } else { "revoke" };
                let mut done = vec![];
                let mut failed = vec![];
                for p in perms(args)? {
                    // `pm` prints errors on stdout/stderr and may still exit 0.
                    let out = sh(&serial, &format!("pm {verb} {pkg} {p} 2>&1; true"), 15)?;
                    let o = out.trim();
                    if o.is_empty() {
                        done.push(p.to_string());
                    } else {
                        failed.push(json!({"permission": p, "error": o}));
                    }
                }
                let result = json!({"package": pkg, "action": verb, "succeeded": done, "failed": failed});
                if done.is_empty() { Err(result.to_string()) } else { Ok(result) }
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn call(hub: &Hub, req: Value) -> Value {
        serde_json::from_str(&handle_message(hub, &req.to_string()).expect("response")).unwrap()
    }

    #[test]
    fn initialize_negotiates_version() {
        let hub = Hub::new();
        let r = call(&hub, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}));
        assert_eq!(r["result"]["protocolVersion"], "2024-11-05");
        let r = call(&hub, json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}));
        assert_eq!(r["result"]["protocolVersion"], LATEST);
        assert_eq!(r["result"]["serverInfo"]["name"], "android-monitor");
    }

    #[test]
    fn notifications_get_no_reply_and_errors_are_typed() {
        let hub = Hub::new();
        assert!(handle_message(&hub, r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
        let r = call(&hub, json!({"jsonrpc":"2.0","id":7,"method":"nope"}));
        assert_eq!(r["error"]["code"], -32601);
        assert_eq!(r["id"], 7);
        let r: Value = serde_json::from_str(&handle_message(&hub, "{not json").unwrap()).unwrap();
        assert_eq!(r["error"]["code"], -32700);
    }

    #[test]
    fn lists_all_tools_and_resources() {
        let hub = Hub::new();
        let r = call(&hub, json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}));
        let names: Vec<&str> = r["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        for n in ["get_snapshot", "get_history", "get_raw", "install_app", "uninstall_app", "launch_app", "list_apps", "grant_permissions"] {
            assert!(names.contains(&n), "missing tool {n}");
        }
        let r = call(&hub, json!({"jsonrpc":"2.0","id":2,"method":"resources/list"}));
        assert!(r["result"]["resources"].as_array().unwrap().len() >= 10);
    }

    #[test]
    fn snapshot_without_device_reports_status() {
        let hub = Hub::new();
        let r = call(&hub, json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_snapshot","arguments":{}}}));
        assert_eq!(r["result"]["isError"], false);
        assert_eq!(r["result"]["structuredContent"]["status"], "starting");
    }

    #[test]
    fn bad_arguments_are_tool_errors_not_protocol_errors() {
        let hub = Hub::new();
        let r = call(&hub, json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_history","arguments":{"metric":"bogus"}}}));
        assert_eq!(r["result"]["isError"], true);
        let r = call(&hub, json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"no_such_tool"}}));
        assert_eq!(r["error"]["code"], -32602);
        // device tools refuse cleanly when nothing is connected
        let r = call(&hub, json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"launch_app","arguments":{"package":"com.x"}}}));
        assert_eq!(r["result"]["isError"], true);
    }

    #[test]
    fn identifiers_are_allow_listed() {
        use super::apps::{activity, ident};
        assert_eq!(ident(Some("com.example.app"), "package"), Ok("com.example.app"));
        assert_eq!(ident(Some("android.permission.READ_MEDIA_IMAGES"), "permission"), Ok("android.permission.READ_MEDIA_IMAGES"));
        assert_eq!(activity(Some("com.x.Outer$Inner")), Ok("'com.x.Outer$Inner'".to_string()));
        assert!(activity(Some("a;b")).is_err() && activity(Some("a'b")).is_err());
        for bad in ["", "a b", "x;id", "a&&b", "$(id)", "a$b", "a|b", "a`b`", "a\nb", "a'b", "../x"] {
            assert!(ident(Some(bad), "package").is_err(), "should reject {bad:?}");
        }
        assert!(ident(None, "package").is_err());
        assert!(ident(Some(&"a".repeat(300)), "package").is_err());
    }
}