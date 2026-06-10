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
// Multi-técnico: JSON array [{name, photoPath}] de TODOS os técnicos conectados. Widget mostra
// todos. cd_active_session_tech_name/_photo_path continuam = o primeiro (compat 1-técnico).
const ACTIVE_SESSIONS_JSON_KEY: &str = "cd_active_sessions";
// Histórico de sessões — JSON array salvo em option pra Flutter ler sem HTTP. Refresh
// a cada SESSION_HISTORY_INTERVAL_SECS via sync_session_history().
const SESSION_HISTORY_KEY: &str = "cd_session_history";
const SESSION_HISTORY_INTERVAL_SECS: u64 = 60; // 1 min (histórico mais fresco no app)
const HEARTBEAT_INTERVAL_SECS: u64 = 30;
const SYSINFO_INTERVAL_SECS: u64 = 600;     // 10 min
const HEALTH_INTERVAL_SECS: u64 = 18000;    // 5 h — coleta pesada (saúde/segurança/rede/periféricos)
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

// Diretório compartilhado pra arquivos que o agent (SYSTEM) grava E a UI/widget (usuário) lê:
// foto do técnico, logo de branding. No Windows = C:\ProgramData\ConectDesk (world-readable);
// LOCALAPPDATA do SYSTEM (systemprofile) NÃO é acessível pelo usuário logado.
fn cd_shared_dir() -> std::path::PathBuf {
    let base = if cfg!(target_os = "windows") {
        std::env::var_os("PROGRAMDATA")
            .or_else(|| std::env::var_os("ALLUSERSPROFILE"))
            .map(std::path::PathBuf::from)
    } else {
        Some(std::path::PathBuf::from("/var/lib"))
    };
    base.map(|d| d.join("ConectDesk"))
        .unwrap_or_else(|| std::env::temp_dir().join("ConectDesk"))
}

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
        // Modo click PRECISA do CM visível: é nele que aparece a tela de aprovação branded
        // (_ConectDeskApprovalScreen em server_page). Esconder o CM aqui fazia o cliente nunca
        // ver o pedido — técnico ficava preso em "aguardando autorização".
        Config::set_option("allow-hide-cm".to_string(), "N".to_string());
        Config::set_option("hide_cm".to_string(), "false".to_string());
    } else {
        Config::set_option("approve-mode".to_string(), "password".to_string());
        // Modo direto/senha: não há prompt de aprovação, então escondemos o CM popup.
        Config::set_option("allow-hide-cm".to_string(), "Y".to_string());
        Config::set_option("hide_cm".to_string(), "true".to_string());
    }
    log::info!("ConectDesk: approve-mode={} (hide_cm={})",
        if require_approval { "click" } else { "password" },
        if require_approval { "false" } else { "true" });
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
        "software": {
            "services": collect_services(),
            "processes": collect_processes(),
            "processesCpu": collect_processes_cpu(),
        },
    })
}

// Serviços Windows (nome + display + status). O painel mostra + permite parar via watchdog.
// Limita a ~120 pra não inflar o payload.
fn collect_services() -> Value {
    #[cfg(target_os = "windows")]
    {
        let ps = "Get-Service | Sort-Object Status,DisplayName | Select-Object -First 200 Name,DisplayName,Status | ConvertTo-Json -Compress";
        if let Ok(o) = hidden_command("powershell.exe")
            .args(["-NoProfile", "-Command", ps]).output()
        {
            let s = String::from_utf8_lossy(&o.stdout);
            if let Ok(v) = serde_json::from_str::<Value>(s.trim()) {
                let arr = if v.is_array() { v } else { json!([v]) };
                let mut out = vec![];
                if let Some(items) = arr.as_array() {
                    for it in items.iter().take(120) {
                        // Status: 4 = Running, 1 = Stopped (enum serializado como número).
                        let st = it.get("Status").and_then(|x| x.as_i64()).unwrap_or(0);
                        out.push(json!({
                            "name": it.get("Name").and_then(|x| x.as_str()).unwrap_or(""),
                            "display": it.get("DisplayName").and_then(|x| x.as_str()).unwrap_or(""),
                            "running": st == 4,
                        }));
                    }
                }
                return json!(out);
            }
        }
    }
    json!([])
}

// Top processos por RAM (nome + MB). tasklist CSV; agrega por nome, ordena desc, top 30.
fn collect_processes() -> Value {
    #[cfg(target_os = "windows")]
    {
        if let Ok(o) = hidden_command("tasklist").args(["/FO", "CSV", "/NH"]).output() {
            let s = String::from_utf8_lossy(&o.stdout);
            use std::collections::HashMap;
            let mut agg: HashMap<String, u64> = HashMap::new();
            for line in s.lines() {
                // "Image","PID","Session","Session#","MemUsage" — MemUsage tipo "12.345 K".
                let cols: Vec<String> = line.split("\",\"").map(|c| c.trim_matches('"').to_string()).collect();
                if cols.len() < 5 { continue; }
                let name = cols[0].clone();
                let mem_kb: u64 = cols[4].chars().filter(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0);
                *agg.entry(name).or_insert(0) += mem_kb;
            }
            let mut v: Vec<(String, u64)> = agg.into_iter().collect();
            v.sort_by(|a, b| b.1.cmp(&a.1));
            let out: Vec<Value> = v.into_iter().take(30)
                .map(|(name, kb)| json!({"name": name, "memMB": kb / 1024}))
                .collect();
            return json!(out);
        }
    }
    json!([])
}

