use chrono::{DateTime, Local};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Mutex, OnceLock,
    },
    time::{Duration, Instant, UNIX_EPOCH},
};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, State, WebviewWindow,
};
use walkdir::WalkDir;

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Usage {
    input: u64,
    cached: u64,
    output: u64,
    reasoning: u64,
    total: u64,
    sessions: u64,
    cost_usd: f64,
}

impl Usage {
    fn add(&mut self, other: &Usage) {
        self.input = self.input.saturating_add(other.input);
        self.cached = self.cached.saturating_add(other.cached);
        self.output = self.output.saturating_add(other.output);
        self.reasoning = self.reasoning.saturating_add(other.reasoning);
        self.total = self.total.saturating_add(other.total);
        self.sessions = self.sessions.saturating_add(other.sessions);
        self.cost_usd += other.cost_usd;
    }
}

#[derive(Clone, Default)]
struct ParsedFile {
    days: BTreeMap<String, Usage>,
    project: String,
    model: String,
    session_id: String,
    modified: u64,
}

#[derive(Default)]
struct SourceCache {
    files: HashMap<PathBuf, ParsedFile>,
}

#[derive(Default)]
struct UsageCache {
    codex: SourceCache,
    claude: SourceCache,
}

#[derive(Default)]
struct OatCache {
    checked_at: Option<Instant>,
    snapshot: Option<OatSnapshot>,
}

struct OatKeyEntry {
    id: String,
    label: String,
    token: String,
}

struct PinState(AtomicBool);

struct DockState {
    docked: AtomicBool,
    edge: AtomicU8,
}

impl Default for DockState {
    fn default() -> Self {
        Self {
            docked: AtomicBool::new(false),
            edge: AtomicU8::new(0),
        }
    }
}

const EDGE_LEFT: u8 = 1;
const EDGE_RIGHT: u8 = 2;
const EDGE_TOP: u8 = 3;
const EDGE_BOTTOM: u8 = 4;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HistoryPoint {
    date: String,
    total: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentSnapshot {
    input: u64,
    cached: u64,
    output: u64,
    reasoning: u64,
    total: u64,
    sessions: u64,
    project: String,
    model: String,
    session_id: String,
    history: Vec<HistoryPoint>,
    cost_usd: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    date: String,
    updated_at: String,
    codex: AgentSnapshot,
    claude: AgentSnapshot,
    total: u64,
    status: &'static str,
    cost_usd: f64,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct OatKeyStatus {
    id: String,
    label: String,
    active: bool,
    available: bool,
    five_hour_remaining: Option<f64>,
    seven_day_remaining: Option<f64>,
    detail: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct OatSnapshot {
    active_mode: String,
    checked_at: String,
    keys: Vec<OatKeyStatus>,
}

#[derive(Serialize)]
struct Settings {
    pinned: bool,
    expanded: bool,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelPrice {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
}

fn prices() -> &'static HashMap<String, ModelPrice> {
    static PRICES: OnceLock<HashMap<String, ModelPrice>> = OnceLock::new();
    PRICES.get_or_init(|| serde_json::from_str(include_str!("../pricing.json")).unwrap_or_default())
}

fn estimate_cost(model: &str, input: u64, cache_read: u64, cache_write: u64, output: u64) -> f64 {
    let Some(price) = prices().get(model) else {
        return 0.0;
    };
    input as f64 * price.input
        + cache_read as f64 * price.cache_read
        + cache_write as f64 * price.cache_write
        + output as f64 * price.output
}

fn string_at<'a>(value: &'a Value, path: &[&str]) -> &'a str {
    let mut current = value;
    for key in path {
        current = &current[*key];
    }
    current.as_str().unwrap_or_default()
}

fn number_at(value: &Value, path: &[&str]) -> u64 {
    let mut current = value;
    for key in path {
        current = &current[*key];
    }
    current.as_u64().unwrap_or(0)
}

fn day_at(value: &Value) -> Option<String> {
    let timestamp = value.get("timestamp")?.as_str()?;
    DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|date| date.with_timezone(&Local).format("%Y-%m-%d").to_string())
}

