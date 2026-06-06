// ConectDesk in-service agent task.
//
// What the Windows SYSTEM service does on top of remote control:
//   - First run: POST /api/agents/enroll → save agentToken in Config.
//   - Every 30s: POST /api/agents/heartbeat → response carries `requireApproval` (Click/Direct)
//     and an optional `wol` list. The service applies the approve mode and dispatches magic packets.
//   - Every 10 min: collect FULL sysinfo (CPU/RAM/disks/uptime/os/MAC/IP) → POST /api/agents/sysinfo.
//   - Every 30 min: check /updates/latest.yml; if newer than CARGO_PKG_VERSION → download +
//     run installer silently (no UAC since we are SYSTEM).
//   - Every 60s: GET /api/agents/me/devices and /api/agents/me/watchdog (panel-configured) → probe
//     TCP/Serial pumps, vigil services/processes (auto-restart when configured), report status back.
//   - Every 30s during a portal session: pings keepalive (handled in connection.rs).
//
// All HTTP uses CONECTDESK_API + reqwest with self-signed cert trust. When CONECTDESK_ENROLL_KEY
// is empty the whole task no-ops (safe for stock/dev builds).

use hbb_common::{
    config::{Config, CONECTDESK_API, CONECTDESK_ENROLL_KEY},
    log, sysinfo, tokio, whoami,
};
use serde_json::{json, Value};
use std::time::Duration;

const TOKEN_KEY: &str = "conectdesk_token";
const HEARTBEAT_INTERVAL_SECS: u64 = 30;
const SYSINFO_INTERVAL_SECS: u64 = 600;     // 10 min
const UPDATE_INTERVAL_SECS: u64 = 1800;     // 30 min
const PROBE_INTERVAL_SECS: u64 = 60;        // bombas + watchdog
const ENROLL_RETRY_SECS: u64 = 60;

fn api_base() -> Option<String> {
    let b = CONECTDESK_API.trim_end_matches('/').to_string();
    if b.is_empty() { None } else { Some(b) }
}

fn http_client(timeout_secs: u64) -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true) // internal self-signed cert
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .ok()
}

fn hostname() -> String { whoami::devicename() }
fn os_name() -> String { std::env::consts::OS.to_string() }
fn rustdesk_id() -> String { Config::get_id() }
fn agent_version() -> &'static str { env!("CARGO_PKG_VERSION") }
fn saved_token() -> String { Config::get_option(TOKEN_KEY) }
fn save_token(t: &str) { Config::set_option(TOKEN_KEY.to_string(), t.to_string()); }

// Last applied approve-mode (so we only set when it changes — avoids spamming Config writes).
static mut LAST_APPROVE: Option<bool> = None;

// -- enroll ------------------------------------------------------------------
async fn enroll() -> Option<String> {
    let key = CONECTDESK_ENROLL_KEY;
    if key.is_empty() { return None; }
    let base = api_base()?;
    let client = http_client(10)?;
    let body = json!({
        "name": hostname(),
        "hostname": hostname(),
        "os": os_name(),
        "osVersion": "",
        "rustdeskId": rustdesk_id(),
        "agentVersion": agent_version(),
    });
    let url = format!("{}/api/agents/enroll", base);
    match client.post(&url).header("x-api-key", key).json(&body).send().await {
        Ok(resp) if resp.status().is_success() => match resp.json::<Value>().await {
            Ok(v) => v.get("agentToken").and_then(|t| t.as_str()).map(|s| s.to_string()),
            Err(e) => { log::error!("ConectDesk enroll: bad json: {:?}", e); None }
        },
        Ok(resp) => { log::error!("ConectDesk enroll: HTTP {}", resp.status()); None }
        Err(e) => { log::error!("ConectDesk enroll: request failed: {:?}", e); None }
    }
}

// -- heartbeat + dynamic approve-mode + WoL dispatch ------------------------
async fn heartbeat(token: &str) -> Option<Value> {
    let base = api_base()?;
    let client = http_client(8)?;
    let body = json!({
        "rustdeskId": rustdesk_id(),
        "agentVersion": agent_version(),
        "forkInstalled": true,
        "rustdeskReady": true,
    });
    let url = format!("{}/api/agents/heartbeat", base);
    match client.post(&url).header("x-agent-token", token).json(&body).send().await {
        Ok(resp) if resp.status().is_success() => resp.json::<Value>().await.ok(),
        Ok(resp) => { log::warn!("ConectDesk heartbeat: HTTP {}", resp.status()); None }
        Err(e) => { log::warn!("ConectDesk heartbeat: {:?}", e); None }
    }
}