// Top processos por uso de CPU (tempo acumulado em segundos) + RAM. Complementa o top-por-RAM.
fn collect_processes_cpu() -> Value {
    #[cfg(target_os = "windows")]
    {
        let ps = "Get-Process | Sort-Object CPU -Descending | Select-Object -First 15 Name,@{n='cpuSec';e={[int]($_.CPU)}},@{n='memMB';e={[int]($_.WS/1MB)}} | ConvertTo-Json -Compress";
        if let Ok(o) = hidden_command("powershell.exe").args(["-NoProfile", "-Command", ps]).output() {
            let s = String::from_utf8_lossy(&o.stdout);
            if let Ok(v) = serde_json::from_str::<Value>(s.trim()) {
                return if v.is_array() { v } else { json!([v]) };
            }
        }
    }
    json!([])
}

// ---- Saúde / segurança / rede / periféricos (coleta pesada via PowerShell) ----
// Roda a cada 5h (HEALTH_INTERVAL_SECS) ou sob demanda (botão "coletar agora"). Cada bloco é
// best-effort: se o PowerShell falhar, devolve null e o painel mostra "—".
#[cfg(target_os = "windows")]
fn ps_json(cmd: &str) -> Value {
    match hidden_command("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", cmd]).output()
    {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            serde_json::from_str::<Value>(s.trim()).unwrap_or(Value::Null)
        }
        Err(_) => Value::Null,
    }
}
#[cfg(target_os = "windows")]
fn as_array(v: Value) -> Value {
    match v { Value::Null => json!([]), Value::Array(_) => v, other => json!([other]) }
}

// Coleta os 4 blocos pesados. Retorna um objeto pra fazer merge no sysinfo (a API merge top-level).
fn collect_health_blocks() -> Value {
    #[cfg(target_os = "windows")]
    {
        // Discos físicos + SMART (HealthStatus: Healthy/Warning/Unhealthy).
        let disks = as_array(ps_json(
            "Get-PhysicalDisk | Select-Object -First 12 FriendlyName,MediaType,HealthStatus,@{n='sizeGB';e={[math]::Round($_.Size/1GB)}} | ConvertTo-Json -Compress"));
        // UPS / nobreak (BatteryStatus: 1=na bateria, 2=AC/carregando).
        let ups = ps_json(
            "Get-CimInstance Win32_Battery | Select-Object -First 1 BatteryStatus,EstimatedChargeRemaining,EstimatedRunTime | ConvertTo-Json -Compress");
        // Eventos críticos recentes (System, erros/críticos, 3 dias, top 12).
        let events = as_array(ps_json(
            "try { Get-WinEvent -FilterHashtable @{LogName='System';Level=1,2;StartTime=(Get-Date).AddDays(-3)} -MaxEvents 12 -ErrorAction Stop | Select-Object @{n='ts';e={$_.TimeCreated.ToString('s')}},Id,ProviderName,LevelDisplayName,@{n='msg';e={$_.Message -replace '\\s+',' ' | ForEach-Object { $_.Substring(0,[math]::Min(140,$_.Length)) }}} | ConvertTo-Json -Compress } catch { '[]' }"));
        // Última atualização instalada (proxy de Windows Update em dia).
        let last_update = ps_json(
            "try { (Get-HotFix | Sort-Object InstalledOn -Descending | Select-Object -First 1).InstalledOn.ToString('yyyy-MM-dd') | ConvertTo-Json -Compress } catch { 'null' }");
        // Defender / antivírus.
        let defender = ps_json(
            "try { Get-MpComputerStatus | Select-Object AntivirusEnabled,RealTimeProtectionEnabled,@{n='sigAgeDays';e={$_.AntivirusSignatureAge}} | ConvertTo-Json -Compress } catch { 'null' }");
        // Firewall por perfil.
        let firewall = as_array(ps_json(
            "Get-NetFirewallProfile | Select-Object Name,Enabled | ConvertTo-Json -Compress"));
        // Licença do Windows (LicenseStatus 1 = ativado).
        let licensed = ps_json(
            "try { (Get-CimInstance SoftwareLicensingProduct -Filter \"Name like 'Windows%25' AND PartialProductKey is not null\" | Select-Object -First 1).LicenseStatus | ConvertTo-Json -Compress } catch { 'null' }");
        // Impressoras (status + fila).
        let printers = as_array(ps_json(
            "Get-Printer | Select-Object -First 20 Name,@{n='status';e={$_.PrinterStatus.ToString()}} | ConvertTo-Json -Compress"));
        // Qualidade do link pro servidor (latência média + perda).
        let net = collect_network_quality();
        // I/O do disco: latência média por transferência (ms). Alto (>20-25ms) = disco lento → PDV
        // travando. Win32_PerfFormattedData evita o sample de 2x do Get-Counter.
        let disk_latency = ps_json(
            "try { [int]((Get-CimInstance Win32_PerfFormattedData_PerfDisk_PhysicalDisk | Where-Object Name -eq '_Total').AvgDisksecPerTransfer * 1000) | ConvertTo-Json -Compress } catch { 'null' }");

        return json!({
            "health": {
                "disks": disks,
                "ups": ups,
                "events": events,
                "diskLatencyMs": disk_latency,
            },
            "security": {
                "lastWindowsUpdate": last_update,
                "defender": defender,
                "firewall": firewall,
                "windowsLicensed": licensed,
            },
            "redeQualidade": net,
            "peripherals": {
                "printers": printers,
            },
        });
    }
    #[allow(unreachable_code)]
    json!({})
}