fn modified(path: &Path) -> u64 {
    path.metadata()
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn parse_codex(path: &Path) -> std::io::Result<ParsedFile> {
    let file = File::open(path)?;
    let mut result = ParsedFile {
        modified: modified(path),
        session_id: path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        ..Default::default()
    };
    let mut session_day = None;
    let mut has_usage = false;

    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(row) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let row_type = row["type"].as_str().unwrap_or_default();
        if row_type == "session_meta" {
            result.project = string_at(&row, &["payload", "cwd"]).to_string();
            result.model = string_at(&row, &["payload", "model"]).to_string();
            let id = string_at(&row, &["payload", "id"]);
            if !id.is_empty() {
                result.session_id = id.to_string();
            }
            session_day = day_at(&row).or(session_day);
            continue;
        }
        if row_type == "turn_context" {
            let model = string_at(&row, &["payload", "model"]);
            if !model.is_empty() {
                result.model = model.to_string();
            }
            continue;
        }
        if row_type != "event_msg" || string_at(&row, &["payload", "type"]) != "token_count" {
            continue;
        }
        let Some(day) = day_at(&row) else { continue };
        let input_all = number_at(
            &row,
            &["payload", "info", "last_token_usage", "input_tokens"],
        );
        let cached = number_at(
            &row,
            &["payload", "info", "last_token_usage", "cached_input_tokens"],
        );
        let output = number_at(
            &row,
            &["payload", "info", "last_token_usage", "output_tokens"],
        );
        let reasoning = number_at(
            &row,
            &[
                "payload",
                "info",
                "last_token_usage",
                "reasoning_output_tokens",
            ],
        );
        let total = number_at(
            &row,
            &["payload", "info", "last_token_usage", "total_tokens"],
        );
        if total == 0 {
            continue;
        }
        result.days.entry(day).or_default().add(&Usage {
            input: input_all.saturating_sub(cached),
            cached,
            output,
            reasoning,
            total,
            sessions: 0,
            cost_usd: estimate_cost(
                &result.model,
                input_all.saturating_sub(cached),
                cached,
                0,
                output,
            ),
        });
        has_usage = true;
    }
    if has_usage {
        if let Some(day) = session_day {
            result.days.entry(day).or_default().sessions += 1;
        }
    }
    Ok(result)
}

fn parse_claude(path: &Path) -> std::io::Result<ParsedFile> {
    let file = File::open(path)?;
    let mut result = ParsedFile {
        modified: modified(path),
        session_id: path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        ..Default::default()
    };
    let mut seen = HashSet::new();
    let mut session_day = None;
    let mut has_usage = false;

    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(row) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if row["type"].as_str() != Some("assistant") || row["message"]["usage"].is_null() {
            continue;
        }
        let key = [
            string_at(&row, &["message", "id"]),
            string_at(&row, &["requestId"]),
            string_at(&row, &["uuid"]),
        ]
        .into_iter()
        .find(|value| !value.is_empty())
        .unwrap_or_default();
        if !key.is_empty() && !seen.insert(key.to_string()) {
            continue;
        }
        let Some(day) = day_at(&row) else { continue };
        let input = number_at(&row, &["message", "usage", "input_tokens"]);
        let cache_read = number_at(&row, &["message", "usage", "cache_read_input_tokens"]);
        let cache_write = number_at(&row, &["message", "usage", "cache_creation_input_tokens"]);
        let cached = cache_read.saturating_add(cache_write);
        let output = number_at(&row, &["message", "usage", "output_tokens"]);
        let total = input.saturating_add(cached).saturating_add(output);
        result.days.entry(day.clone()).or_default().add(&Usage {
            input,
            cached,
            output,
            reasoning: 0,
            total,
            sessions: 0,
            cost_usd: estimate_cost(
                string_at(&row, &["message", "model"]),
                input,
                cache_read,
                cache_write,
                output,
            ),
        });
        result.project = string_at(&row, &["cwd"]).to_string();
        result.model = string_at(&row, &["message", "model"]).to_string();
        let id = string_at(&row, &["sessionId"]);
        if !id.is_empty() {
            result.session_id = id.to_string();
        }
        session_day.get_or_insert(day);
        has_usage = true;
    }
    if has_usage {
        if let Some(day) = session_day {
            result.days.entry(day).or_default().sessions += 1;
        }
    }
    Ok(result)
}

fn jsonl_files(root: &Path) -> Vec<PathBuf> {
    if !root.exists() {
        return Vec::new();
    }
    WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_type().is_file()
                && entry.path().extension().is_some_and(|ext| ext == "jsonl")
        })
        .map(|entry| entry.into_path())
        .collect()
}

