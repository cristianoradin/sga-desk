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

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

// Helper: cria Command sem janela de console flickando no cliente (CREATE_NO_WINDOW = 0x08000000).
// Aplicado em sc.exe / wmic / ipconfig / tasklist / powershell.exe — todos eram CONSOLE subsystem,
// então sem essa flag uma janela preta pisca por 100ms a cada execução.
fn hidden_command(program: &str) -> std::process::Command {
    #[allow(unused_mut)]
    let mut cmd = std::process::Command::new(program);
    #[cfg(target_os = "windows")]
    cmd.creation_flags(0x08000000);
    cmd
}

use hbb_common::{
    config::{Config, CONECTDESK_API, CONECTDESK_ENROLL_KEY},
    log, sysinfo, tokio, whoami,
};
use serde_json::{json, Value};
use std::time::Duration;

const TOKEN_KEY: &str = "conectdesk_token";
const BRANDING_TS_KEY: &str = "cd_logo_updated_at";
const BRAND_NAME_KEY: &str = "cd_brand_name";
const BRAND_LOGO_PATH_KEY: &str = "cd_brand_logo_path";
// Sessão ativa (técnico atual) — sincronizada quando heartbeat retorna activeSession.
// O Flutter UI (server_page approval screen + desktop_home_page card) consome estes options.
const ACTIVE_SESSION_ID_KEY: &str = "cd_active_session_id";
const ACTIVE_SESSION_TECH_NAME_KEY: &str = "cd_active_session_tech_name";
const ACTIVE_SESSION_TECH_PHOTO_PATH_KEY: &str = "cd_active_session_tech_photo_path";
const HEARTBEAT_INTERVAL_SECS: u64 = 30;
const SYSINFO_INTERVAL_SECS: u64 = 600;     // 10 min
const UPDATE_INTERVAL_SECS: u64 = 1800;     // 30 min
const BRANDING_INTERVAL_SECS: u64 = 300;    // 5 min
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
    } else {
        Config::set_option("approve-mode".to_string(), "password".to_string());
    }
    // ConectDesk: força esconder janela CM separada. A UI principal já cobre o papel
    // da CM (tela aprovação branded + card técnico em sessão). Sem isso o cliente vê
    // 2 janelas (app principal + CM popup). Setamos allow-hide-cm + hide_cm true.
    Config::set_option("allow-hide-cm".to_string(), "Y".to_string());
    Config::set_option("hide_cm".to_string(), "true".to_string());
    log::info!("ConectDesk: approve-mode={} (CM hidden, UI principal mostra)",
        if require_approval { "click" } else { "password" });
}