// Latência média (ms) + perda (%) num ping ao host do servidor — qualidade do link do posto.
#[cfg(target_os = "windows")]
fn collect_network_quality() -> Value {
    let host = api_base()
        .and_then(|b| b.split("://").nth(1).map(|s| s.split('/').next().unwrap_or("").split(':').next().unwrap_or("").to_string()))
        .unwrap_or_default();
    if host.is_empty() { return Value::Null; }
    let cmd = format!(
        "try {{ $r = Test-Connection -ComputerName '{}' -Count 4 -ErrorAction Stop; \
         [pscustomobject]@{{ avgMs=[math]::Round(($r | Measure-Object ResponseTime -Average).Average); \
         loss=0 }} | ConvertTo-Json -Compress }} catch {{ '{{\"avgMs\":null,\"loss\":100}}' }}", host);
    ps_json(&cmd)
}
#[cfg(not(target_os = "windows"))]
fn collect_network_quality() -> Value { Value::Null }

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

// Envia os blocos pesados de saúde (merge no sysinfo via setSysinfo da API). Timeout maior — o
// PowerShell (eventlog, Test-Connection) é lento. Chamado a cada 5h e no "coletar agora".
async fn send_health(token: &str) -> bool {
    let Some(base) = api_base() else { return false };
    let Some(client) = http_client(60) else { return false };
    let body = collect_health_blocks();
    let url = format!("{}/api/agents/sysinfo", base);
    match client.post(&url).header("x-agent-token", token).json(&body).send().await {
        Ok(resp) => resp.status().is_success(),
        Err(e) => { log::warn!("ConectDesk health: {:?}", e); false }
    }
}

// -- Comandos remotos (reboot / limpar temp / matar processo) ----------------
// Enfileirados pelo painel (admin) e drenados no heartbeat. Allowlist estrita de ações; nada de
// shell arbitrário. Roda como SYSTEM, então cada ação é fechada e validada. Faz ack pro painel.
async fn run_remote_command(token: &str, cmd: &Value) {
    let id = cmd.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let action = cmd.get("action").and_then(|v| v.as_str()).unwrap_or("");
    let arg = cmd.get("arg").and_then(|v| v.as_str()).unwrap_or("");
    if id.is_empty() { return; }
    log::info!("ConectDesk: comando remoto '{}' (arg='{}')", action, arg);
    let (ok, msg) = exec_remote_action(action, arg);
    // ack pro painel (auditoria).
    if let (Some(base), Some(client)) = (api_base(), http_client(10)) {
        let url = format!("{}/api/agents/me/command-ack", base);
        let body = json!({ "id": id, "ok": ok, "message": msg });
        let _ = client.post(&url).header("x-agent-token", token).json(&body).send().await;
    }
}

