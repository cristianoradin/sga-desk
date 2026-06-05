// ConectDesk in-service agent task.
//
// Phase 1 of the unified client: the SYSTEM service does what the Electron agent used to do
// (enroll on first run + periodic heartbeat). Runs in a background tokio task spawned from
// start_server. Safe to ship: if CONECTDESK_ENROLL_KEY is empty, the task no-ops and the
// legacy Electron agent keeps running. When both run, heartbeats are just redundant — same
// record updated by rustdesk_id on the server side.

use hbb_common::{
    config::{Config, CONECTDESK_API, CONECTDESK_ENROLL_KEY},
    log,
};
use serde_json::Value;
use std::time::Duration;

const TOKEN_KEY: &str = "conectdesk_token";
const HEARTBEAT_INTERVAL_SECS: u64 = 30;
const ENROLL_RETRY_SECS: u64 = 60;

fn api_base() -> Option<String> {
    let b = CONECTDESK_API.trim_end_matches('/').to_string();
    if b.is_empty() { None } else { Some(b) }
}

fn http_client() -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true) // internal self-signed cert
        .timeout(Duration::from_secs(10))
        .build()
        .ok()
}

fn hostname() -> String {
    whoami::devicename()
}

fn os_name() -> String {
    std::env::consts::OS.to_string()
}

fn os_version() -> String {
    // Not critical for phase 1; the server tolerates empty. Will be filled in phase 2.
    String::new()
}

fn rustdesk_id() -> String {
    Config::get_id()
}

fn agent_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

fn saved_token() -> String {
    Config::get_option(TOKEN_KEY)
}

fn save_token(t: &str) {
    Config::set_option(TOKEN_KEY.to_string(), t.to_string());
}

// POST /api/agents/enroll — auth: x-api-key (enroll key baked at build time).
async fn enroll() -> Option<String> {
    let key = CONECTDESK_ENROLL_KEY;
    if key.is_empty() {
        log::debug!("ConectDesk agent: enroll key empty — skipping enroll (Electron agent likely handles it)");
        return None;
    }
    let base = api_base()?;
    let client = http_client()?;
    let body = serde_json::json!({
        "name": hostname(),
        "hostname": hostname(),
        "os": os_name(),
        "osVersion": os_version(),
        "rustdeskId": rustdesk_id(),
        "agentVersion": agent_version(),
    });
    let url = format!("{}/api/agents/enroll", base);
    match client.post(&url).header("x-api-key", key).json(&body).send().await {
        Ok(resp) if resp.status().is_success() => match resp.json::<Value>().await {
            Ok(v) => v.get("agentToken").and_then(|t| t.as_str()).map(|s| s.to_string()),
            Err(e) => {
                log::error!("ConectDesk enroll: bad json: {:?}", e);
                None
            }
        },
        Ok(resp) => {
            log::error!("ConectDesk enroll: HTTP {}", resp.status());
            None
        }
        Err(e) => {
            log::error!("ConectDesk enroll: request failed: {:?}", e);
            None
        }
    }
}

// POST /api/agents/heartbeat — auth: x-agent-token.
async fn heartbeat(token: &str) -> bool {
    let Some(base) = api_base() else { return false };
    let Some(client) = http_client() else { return false };
    let body = serde_json::json!({
        "rustdeskId": rustdesk_id(),
        "agentVersion": agent_version(),
        "forkInstalled": true,
        "rustdeskReady": true, // we ARE the service — if we're running, RustDesk is up
    });
    let url = format!("{}/api/agents/heartbeat", base);
    match client.post(&url).header("x-agent-token", token).json(&body).send().await {
        Ok(resp) => resp.status().is_success(),
        Err(e) => {
            log::warn!("ConectDesk heartbeat: request failed: {:?}", e);
            false
        }
    }
}

pub fn start() {
    // Spawn a long-lived task on the tokio runtime that already runs the service.
    tokio::spawn(async move {
        log::info!("ConectDesk in-service agent: starting (api={}, enroll_key_set={})",
                   CONECTDESK_API, !CONECTDESK_ENROLL_KEY.is_empty());
        // Initial delay so the service finishes its own startup first.
        tokio::time::sleep(Duration::from_secs(15)).await;
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
            // Heartbeat loop
            loop {
                let ok = heartbeat(&token).await;
                if !ok {
                    // 401 likely means the token is stale → re-enroll on next outer iteration
                    log::warn!("ConectDesk: heartbeat failed — will re-check");
                    break;
                }
                tokio::time::sleep(Duration::from_secs(HEARTBEAT_INTERVAL_SECS)).await;
            }
        }
    });
}