fn scan_source(
    source: &mut SourceCache,
    root: &Path,
    parser: fn(&Path) -> std::io::Result<ParsedFile>,
) {
    let files = jsonl_files(root);
    let live: HashSet<_> = files.iter().cloned().collect();
    for path in files {
        let stamp = modified(&path);
        if source
            .files
            .get(&path)
            .is_some_and(|cached| cached.modified == stamp)
        {
            continue;
        }
        if let Ok(parsed) = parser(&path) {
            source.files.insert(path, parsed);
        }
    }
    source.files.retain(|path, _| live.contains(path));
}

fn project_name(value: &str) -> String {
    if value.is_empty() {
        return "No active project".to_string();
    }
    Path::new(value)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| value.to_string())
}

fn summarize(source: &SourceCache, today: &str) -> AgentSnapshot {
    let mut current = Usage::default();
    let mut history: BTreeMap<String, Usage> = BTreeMap::new();
    let latest = source.files.values().max_by_key(|entry| entry.modified);
    for file in source.files.values() {
        for (day, usage) in &file.days {
            history.entry(day.clone()).or_default().add(usage);
            if day == today {
                current.add(usage);
            }
        }
    }
    let history = history
        .into_iter()
        .rev()
        .take(7)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|(date, usage)| HistoryPoint {
            date,
            total: usage.total,
        })
        .collect();
    AgentSnapshot {
        input: current.input,
        cached: current.cached,
        output: current.output,
        reasoning: current.reasoning,
        total: current.total,
        sessions: current.sessions,
        project: project_name(latest.map(|item| item.project.as_str()).unwrap_or_default()),
        model: latest
            .map(|item| item.model.clone())
            .filter(|item| !item.is_empty())
            .unwrap_or_else(|| "Unknown model".to_string()),
        session_id: latest
            .map(|item| item.session_id.clone())
            .unwrap_or_default(),
        history,
        cost_usd: current.cost_usd,
    }
}

fn home_dir() -> Result<PathBuf, String> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .ok_or_else(|| "Home directory is unavailable".to_string())
}

fn parse_shared_oat_keys(contents: &str) -> Vec<OatKeyEntry> {
    contents
        .lines()
        .filter_map(|line| {
            let value = line.trim().trim_matches('"');
            let (label, token) = value.split_once(':')?;
            if label.is_empty() || !token.starts_with("sk-ant-oat") {
                return None;
            }
            Some((label.to_string(), token.to_string()))
        })
        .enumerate()
        .map(|(index, (label, token))| OatKeyEntry {
            id: (index + 1).to_string(),
            label,
            token,
        })
        .collect()
}

fn parse_local_oat_key(contents: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        let (_, value) = line.split_once('=')?;
        let token = value.trim().trim_matches(['"', '\'']);
        token.starts_with("sk-ant-oat").then(|| token.to_string())
    })
}

fn oat_paths(home: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let root = std::env::var_os("CLAUDE_OAT_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude-oat-switch"));
    let local = std::env::var_os("CLAUDE_OAT_LOCAL_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude-oat-local"));
    let mode = std::env::var_os("CLAUDE_OAT_MODE_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude-oat-mode"));
    (root.join("keys.sh"), local, mode)
}

fn load_oat_entries(home: &Path) -> Result<Option<(Vec<OatKeyEntry>, String)>, String> {
    let (shared_path, local_path, mode_path) = oat_paths(home);
    if !shared_path.is_file() {
        return Ok(None);
    }
    let shared = fs::read_to_string(&shared_path)
        .map_err(|error| format!("Could not read cckey configuration: {error}"))?;
    let mut entries = parse_shared_oat_keys(&shared);
    if let Ok(local) = fs::read_to_string(local_path) {
        if let Some(token) = parse_local_oat_key(&local) {
            entries.push(OatKeyEntry {
                id: "mine".to_string(),
                label: "Your key".to_string(),
                token,
            });
        }
    }
    let active_mode = fs::read_to_string(mode_path)
        .unwrap_or_else(|_| "1".to_string())
        .trim()
        .to_string();
    Ok(Some((entries, active_mode)))
}

fn remaining_percent(value: Option<&reqwest::header::HeaderValue>) -> Option<f64> {
    let utilization = value?.to_str().ok()?.parse::<f64>().ok()?;
    Some(((1.0 - utilization).clamp(0.0, 1.0) * 100.0).round())
}