#[cfg(target_os = "windows")]
fn exec_remote_action(action: &str, arg: &str) -> (bool, String) {
    match action {
        "reboot" => {
            // 30s de aviso + mensagem; admin pode cancelar com shutdown /a manualmente.
            let r = hidden_command("shutdown.exe")
                .args(["/r", "/t", "30", "/c", "ConectDesk: reinicio remoto solicitado pelo suporte"]).status();
            match r { Ok(s) if s.success() => (true, "reboot agendado (30s)".into()), _ => (false, "falha ao agendar reboot".into()) }
        }
        "cancel_reboot" => {
            let _ = hidden_command("shutdown.exe").args(["/a"]).status();
            (true, "reboot cancelado".into())
        }
        "cleanup_temp" => {
            let mut freed = 0u64;
            for dir in [std::env::var("TEMP").ok(), Some("C:\\Windows\\Temp".to_string())].into_iter().flatten() {
                if let Ok(rd) = std::fs::read_dir(&dir) {
                    for e in rd.flatten() {
                        let p = e.path();
                        // symlink_metadata NÃO segue links → uma entrada que é symlink/junction
                        // (ex: apontando pra C:\Windows) é removida como link, nunca recursada.
                        // Sem isto, remove_dir_all seguiria o link e apagaria o alvo (somos SYSTEM).
                        let Ok(m) = std::fs::symlink_metadata(&p) else { continue };
                        let ft = m.file_type();
                        if ft.is_symlink() {
                            let _ = std::fs::remove_file(&p).or_else(|_| std::fs::remove_dir(&p));
                        } else if ft.is_file() {
                            freed += m.len();
                            let _ = std::fs::remove_file(&p);
                        } else if ft.is_dir() {
                            let _ = std::fs::remove_dir_all(&p);
                        }
                    }
                }
            }
            (true, format!("temp limpo (~{} MB liberados)", freed / (1024*1024)))
        }
        "kill_process" => {
            // arg = nome da imagem (ex: chrome.exe). Valida: só alfanum/._- + termina .exe.
            let name = arg.trim();
            let valid = !name.is_empty() && name.to_lowercase().ends_with(".exe")
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-');
            if !valid { return (false, "nome de processo inválido".into()); }
            let r = hidden_command("taskkill.exe").args(["/F", "/IM", name]).status();
            match r { Ok(s) if s.success() => (true, format!("{} encerrado", name)), _ => (false, format!("falha ao encerrar {}", name)) }
        }
        // ---- Serviços (arg = nome do serviço Windows) ----
        "service_start" | "service_stop" | "service_restart" => {
            let name = arg.trim();
            // Nome de serviço Windows: alfanum + _ - . (sem espaço/aspas — defesa extra; Command
            // não usa shell mesmo). Vazio → recusa.
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')) {
                return (false, "nome de serviço inválido".into());
            }
            match action {
                "service_start" => { let _ = hidden_command("sc.exe").args(["start", name]).status(); (true, format!("serviço {} iniciado", name)) }
                "service_stop" => { let _ = hidden_command("sc.exe").args(["stop", name]).status(); (true, format!("serviço {} parado", name)) }
                _ => {
                    let _ = hidden_command("sc.exe").args(["stop", name]).status();
                    std::thread::sleep(std::time::Duration::from_secs(3));
                    let _ = hidden_command("sc.exe").args(["start", name]).status();
                    (true, format!("serviço {} reiniciado", name))
                }
            }
        }
        // ---- Impressão: limpa fila + reinicia spooler (impressora fiscal travada) ----
        "print_clear" => {
            let ps = "Stop-Service Spooler -Force -ErrorAction SilentlyContinue; \
                Remove-Item \"$env:SystemRoot\\System32\\spool\\PRINTERS\\*\" -Force -ErrorAction SilentlyContinue; \
                Start-Service Spooler -ErrorAction SilentlyContinue";
            let _ = hidden_command("powershell.exe").args(["-NoProfile", "-Command", ps]).status();
            (true, "fila de impressão limpa + spooler reiniciado".into())
        }
        // ---- Windows Update: dispara verificação/instalação ----
        "windows_update" => {
            let _ = hidden_command("UsoClient.exe").args(["StartInstall"]).status();
            (true, "Windows Update disparado".into())
        }
        // ---- Energia / sessão ----
        "shutdown" => {
            let r = hidden_command("shutdown.exe").args(["/s", "/t", "30", "/c", "ConectDesk: desligamento solicitado pelo suporte"]).status();
            match r { Ok(s) if s.success() => (true, "desligamento agendado (30s)".into()), _ => (false, "falha ao desligar".into()) }
        }
        "logoff" => { let _ = hidden_command("shutdown.exe").args(["/l"]).status(); (true, "logoff solicitado".into()) }
        "lock" => {
            // Bloqueia a estação. Como SYSTEM pode não atingir a sessão do usuário — best-effort.
            let _ = hidden_command("rundll32.exe").args(["user32.dll,LockWorkStation"]).status();
            (true, "bloqueio de tela solicitado".into())
        }
        // ---- Agendar reboot pra um horário HH:MM (próxima ocorrência) ----
        "schedule_reboot" => {
            let t = arg.trim();
            // valida HH:MM
            let ok = t.len() == 5 && t.as_bytes()[2] == b':'
                && t[..2].parse::<u8>().map(|h| h < 24).unwrap_or(false)
                && t[3..].parse::<u8>().map(|m| m < 60).unwrap_or(false);
            if !ok { return (false, "horário inválido (use HH:MM)".into()); }
            let ps = format!(
                "$t=[datetime]::Today.AddHours({}).AddMinutes({}); if($t -lt (Get-Date)){{$t=$t.AddDays(1)}}; $s=[int]($t-(Get-Date)).TotalSeconds; shutdown /r /t $s /c 'ConectDesk: reinicio agendado pelo suporte'",
                &t[..2], &t[3..]
            );
            let _ = hidden_command("powershell.exe").args(["-NoProfile", "-Command", &ps]).status();
            (true, format!("reinício agendado para {}", t))
        }
        // ---- Reinicia o próprio serviço ConectDesk (detached, senão mata a si mesmo antes) ----
        "restart_agent" => {
            let ps = "Start-Process powershell -WindowStyle Hidden -ArgumentList '-NoProfile','-Command',\"Stop-Service ConectDesk -Force -ErrorAction SilentlyContinue; Start-Sleep -Seconds 3; Start-Service ConectDesk -ErrorAction SilentlyContinue\"";
            let _ = hidden_command("powershell.exe").args(["-NoProfile", "-Command", ps]).spawn();
            (true, "agente ConectDesk reiniciando".into())
        }
        // ---- Rede: limpa cache DNS + renova IP ----
        "flush_dns" => {
            let _ = hidden_command("ipconfig.exe").args(["/flushdns"]).status();
            let _ = hidden_command("ipconfig.exe").args(["/registerdns"]).status();
            (true, "DNS limpo + registrado".into())
        }
        // ---- Mensagem na tela do cliente ----
        "message" => {
            let txt: String = arg.chars().filter(|c| *c != '\r' && *c != '\n').take(220).collect();
            if txt.trim().is_empty() { return (false, "mensagem vazia".into()); }
            // msg.exe envia pra todas as sessões interativas. Command::args → texto é 1 argv.
            let _ = hidden_command("msg.exe").args(["*", "/TIME:60", &txt]).status();
            (true, "mensagem enviada ao cliente".into())
        }
        // ---- Limpeza profunda: temp + lixeira + cache Windows Update + prefetch ----
        "deep_clean" => {
            let ps = "Get-ChildItem \"$env:TEMP\",\"$env:SystemRoot\\Temp\",\"$env:SystemRoot\\Prefetch\" -Force -ErrorAction SilentlyContinue | Remove-Item -Recurse -Force -ErrorAction SilentlyContinue; \
                Clear-RecycleBin -Force -ErrorAction SilentlyContinue; \
                Stop-Service wuauserv -Force -ErrorAction SilentlyContinue; \
                Remove-Item \"$env:SystemRoot\\SoftwareDistribution\\Download\\*\" -Recurse -Force -ErrorAction SilentlyContinue; \
                Start-Service wuauserv -ErrorAction SilentlyContinue";
            let _ = hidden_command("powershell.exe").args(["-NoProfile", "-Command", ps]).status();
            (true, "limpeza profunda concluída (temp, lixeira, cache de update, prefetch)".into())
        }
        // ---- Reparo do sistema ----
        "chkdsk" => {
            // /scan = online, não precisa reboot.
            let _ = hidden_command("chkdsk.exe").args(["C:", "/scan"]).status();
            (true, "chkdsk (scan online) executado".into())
        }
        "sfc" => {
            let _ = hidden_command("sfc.exe").args(["/scannow"]).status();
            (true, "sfc /scannow executado".into())
        }
        other => (false, format!("ação desconhecida: {}", other)),
    }
}
#[cfg(not(target_os = "windows"))]
fn exec_remote_action(_action: &str, _arg: &str) -> (bool, String) { (false, "não suportado".into()) }

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
        // O agent roda como SYSTEM e exec_path vem do servidor → se o painel/servidor for
        // comprometido, isto vira execução arbitrária como SYSTEM. Restringe o que pode ser
        // lançado: caminho absoluto, .exe existente, dentro de Program Files / Windows
        // (diretórios protegidos por ACL — só admin/installer escrevem). Bloqueia
        // Temp/Downloads/perfis de usuário, onde um atacante consegue plantar um binário.
        if !watchdog_exec_allowed(exec_path) {
            log::warn!("ConectDesk watchdog: exec_path RECUSADO (fora de Program Files/Windows ou inexistente): {}", exec_path);
            return;
        }
        log::info!("ConectDesk watchdog: relaunch {}", exec_path);
        let _ = hidden_command(exec_path).spawn();
    }
}

