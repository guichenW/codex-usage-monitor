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

#[path = "warmup_schedule.rs"]
mod schedule;
pub use schedule::{cancel_deferred, tick};

const BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const QUOTA_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
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
    WaitingForReset,
    Pending,
    Completed,
    Failed,
    Skipped,
    Cancelled,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verification {
    #[default]
    NotChecked,
    Pending,
    WindowStarted,
    AlreadyActive,
    NotConfirmed,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuotaSnapshot {
    pub checked_at: u64,
    pub reset_at: Option<u64>,
    pub window_seconds: Option<u64>,
    pub used_percent: Option<f64>,
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
    #[serde(default)]
    pub next_check_at: Option<u64>,
    #[serde(default)]
    pub request_started_at: Option<u64>,
    #[serde(default)]
    pub quota_before: Option<QuotaSnapshot>,
    #[serde(default)]
    pub quota_after: Option<QuotaSnapshot>,
    #[serde(default)]
    quota_probe: Option<QuotaSnapshot>,
    #[serde(default)]
    before_window_idle: bool,
    #[serde(default)]
    window_check_version: u8,
    #[serde(default)]
    pub verification: Verification,
    #[serde(default)]
    verification_attempts: u8,
    #[serde(default)]
    quota_check_failures: u8,
    #[serde(default)]
    scheduled_settings: Option<Settings>,
    #[serde(default)]
    account_fingerprint: Option<String>,
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

pub fn latest_summary(path: &Path, chinese: bool) -> String {
    let _lock = HISTORY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    match read_history(path) {
        Ok(history) => match history.records.last_key_value() {
            Some((_, record)) => {
                let status = match (&record.status, chinese) {
                    (Status::WaitingForReset, true) if record.quota_probe.is_some() => {
                        "正在确认窗口是否空闲"
                    }
                    (Status::WaitingForReset, false) if record.quota_probe.is_some() => {
                        "checking whether the window is idle"
                    }
                    (Status::WaitingForReset, true) => "等待额度窗口重置",
                    (Status::WaitingForReset, false) => "waiting for quota window reset",
                    (Status::Completed, true)
                        if record.verification == Verification::WindowStarted =>
                    {
                        "新窗口已确认"
                    }
                    (Status::Completed, false)
                        if record.verification == Verification::WindowStarted =>
                    {
                        "new window confirmed"
                    }
                    (Status::Completed, true) if record.verification == Verification::Pending => {
                        "请求完成，正在确认窗口"
                    }
                    (Status::Completed, false) if record.verification == Verification::Pending => {
                        "request completed, checking window"
                    }
                    (Status::Completed, true)
                        if matches!(
                            record.verification,
                            Verification::NotConfirmed | Verification::Unavailable
                        ) =>
                    {
                        "请求完成，新窗口未确认"
                    }
                    (Status::Completed, false)
                        if matches!(
                            record.verification,
                            Verification::NotConfirmed | Verification::Unavailable
                        ) =>
                    {
                        "request completed, window unconfirmed"
                    }
                    (Status::Skipped, true)
                        if record.verification == Verification::AlreadyActive =>
                    {
                        "已有新窗口，已跳过"
                    }
                    (Status::Skipped, false)
                        if record.verification == Verification::AlreadyActive =>
                    {
                        "new window already active, skipped"
                    }
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
                let mut summary = format!(
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
                );
                if let Some(next) = record.next_check_at {
                    summary.push_str(&format!(
                        "\n{}: {}",
                        if chinese {
                            "下次检查"
                        } else {
                            "Next check"
                        },
                        format_epoch(next)
                    ));
                } else if record.verification == Verification::WindowStarted {
                    if let Some(reset) = record.quota_after.as_ref().and_then(|q| q.reset_at) {
                        summary.push_str(&format!(
                            "\n{}: {}",
                            if chinese {
                                "窗口重置"
                            } else {
                                "Window resets"
                            },
                            format_epoch(reset)
                        ));
                    }
                }
                summary
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

pub fn format_epoch(seconds: u64) -> String {
    UNIX_EPOCH
        .checked_add(std::time::Duration::from_secs(seconds))
        .and_then(native_interop::system_time_to_local)
        .map(|time| {
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                time.wYear, time.wMonth, time.wDay, time.wHour, time.wMinute, time.wSecond
            )
        })
        .unwrap_or_else(|| seconds.to_string())
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
#[derive(Clone, Default, Deserialize)]
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

struct Client {
    credentials: poller::CodexTokenData,
    cache: Catalog,
    agent: ureq::Agent,
}

impl Client {
    fn load() -> Result<Self, Failure> {
        let credentials = poller::read_codex_credentials()
            .ok_or_else(|| Failure::new("No signed-in Codex account was found."))?;
        let cache = poller::codex_auth_path()
            .and_then(|p| p.parent().map(|d| d.join("models_cache.json")))
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice::<Catalog>(&b).ok())
            .unwrap_or_default();
        let agent = poller::build_agent()
            .map_err(|_| Failure::new("Unable to initialize the configured proxy/TLS client."))?;
        Ok(Self {
            credentials,
            cache,
            agent,
        })
    }

    fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        let account = self
            .credentials
            .account_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .unwrap_or(&self.credentials.access_token);
        format!("{:x}", Sha256::digest(account.as_bytes()))
    }

    fn quota(&self) -> Result<QuotaSnapshot, Failure> {
        self.quota_at(QUOTA_URL)
    }

    fn quota_at(&self, url: &str) -> Result<QuotaSnapshot, Failure> {
        let response = auth_request(
            &self.agent,
            "GET",
            url,
            &self.credentials.access_token,
            self.credentials.account_id.as_deref(),
            FALLBACK_CLIENT_VERSION,
        )
        .set("Cache-Control", "no-cache")
        .call()
        .map_err(transport_error)?;
        let value: Value = serde_json::from_reader(response.into_reader().take(1024 * 1024))
            .map_err(|_| Failure::new("Unable to read the Codex quota window."))?;
        quota_snapshot(&value, epoch())
    }

    fn send(&self, preferred: &str, permitted: impl Fn() -> bool) -> Result<Completed, Failure> {
        send_with(
            &self.agent,
            BASE_URL,
            &self.credentials.access_token,
            self.credentials.account_id.as_deref(),
            self.cache.clone(),
            preferred,
            permitted,
        )
    }
}

fn quota_snapshot(value: &Value, now: u64) -> Result<QuotaSnapshot, Failure> {
    let limits = value
        .get("rate_limit")
        .and_then(Value::as_object)
        .ok_or_else(|| Failure::new("The quota response has no rate-limit information."))?;
    let primary = limits.get("primary_window").filter(|v| v.is_object());
    let secondary = limits.get("secondary_window").filter(|v| v.is_object());
    let window = [primary, secondary]
        .into_iter()
        .flatten()
        .find(|v| v["limit_window_seconds"].as_u64() == Some(18_000))
        .or_else(|| primary.filter(|v| v.get("limit_window_seconds").is_none_or(Value::is_null)));
    let Some(window) = window else {
        return Ok(QuotaSnapshot {
            checked_at: now,
            reset_at: None,
            window_seconds: None,
            used_percent: None,
        });
    };
    let reset = window
        .get("reset_at")
        .and_then(Value::as_i64)
        .ok_or_else(|| Failure::new("The five-hour quota window has an invalid reset time."))?;
    let used = window["used_percent"]
        .as_f64()
        .filter(|used| used.is_finite() && *used >= 0.0)
        .ok_or_else(|| Failure::new("The five-hour quota window has invalid usage information."))?;
    Ok(QuotaSnapshot {
        checked_at: now,
        reset_at: (reset > 0).then_some(reset as u64),
        window_seconds: Some(18_000),
        used_percent: Some(used),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use windows::Win32::Foundation::SYSTEMTIME;

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
    fn streaming_success_requires_a_completed_model_response() {
        assert_eq!(read_completed(b"data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"total_tokens\":9}}}\n\n".as_slice()).unwrap(), Some(9));
        assert!(read_completed(b"data: {\"type\":\"response.failed\"}\n".as_slice()).is_err());
        assert!(read_completed(b"data: [DONE]\n".as_slice()).is_err());
    }

    #[test]
    fn quota_parser_uses_five_hour_duration_instead_of_weekly_position_or_zero_percent() {
        let value = json!({"rate_limit": {
            "primary_window": {"used_percent":55,"reset_at":50_000,"limit_window_seconds":604_800},
            "secondary_window": {"used_percent":0,"reset_at":1100,"limit_window_seconds":18_000}
        }});
        let quota = quota_snapshot(&value, 1000).unwrap();
        assert_eq!(quota.reset_at, Some(1100));
        assert_eq!(quota.used_percent, Some(0.0));
        let weekly_only = json!({"rate_limit":{"primary_window":{"used_percent":55,"reset_at":50_000,"limit_window_seconds":604_800}}});
        assert_eq!(quota_snapshot(&weekly_only, 1000).unwrap().reset_at, None);
        let legacy = json!({"rate_limit":{"primary_window":{"used_percent":0,"reset_at":1100}}});
        assert_eq!(quota_snapshot(&legacy, 1000).unwrap().reset_at, Some(1100));
        assert!(quota_snapshot(&json!({"rate_limit":null}), 1000).is_err());
        assert!(quota_snapshot(
            &json!({"rate_limit":{"primary_window":{"reset_at":"invalid"}}}),
            1000
        )
        .is_err());
    }

    #[test]
    fn quota_probe_is_an_authenticated_read_only_get_against_the_mock_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            assert_eq!(first, "GET /wham/usage HTTP/1.1\r\n");
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                headers.push_str(&line.to_ascii_lowercase());
            }
            assert!(headers.contains("authorization: bearer mock-token"));
            assert!(headers.contains("chatgpt-account-id: mock-account"));
            let body = r#"{"rate_limit":{"primary_window":{"used_percent":0,"reset_at":1100,"limit_window_seconds":18000}}}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        let client = Client {
            credentials: poller::CodexTokenData {
                access_token: "mock-token".into(),
                account_id: Some("mock-account".into()),
            },
            cache: Catalog::default(),
            agent: ureq::AgentBuilder::new()
                .try_proxy_from_env(false)
                .timeout(std::time::Duration::from_secs(3))
                .build(),
        };
        assert_eq!(
            client
                .quota_at(&format!("http://{address}/wham/usage"))
                .unwrap()
                .reset_at,
            Some(1100)
        );
        server.join().unwrap();
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
