//! Opt-in weekly wall-clock scheduling and one small Codex inference per slot.
use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{native_interop, poller, retry_after, settings_store};

const BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const FALLBACK_CLIENT_VERSION: &str = "0.164.0";
const MAX_RECORDS: usize = 512;
const DEFAULT_TIMES: [&str; 3] = ["07:00", "13:00", "19:00"];
static RUNNING: AtomicBool = AtomicBool::new(false);
static HISTORY_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub enabled: bool,
    /// Monday through Sunday, expressed in the computer's local time.
    pub days: [Vec<String>; 7],
    /// Empty selects a visible lightweight model from the account's catalog.
    pub model: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            days: std::array::from_fn(|_| DEFAULT_TIMES.iter().map(|s| s.to_string()).collect()),
            model: String::new(),
        }
    }
}

pub fn parse_times(text: &str) -> Result<Vec<String>, &'static str> {
    let mut minutes = Vec::new();
    for part in
        text.split(|c: char| c.is_whitespace() || matches!(c, ',' | '，' | '、' | ';' | '；'))
    {
        if part.is_empty() {
            continue;
        }
        let (hours, mins) = part
            .split_once(':')
            .ok_or("Use times such as 07:00, 13:00, 19:00.")?;
        if hours.is_empty()
            || hours.len() > 2
            || mins.len() != 2
            || !hours
                .bytes()
                .chain(mins.bytes())
                .all(|c| c.is_ascii_digit())
        {
            return Err("Invalid time format.");
        }
        let hours: u16 = hours.parse().map_err(|_| "Invalid hour.")?;
        let mins: u16 = mins.parse().map_err(|_| "Invalid minute.")?;
        if hours >= 24 || mins >= 60 {
            return Err("Time is outside 00:00-23:59.");
        }
        minutes.push(hours * 60 + mins);
    }
    minutes.sort_unstable();
    minutes.dedup();
    if minutes.len() > 24 {
        return Err("Use at most 24 times per day.");
    }
    Ok(minutes
        .into_iter()
        .map(|m| format!("{:02}:{:02}", m / 60, m % 60))
        .collect())
}

impl Settings {
    pub fn validated(mut self) -> Result<Self, &'static str> {
        for day in &mut self.days {
            *day = parse_times(&day.join(","))?;
        }
        self.model = self.model.trim().to_owned();
        if self.model.len() > 128
            || !self
                .model
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        {
            return Err("Invalid model name.");
        }
        if self.enabled && self.days.iter().all(Vec::is_empty) {
            return Err("Configure at least one time before enabling warm-up.");
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slot {
    pub key: String,
}

pub fn due_slot(
    settings: &Settings,
    local: &windows::Win32::Foundation::SYSTEMTIME,
) -> Option<Slot> {
    if !settings.enabled {
        return None;
    }
    let weekday = (local.wDayOfWeek as usize + 6) % 7;
    let time = format!("{:02}:{:02}", local.wHour, local.wMinute);
    settings.days[weekday]
        .iter()
        .any(|value| value == &time)
        .then(|| Slot {
            key: format!(
                "{:04}-{:02}-{:02} {time}",
                local.wYear, local.wMonth, local.wDay
            ),
        })
}

pub fn current_slot(settings: &Settings) -> Option<Slot> {
    due_slot(
        settings,
        &native_interop::system_time_to_local(SystemTime::now())?,
    )
}

pub struct RunningGuard;
impl Drop for RunningGuard {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::Release);
    }
}
pub fn try_start() -> Option<RunningGuard> {
    RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .ok()
        .map(|_| RunningGuard)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    Completed,
    Failed,
    Skipped,
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub slot: String,
    pub attempted_at: u64,
    pub finished_at: Option<u64>,
    pub status: Status,
    pub model: Option<String>,
    pub total_tokens: Option<u64>,
    pub detail: String,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct History {
    records: BTreeMap<String, Record>,
    blocked_until: u64,
}

fn read_history(path: &Path) -> io::Result<History> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Warm-up history is invalid; no request was sent.",
            )
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(History::default()),
        Err(error) => Err(error),
    }
}

fn epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Debug)]
pub struct Completed {
    pub model: String,
    pub total_tokens: Option<u64>,
}