#[cfg(target_os = "windows")]
fn watchdog_exec_allowed(exec_path: &str) -> bool {
    let p = std::path::Path::new(exec_path);
    if !p.is_absolute() { return false; }
    // Sem metachars de shell/argumentos embutidos (defesa extra; Command não usa shell, mas
    // evita exec_path com aspas/redirecionamento sendo tratado como programa estranho).
    if exec_path.contains('"') || exec_path.contains('&') || exec_path.contains('|') { return false; }
    let lower = exec_path.to_lowercase();
    if !lower.ends_with(".exe") { return false; }
    if !p.is_file() { return false; }
    // Tem que estar sob um diretório de sistema protegido por ACL.
    let allowed_roots = [
        std::env::var("ProgramFiles").unwrap_or_else(|_| "C:\\Program Files".into()).to_lowercase(),
        std::env::var("ProgramFiles(x86)").unwrap_or_else(|_| "C:\\Program Files (x86)".into()).to_lowercase(),
        std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into()).to_lowercase(),
        "c:\\programdata\\conectdesk".into(),
    ];
    allowed_roots.iter().any(|root| lower.starts_with(root.as_str()))
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

// Reporta progresso de atualização pro painel (POST /api/agents/me/update-status).
// Best-effort — falha silenciosamente. token vazio = sem op (agent ainda não enrollou).
async fn report_update_status(token: &str, status: &str, message: &str) {
    if token.is_empty() { return; }
    let Some(base) = api_base() else { return };
    let Some(client) = http_client(10) else { return };
    let url = format!("{}/api/agents/me/update-status", base);
    let body = json!({"status": status, "message": message});
    let _ = client.post(&url).header("x-agent-token", token).json(&body).send().await;
}