fn apply_approve_mode(require_approval: bool) {
    // Only flip when actually changing — avoids burning Config writes every heartbeat.
    unsafe {
        if LAST_APPROVE == Some(require_approval) { return; }
        LAST_APPROVE = Some(require_approval);
    }
    if require_approval {
        Config::set_option("approve-mode".to_string(), "click".to_string());
        Config::set_option("allow-hide-cm".to_string(), "N".to_string());
        log::info!("ConectDesk: approve-mode=click (cliente vê prompt)");
    } else {
        Config::set_option("approve-mode".to_string(), "password".to_string());
        Config::set_option("allow-hide-cm".to_string(), "Y".to_string());
        log::info!("ConectDesk: approve-mode=password (direto, hidden CM)");
    }
}

// -- sysinfo (paridade com o que o Electron mandava) ------------------------
fn collect_sysinfo() -> Value {
    use sysinfo::System;
    let mut sys = System::new_all();
    sys.refresh_all();
    let total_mem = sys.total_memory();
    let used_mem = sys.used_memory();
    let mem_pct = if total_mem > 0 { (used_mem * 100 / total_mem) as u32 } else { 0 };
    let os_version = System::os_version().unwrap_or_default();
    let uptime = System::uptime();

    // Net (first usable interface).
    let (mac, ip4) = first_iface();

    json!({
        "sistema": {
            "hostname": hostname(),
            "os": os_name(),
            "osVersion": os_version,
            "uptimeSec": uptime as i64,
        },
        "hardware": {
            "cpu": {
                "cores": sys.cpus().len(),
                "brand": sys.cpus().first().map(|c| c.brand().to_string()).unwrap_or_default(),
            },
            "mem": {
                "totalGB": total_mem / (1024 * 1024 * 1024),
                "usedPct": mem_pct,
            },
            "disks": collect_disks(),
        },
        "rede": {
            "mac": mac,
            "ip": ip4,
        },
    })
}

fn collect_disks() -> Value {
    // sysinfo nesse fork pode não expor Disks API publicamente — usamos WMI no Windows.
    #[cfg(target_os = "windows")]
    {
        let mut out = vec![];
        if let Ok(o) = std::process::Command::new("wmic")
            .args(["logicaldisk", "where", "drivetype=3", "get", "Caption,Size,FreeSpace", "/FORMAT:CSV"])
            .output()
        {
            let s = String::from_utf8_lossy(&o.stdout);
            for line in s.lines().skip(1) {
                let parts: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
                if parts.len() < 4 { continue; }
                let mount = parts[1];
                let free: u64 = parts[2].parse().unwrap_or(0);
                let total: u64 = parts[3].parse().unwrap_or(0);
                if total == 0 || mount.is_empty() { continue; }
                let used = total.saturating_sub(free);
                let pct = (used * 100 / total) as u32;
                out.push(json!({
                    "mount": mount,
                    "totalGB": total / (1024 * 1024 * 1024),
                    "usedPct": pct,
                }));
            }
        }
        return json!(out);
    }
    #[allow(unreachable_code)]
    json!([])
}

#[cfg(target_os = "windows")]
fn first_iface() -> (String, String) {
    // ipconfig /all → procura primeira interface ativa com MAC + IPv4.
    let mut mac = String::new();
    let mut ip4 = String::new();
    if let Ok(o) = std::process::Command::new("ipconfig").arg("/all").output() {
        let s = String::from_utf8_lossy(&o.stdout);
        for line in s.lines() {
            let l = line.trim();
            if l.contains("Physical Address") || l.contains("Endereço Físico") {
                if let Some(v) = l.split(':').nth(1) {
                    let candidate = v.trim().replace('-', ":");
                    if mac.is_empty() && candidate.len() >= 11 { mac = candidate; }
                }
            } else if l.starts_with("IPv4") || l.starts_with("Endereço IPv4") {
                if let Some(v) = l.split(':').nth(1) {
                    let candidate = v.trim().trim_end_matches("(Preferred)").trim_end_matches("(Preferencial)").trim().to_string();
                    if ip4.is_empty() && !candidate.is_empty() { ip4 = candidate; }
                }
            }
        }
    }
    (mac, ip4)
}
#[cfg(not(target_os = "windows"))]
fn first_iface() -> (String, String) { (String::new(), String::new()) }