#[derive(Debug)]
pub struct Failure {
    pub detail: String,
    pub retry_after_secs: Option<u64>,
    pub cancelled: bool,
}
impl Failure {
    fn new(detail: &str) -> Self {
        Self {
            detail: detail.to_owned(),
            retry_after_secs: None,
            cancelled: false,
        }
    }
    fn cancelled() -> Self {
        Self {
            detail: "Settings changed before sending; request cancelled.".into(),
            retry_after_secs: None,
            cancelled: true,
        }
    }
}

/// The claim reaches disk before any network work. A restart, repeated timer
/// tick or uncertain response therefore cannot repeat this date/time slot.
pub fn execute_once(
    path: &Path,
    slot: &Slot,
    send: impl FnOnce() -> Result<Completed, Failure>,
) -> io::Result<Option<Record>> {
    let now = epoch();
    let initial = {
        let _lock = HISTORY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut history = read_history(path)?;
        if history.records.contains_key(&slot.key) {
            return Ok(None);
        }
        let blocked = history.blocked_until > now;
        let record = Record {
            slot: slot.key.clone(),
            attempted_at: now,
            finished_at: None,
            status: if blocked {
                Status::Skipped
            } else {
                Status::Pending
            },
            model: None,
            total_tokens: None,
            detail: if blocked {
                "Server Retry-After deadline has not passed.".into()
            } else {
                "Attempt claimed before sending.".into()
            },
        };
        history.records.insert(slot.key.clone(), record.clone());
        while history.records.len() > MAX_RECORDS {
            history.records.pop_first();
        }
        settings_store::save(path, || Some(&history))?;
        record
    };
    if initial.status == Status::Skipped {
        return Ok(Some(initial));
    }
    let result = send();
    let _lock = HISTORY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut history = read_history(path)?;
    let mut record = initial;
    record.finished_at = Some(epoch());
    match result {
        Ok(done) => {
            record.status = Status::Completed;
            record.model = Some(done.model);
            record.total_tokens = done.total_tokens;
            record.detail =
                "Model response completed. Quota window timing is controlled by the server.".into();
        }
        Err(error) => {
            record.status = if error.cancelled {
                Status::Cancelled
            } else {
                Status::Failed
            };
            record.detail = error.detail;
            if let Some(seconds) = error.retry_after_secs {
                history.blocked_until = history.blocked_until.max(epoch().saturating_add(seconds));
            }
        }
    }
    history.records.insert(slot.key.clone(), record.clone());
    settings_store::save(path, || Some(history))?;
    Ok(Some(record))
}

pub fn latest_summary(path: &Path, chinese: bool) -> String {
    let _lock = HISTORY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    match read_history(path) {
        Ok(history) => match history.records.last_key_value() {
            Some((_, record)) => {
                let status = match (&record.status, chinese) {
                    (Status::Completed, true) => "请求完成",
                    (Status::Pending, true) => "进行中或上次中断",
                    (Status::Failed, true) => "失败",
                    (Status::Skipped, true) => "跳过",
                    (Status::Cancelled, true) => "已取消",
                    (Status::Completed, false) => "completed",
                    (Status::Pending, false) => "pending/interrupted",
                    (Status::Failed, false) => "failed",
                    (Status::Skipped, false) => "skipped",
                    (Status::Cancelled, false) => "cancelled",
                };
                format!(
                    "{}: {} — {status}{}",
                    if chinese {
                        "最近记录"
                    } else {
                        "Last attempt"
                    },
                    record.slot,
                    record
                        .model
                        .as_ref()
                        .map(|m| format!(" ({m})"))
                        .unwrap_or_default()
                )
            }
            None => {
                if chinese {
                    "尚无触发记录。".into()
                } else {
                    "No warm-up attempts yet.".into()
                }
            }
        },
        Err(_) => {
            if chinese {
                "无法读取执行记录；为避免重复请求，已停止触发。".into()
            } else {
                "History is unreadable; attempts are blocked to avoid duplicates.".into()
            }
        }
    }
}

#[derive(Clone, Deserialize)]
struct ReasoningLevel {
    effort: String,
}
#[derive(Clone, Deserialize)]
struct Model {
    slug: String,
    #[serde(default)]
    visibility: String,
    #[serde(default)]
    priority: i64,
    #[serde(default)]
    supported_reasoning_levels: Vec<ReasoningLevel>,
    #[serde(default)]
    support_verbosity: bool,
}
#[derive(Default, Deserialize)]
struct Catalog {
    #[serde(default)]
    client_version: String,
    #[serde(default)]
    models: Vec<Model>,
}