#[cfg(target_os = "windows")]
async fn maybe_update() {
    let token = saved_token();
    let Some(base) = api_base() else { return };
    let Some(client) = http_client(30) else { return };
    report_update_status(&token, "checking", "Verificando fork-latest.yml").await;
    let url = format!("{}/updates/fork-latest.yml", base);
    let body = match client.get(&url).send().await {
        Ok(r) if r.status().is_success() => r.text().await.unwrap_or_default(),
        _ => { report_update_status(&token, "failed", "Falha ao buscar feed").await; return; }
    };
    let Some((remote_build, remote_version, exe, sha512)) = parse_fork_feed(&body) else {
        report_update_status(&token, "failed", "Feed inválido").await;
        return;
    };
    let local = local_build_id();
    if remote_build <= local {
        log::debug!("ConectDesk update: up-to-date (local={}, remote={})", local, remote_build);
        report_update_status(&token, "done", &format!("Já está em {} ({})", remote_version, local)).await;
        return;
    }
    log::info!("ConectDesk update: {} -> {} (downloading {})", local, remote_version, exe);
    report_update_status(&token, "downloading", &format!("Baixando {} (build {})", remote_version, remote_build)).await;
    let url_safe = exe.replace(' ', "%20");
    let dl_url = format!("{}/updates/{}", base, url_safe);
    let bytes = match client.get(&dl_url).send().await {
        Ok(r) if r.status().is_success() => match r.bytes().await { Ok(b) => b, Err(_) => { report_update_status(&token, "failed", "Falha ao ler corpo do download").await; return; } },
        _ => { log::warn!("ConectDesk update: download failed"); report_update_status(&token, "failed", "Download falhou").await; return; }
    };
    // Verifica integridade: SHA-512 do download tem que bater com o do feed. Sem isto, um MITM no
    // feed (TLS aceita cert inválido) poderia entregar um .exe malicioso instalado como SYSTEM.
    // Se o feed traz sha512, é OBRIGATÓRIO bater; se não traz, recusa (fail-closed) — todo publish
    // do publish-fork.sh inclui o hash.
    match sha512.as_deref() {
        Some(expected) => {
            use sha2::{Digest, Sha512};
            let mut hasher = Sha512::new();
            hasher.update(&bytes);
            let got = hasher.finalize();
            let got_hex = got.iter().map(|b| format!("{:02x}", b)).collect::<String>();
            if !got_hex.eq_ignore_ascii_case(expected.trim()) {
                log::error!("ConectDesk update: SHA-512 NÃO bate (esperado {}, obtido {})", expected, got_hex);
                report_update_status(&token, "failed", "Integridade do download falhou (SHA-512) — atualização abortada").await;
                return;
            }
            log::info!("ConectDesk update: SHA-512 verificado OK");
        }
        None => {
            log::error!("ConectDesk update: feed sem sha512 — abortando (fail-closed)");
            report_update_status(&token, "failed", "Feed sem checksum — atualização recusada").await;
            return;
        }
    }
    let tmp = std::env::temp_dir().join(format!("ConectDesk-Update-{}.exe", remote_build));
    if std::fs::write(&tmp, &bytes).is_err() {
        log::error!("ConectDesk update: failed to write {:?}", tmp);
        report_update_status(&token, "failed", "Falha ao gravar instalador temporário").await;
        return;
    }
    report_update_status(&token, "installing", &format!("Instalando {} (silent)", remote_version)).await;
    // Detached PowerShell: stop service, install silent, restart. The current process gets killed
    // mid-install when the service stops — that's expected. The detached child survives and finishes.
    // O binário é o portátil do fork — ele instala com `--silent-install` (copia pro Program
    // Files + registra o serviço). O `/S` (flag NSIS) era IGNORADO → o exe rodava como app e o
    // serviço NUNCA atualizava (download ok, update não aplicava). Esse era o motivo das máquinas
    // não auto-atualizarem. `--silent-install` (= o que o instalador manual usa) corrige.
    //
    // Blindagem pós-install (PDV não pode ficar sem serviço): depois do --silent-install, um loop
    // de até ~30s garante o serviço Running — se o install_me copiou mas não iniciou (sc start
    // falhou/timeout), o Start-Service recupera. O processo atual morre quando o serviço para
    // durante o install; o PowerShell é detached (Start-Process) e sobrevive pra fazer essa
    // verificação. Sucesso volta naturalmente: o novo serviço sobe → heartbeat → painel online.
    let ps = format!(
        "Start-Process powershell -WindowStyle Hidden -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-Command',\"& '{}' --silent-install; Start-Sleep -Seconds 12; for ($i=0; $i -lt 6; $i++){{ $s=Get-Service ConectDesk -ErrorAction SilentlyContinue; if($s -and $s.Status -eq 'Running'){{break}}; Start-Service ConectDesk -ErrorAction SilentlyContinue; Start-Sleep -Seconds 5 }}\"",
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
    let resp = match client.get(&url).header("x-agent-token", token).send().await {
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

    // ProgramData (world-readable) — mesmo motivo da foto: agent=SYSTEM grava, widget=user lê.
    let dir = cd_shared_dir();
    if std::fs::create_dir_all(&dir).is_err() { return; }
    let path = dir.join("branding.png");
    if std::fs::write(&path, &bytes).is_err() { return; }
    Config::set_option(BRAND_LOGO_PATH_KEY.to_string(), path.to_string_lossy().to_string());
    Config::set_option(BRANDING_TS_KEY.to_string(), updated_at.to_string());
    log::info!("ConectDesk branding: sync ok ({} bytes, updated_at={})", bytes.len(), updated_at);
}

// Grava os NOMES dos técnicos na hora (do heartbeat), sem esperar o download das fotos. Garante
// que o widget mostre nome mesmo se /photos demorar ou falhar. photoPath fica vazio até a foto vir.
fn set_sessions_names(sess: &[(String, String)], stamp: &str) {
    Config::set_option(ACTIVE_SESSION_ID_KEY.to_string(), stamp.to_string());
    let out: Vec<Value> = sess.iter().map(|(_, name)| json!({"name": name, "photoPath": ""})).collect();
    let first = sess.first().map(|(_, n)| n.clone()).unwrap_or_default();
    Config::set_option(ACTIVE_SESSIONS_JSON_KEY.to_string(), serde_json::to_string(&out).unwrap_or("[]".into()));
    Config::set_option(ACTIVE_SESSION_TECH_NAME_KEY.to_string(), first);
    Config::set_option(ACTIVE_SESSION_TECH_PHOTO_PATH_KEY.to_string(), String::new());
}

// Enriquece com as fotos: baixa /photos, grava uma foto por sessão (session_tech_<id8>.png em
// ProgramData) e regrava o JSON com photoPath. Casa por technician (a resposta pode vir noutra
// ordem). Se /photos falhar, os nomes já setados por set_sessions_names continuam valendo.
async fn sync_active_sessions(token: &str, stamp: &str, sess: &[(String, String)]) {
    let Some(base) = api_base() else { return };
    let Some(client) = http_client(20) else { return };
    let url = format!("{}/api/agents/me/active-session/photos", base);
    let resp = match client.get(&url).header("x-agent-token", token).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => return,
    };
    let v: Value = match resp.json().await { Ok(v) => v, Err(_) => return };
    let sessions = match v.get("sessions").and_then(|x| x.as_array()) {
        Some(a) => a,
        None => return,
    };

    let dir = cd_shared_dir();
    let _ = std::fs::create_dir_all(&dir);
    let mut out: Vec<Value> = Vec::new();
    let mut first_photo = String::new();
    for s in sessions.iter() {
        let sid = s.get("sessionId").and_then(|x| x.as_str()).unwrap_or("");
        let name = s.get("technician").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let photo = s.get("photo").and_then(|x| x.as_str()).unwrap_or("");
        let mut photo_path = String::new();
        if !photo.is_empty() {
            let b64 = match photo.split_once("base64,") { Some((_, r)) => r.trim(), None => photo.trim() };
            if let Some(bytes) = base64_decode(b64) {
                let id8: String = sid.chars().take(8).filter(|c| c.is_alphanumeric()).collect();
                let p = dir.join(format!("session_tech_{}.png", id8));
                if std::fs::write(&p, &bytes).is_ok() {
                    photo_path = p.to_string_lossy().to_string();
                }
            }
        }
        if first_photo.is_empty() && !photo_path.is_empty() { first_photo = photo_path.clone(); }
        out.push(json!({"name": name, "photoPath": photo_path}));
    }
    // Se /photos não trouxe nada útil, mantém os nomes já gravados.
    if out.is_empty() { let _ = stamp; let _ = sess; return; }

    Config::set_option(ACTIVE_SESSIONS_JSON_KEY.to_string(), serde_json::to_string(&out).unwrap_or("[]".into()));
    if let Some(first) = out.first().and_then(|o| o.get("name")).and_then(|n| n.as_str()) {
        Config::set_option(ACTIVE_SESSION_TECH_NAME_KEY.to_string(), first.to_string());
    }
    Config::set_option(ACTIVE_SESSION_TECH_PHOTO_PATH_KEY.to_string(), first_photo);
    log::info!("ConectDesk: {} técnico(s) ativo(s) sincronizado(s)", out.len());
}

fn clear_active_session() {
    Config::set_option(ACTIVE_SESSION_ID_KEY.to_string(), String::new());
    Config::set_option(ACTIVE_SESSION_TECH_NAME_KEY.to_string(), String::new());
    Config::set_option(ACTIVE_SESSION_TECH_PHOTO_PATH_KEY.to_string(), String::new());
    Config::set_option(ACTIVE_SESSIONS_JSON_KEY.to_string(), String::new());
}

// Histórico de sessões — GET /api/agents/me/sessions retorna últimas 50 sessões.
// Salvamos em option como JSON minificado pra Flutter UI consumir via mainGetOption.
// Mantemos só campos pequenos (id, técnico, started_at, ended_at, reason) — sem
// technician_photo (esse é base64 e estoura option grande).
async fn sync_session_history(token: &str) {
    let Some(base) = api_base() else { return };
    let Some(client) = http_client(15) else { return };
    let url = format!("{}/api/agents/me/sessions", base);
    let resp = match client.get(&url).header("x-agent-token", token).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => return,
    };
    let v: Value = match resp.json().await { Ok(v) => v, Err(_) => return };
    let sessions = match v.get("sessions").and_then(|x| x.as_array()) {
        Some(arr) => arr,
        None => return,
    };
    let trimmed: Vec<Value> = sessions.iter().take(15).map(|s| {
        let mut obj = serde_json::Map::new();
        if let Some(x) = s.get("id") { obj.insert("id".into(), x.clone()); }
        if let Some(x) = s.get("technician") { obj.insert("technician".into(), x.clone()); }
        if let Some(x) = s.get("reason") { obj.insert("reason".into(), x.clone()); }
        if let Some(x) = s.get("created_at") { obj.insert("created_at".into(), x.clone()); }
        if let Some(x) = s.get("ended_at") { obj.insert("ended_at".into(), x.clone()); }
        if let Some(x) = s.get("client_ip") { obj.insert("client_ip".into(), x.clone()); }
        Value::Object(obj)
    }).collect();
    let json = serde_json::to_string(&trimmed).unwrap_or("[]".into());
    Config::set_option(SESSION_HISTORY_KEY.to_string(), json);
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
                    // "Coletar agora" do painel: força sysinfo leve + bloco pesado de saúde na hora.
                    if resp.get("refreshSysinfo").and_then(|v| v.as_bool()).unwrap_or(false) {
                        log::info!("ConectDesk: refreshSysinfo=true — coletando sysinfo + saúde agora");
                        let _ = send_sysinfo(&token).await;
                        let _ = send_health(&token).await;
                    }
                    // Comandos remotos enfileirados pelo painel (reboot / limpar temp / matar processo).
                    if let Some(cmds) = resp.get("commands").and_then(|v| v.as_array()) {
                        for c in cmds { run_remote_command(&token, c).await; }
                    }
                    // Sessões ativas (multi-técnico): mostra TODOS os técnicos conectados.
                    // Usa activeSessions (lista); cai pra activeSession (singular) em servidores
                    // antigos. Quando há sessões, ressincroniza nomes+fotos; quando some, limpa.
                    let sessions_list = resp.get("activeSessions").and_then(|v| v.as_array());
                    let has_any = match (sessions_list, resp.get("activeSession")) {
                        (Some(a), _) => !a.is_empty(),
                        (None, Some(s)) => !s.is_null(),
                        _ => false,
                    };
                    if has_any {
                        // Lista [(id, technician)] do heartbeat. Funciona com activeSessions
                        // (plural) ou activeSession (singular, server antigo).
                        let mut sess: Vec<(String, String)> = vec![];
                        if let Some(a) = sessions_list {
                            for s in a {
                                let id = s.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                let tech = s.get("technician").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                if !id.is_empty() { sess.push((id, tech)); }
                            }
                        } else if let Some(s) = resp.get("activeSession") {
                            let id = s.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            let tech = s.get("technician").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            if !id.is_empty() { sess.push((id, tech)); }
                        }
                        let stamp = sess.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>().join(",");
                        // Nome aparece NA HORA (do heartbeat), foto vem depois via /photos.
                        let last_stamp = Config::get_option(ACTIVE_SESSION_ID_KEY);
                        if stamp != last_stamp {
                            set_sessions_names(&sess, &stamp);
                        }
                        // Re-sincroniza fotos enquanto algum técnico ainda não tem foto local.
                        let has_photos = !Config::get_option(ACTIVE_SESSIONS_JSON_KEY).is_empty()
                            && Config::get_option(ACTIVE_SESSION_TECH_PHOTO_PATH_KEY).len() > 0;
                        if !stamp.is_empty() && (stamp != last_stamp || !has_photos) {
                            sync_active_sessions(&token, &stamp, &sess).await;
                        }
                    } else if !Config::get_option(ACTIVE_SESSION_ID_KEY).is_empty() {
                        clear_active_session();
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
            // Saúde/segurança/rede/periféricos a cada 5h (e no primeiro tick após subir).
            // tick=0 já cobre o boot; depois a cada 5h. (Sem `tick==1` redundante que disparava 2x.)
            if tick * HEARTBEAT_INTERVAL_SECS % HEALTH_INTERVAL_SECS == 0 {
                let _ = send_health(&token).await;
            }
            // Update agora é ON-DEMAND (vem via heartbeat resp.requestUpdate quando o painel
            // marca). Removido o auto-loop de 30min — gerava re-install eterno porque o
            // publish-fork.sh usava timestamp do publish, diferente do build_id compilado.
            // Branding (logo + nome empresa do cliente) a cada 5min.
            if tick * HEARTBEAT_INTERVAL_SECS % BRANDING_INTERVAL_SECS == 0 {
                sync_branding(&token).await;
            }
            // Histórico de sessões (cd_session_history option) — mesma cadência do branding.
            if tick * HEARTBEAT_INTERVAL_SECS % SESSION_HISTORY_INTERVAL_SECS == 0 {
                sync_session_history(&token).await;
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
