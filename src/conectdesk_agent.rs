// ConectDesk in-service agent task.
//
// What the Windows SYSTEM service does on top of remote control:
//   - First run: POST /api/agents/enroll → save agentToken in Config.
//   - Every 30s: POST /api/agents/heartbeat with rustdeskId/version/fork/ready.
//   - Every 10 min: collect sysinfo (CPU/RAM/disk/OS) → POST /api/agents/sysinfo.
//   - Every 30 min: check /updates/latest.yml; if newer than CARGO_PKG_VERSION → download +
//     run msiexec /qn (silent install, no UAC since we are SYSTEM) → restart service.
//
// When CONECTDESK_ENROLL_KEY is empty, the whole task no-ops (safe for stock/dev builds).

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

// -- heartbeat ---------------------------------------------------------------
async fn heartbeat(token: &str) -> bool {
    let Some(base) = api_base() else { return false };
    let Some(client) = http_client(8) else { return false };
    let body = json!({
        "rustdeskId": rustdesk_id(),
        "agentVersion": agent_version(),
        "forkInstalled": true,
        "rustdeskReady": true, // service IS this binary
    });
    let url = format!("{}/api/agents/heartbeat", base);
    match client.post(&url).header("x-agent-token", token).json(&body).send().await {
        Ok(resp) => resp.status().is_success(),
        Err(e) => { log::warn!("ConectDesk heartbeat: {:?}", e); false }
    }
}

// -- sysinfo -----------------------------------------------------------------
fn collect_sysinfo() -> Value {
    use sysinfo::System;
    let mut sys = System::new_all();
    sys.refresh_all();
    let total_mem = sys.total_memory();
    let used_mem = sys.used_memory();
    let mem_pct = if total_mem > 0 { (used_mem * 100 / total_mem) as u32 } else { 0 };
    // Disks come from a SEPARATE struct in this sysinfo fork; if it isn't reachable just skip them.
    // CPU% would need refresh-with-delay + a different API; omitted in phase 1 (we still report cores).
    json!({
        "sistema": {
            "hostname": hostname(),
            "os": os_name(),
        },
        "hardware": {
            "cpu": {
                "cores": sys.cpus().len(),
            },
            "mem": {
                "totalGB": total_mem / (1024 * 1024 * 1024),
                "usedPct": mem_pct,
            },
        },
    })
}

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

// Parse a tiny YAML: looks for `version: x.y.z` and `path: file.exe` (or url:).
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
    // Download the installer to a temp file.
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
    // Launch installer silently. NSIS one-click installer supports /S.
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
        // Give the rest of the service time to come up.
        tokio::time::sleep(Duration::from_secs(15)).await;
        // Tick counters in units of HEARTBEAT_INTERVAL_SECS (default 30s).
        let mut tick: u64 = 0;
        loop {
            // Ensure we have a token.
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
            // Heartbeat.
            if !heartbeat(&token).await {
                // Likely 401 → stale token; clear and re-enroll on next loop.
                log::warn!("ConectDesk: heartbeat failed — clearing token");
                save_token("");
                tokio::time::sleep(Duration::from_secs(ENROLL_RETRY_SECS)).await;
                continue;
            }
            // Sysinfo every SYSINFO_INTERVAL_SECS.
            if tick * HEARTBEAT_INTERVAL_SECS % SYSINFO_INTERVAL_SECS == 0 {
                let _ = send_sysinfo(&token).await;
            }
            // Update check every UPDATE_INTERVAL_SECS.
            if tick * HEARTBEAT_INTERVAL_SECS % UPDATE_INTERVAL_SECS == 0 {
                maybe_update().await;
            }
            tick = tick.wrapping_add(1);
            tokio::time::sleep(Duration::from_secs(HEARTBEAT_INTERVAL_SECS)).await;
        }
    });
}