fn probe_oat_key(
    client: &reqwest::blocking::Client,
    entry: OatKeyEntry,
    active: bool,
) -> OatKeyStatus {
    let response = client
        .post("https://api.anthropic.com/v1/messages")
        .bearer_auth(&entry.token)
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("anthropic-version", "2023-06-01")
        .json(&serde_json::json!({
            "model": "claude-haiku-4-5-20251001",
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "OK"}]
        }))
        .send();
    match response {
        Ok(response) if response.status().is_success() => {
            let headers = response.headers();
            OatKeyStatus {
                id: entry.id,
                label: entry.label,
                active,
                available: true,
                five_hour_remaining: remaining_percent(
                    headers.get("anthropic-ratelimit-unified-5h-utilization"),
                ),
                seven_day_remaining: remaining_percent(
                    headers.get("anthropic-ratelimit-unified-7d-utilization"),
                ),
                detail: "Available".to_string(),
            }
        }
        Ok(response) => OatKeyStatus {
            id: entry.id,
            label: entry.label,
            active,
            available: false,
            five_hour_remaining: None,
            seven_day_remaining: None,
            detail: format!("Unreachable (HTTP {})", response.status().as_u16()),
        },
        Err(error) => OatKeyStatus {
            id: entry.id,
            label: entry.label,
            active,
            available: false,
            five_hour_remaining: None,
            seven_day_remaining: None,
            detail: if error.is_timeout() {
                "Timed out".to_string()
            } else {
                "Unreachable".to_string()
            },
        },
    }
}

fn collect_oat_status(app: &AppHandle, force: bool) -> Result<Option<OatSnapshot>, String> {
    let home = home_dir()?;
    let Some((entries, active_mode)) = load_oat_entries(&home)? else {
        return Ok(None);
    };
    let cache_state = app.state::<Mutex<OatCache>>();
    {
        let cache = cache_state
            .lock()
            .map_err(|_| "OAT status cache lock failed")?;
        if !force
            && cache
                .checked_at
                .is_some_and(|checked| checked.elapsed() < Duration::from_secs(300))
        {
            return Ok(cache.snapshot.clone());
        }
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(12))
        .user_agent("Agent-Meter/0.6")
        .build()
        .map_err(|_| "Could not initialize the OAT quota checker")?;
    let keys = entries
        .into_iter()
        .map(|entry| {
            let active = entry.id == active_mode;
            probe_oat_key(&client, entry, active)
        })
        .collect();
    let snapshot = OatSnapshot {
        active_mode,
        checked_at: Local::now().to_rfc3339(),
        keys,
    };
    let mut cache = cache_state
        .lock()
        .map_err(|_| "OAT status cache lock failed")?;
    cache.checked_at = Some(Instant::now());
    cache.snapshot = Some(snapshot.clone());
    Ok(Some(snapshot))
}

#[tauri::command]
async fn get_oat_status(app: AppHandle, force: bool) -> Result<Option<OatSnapshot>, String> {
    tauri::async_runtime::spawn_blocking(move || collect_oat_status(&app, force))
        .await
        .map_err(|error| format!("OAT status task failed: {error}"))?
}

#[tauri::command]
fn get_usage(cache: State<'_, Mutex<UsageCache>>) -> Result<Snapshot, String> {
    let home = home_dir()?;
    let mut cache = cache.lock().map_err(|_| "Usage cache lock failed")?;
    scan_source(
        &mut cache.codex,
        &home.join(".codex").join("sessions"),
        parse_codex,
    );
    scan_source(
        &mut cache.claude,
        &home.join(".claude").join("projects"),
        parse_claude,
    );
    let today = Local::now().format("%Y-%m-%d").to_string();
    let codex = summarize(&cache.codex, &today);
    let claude = summarize(&cache.claude, &today);
    let total = codex.total.saturating_add(claude.total);
    let cost_usd = codex.cost_usd + claude.cost_usd;
    Ok(Snapshot {
        date: today,
        updated_at: Local::now().to_rfc3339(),
        codex,
        claude,
        total,
        status: if total > 0 { "live" } else { "empty" },
        cost_usd,
    })
}

#[tauri::command]
fn hide_window(window: WebviewWindow) -> Result<(), String> {
    window.hide().map_err(|error| error.to_string())
}