fn choose_model(catalog: &Catalog, preferred: &str) -> Result<Model, Failure> {
    if !preferred.is_empty() {
        return Ok(catalog
            .models
            .iter()
            .find(|m| m.slug == preferred)
            .cloned()
            .unwrap_or(Model {
                slug: preferred.into(),
                visibility: String::new(),
                priority: 0,
                supported_reasoning_levels: Vec::new(),
                support_verbosity: false,
            }));
    }
    catalog
        .models
        .iter()
        .filter(|m| !m.slug.is_empty() && m.visibility != "hide")
        .min_by_key(|m| {
            let light = if m.slug.contains("mini") {
                0
            } else if m.slug.contains("luna") {
                1
            } else if m.slug.contains("nano") {
                2
            } else {
                3
            };
            (light, m.priority)
        })
        .cloned()
        .ok_or_else(|| {
            Failure::new(
                "No model catalog is available. Open Codex once or configure a model name.",
            )
        })
}

fn auth_request(
    agent: &ureq::Agent,
    method: &str,
    url: &str,
    token: &str,
    account_id: Option<&str>,
    version: &str,
) -> ureq::Request {
    let mut request = agent
        .request(method, url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("originator", "codex_cli_rs")
        .set("version", version)
        .set(
            "User-Agent",
            &format!(
                "codex_cli_rs/{version} (Windows; x86_64) codex-usage-monitor/{}",
                env!("CARGO_PKG_VERSION")
            ),
        );
    if let Some(id) = account_id.filter(|s| !s.is_empty()) {
        request = request.set("ChatGPT-Account-Id", id);
    }
    request
}

fn transport_error(error: ureq::Error) -> Failure {
    match error {
        ureq::Error::Status(status, response) => {
            let retry = response
                .header("Retry-After")
                .and_then(|hint| retry_after::delay_ms(hint, SystemTime::now()))
                .map(|ms| (ms as u64).div_ceil(1000));
            Failure {
                detail: format!("Codex warm-up HTTP {status}."),
                retry_after_secs: retry.or((status == 429).then_some(3600)),
                cancelled: false,
            }
        }
        _ => Failure::new("Codex warm-up network or TLS request failed."),
    }
}

fn read_completed(reader: impl Read) -> Result<Option<u64>, Failure> {
    let reader = BufReader::new(reader.take(2 * 1024 * 1024));
    for line in reader.lines() {
        let line = line.map_err(|_| {
            Failure::new("Response stream interrupted; this slot will not be retried.")
        })?;
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(data.trim()) else {
            continue;
        };
        match value["type"].as_str() {
            Some("response.completed")
                if value["response"]["status"]
                    .as_str()
                    .is_none_or(|s| s == "completed") =>
            {
                return Ok(value["response"]["usage"]["total_tokens"].as_u64())
            }
            Some("response.failed" | "response.incomplete" | "error") => {
                return Err(Failure::new("The model response failed or was incomplete."))
            }
            _ => {}
        }
    }
    Err(Failure::new(
        "No completed event received; this slot will not be retried.",
    ))
}

fn send_with(
    agent: &ureq::Agent,
    base: &str,
    token: &str,
    account_id: Option<&str>,
    cached: Catalog,
    preferred: &str,
    permitted: impl Fn() -> bool,
) -> Result<Completed, Failure> {
    if !permitted() {
        return Err(Failure::cancelled());
    }
    let version = if !cached.client_version.is_empty()
        && cached.client_version.len() < 64
        && cached
            .client_version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
    {
        cached.client_version.as_str()
    } else {
        FALLBACK_CLIENT_VERSION
    };
    let catalog = match auth_request(
        agent,
        "GET",
        &format!("{base}/models"),
        token,
        account_id,
        version,
    )
    .query("client_version", version)
    .call()
    {
        Ok(response) => response
            .into_json::<Catalog>()
            .map_err(|_| Failure::new("Unable to read the Codex model catalog."))?,
        Err(ureq::Error::Status(404, _)) if !cached.models.is_empty() || !preferred.is_empty() => {
            Catalog {
                client_version: version.to_owned(),
                models: cached.models.clone(),
            }
        }
        Err(error) => return Err(transport_error(error)),
    };
    let model = choose_model(&catalog, preferred)?;
    let mut body = json!({"model":model.slug,"instructions":"Reply with OK only. Do not use tools.","input":[{"role":"user","content":[{"type":"input_text","text":"hi"}]}],"store":false,"stream":true});
    if let Some(level) = ["low", "minimal", "none"]
        .iter()
        .find_map(|wanted| {
            model
                .supported_reasoning_levels
                .iter()
                .find(|l| l.effort == *wanted)
        })
        .or_else(|| model.supported_reasoning_levels.first())
    {
        body["reasoning"] = json!({"effort":level.effort});
    }
    if model.support_verbosity {
        body["text"] = json!({"verbosity":"low"});
    }
    // Recheck after discovery so disabling/changing the schedule cancels a
    // prepared request. An already submitted model request is not replayed.
    if !permitted() {
        return Err(Failure::cancelled());
    }
    let response = auth_request(
        agent,
        "POST",
        &format!("{base}/responses"),
        token,
        account_id,
        version,
    )
    .set("Content-Type", "application/json")
    .set("Accept", "text/event-stream")
    .set("OpenAI-Beta", "responses=experimental")
    .send_json(body)
    .map_err(transport_error)?;
    Ok(Completed {
        model: model.slug,
        total_tokens: read_completed(response.into_reader())?,
    })
}

pub fn send(preferred: &str, permitted: impl Fn() -> bool) -> Result<Completed, Failure> {
    if !permitted() {
        return Err(Failure::cancelled());
    }
    let credentials = poller::read_codex_credentials()
        .ok_or_else(|| Failure::new("No signed-in Codex account was found."))?;
    let cache = poller::codex_auth_path()
        .and_then(|p| p.parent().map(|d| d.join("models_cache.json")))
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice::<Catalog>(&b).ok())
        .unwrap_or_default();
    let agent = poller::build_agent()
        .map_err(|_| Failure::new("Unable to initialize the configured proxy/TLS client."))?;
    send_with(
        &agent,
        BASE_URL,
        &credentials.access_token,
        credentials.account_id.as_deref(),
        cache,
        preferred,
        permitted,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use windows::Win32::Foundation::SYSTEMTIME;

    fn path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("codex-warmup-{}-{name}.json", std::process::id()))
    }

    #[test]
    fn weekly_times_are_validated_and_deduplicated() {
        assert_eq!(
            parse_times("19:00，7:00 13:00,07:00").unwrap(),
            vec!["07:00", "13:00", "19:00"]
        );
        assert!(parse_times("").unwrap().is_empty());
        for invalid in ["24:00", "07:60", "7:0", "7", "-1:00"] {
            assert!(parse_times(invalid).is_err());
        }
        assert!(!Settings::default().enabled);
        let mut settings = Settings::default();
        settings.enabled = true;
        settings.days = std::array::from_fn(|_| Vec::new());
        assert!(settings.validated().is_err());
    }

    #[test]
    fn matches_weekdays_and_current_minute_without_catch_up() {
        let mut settings = Settings::default();
        settings.enabled = true;
        settings.days[6].clear();
        let mut local = SYSTEMTIME {
            wYear: 2026,
            wMonth: 10,
            wDay: 26,
            wDayOfWeek: 1,
            wHour: 7,
            wMinute: 0,
            ..Default::default()
        };
        assert_eq!(due_slot(&settings, &local).unwrap().key, "2026-10-26 07:00");
        local.wMinute = 1;
        assert!(due_slot(&settings, &local).is_none());
        local.wDayOfWeek = 0;
        local.wMinute = 0;
        assert!(due_slot(&settings, &local).is_none());
        settings.enabled = false;
        local.wDayOfWeek = 1;
        assert!(due_slot(&settings, &local).is_none());
    }

    #[test]
    fn saved_claim_prevents_duplicates_and_restart_replays() {
        let path = path("dedupe");
        let _ = std::fs::remove_file(&path);
        let slot = Slot {
            key: "2026-10-29 07:00".into(),
        };
        let record = execute_once(&path, &slot, || {
            Ok(Completed {
                model: "test-model".into(),
                total_tokens: Some(8),
            })
        })
        .unwrap()
        .unwrap();
        assert_eq!(record.status, Status::Completed);
        assert!(execute_once(&path, &slot, || panic!("duplicate inference"))
            .unwrap()
            .is_none());
        let interrupted = Slot {
            key: "2026-10-29 13:00".into(),
        };
        let _ = std::panic::catch_unwind(|| {
            let _ = execute_once(&path, &interrupted, || {
                panic!("simulated process termination")
            });
        });
        assert!(
            execute_once(&path, &interrupted, || panic!("interrupted slot replayed"))
                .unwrap()
                .is_none()
        );
        assert!(latest_summary(&path, false).contains("pending/interrupted"));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn retry_after_blocks_later_slots_and_bad_history_fails_closed() {
        let path = path("retry");
        let _ = std::fs::remove_file(&path);
        execute_once(
            &path,
            &Slot {
                key: "2026-10-29 07:00".into(),
            },
            || {
                Err(Failure {
                    detail: "HTTP 429".into(),
                    retry_after_secs: Some(3600),
                    cancelled: false,
                })
            },
        )
        .unwrap();
        let later = execute_once(
            &path,
            &Slot {
                key: "2026-10-29 07:01".into(),
            },
            || panic!("rate limit bypassed"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(later.status, Status::Skipped);
        std::fs::write(&path, b"not json").unwrap();
        assert!(execute_once(
            &path,
            &Slot {
                key: "2026-10-29 19:00".into()
            },
            || panic!("untracked inference")
        )
        .is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn streaming_success_requires_a_completed_model_response() {
        assert_eq!(read_completed(b"data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"total_tokens\":9}}}\n\n".as_slice()).unwrap(), Some(9));
        assert!(read_completed(b"data: {\"type\":\"response.failed\"}\n".as_slice()).is_err());
        assert!(read_completed(b"data: [DONE]\n".as_slice()).is_err());
    }

    #[test]
    fn posts_one_small_inference_with_account_auth_to_the_mock_backend() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut posted = Value::Null;
            for step in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let mut headers = String::new();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse::<usize>().unwrap();
                    }
                    headers.push_str(&line.to_ascii_lowercase());
                }
                assert!(headers.contains("authorization: bearer fake-token"));
                assert!(headers.contains("chatgpt-account-id: fake-account"));
                let response = if step == 0 {
                    assert!(first.starts_with("GET /models?client_version="));
                    r#"{"models":[{"slug":"test-large","priority":0},{"slug":"test-mini","supported_reasoning_levels":[{"effort":"low"}]}]}"#.to_owned()
                } else {
                    assert!(first.starts_with("POST /responses "));
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body).unwrap();
                    posted = serde_json::from_slice(&body).unwrap();
                    "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"total_tokens\":7}}}\n\n".into()
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                )
                .unwrap();
            }
            posted
        });
        let agent = ureq::AgentBuilder::new()
            .try_proxy_from_env(false)
            .timeout(std::time::Duration::from_secs(4))
            .build();
        let result = send_with(
            &agent,
            &format!("http://{address}"),
            "fake-token",
            Some("fake-account"),
            Catalog::default(),
            "",
            || true,
        )
        .unwrap();
        assert_eq!(result.model, "test-mini");
        assert_eq!(result.total_tokens, Some(7));
        let body = server.join().unwrap();
        assert_eq!(body["input"][0]["content"][0]["text"], "hi");
        assert_eq!(body["reasoning"]["effort"], "low");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn changing_settings_during_discovery_prevents_the_inference_post() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let allowed = std::sync::Arc::new(AtomicBool::new(true));
        let server_allowed = allowed.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
            }
            server_allowed.store(false, Ordering::Release);
            let body = r#"{"models":[{"slug":"test-mini"}]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        let agent = ureq::AgentBuilder::new()
            .try_proxy_from_env(false)
            .timeout(std::time::Duration::from_secs(3))
            .build();
        let result = send_with(
            &agent,
            &format!("http://{address}"),
            "fake",
            None,
            Catalog::default(),
            "",
            || allowed.load(Ordering::Acquire),
        );
        assert!(result.unwrap_err().cancelled);
        server.join().unwrap();
    }

    #[test]
    fn disabling_cancels_before_network_access() {
        let agent = ureq::AgentBuilder::new().try_proxy_from_env(false).build();
        assert!(
            send_with(
                &agent,
                "http://127.0.0.1:1",
                "fake",
                None,
                Catalog::default(),
                "",
                || false
            )
            .unwrap_err()
            .cancelled
        );
    }
}