async fn send_sysinfo(token: &str) -> bool {
    let Some(base) = api_base() else { return false };
    let Some(client) = http_client(15) else { return false };
    let body = collect_sysinfo();
    let url = format!("{}/api/agents/sysinfo", base);
    match client.post(&url).header("x-agent-token", token).json(&body).send().await {
        Ok(resp) => resp.status().is_success(),
        Err(e) => { log::warn!("ConectDesk sysinfo: {:?}", e); false }
    }
}

// -- WoL: send magic packet UDP broadcast on port 9 -------------------------
async fn dispatch_wol(targets: &[Value]) {
    for t in targets {
        let Some(mac) = t.get("mac").and_then(|v| v.as_str()) else { continue };
        let bytes: Vec<u8> = mac.split(|c| c == ':' || c == '-')
            .filter_map(|p| u8::from_str_radix(p, 16).ok()).collect();
        if bytes.len() != 6 { continue; }
        let mut packet = vec![0xFF_u8; 6];
        for _ in 0..16 { packet.extend_from_slice(&bytes); }
        match std::net::UdpSocket::bind("0.0.0.0:0") {
            Ok(sock) => {
                let _ = sock.set_broadcast(true);
                let _ = sock.send_to(&packet, "255.255.255.255:9");
                let _ = sock.send_to(&packet, "255.255.255.255:7");
                log::info!("ConectDesk WoL: magic packet enviado p/ {}", mac);
            }
            Err(e) => log::warn!("ConectDesk WoL: bind falhou: {:?}", e),
        }
    }
}

// -- Devices (pumps): probe TCP/Serial --------------------------------------
async fn probe_devices(token: &str) {
    let Some(base) = api_base() else { return };
    let Some(client) = http_client(8) else { return };
    let url = format!("{}/api/agents/me/devices", base);
    let resp = match client.get(&url).header("x-agent-token", token).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => return,
    };
    let v: Value = match resp.json().await { Ok(v) => v, Err(_) => return };
    let Some(arr) = v.get("devices").and_then(|a| a.as_array()) else { return };
    for d in arr {
        let id = d.get("id").and_then(|x| x.as_str()).unwrap_or("");
        let kind = d.get("kind").and_then(|x| x.as_str()).unwrap_or("");
        let cfg = d.get("config").cloned().unwrap_or(json!({}));
        let (status, latency_ms) = match kind {
            "tcp" => probe_tcp(&cfg).await,
            "serial" => probe_serial(&cfg),
            _ => ("unknown".to_string(), None),
        };
        let report = json!({ "id": id, "status": status, "latencyMs": latency_ms });
        let url2 = format!("{}/api/agents/me/devices/report", base);
        let _ = client.post(&url2).header("x-agent-token", token).json(&report).send().await;
    }
}

async fn probe_tcp(cfg: &Value) -> (String, Option<u32>) {
    let ip = cfg.get("ip").and_then(|v| v.as_str()).unwrap_or("");
    let port = cfg.get("port").and_then(|v| v.as_u64()).unwrap_or(9999) as u16;
    if ip.is_empty() { return ("down".to_string(), None); }
    let addr = format!("{}:{}", ip, port);
    let start = std::time::Instant::now();
    match tokio::time::timeout(Duration::from_secs(3), tokio::net::TcpStream::connect(&addr)).await {
        Ok(Ok(_)) => {
            let ms = start.elapsed().as_millis() as u32;
            ((if ms > 200 { "slow" } else { "ok" }).to_string(), Some(ms))
        }
        _ => ("down".to_string(), None),
    }
}