#[tauri::command]
fn toggle_pin(window: WebviewWindow, pin_state: State<'_, PinState>) -> Result<bool, String> {
    let next = !pin_state.0.load(Ordering::SeqCst);
    window
        .set_always_on_top(next)
        .map_err(|error| error.to_string())?;
    pin_state.0.store(next, Ordering::SeqCst);
    Ok(next)
}

#[tauri::command]
fn get_settings(window: WebviewWindow, pin_state: State<'_, PinState>) -> Result<Settings, String> {
    Ok(Settings {
        pinned: pin_state.0.load(Ordering::SeqCst),
        expanded: window
            .inner_size()
            .map_err(|error| error.to_string())?
            .height
            > 300,
    })
}

#[tauri::command]
fn toggle_expanded(window: WebviewWindow) -> Result<bool, String> {
    let expanded = window
        .inner_size()
        .map_err(|error| error.to_string())?
        .height
        > 300;
    if expanded {
        window
            .set_size(tauri::LogicalSize::new(330.0, 146.0))
            .map_err(|error| error.to_string())?;
    } else {
        set_expanded_size(&window, 690.0)?;
    }
    Ok(!expanded)
}

fn set_expanded_size(window: &WebviewWindow, desired_height: f64) -> Result<(), String> {
    let Some(monitor) = window
        .current_monitor()
        .map_err(|error| error.to_string())?
    else {
        return window
            .set_size(tauri::LogicalSize::new(392.0, desired_height))
            .map_err(|error| error.to_string());
    };
    let scale = monitor.scale_factor();
    let work = monitor.work_area();
    let margin = (16.0 * scale).round() as i32;
    let available_height = (work.size.height as f64 / scale - 32.0).max(420.0);
    let height = desired_height.min(available_height);
    window
        .set_size(tauri::LogicalSize::new(392.0, height))
        .map_err(|error| error.to_string())?;

    let position = window.outer_position().map_err(|error| error.to_string())?;
    let size = window.outer_size().map_err(|error| error.to_string())?;
    let min_x = work.position.x + margin;
    let min_y = work.position.y + margin;
    let max_x = (work.position.x + work.size.width as i32 - size.width as i32 - margin).max(min_x);
    let max_y =
        (work.position.y + work.size.height as i32 - size.height as i32 - margin).max(min_y);
    let fitted = PhysicalPosition::new(
        position.x.clamp(min_x, max_x),
        position.y.clamp(min_y, max_y),
    );
    if fitted != position {
        window
            .set_position(fitted)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn set_oat_panel_visible(window: WebviewWindow, visible: bool) -> Result<(), String> {
    let expanded = window
        .inner_size()
        .map_err(|error| error.to_string())?
        .height
        > 300;
    if expanded {
        set_expanded_size(&window, if visible { 790.0 } else { 690.0 })?;
    }
    Ok(())
}

fn handle_window_moved(window: &tauri::Window, position: PhysicalPosition<i32>) {
    let dock_state = window.state::<DockState>();
    if !dock_state.docked.load(Ordering::SeqCst)
        && window.inner_size().is_ok_and(|size| size.height > 300)
    {
        return;
    }
    let Ok(Some(monitor)) = window.monitor_from_point(position.x as f64, position.y as f64) else {
        return;
    };
    let work = monitor.work_area();
    let Ok(size) = window.outer_size() else {
        return;
    };
    let scale = monitor.scale_factor();
    let snap_threshold = (14.0 * scale).round() as i32;
    let release_threshold = (42.0 * scale).round() as i32;
    let left = work.position.x;
    let top = work.position.y;
    let right = left + work.size.width as i32;
    let bottom = top + work.size.height as i32;
    let window_right = position.x + size.width as i32;
    let window_bottom = position.y + size.height as i32;
    let distances = [
        (EDGE_LEFT, (position.x - left).abs()),
        (EDGE_RIGHT, (right - window_right).abs()),
        (EDGE_TOP, (position.y - top).abs()),
        (EDGE_BOTTOM, (bottom - window_bottom).abs()),
    ];

    if !dock_state.docked.load(Ordering::SeqCst) {
        let Some((edge, _)) = distances
            .into_iter()
            .filter(|(_, distance)| *distance <= snap_threshold)
            .min_by_key(|(_, distance)| *distance)
        else {
            return;
        };
        let dock_width = (168.0 * scale).round() as u32;
        let dock_height = (48.0 * scale).round() as u32;
        let max_x = right - dock_width as i32;
        let max_y = bottom - dock_height as i32;
        let x = match edge {
            EDGE_LEFT => left,
            EDGE_RIGHT => max_x,
            _ => position.x.clamp(left, max_x),
        };
        let y = match edge {
            EDGE_TOP => top,
            EDGE_BOTTOM => max_y,
            _ => position.y.clamp(top, max_y),
        };
        dock_state.docked.store(true, Ordering::SeqCst);
        dock_state.edge.store(edge, Ordering::SeqCst);
        let _ = window.set_size(PhysicalSize::new(dock_width, dock_height));
        let _ = window.set_position(PhysicalPosition::new(x, y));
        let _ = window.emit("edge-docked", true);
        return;
    }

    if distances
        .iter()
        .any(|(_, distance)| *distance <= release_threshold)
    {
        return;
    }

    let compact_width = (330.0 * scale).round() as u32;
    let compact_height = (146.0 * scale).round() as u32;
    let inset = (48.0 * scale).round() as i32;
    let edge = dock_state.edge.load(Ordering::SeqCst);
    let max_x = right - compact_width as i32;
    let max_y = bottom - compact_height as i32;
    let x = match edge {
        EDGE_LEFT => (left + inset).min(max_x),
        EDGE_RIGHT => (max_x - inset).max(left),
        _ => position.x.clamp(left, max_x),
    };
    let y = match edge {
        EDGE_TOP => (top + inset).min(max_y),
        EDGE_BOTTOM => (max_y - inset).max(top),
        _ => position.y.clamp(top, max_y),
    };
    dock_state.docked.store(false, Ordering::SeqCst);
    let _ = window.set_size(PhysicalSize::new(compact_width, compact_height));
    let _ = window.set_position(PhysicalPosition::new(x, y));
    let _ = window.emit("edge-docked", false);
}

#[tauri::command]
fn check_edge_docking(window: tauri::Window) {
    if let Ok(position) = window.outer_position() {
        handle_window_moved(&window, position);
    }
}

fn show(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

pub fn run() {
    tauri::Builder::default()
        .manage(Mutex::new(UsageCache::default()))
        .manage(Mutex::new(OatCache::default()))
        .manage(PinState(AtomicBool::new(true)))
        .manage(DockState::default())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .invoke_handler(tauri::generate_handler![
            get_usage,
            hide_window,
            toggle_pin,
            get_settings,
            toggle_expanded,
            get_oat_status,
            set_oat_panel_visible,
            check_edge_docking
        ])
        .setup(|app| {
            let show_item = MenuItem::with_id(app, "show", "Show Agent Meter", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_item, &quit_item])?;
            TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().expect("application icon").clone())
                .tooltip("Agent Meter")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if matches!(
                        event,
                        TrayIconEvent::Click {
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            ..
                        }
                    ) {
                        show(tray.app_handle());
                    }
                })
                .build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| match event {
            tauri::WindowEvent::Moved(position) => handle_window_moved(window, *position),
            tauri::WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                let _ = window.hide();
            }
            _ => {}
        })
        .run(tauri::generate_context!())
        .expect("error while running Agent Meter");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_add_saturates_and_sums() {
        let mut usage = Usage {
            input: 1,
            cached: 2,
            output: 3,
            reasoning: 4,
            total: 10,
            sessions: 1,
            cost_usd: 1.0,
        };
        usage.add(&Usage {
            input: 4,
            cached: 3,
            output: 2,
            reasoning: 1,
            total: 10,
            sessions: 2,
            cost_usd: 2.0,
        });
        assert_eq!(
            (
                usage.input,
                usage.cached,
                usage.output,
                usage.reasoning,
                usage.total,
                usage.sessions
            ),
            (5, 5, 5, 5, 20, 3)
        );
        assert_eq!(usage.cost_usd, 3.0);
    }

    #[test]
    fn project_name_reads_cross_platform_paths() {
        assert_eq!(project_name(r"C:\work\alpha"), "alpha");
        assert_eq!(project_name("/Users/test/work/beta"), "beta");
    }

    #[test]
    fn oat_key_parser_ignores_comments_and_keeps_labels() {
        let keys = parse_shared_oat_keys(
            "# no secrets here\n  \"team-a:sk-ant-oat01-redacted-a\"\ninvalid\n\"team-b:sk-ant-oat01-redacted-b\"",
        );
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].id, "1");
        assert_eq!(keys[0].label, "team-a");
        assert_eq!(keys[1].label, "team-b");
    }
}
