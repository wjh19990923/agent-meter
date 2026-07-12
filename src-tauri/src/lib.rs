use chrono::{DateTime, Local};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, OnceLock,
    },
    time::UNIX_EPOCH,
};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, State, WebviewWindow,
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

struct PinState(AtomicBool);

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

#[tauri::command]
fn get_usage(cache: State<'_, Mutex<UsageCache>>) -> Result<Snapshot, String> {
    let home = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .ok_or("USERPROFILE is unavailable")?;
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
    let (width, height) = if expanded {
        (330.0, 146.0)
    } else {
        (392.0, 690.0)
    };
    window
        .set_size(tauri::LogicalSize::new(width, height))
        .map_err(|error| error.to_string())?;
    Ok(!expanded)
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
        .manage(PinState(AtomicBool::new(true)))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .invoke_handler(tauri::generate_handler![
            get_usage,
            hide_window,
            toggle_pin,
            get_settings,
            toggle_expanded
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
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
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
    fn project_name_reads_windows_path_on_windows() {
        assert_eq!(project_name(r"C:\work\alpha"), "alpha");
    }
}