#[cfg(target_os = "windows")]
fn probe_serial(cfg: &Value) -> (String, Option<u32>) {
    let com = cfg.get("com").and_then(|v| v.as_str()).unwrap_or("");
    if com.is_empty() { return ("down".to_string(), None); }
    // Just check if the port exists via wmic. Bytes-trafegando vai numa próxima rev.
    if let Ok(o) = std::process::Command::new("wmic")
        .args(["path", "Win32_SerialPort", "get", "DeviceID"])
        .output()
    {
        let s = String::from_utf8_lossy(&o.stdout);
        if s.to_uppercase().contains(&com.to_uppercase()) { return ("ok".to_string(), None); }
    }
    ("down".to_string(), None)
}
#[cfg(not(target_os = "windows"))]
fn probe_serial(_cfg: &Value) -> (String, Option<u32>) { ("unknown".to_string(), None) }

// -- Watchdog: vigia serviços / processos -----------------------------------
async fn run_watchdog(token: &str) {
    let Some(base) = api_base() else { return };
    let Some(client) = http_client(8) else { return };
    let url = format!("{}/api/agents/me/watchdog", base);
    let resp = match client.get(&url).header("x-agent-token", token).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => return,
    };
    let v: Value = match resp.json().await { Ok(v) => v, Err(_) => return };
    let Some(arr) = v.get("items").and_then(|a| a.as_array()) else { return };
    for it in arr {
        let id = it.get("id").and_then(|x| x.as_str()).unwrap_or("");
        let kind = it.get("kind").and_then(|x| x.as_str()).unwrap_or("");
        let name = it.get("name").and_then(|x| x.as_str()).unwrap_or("");
        let auto = it.get("auto_restart").and_then(|x| x.as_i64()).unwrap_or(0) == 1;
        let exec_path = it.get("exec_path").and_then(|x| x.as_str()).unwrap_or("");
        let running = check_alive(kind, name);
        let status = if running { "running" } else { "stopped" };
        if !running && auto {
            attempt_restart(kind, name, exec_path);
        }
        let report = json!({ "id": id, "status": status });
        let url2 = format!("{}/api/agents/me/watchdog/report", base);
        let _ = client.post(&url2).header("x-agent-token", token).json(&report).send().await;
    }
}

#[cfg(target_os = "windows")]
fn check_alive(kind: &str, name: &str) -> bool {
    if kind == "service" {
        if let Ok(o) = std::process::Command::new("sc.exe").args(["query", name]).output() {
            let s = String::from_utf8_lossy(&o.stdout);
            return s.contains("RUNNING");
        }
        return false;
    }
    if kind == "process" {
        if let Ok(o) = std::process::Command::new("tasklist").args(["/NH", "/FI", &format!("IMAGENAME eq {}", name)]).output() {
            let s = String::from_utf8_lossy(&o.stdout);
            return s.to_lowercase().contains(&name.to_lowercase());
        }
    }
    false
}
#[cfg(not(target_os = "windows"))]
fn check_alive(_kind: &str, _name: &str) -> bool { true }

#[cfg(target_os = "windows")]
fn attempt_restart(kind: &str, name: &str, exec_path: &str) {
    if kind == "service" {
        log::info!("ConectDesk watchdog: restart service {}", name);
        let _ = std::process::Command::new("sc.exe").args(["start", name]).status();
    } else if kind == "process" && !exec_path.is_empty() {
        log::info!("ConectDesk watchdog: relaunch {}", exec_path);
        let _ = std::process::Command::new(exec_path).spawn();
    }
}
#[cfg(not(target_os = "windows"))]
fn attempt_restart(_kind: &str, _name: &str, _exec_path: &str) {}

// -- auto-update -------------------------------------------------------------
fn version_gt(a: &str, b: &str) -> bool {
    let pa: Vec<u32> = a.split('.').map(|n| n.parse().unwrap_or(0)).collect();
    let pb: Vec<u32> = b.split('.').map(|n| n.parse().unwrap_or(0)).collect();
    let n = pa.len().max(pb.len());
    for i in 0..n {
        let x = pa.get(i).copied().unwrap_or(0);
        let y = pb.get(i).copied().unwrap_or(0);
        if x != y { return x > y; }
    }
    false
}