// -- sysinfo (paridade com o que o Electron mandava) ------------------------
fn collect_sysinfo() -> Value {
    use sysinfo::System;
    let mut sys = System::new_all();
    sys.refresh_all();
    let total_mem = sys.total_memory();
    let used_mem = sys.used_memory();
    let mem_pct = if total_mem > 0 { (used_mem * 100 / total_mem) as u32 } else { 0 };
    let os_version = sys.os_version().unwrap_or_default();
    let uptime = sys.uptime();

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
        if let Ok(o) = hidden_command("wmic")
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
    if let Ok(o) = hidden_command("ipconfig").arg("/all").output() {
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
    if let Ok(o) = hidden_command("wmic")
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
        if let Ok(o) = hidden_command("sc.exe").args(["query", name]).output() {
            let s = String::from_utf8_lossy(&o.stdout);
            return s.contains("RUNNING");
        }
        return false;
    }
    if kind == "process" {
        if let Ok(o) = hidden_command("tasklist").args(["/NH", "/FI", &format!("IMAGENAME eq {}", name)]).output() {
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
        let _ = hidden_command("sc.exe").args(["start", name]).status();
    } else if kind == "process" && !exec_path.is_empty() {
        log::info!("ConectDesk watchdog: relaunch {}", exec_path);
        let _ = hidden_command(exec_path).spawn();
    }
}
#[cfg(not(target_os = "windows"))]
fn attempt_restart(_kind: &str, _name: &str, _exec_path: &str) {}

// -- auto-update -------------------------------------------------------------
// Build id baked at compile time. Compared against the feed's `build_id` (also Unix seconds)
// so the in-service agent self-updates without depending on Cargo.toml semver bumps.
fn local_build_id() -> u64 {
    env!("CONECTDESK_BUILD_ID").parse::<u64>().unwrap_or(0)
}

// fork-latest.yml format (separate from electron's latest.yml):
//   build_id: 1780797000
//   version: 1.4.7-cd-20260607
//   exe: ConectDesk-Setup.exe
//   sha512: <hex>   (optional — when present, fork verifies after download)
fn parse_fork_feed(s: &str) -> Option<(u64, String, String, Option<String>)> {
    let mut build_id = None;
    let mut version = String::new();
    let mut exe = None;
    let mut sha512 = None;
    for line in s.lines() {
        let l = line.trim();
        if let Some(v) = l.strip_prefix("build_id:") {
            build_id = v.trim().trim_matches('\'').trim_matches('"').parse::<u64>().ok();
        } else if let Some(v) = l.strip_prefix("version:") {
            version = v.trim().trim_matches('\'').trim_matches('"').to_string();
        } else if let Some(v) = l.strip_prefix("exe:") {
            exe = Some(v.trim().trim_matches('\'').trim_matches('"').to_string());
        } else if let Some(v) = l.strip_prefix("sha512:") {
            let s = v.trim().trim_matches('\'').trim_matches('"').to_string();
            if !s.is_empty() { sha512 = Some(s); }
        }
    }
    Some((build_id?, version, exe?, sha512))
}

#[cfg(target_os = "windows")]
async fn maybe_update() {
    let Some(base) = api_base() else { return };
    let Some(client) = http_client(30) else { return };
    let url = format!("{}/updates/fork-latest.yml", base);
    let body = match client.get(&url).send().await {
        Ok(r) if r.status().is_success() => r.text().await.unwrap_or_default(),
        _ => return,
    };
    let Some((remote_build, remote_version, exe, _sha512)) = parse_fork_feed(&body) else { return };
    let local = local_build_id();
    if remote_build <= local {
        log::debug!("ConectDesk update: up-to-date (local={}, remote={})", local, remote_build);
        return;
    }
    log::info!("ConectDesk update: {} -> {} (downloading {})", local, remote_version, exe);
    let url_safe = exe.replace(' ', "%20");
    let dl_url = format!("{}/updates/{}", base, url_safe);
    let bytes = match client.get(&dl_url).send().await {
        Ok(r) if r.status().is_success() => match r.bytes().await { Ok(b) => b, Err(_) => return },
        _ => { log::warn!("ConectDesk update: download failed"); return; }
    };
    let tmp = std::env::temp_dir().join(format!("ConectDesk-Update-{}.exe", remote_build));
    if std::fs::write(&tmp, &bytes).is_err() {
        log::error!("ConectDesk update: failed to write {:?}", tmp);
        return;
    }
    // Detached PowerShell: stop service, install silent, restart. The current process gets killed
    // mid-install when the service stops — that's expected. The detached child survives and finishes.
    let ps = format!(
        "Start-Process powershell -WindowStyle Hidden -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-Command',\"Stop-Service ConectDesk -Force -ErrorAction SilentlyContinue; Start-Sleep -Seconds 3; & '{}' /S; Start-Sleep -Seconds 8; Start-Service ConectDesk -ErrorAction SilentlyContinue\"",
        tmp.display()
    );
    log::info!("ConectDesk update: launching detached installer for build {}", remote_build);
    let _ = hidden_command("powershell.exe")
        .args(&["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &ps])
        .spawn();
}

#[cfg(not(target_os = "windows"))]
async fn maybe_update() {}

// Synchronize per-client branding (logo + brand_name) from the API.
// Saves the PNG to %LOCALAPPDATA%\ConectDesk\branding.png (or ~/.config on Linux) and writes
// cd_brand_name + cd_brand_logo_path into Config so the CM (Flutter UI) can render them.
// Skips download when logo_updated_at hasn't moved.
async fn sync_branding(token: &str) {
    let Some(base) = api_base() else { return };
    let Some(client) = http_client(15) else { return };
    let url = format!("{}/api/agents/me/branding", base);
    let resp = match client.get(&url).bearer_auth(token).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => return,
    };
    let v: Value = match resp.json().await { Ok(v) => v, Err(_) => return };
    let brand_name = v.get("brandName").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let logo_url = v.get("logoUrl").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let updated_at = v.get("logoUpdatedAt").and_then(|x| x.as_u64()).unwrap_or(0);

    // Always sync brand_name (cheap string write).
    Config::set_option(BRAND_NAME_KEY.to_string(), brand_name.clone());

    // No logo configured → clear the local cached path (CM falls back to SGA default).
    if logo_url.is_empty() || updated_at == 0 {
        Config::set_option(BRAND_LOGO_PATH_KEY.to_string(), String::new());
        Config::set_option(BRANDING_TS_KEY.to_string(), "0".to_string());
        return;
    }

    // Skip download when the timestamp hasn't advanced.
    let local_ts: u64 = Config::get_option(BRANDING_TS_KEY).parse().unwrap_or(0);
    let local_path = Config::get_option(BRAND_LOGO_PATH_KEY);
    if local_ts == updated_at && !local_path.is_empty() && std::path::Path::new(&local_path).exists() {
        return;
    }

    let full_url = if logo_url.starts_with("http") { logo_url } else { format!("{}{}", base, logo_url) };
    let bytes = match client.get(&full_url).send().await {
        Ok(r) if r.status().is_success() => match r.bytes().await { Ok(b) => b, Err(_) => return },
        _ => return,
    };

    // %LOCALAPPDATA%\ConectDesk on Windows, $XDG_CONFIG_HOME/ConectDesk or ~/.config/ConectDesk
    // on Linux, fallback to %TEMP%\ConectDesk. Avoid the extra `dirs` crate dependency.
    let base_dir = if cfg!(target_os = "windows") {
        std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME").map(std::path::PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
    };
    let dir = base_dir
        .map(|d| d.join("ConectDesk"))
        .unwrap_or_else(|| std::env::temp_dir().join("ConectDesk"));
    if std::fs::create_dir_all(&dir).is_err() { return; }
    let path = dir.join("branding.png");
    if std::fs::write(&path, &bytes).is_err() { return; }
    Config::set_option(BRAND_LOGO_PATH_KEY.to_string(), path.to_string_lossy().to_string());
    Config::set_option(BRANDING_TS_KEY.to_string(), updated_at.to_string());
    log::info!("ConectDesk branding: sync ok ({} bytes, updated_at={})", bytes.len(), updated_at);
}

// Sincroniza foto + nome do técnico da sessão ativa. Chamada SOMENTE quando heartbeat
// retorna activeSession.id != ao último salvo (evita re-download a cada 30s).
async fn sync_active_session_photo(token: &str, session_id: &str, tech_name: &str) {
    // Persiste id + nome imediatamente (Flutter mostra nome mesmo se download falhar).
    Config::set_option(ACTIVE_SESSION_ID_KEY.to_string(), session_id.to_string());
    Config::set_option(ACTIVE_SESSION_TECH_NAME_KEY.to_string(), tech_name.to_string());

    let Some(base) = api_base() else { return };
    let Some(client) = http_client(15) else { return };
    let url = format!("{}/api/agents/me/active-session/photo", base);
    let resp = match client.get(&url).bearer_auth(token).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => return,
    };
    let v: Value = match resp.json().await { Ok(v) => v, Err(_) => return };
    let photo = v.get("photo").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if photo.is_empty() {
        Config::set_option(ACTIVE_SESSION_TECH_PHOTO_PATH_KEY.to_string(), String::new());
        return;
    }

    // photo é data URL "data:image/png;base64,..." OU base64 puro.
    let b64 = match photo.split_once("base64,") {
        Some((_, rest)) => rest.trim().to_string(),
        None => photo.trim().to_string(),
    };
    let bytes = match base64_decode(&b64) {
        Some(b) => b,
        None => { log::warn!("ConectDesk: tech photo base64 inválido"); return; }
    };

    let base_dir = if cfg!(target_os = "windows") {
        std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME").map(std::path::PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
    };
    let dir = base_dir
        .map(|d| d.join("ConectDesk"))
        .unwrap_or_else(|| std::env::temp_dir().join("ConectDesk"));
    if std::fs::create_dir_all(&dir).is_err() { return; }
    let path = dir.join("session_tech.png");
    if std::fs::write(&path, &bytes).is_err() { return; }
    Config::set_option(ACTIVE_SESSION_TECH_PHOTO_PATH_KEY.to_string(), path.to_string_lossy().to_string());
    log::info!("ConectDesk: tech photo gravada ({} bytes) session={}", bytes.len(), session_id);
}

fn clear_active_session() {
    Config::set_option(ACTIVE_SESSION_ID_KEY.to_string(), String::new());
    Config::set_option(ACTIVE_SESSION_TECH_NAME_KEY.to_string(), String::new());
    Config::set_option(ACTIVE_SESSION_TECH_PHOTO_PATH_KEY.to_string(), String::new());
}

// Decodificador base64 sem trazer crate nova. Aceita padding `=` opcional e ignora whitespace.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lookup = [255u8; 256];
    for (i, &c) in TABLE.iter().enumerate() { lookup[c as usize] = i as u8; }
    lookup[b'-' as usize] = 62; lookup[b'_' as usize] = 63; // url-safe variant
    let clean: Vec<u8> = s.bytes().filter(|&b| !b.is_ascii_whitespace() && b != b'=').collect();
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for &b in &clean {
        let v = lookup[b as usize];
        if v == 255 { return None; }
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

// Clean up the legacy "RustDesk" service if it co-exists with our gated "ConectDesk" service.
// Run once on agent start. The fork runs as SYSTEM, so `sc.exe delete` works without UAC.
#[cfg(target_os = "windows")]
fn migrate_stock_service() {
    let has = |name: &str| -> bool {
        hidden_command("sc.exe")
            .args(&["query", name])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    if !has("ConectDesk") || !has("RustDesk") { return; }
    log::info!("ConectDesk: legacy RustDesk service detected, removing");
    let _ = hidden_command("sc.exe").args(&["stop", "RustDesk"]).status();
    std::thread::sleep(Duration::from_secs(2));
    let _ = hidden_command("sc.exe").args(&["delete", "RustDesk"]).status();
}

#[cfg(not(target_os = "windows"))]
fn migrate_stock_service() {}

// -- main loop ---------------------------------------------------------------
pub fn start() {
    tokio::spawn(async move {
        log::info!(
            "ConectDesk in-service agent: starting (api={}, enroll_key_set={}, build_id={})",
            CONECTDESK_API,
            !CONECTDESK_ENROLL_KEY.is_empty(),
            local_build_id(),
        );
        // One-shot cleanup of the legacy RustDesk service if it co-exists with our ConectDesk service.
        // SYSTEM context — no UAC needed.
        migrate_stock_service();
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
            // Heartbeat — response carries dynamic config + WoL queue + on-demand flags.
            match heartbeat(&token).await {
                Some(resp) => {
                    if let Some(req_appr) = resp.get("requireApproval").and_then(|v| v.as_bool()) {
                        apply_approve_mode(req_appr);
                    }
                    if let Some(wol) = resp.get("wol").and_then(|v| v.as_array()) {
                        if !wol.is_empty() { dispatch_wol(wol).await; }
                    }
                    // ConectDesk: update on-demand. O painel marca requestUpdate=true só quando
                    // o técnico clica "Atualizar". Antes era loop automatic → re-instalava em
                    // ciclo eterno por causa do build_id != compile_id no publish-fork.sh.
                    if resp.get("requestUpdate").and_then(|v| v.as_bool()).unwrap_or(false) {
                        log::info!("ConectDesk: requestUpdate=true do painel — disparando update");
                        maybe_update().await;
                    }
                    // Sessão ativa: detecta nova ou fim. Quando muda id, baixa foto. Quando some, limpa.
                    match resp.get("activeSession") {
                        Some(s) if !s.is_null() => {
                            let new_id = s.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            let new_name = s.get("technician").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            let last_id = Config::get_option(ACTIVE_SESSION_ID_KEY);
                            if !new_id.is_empty() && new_id != last_id {
                                sync_active_session_photo(&token, &new_id, &new_name).await;
                            }
                        }
                        _ => {
                            if !Config::get_option(ACTIVE_SESSION_ID_KEY).is_empty() {
                                clear_active_session();
                            }
                        }
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
            // Update agora é ON-DEMAND (vem via heartbeat resp.requestUpdate quando o painel
            // marca). Removido o auto-loop de 30min — gerava re-install eterno porque o
            // publish-fork.sh usava timestamp do publish, diferente do build_id compilado.
            // Branding (logo + nome empresa do cliente) a cada 5min.
            if tick * HEARTBEAT_INTERVAL_SECS % BRANDING_INTERVAL_SECS == 0 {
                sync_branding(&token).await;
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