fn parse_latest_yml(s: &str) -> Option<(String, String)> {
    let mut version = None;
    let mut path = None;
    for line in s.lines() {
        let l = line.trim();
        if let Some(v) = l.strip_prefix("version:") {
            version = Some(v.trim().trim_matches('\'').trim_matches('"').to_string());
        } else if let Some(v) = l.strip_prefix("path:") {
            path = Some(v.trim().trim_matches('\'').trim_matches('"').to_string());
        } else if path.is_none() {
            if let Some(v) = l.strip_prefix("- url:") {
                path = Some(v.trim().trim_matches('\'').trim_matches('"').to_string());
            } else if let Some(v) = l.strip_prefix("url:") {
                path = Some(v.trim().trim_matches('\'').trim_matches('"').to_string());
            }
        }
    }
    Some((version?, path?))
}

#[cfg(target_os = "windows")]
async fn maybe_update() {
    let Some(base) = api_base() else { return };
    let Some(client) = http_client(20) else { return };
    let url = format!("{}/updates/latest.yml", base);
    let body = match client.get(&url).send().await {
        Ok(r) if r.status().is_success() => r.text().await.unwrap_or_default(),
        _ => return,
    };
    let Some((latest, path)) = parse_latest_yml(&body) else { return };
    if !version_gt(&latest, agent_version()) { return; }
    log::info!("ConectDesk update: {} -> {} (downloading {})", agent_version(), latest, path);
    let url_safe = path.replace(' ', "%20");
    let dl_url = format!("{}/updates/{}", base, url_safe);
    let bytes = match client.get(&dl_url).send().await {
        Ok(r) if r.status().is_success() => match r.bytes().await { Ok(b) => b, Err(_) => return },
        _ => return,
    };
    let tmp = std::env::temp_dir().join(format!("ConectDesk-Update-{}.exe", latest));
    if std::fs::write(&tmp, &bytes).is_err() {
        log::error!("ConectDesk update: failed to write {:?}", tmp);
        return;
    }
    log::info!("ConectDesk update: launching {} /S", tmp.display());
    let _ = std::process::Command::new(&tmp).arg("/S").spawn();
}

#[cfg(not(target_os = "windows"))]
async fn maybe_update() {}

// -- main loop ---------------------------------------------------------------
pub fn start() {
    tokio::spawn(async move {
        log::info!(
            "ConectDesk in-service agent: starting (api={}, enroll_key_set={})",
            CONECTDESK_API,
            !CONECTDESK_ENROLL_KEY.is_empty(),
        );
        tokio::time::sleep(Duration::from_secs(15)).await;
        let mut tick: u64 = 0;
        loop {
            let mut token = saved_token();
            if token.is_empty() {
                if let Some(t) = enroll().await {
                    save_token(&t);
                    token = t;
                    log::info!("ConectDesk: enrolled");
                } else {
                    tokio::time::sleep(Duration::from_secs(ENROLL_RETRY_SECS)).await;
                    continue;
                }
            }
            // Heartbeat — response carries dynamic config + WoL queue.
            match heartbeat(&token).await {
                Some(resp) => {
                    if let Some(req_appr) = resp.get("requireApproval").and_then(|v| v.as_bool()) {
                        apply_approve_mode(req_appr);
                    }
                    if let Some(wol) = resp.get("wol").and_then(|v| v.as_array()) {
                        if !wol.is_empty() { dispatch_wol(wol).await; }
                    }
                }
                None => {
                    // Token stale → clear, re-enroll.
                    log::warn!("ConectDesk: heartbeat sem resposta — limpando token");
                    save_token("");
                    tokio::time::sleep(Duration::from_secs(ENROLL_RETRY_SECS)).await;
                    continue;
                }
            }
            // Sysinfo periódico.
            if tick * HEARTBEAT_INTERVAL_SECS % SYSINFO_INTERVAL_SECS == 0 {
                let _ = send_sysinfo(&token).await;
            }
            // Update.
            if tick * HEARTBEAT_INTERVAL_SECS % UPDATE_INTERVAL_SECS == 0 {
                maybe_update().await;
            }
            // Watchdog + Bombas a cada 60s (=2 ticks).
            if tick * HEARTBEAT_INTERVAL_SECS % PROBE_INTERVAL_SECS == 0 {
                run_watchdog(&token).await;
                probe_devices(&token).await;
            }
            tick = tick.wrapping_add(1);
            tokio::time::sleep(Duration::from_secs(HEARTBEAT_INTERVAL_SECS)).await;
        }
    });
}
