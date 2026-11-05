//! Durable deferred jobs. Only read-only quota checks may be retried; a claimed
//! inference is never replayed, including after an uncertain response or crash.
use super::*;

const RESET_GRACE: u64 = 5;
const MAX_WAIT: u64 = 6 * 60 * 60;
const CHECK_RETRY: u64 = 30;
const VERIFY_RETRY: u64 = 15;
const MAX_VERIFICATIONS: u8 = 4;
const WINDOW_CHECK_VERSION: u8 = 1;
const PROBE_DELAY: u64 = 15;
const RESET_JITTER: u64 = 3;
const FULL_WINDOW_SLACK: u64 = 120;

#[derive(Debug, PartialEq, Eq)]
enum WindowCheck {
    Idle,
    Active,
    Probe,
}

// An unused account may report reset_at = query time + five hours. One read
// cannot distinguish it from a newly started window rounded down to zero use.
fn full_unused_window(quota: &QuotaSnapshot) -> bool {
    quota.used_percent == Some(0.0)
        && quota.window_seconds.is_some_and(|span| {
            span == 18_000
                && quota.reset_at.is_some_and(|reset| {
                    reset.saturating_sub(quota.checked_at).abs_diff(span) <= FULL_WINDOW_SLACK
                })
        })
}

fn stable_reset(previous: &QuotaSnapshot, current: &QuotaSnapshot) -> bool {
    current.checked_at.saturating_sub(previous.checked_at) >= PROBE_DELAY
        && previous
            .reset_at
            .zip(current.reset_at)
            .is_some_and(|(a, b)| a.abs_diff(b) <= RESET_JITTER)
}

fn window_check(quota: &QuotaSnapshot, previous: Option<&QuotaSnapshot>) -> WindowCheck {
    if quota.reset_at.is_none_or(|reset| reset <= quota.checked_at) {
        return WindowCheck::Idle;
    }
    if !full_unused_window(quota) {
        return WindowCheck::Active;
    }
    let Some(previous) = previous.filter(|q| full_unused_window(q)) else {
        return WindowCheck::Probe;
    };
    if stable_reset(previous, quota) {
        return WindowCheck::Active;
    }
    let elapsed = quota.checked_at.saturating_sub(previous.checked_at);
    let drift = i128::from(quota.reset_at.unwrap()) - i128::from(previous.reset_at.unwrap());
    if elapsed >= PROBE_DELAY
        && drift > i128::from(RESET_JITTER)
        && (drift - i128::from(elapsed)).abs() <= i128::from(RESET_JITTER)
    {
        WindowCheck::Idle
    } else {
        WindowCheck::Probe
    }
}

trait Backend {
    fn now(&self) -> u64;
    fn fingerprint(&mut self) -> Result<String, Failure>;
    fn quota(&mut self) -> Result<QuotaSnapshot, Failure>;
    fn send(&mut self) -> Result<Completed, Failure>;
}

struct LiveBackend<'a> {
    client: Option<Client>,
    settings: &'a Settings,
    permitted: &'a dyn Fn() -> bool,
}

impl LiveBackend<'_> {
    fn client(&mut self) -> Result<&Client, Failure> {
        if self.client.is_none() {
            self.client = Some(Client::load()?);
        }
        Ok(self.client.as_ref().unwrap())
    }
}

impl Backend for LiveBackend<'_> {
    fn now(&self) -> u64 {
        epoch()
    }
    fn fingerprint(&mut self) -> Result<String, Failure> {
        Ok(self.client()?.fingerprint())
    }
    fn quota(&mut self) -> Result<QuotaSnapshot, Failure> {
        self.client()?.quota()
    }
    fn send(&mut self) -> Result<Completed, Failure> {
        let preferred = self.settings.model.clone();
        let permitted = self.permitted;
        self.client()?.send(&preferred, permitted)
    }
}

pub fn tick(
    path: &Path,
    slot: Option<&Slot>,
    settings: &Settings,
    permitted: impl Fn() -> bool,
) -> io::Result<Option<Record>> {
    let mut backend = LiveBackend {
        client: None,
        settings,
        permitted: &permitted,
    };
    tick_with(path, slot, settings, &permitted, &mut backend)
}

fn waiting_record(slot: &Slot, settings: &Settings, now: u64) -> Record {
    Record {
        slot: slot.key.clone(),
        attempted_at: now,
        finished_at: None,
        status: Status::WaitingForReset,
        model: None,
        total_tokens: None,
        detail: "Scheduled; checking the five-hour quota window before sending.".into(),
        next_check_at: Some(now),
        request_started_at: None,
        quota_before: None,
        quota_after: None,
        quota_probe: None,
        before_window_idle: false,
        window_check_version: WINDOW_CHECK_VERSION,
        verification: Verification::NotChecked,
        verification_attempts: 0,
        quota_check_failures: 0,
        scheduled_settings: Some(settings.clone()),
        account_fingerprint: None,
    }
}

fn finish(record: &mut Record, status: Status, now: u64, detail: &str) {
    record.status = status;
    record.finished_at = Some(now);
    record.next_check_at = None;
    record.detail = detail.into();
}

fn cancelled(record: &mut Record, now: u64) {
    if record.status == Status::Completed {
        record.verification = Verification::Unavailable;
        record.next_check_at = None;
        record.detail =
            "Request completed; settings changed before quota verification finished.".into();
    } else {
        finish(
            record,
            Status::Cancelled,
            now,
            "Settings or account changed before sending; deferred request cancelled.",
        );
    }
}

/// A UI settings change also cancels sleeping jobs while the timer is off.
pub fn cancel_deferred(path: &Path) -> io::Result<()> {
    let _lock = HISTORY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut history = read_history(path)?;
    let mut changed = false;
    for record in history.records.values_mut() {
        if record.status == Status::WaitingForReset
            || (record.status == Status::Completed && record.verification == Verification::Pending)
        {
            cancelled(record, epoch());
            changed = true;
        }
    }
    if changed {
        settings_store::save(path, || Some(history))?;
    }
    Ok(())
}

fn prepare(
    path: &Path,
    slot: Option<&Slot>,
    settings: &Settings,
    now: u64,
) -> io::Result<Option<Record>> {
    let _lock = HISTORY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut history = read_history(path)?;
    let mut changed = false;
    for record in history.records.values_mut() {
        if record.status != Status::WaitingForReset
            && !(record.status == Status::Completed && record.verification == Verification::Pending)
        {
            continue;
        }
        if !settings.enabled || record.scheduled_settings.as_ref() != Some(settings) {
            cancelled(record, now);
            changed = true;
        } else if record.status == Status::WaitingForReset
            && now.saturating_sub(record.attempted_at) > MAX_WAIT
        {
            finish(
                record,
                Status::Skipped,
                now,
                "Deferred job expired after six hours; no late request was sent.",
            );
            changed = true;
        } else if record.status == Status::WaitingForReset
            && record.window_check_version < WINDOW_CHECK_VERSION
            && record.request_started_at.is_none()
        {
            // v4/v5 could postpone an idle placeholder for five hours. Recheck
            // unsent jobs now; never reopen a claimed, failed or completed POST.
            record.window_check_version = WINDOW_CHECK_VERSION;
            record.quota_probe = None;
            record.next_check_at = Some(now);
            record.detail = "Rechecking an unsent job with corrected idle-window detection.".into();
            changed = true;
        }
    }
    if let Some(slot) = slot.filter(|_| settings.enabled) {
        if !history.records.contains_key(&slot.key) {
            history
                .records
                .insert(slot.key.clone(), waiting_record(slot, settings, now));
            changed = true;
        }
    }
    while history.records.len() > MAX_RECORDS {
        let removable = history
            .records
            .iter()
            .find(|(_, r)| {
                r.status != Status::WaitingForReset
                    && !(r.status == Status::Completed && r.verification == Verification::Pending)
            })
            .map(|(key, _)| key.clone());
        let Some(key) = removable else { break };
        history.records.remove(&key);
    }
    let next = history
        .records
        .values()
        .filter(|r| {
            r.next_check_at.is_some_and(|t| t <= now)
                && (r.status == Status::WaitingForReset
                    || (r.status == Status::Completed && r.verification == Verification::Pending))
        })
        .min_by_key(|r| (r.next_check_at, r.attempted_at))
        .cloned();
    if changed {
        settings_store::save(path, || Some(&history))?;
    }
    Ok(next)
}

// Merge after network I/O. A UI cancellation must win over a stale quota check.
fn store(path: &Path, record: &Record, blocked_until: Option<u64>) -> io::Result<bool> {
    let _lock = HISTORY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut history = read_history(path)?;
    if history
        .records
        .get(&record.slot)
        .is_some_and(|r| r.status == Status::Cancelled)
        && record.status != Status::Cancelled
    {
        return Ok(false);
    }
    history.records.insert(record.slot.clone(), record.clone());
    if let Some(until) = blocked_until {
        history.blocked_until = history.blocked_until.max(until);
    }
    settings_store::save(path, || Some(history))?;
    Ok(true)
}

fn blocked_until(path: &Path) -> io::Result<u64> {
    let _lock = HISTORY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    Ok(read_history(path)?.blocked_until)
}

fn wait_for(record: &mut Record, until: u64, reason: &str, now: u64) {
    if until > record.attempted_at.saturating_add(MAX_WAIT) {
        finish(
            record,
            Status::Skipped,
            now,
            "The reset/backoff is beyond this job's six-hour deferral limit.",
        );
    } else {
        record.status = Status::WaitingForReset;
        record.next_check_at = Some(until);
        record.detail = format!("{reason} Next quota check: {}.", format_epoch(until));
    }
}

fn quota_failure(record: &mut Record, failure: Failure, now: u64) -> Option<u64> {
    let blocked = failure.retry_after_secs.map(|s| now.saturating_add(s));
    if failure.cancelled {
        cancelled(record, now);
    } else if let Some(until) = blocked {
        wait_for(
            record,
            until.saturating_add(RESET_GRACE),
            &failure.detail,
            now,
        );
    } else {
        record.quota_check_failures = record.quota_check_failures.saturating_add(1);
        if record.quota_check_failures < 3 {
            wait_for(
                record,
                now.saturating_add(CHECK_RETRY),
                &failure.detail,
                now,
            );
        } else {
            finish(
                record,
                Status::Failed,
                now,
                &format!(
                    "Quota check failed three times; no inference was sent. {}",
                    failure.detail
                ),
            );
        }
    }
    blocked
}

fn verify(record: &mut Record, backend: &mut impl Backend) -> Option<u64> {
    record.verification_attempts += 1;
    let result = backend.quota();
    let now = backend.now();
    let mut blocked = None;
    match result {
        Ok(after) => {
            let previous_reset = record
                .quota_before
                .as_ref()
                .and_then(|q| q.reset_at)
                .unwrap_or(0);
            let active = window_check(&after, record.quota_after.as_ref()) == WindowCheck::Active;
            let confirmed = active
                && after.reset_at.is_some_and(|reset| {
                    reset > now
                        && (record.before_window_idle
                            || (reset > previous_reset.saturating_add(60)
                                && previous_reset <= record.request_started_at.unwrap_or(0)))
                });
            record.quota_after = Some(after);
            if confirmed {
                record.verification = Verification::WindowStarted;
                record.next_check_at = None;
                record.detail = "Model request completed; a new five-hour quota window was confirmed by the server.".into();
                return None;
            }
            record.verification = Verification::NotConfirmed;
            record.detail = "Model request completed, but a new five-hour window has not been confirmed. No inference will be replayed.".into();
        }
        Err(error) => {
            blocked = error.retry_after_secs.map(|s| now.saturating_add(s));
            record.verification = Verification::Unavailable;
            record.detail = format!("Model request completed; quota verification failed. {} No inference will be replayed.", error.detail);
        }
    }
    if record.verification_attempts < MAX_VERIFICATIONS {
        record.verification = Verification::Pending;
        record.next_check_at = Some(blocked.unwrap_or_else(|| now.saturating_add(VERIFY_RETRY)));
    } else {
        record.next_check_at = None;
    }
    blocked
}

fn tick_with(
    path: &Path,
    slot: Option<&Slot>,
    settings: &Settings,
    permitted: &dyn Fn() -> bool,
    backend: &mut impl Backend,
) -> io::Result<Option<Record>> {
    let Some(mut record) = prepare(path, slot, settings, backend.now())? else {
        return Ok(None);
    };
    if !permitted() {
        cancelled(&mut record, backend.now());
        store(path, &record, None)?;
        return Ok(Some(record));
    }
    let fingerprint = match backend.fingerprint() {
        Ok(fingerprint) => fingerprint,
        Err(error) if record.status == Status::Completed => {
            record.verification = Verification::Unavailable;
            record.next_check_at = None;
            record.detail = format!(
                "Request completed; quota verification unavailable. {}",
                error.detail
            );
            store(path, &record, None)?;
            return Ok(Some(record));
        }
        Err(error) => {
            let blocked = quota_failure(&mut record, error, backend.now());
            store(path, &record, blocked)?;
            return Ok(Some(record));
        }
    };
    if record
        .account_fingerprint
        .as_ref()
        .is_some_and(|old| old != &fingerprint)
    {
        cancelled(&mut record, backend.now());
        store(path, &record, None)?;
        return Ok(Some(record));
    }
    record.account_fingerprint = Some(fingerprint);
    let blocked = blocked_until(path)?;
    if blocked > backend.now() {
        if record.status == Status::Completed {
            record.next_check_at = Some(blocked);
        } else {
            wait_for(
                &mut record,
                blocked.saturating_add(RESET_GRACE),
                "Server Retry-After has not passed.",
                backend.now(),
            );
        }
        store(path, &record, None)?;
        return Ok(Some(record));
    }
    if record.status == Status::Completed {
        let blocked = verify(&mut record, backend);
        store(path, &record, blocked)?;
        return Ok(Some(record));
    }
    let before = match backend.quota() {
        Ok(before) => before,
        Err(error) => {
            let blocked = quota_failure(&mut record, error, backend.now());
            store(path, &record, blocked)?;
            return Ok(Some(record));
        }
    };
    if !permitted() {
        cancelled(&mut record, backend.now());
        store(path, &record, None)?;
        return Ok(Some(record));
    }
    let now = backend.now();
    let classification = window_check(&before, record.quota_probe.as_ref());
    if classification == WindowCheck::Probe {
        let prior_probe = record.quota_probe.replace(before.clone());
        record.quota_before = Some(before);
        if prior_probe.is_some() {
            let blocked = quota_failure(
                &mut record,
                Failure::new(
                    "Quota window state is ambiguous; only the read-only probe will be retried.",
                ),
                now,
            );
            store(path, &record, blocked)?;
        } else {
            wait_for(&mut record, now.saturating_add(PROBE_DELAY), "Zero use and a full-window reset may be an idle placeholder; checking whether its reset moves.", now);
            store(path, &record, None)?;
        }
        return Ok(Some(record));
    }
    record.quota_probe = None;
    if classification == WindowCheck::Active {
        let reset = before.reset_at.unwrap();
        let renewed = record
            .quota_before
            .as_ref()
            .and_then(|q| q.reset_at)
            .is_some_and(|old| old <= now && reset > old.saturating_add(60))
            || record.quota_before.as_ref().is_some_and(|old| {
                full_unused_window(old)
                    && old
                        .reset_at
                        .is_some_and(|old_reset| reset > old_reset.saturating_add(RESET_JITTER))
            });
        record.quota_after = Some(before.clone());
        if renewed {
            record.verification = Verification::AlreadyActive;
            finish(
                &mut record,
                Status::Skipped,
                now,
                "A new window was already started while waiting; no extra inference was needed.",
            );
        } else {
            record.quota_before = Some(before);
            record.quota_check_failures = 0;
            wait_for(
                &mut record,
                reset.saturating_add(RESET_GRACE),
                "The existing five-hour window is still active.",
                now,
            );
        }
        store(path, &record, None)?;
        return Ok(Some(record));
    }
    record.before_window_idle = true;
    record.quota_before = Some(before);
    record.quota_after = None;
    record.status = Status::Pending;
    record.next_check_at = None;
    record.request_started_at = Some(now);
    record.detail = format!(
        "Inference claimed at {} after the old window ended; it will not be replayed.",
        format_epoch(now)
    );
    if !store(path, &record, None)? {
        return Ok(None);
    }
    crate::diagnose::log(format!(
        "codex warm-up slot={} actual_start={} old_reset={:?}",
        record.slot,
        format_epoch(now),
        record
            .quota_before
            .as_ref()
            .and_then(|q| q.reset_at)
            .map(format_epoch)
    ));
    let result = if permitted() {
        backend.send()
    } else {
        Err(Failure::cancelled())
    };
    let now = backend.now();
    match result {
        Ok(done) => {
            finish(
                &mut record,
                Status::Completed,
                now,
                "Model request completed; checking whether a new window started.",
            );
            record.model = Some(done.model);
            record.total_tokens = done.total_tokens;
            record.verification = Verification::Pending;
            record.next_check_at = Some(now);
            // Persist the completed POST before any verification request.
            if !store(path, &record, None)? {
                return Ok(None);
            }
            if permitted() {
                let blocked = verify(&mut record, backend);
                store(path, &record, blocked)?;
            } else {
                cancelled(&mut record, now);
                store(path, &record, None)?;
            }
        }
        Err(error) => {
            let blocked = error
                .retry_after_secs
                .map(|seconds| now.saturating_add(seconds));
            finish(
                &mut record,
                if error.cancelled {
                    Status::Cancelled
                } else {
                    Status::Failed
                },
                now,
                &error.detail,
            );
            store(path, &record, blocked)?;
        }
    }
    Ok(Some(record))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    struct TestFile(PathBuf);
    impl TestFile {
        fn new() -> Self {
            static SEQUENCE: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "codex-warmup-schedule-{}-{}.json",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_file(&path);
            Self(path)
        }
        fn record(&self, key: &str) -> Record {
            read_history(&self.0).unwrap().records[key].clone()
        }
    }
    impl Drop for TestFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    struct Fake {
        now: u64,
        account: String,
        quotas: VecDeque<Result<QuotaSnapshot, Failure>>,
        posts: usize,
        on_post: Option<Box<dyn FnMut()>>,
        on_quota: Option<Box<dyn FnMut()>>,
        post_failure: Option<Failure>,
    }
    impl Fake {
        fn new(now: u64) -> Self {
            Self {
                now,
                account: "account-a".into(),
                quotas: VecDeque::new(),
                posts: 0,
                on_post: None,
                on_quota: None,
                post_failure: None,
            }
        }
        fn snapshot(&mut self, reset: Option<u64>) {
            self.quotas.push_back(Ok(QuotaSnapshot {
                checked_at: self.now,
                reset_at: reset,
                window_seconds: Some(18_000),
                used_percent: Some(if reset.is_some() { 1.0 } else { 0.0 }),
            }));
        }
        fn zero_snapshot(&mut self, reset: Option<u64>) {
            self.snapshot(reset);
            self.quotas
                .back_mut()
                .unwrap()
                .as_mut()
                .unwrap()
                .used_percent = Some(0.0);
        }
        fn error(&mut self) {
            self.quotas
                .push_back(Err(Failure::new("mock quota unavailable")));
        }
    }
    impl Backend for Fake {
        fn now(&self) -> u64 {
            self.now
        }
        fn fingerprint(&mut self) -> Result<String, Failure> {
            Ok(self.account.clone())
        }
        fn quota(&mut self) -> Result<QuotaSnapshot, Failure> {
            if let Some(callback) = &mut self.on_quota {
                callback();
            }
            let mut result = self.quotas.pop_front().expect("unexpected quota request");
            if let Ok(quota) = &mut result {
                quota.checked_at = self.now;
            }
            result
        }
        fn send(&mut self) -> Result<Completed, Failure> {
            self.posts += 1;
            if let Some(callback) = &mut self.on_post {
                callback();
            }
            if let Some(error) = self.post_failure.take() {
                return Err(error);
            }
            Ok(Completed {
                model: "test-luna".into(),
                total_tokens: Some(26),
            })
        }
    }
    fn settings() -> Settings {
        Settings {
            enabled: true,
            ..Default::default()
        }
    }
    fn slot() -> Slot {
        Slot {
            key: "2026-11-04 05:05".into(),
        }
    }

    #[test]
    fn real_0505_regression_waits_for_051736_then_confirms_one_new_window() {
        let file = TestFile::new();
        let settings = settings();
        let slot = slot();
        let mut backend = Fake::new(1_793_739_901); // 05:05:01 +08:00
        let old_reset = 1_793_740_656; // 05:17:36
        backend.zero_snapshot(Some(old_reset));
        let record = tick_with(&file.0, Some(&slot), &settings, &|| true, &mut backend)
            .unwrap()
            .unwrap();
        assert_eq!(record.status, Status::WaitingForReset);
        assert_eq!(record.next_check_at, Some(old_reset + RESET_GRACE));
        assert_eq!(backend.posts, 0); // Even a rounded zero percent has an active window.
        assert!(latest_summary(&file.0, true).contains("05:17:41"));
        backend.now = old_reset;
        assert!(tick_with(&file.0, None, &settings, &|| true, &mut backend)
            .unwrap()
            .is_none());
        // A restarted process recovers this job without needing the original minute.
        let mut resumed = Fake::new(old_reset + RESET_GRACE);
        resumed.snapshot(Some(old_reset));
        resumed.snapshot(Some(resumed.now + 18_000));
        let claimed_path = file.0.clone();
        resumed.on_post = Some(Box::new(move || {
            assert_eq!(
                read_history(&claimed_path).unwrap().records["2026-11-04 05:05"].status,
                Status::Pending
            );
        }));
        let record = tick_with(&file.0, None, &settings, &|| true, &mut resumed)
            .unwrap()
            .unwrap();
        assert_eq!(record.status, Status::Completed);
        assert_eq!(record.verification, Verification::WindowStarted);
        assert_eq!(record.request_started_at, Some(old_reset + RESET_GRACE));
        assert_eq!(record.total_tokens, Some(26));
        assert!(latest_summary(&file.0, true).contains("新窗口已确认"));
        assert!(
            tick_with(&file.0, Some(&slot), &settings, &|| true, &mut resumed)
                .unwrap()
                .is_none()
        );
        assert_eq!(resumed.posts, 1);
    }

    #[test]
    fn an_interrupted_claim_is_never_replayed() {
        let file = TestFile::new();
        let mut backend = Fake::new(1000);
        backend.snapshot(None);
        backend.on_post = Some(Box::new(|| panic!("simulated process exit during POST")));
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tick_with(&file.0, Some(&slot()), &settings(), &|| true, &mut backend).unwrap();
        }))
        .is_err());
        backend.on_post = None;
        assert_eq!(file.record(&slot().key).status, Status::Pending);
        assert!(
            tick_with(&file.0, Some(&slot()), &settings(), &|| true, &mut backend)
                .unwrap()
                .is_none()
        );
        assert_eq!(backend.posts, 1);
    }

    #[test]
    fn completed_post_is_durable_before_verification_and_restart_only_queries_quota() {
        let file = TestFile::new();
        let mut backend = Fake::new(1000);
        backend.snapshot(None);
        backend.on_post = Some(Box::new(|| {}));
        let query_number = std::rc::Rc::new(std::cell::Cell::new(0));
        let count = query_number.clone();
        backend.on_quota = Some(Box::new(move || {
            let number = count.get() + 1;
            count.set(number);
            if number == 2 {
                panic!("process exited before verification GET");
            }
        }));
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tick_with(&file.0, Some(&slot()), &settings(), &|| true, &mut backend).unwrap();
        }))
        .is_err());
        assert_eq!(file.record(&slot().key).status, Status::Completed);
        assert_eq!(file.record(&slot().key).total_tokens, Some(26));
        let mut resumed = Fake::new(1001);
        resumed.snapshot(Some(19_000));
        let result = tick_with(&file.0, None, &settings(), &|| true, &mut resumed)
            .unwrap()
            .unwrap();
        assert_eq!(result.verification, Verification::WindowStarted);
        assert_eq!(resumed.posts, 0);
    }

    #[test]
    fn cancelled_or_changed_schedules_do_not_send_deferred_jobs() {
        for change in [false, true] {
            let file = TestFile::new();
            let mut backend = Fake::new(1000);
            backend.snapshot(Some(1100));
            tick_with(&file.0, Some(&slot()), &settings(), &|| true, &mut backend).unwrap();
            let mut modified = settings();
            if change {
                modified.model = "another-model".into();
            } else {
                modified.enabled = false;
            }
            backend.now = 1105;
            assert!(tick_with(&file.0, None, &modified, &|| true, &mut backend)
                .unwrap()
                .is_none());
            assert_eq!(file.record(&slot().key).status, Status::Cancelled);
            assert_eq!(backend.posts, 0);
        }
    }

    #[test]
    fn settings_change_after_quota_query_cancels_before_the_post() {
        let file = TestFile::new();
        let allowed = std::rc::Rc::new(std::cell::Cell::new(true));
        let changed = allowed.clone();
        let mut backend = Fake::new(1000);
        backend.snapshot(None);
        backend.on_quota = Some(Box::new(move || changed.set(false)));
        let record = tick_with(
            &file.0,
            Some(&slot()),
            &settings(),
            &|| allowed.get(),
            &mut backend,
        )
        .unwrap()
        .unwrap();
        assert_eq!(record.status, Status::Cancelled);
        assert_eq!(backend.posts, 0);
    }

    #[test]
    fn ui_cancellation_wins_over_a_stale_quota_check_before_claiming() {
        let file = TestFile::new();
        let path = file.0.clone();
        let mut backend = Fake::new(1000);
        backend.snapshot(None);
        backend.on_quota = Some(Box::new(move || cancel_deferred(&path).unwrap()));
        assert!(
            tick_with(&file.0, Some(&slot()), &settings(), &|| true, &mut backend)
                .unwrap()
                .is_none()
        );
        assert_eq!(file.record(&slot().key).status, Status::Cancelled);
        assert_eq!(backend.posts, 0);
    }

    #[test]
    fn a_window_started_by_other_activity_while_waiting_needs_no_extra_post() {
        let file = TestFile::new();
        let mut backend = Fake::new(1000);
        backend.snapshot(Some(1100));
        tick_with(&file.0, Some(&slot()), &settings(), &|| true, &mut backend).unwrap();
        backend.now = 1105;
        backend.snapshot(Some(19_100));
        let record = tick_with(&file.0, None, &settings(), &|| true, &mut backend)
            .unwrap()
            .unwrap();
        assert_eq!(record.status, Status::Skipped);
        assert_eq!(record.verification, Verification::AlreadyActive);
        assert_eq!(backend.posts, 0);
    }

    #[test]
    fn overlapping_deferred_slots_share_one_new_window() {
        let file = TestFile::new();
        let mut backend = Fake::new(1000);
        let later = Slot {
            key: "2026-11-04 05:06".into(),
        };
        backend.snapshot(Some(1200));
        tick_with(&file.0, Some(&slot()), &settings(), &|| true, &mut backend).unwrap();
        backend.now = 1060;
        backend.snapshot(Some(1200));
        tick_with(&file.0, Some(&later), &settings(), &|| true, &mut backend).unwrap();
        backend.now = 1205;
        backend.snapshot(Some(1200));
        backend.snapshot(Some(19_205));
        tick_with(&file.0, None, &settings(), &|| true, &mut backend).unwrap();
        backend.snapshot(Some(19_205));
        let result = tick_with(&file.0, None, &settings(), &|| true, &mut backend)
            .unwrap()
            .unwrap();
        assert_eq!(result.verification, Verification::AlreadyActive);
        assert_eq!(backend.posts, 1);
    }

    #[test]
    fn failed_quota_checks_retry_reads_then_fail_without_an_inference() {
        let file = TestFile::new();
        let scheduled_slot = slot();
        let mut backend = Fake::new(1000);
        for index in 0..3 {
            backend.error();
            backend.now = 1000 + index * CHECK_RETRY;
            tick_with(
                &file.0,
                if index == 0 {
                    Some(&scheduled_slot)
                } else {
                    None
                },
                &settings(),
                &|| true,
                &mut backend,
            )
            .unwrap();
        }
        assert_eq!(file.record(&slot().key).status, Status::Failed);
        assert_eq!(backend.posts, 0);
    }

    #[test]
    fn server_retry_after_is_persisted_and_later_jobs_respect_it() {
        let file = TestFile::new();
        let mut backend = Fake::new(1000);
        backend.quotas.push_back(Err(Failure {
            detail: "HTTP 429".into(),
            retry_after_secs: Some(60),
            cancelled: false,
        }));
        tick_with(&file.0, Some(&slot()), &settings(), &|| true, &mut backend).unwrap();
        backend.now = 1020;
        let later = Slot {
            key: "2026-11-04 05:06".into(),
        };
        let record = tick_with(&file.0, Some(&later), &settings(), &|| true, &mut backend)
            .unwrap()
            .unwrap();
        assert_eq!(record.next_check_at, Some(1065));
        assert_eq!(backend.posts, 0);
        assert_eq!(blocked_until(&file.0).unwrap(), 1060);
    }

    #[test]
    fn failed_or_uncertain_inference_is_not_retried() {
        let file = TestFile::new();
        let mut backend = Fake::new(1000);
        backend.snapshot(None);
        backend.post_failure = Some(Failure {
            detail: "stream interrupted".into(),
            retry_after_secs: Some(60),
            cancelled: false,
        });
        let record = tick_with(&file.0, Some(&slot()), &settings(), &|| true, &mut backend)
            .unwrap()
            .unwrap();
        assert_eq!(record.status, Status::Failed);
        assert!(
            tick_with(&file.0, Some(&slot()), &settings(), &|| true, &mut backend)
                .unwrap()
                .is_none()
        );
        assert_eq!(backend.posts, 1);
    }

    #[test]
    fn lagging_quota_confirmation_retries_gets_without_repeating_the_post() {
        let file = TestFile::new();
        let mut backend = Fake::new(1000);
        backend.snapshot(Some(900));
        backend.snapshot(Some(900));
        let record = tick_with(&file.0, Some(&slot()), &settings(), &|| true, &mut backend)
            .unwrap()
            .unwrap();
        assert_eq!(record.verification, Verification::Pending);
        backend.now += VERIFY_RETRY;
        backend.snapshot(Some(19_000));
        let record = tick_with(&file.0, None, &settings(), &|| true, &mut backend)
            .unwrap()
            .unwrap();
        assert_eq!(record.verification, Verification::WindowStarted);
        assert_eq!(backend.posts, 1);
    }

    #[test]
    fn completion_does_not_claim_a_new_window_without_evidence() {
        for broken in [false, true] {
            let file = TestFile::new();
            let scheduled_slot = slot();
            let mut backend = Fake::new(1000);
            backend.snapshot(None);
            for number in 0..MAX_VERIFICATIONS {
                if broken {
                    backend.error();
                } else {
                    backend.snapshot(None);
                }
                backend.now = 1000 + number as u64 * VERIFY_RETRY;
                tick_with(
                    &file.0,
                    if number == 0 {
                        Some(&scheduled_slot)
                    } else {
                        None
                    },
                    &settings(),
                    &|| true,
                    &mut backend,
                )
                .unwrap();
            }
            let record = file.record(&slot().key);
            assert_eq!(record.status, Status::Completed);
            assert_eq!(
                record.verification,
                if broken {
                    Verification::Unavailable
                } else {
                    Verification::NotConfirmed
                }
            );
            assert_eq!(record.next_check_at, None);
            assert_eq!(backend.posts, 1);
            assert!(latest_summary(&file.0, true).contains("新窗口未确认"));
        }
    }

    #[test]
    fn switching_accounts_cancels_deferred_inference_before_any_new_quota_read() {
        let file = TestFile::new();
        let mut backend = Fake::new(1000);
        backend.snapshot(Some(1100));
        tick_with(&file.0, Some(&slot()), &settings(), &|| true, &mut backend).unwrap();
        backend.now = 1105;
        backend.account = "account-b".into();
        let record = tick_with(&file.0, None, &settings(), &|| true, &mut backend)
            .unwrap()
            .unwrap();
        assert_eq!(record.status, Status::Cancelled);
        assert_eq!(backend.posts, 0);
    }

    #[test]
    fn sleeping_jobs_expire_without_replaying_old_schedules() {
        let file = TestFile::new();
        let mut backend = Fake::new(1000);
        backend.snapshot(Some(1100));
        tick_with(&file.0, Some(&slot()), &settings(), &|| true, &mut backend).unwrap();
        backend.now += MAX_WAIT + 1;
        assert!(
            tick_with(&file.0, None, &settings(), &|| true, &mut backend)
                .unwrap()
                .is_none()
        );
        assert_eq!(file.record(&slot().key).status, Status::Skipped);
        assert_eq!(backend.posts, 0);
    }

    #[test]
    fn old_history_is_compatible_and_bad_history_prevents_untracked_requests() {
        let file = TestFile::new();
        std::fs::write(&file.0, br#"{"records":{"2026-11-04 05:05":{"slot":"2026-11-04 05:05","attempted_at":1000,"finished_at":1001,"status":"completed","model":"old-model","total_tokens":26,"detail":"Old completion"}},"blocked_until":0}"#).unwrap();
        let mut backend = Fake::new(1002);
        assert!(
            tick_with(&file.0, Some(&slot()), &settings(), &|| true, &mut backend)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            file.record(&slot().key).verification,
            Verification::NotChecked
        );
        std::fs::write(&file.0, b"invalid JSON").unwrap();
        assert!(tick_with(&file.0, Some(&slot()), &settings(), &|| true, &mut backend).is_err());
        assert_eq!(backend.posts, 0);
    }

    #[test]
    fn idle_ticks_do_not_query_the_backend() {
        let file = TestFile::new();
        let mut backend = Fake::new(1000);
        assert!(
            tick_with(&file.0, None, &settings(), &|| true, &mut backend)
                .unwrap()
                .is_none()
        );
        assert_eq!(backend.posts, 0);
    }

    #[test]
    fn nov5_idle_placeholder_is_probed_then_sent_and_zero_use_fixed_reset_is_verified() {
        let file = TestFile::new();
        let today = Slot {
            key: "2026-11-05 05:05".into(),
        };
        let mut backend = Fake::new(1_793_826_312); // Actual 05:05:12 observation.
        backend.zero_snapshot(Some(backend.now + 18_000));
        let first = tick_with(&file.0, Some(&today), &settings(), &|| true, &mut backend)
            .unwrap()
            .unwrap();
        assert_eq!(first.next_check_at, Some(backend.now + PROBE_DELAY));
        assert!(first.quota_probe.is_some());
        assert_eq!(backend.posts, 0);
        assert!(latest_summary(&file.0, true).contains("正在确认窗口是否空闲"));

        // A restart continues the saved probe. The idle estimate follows time.
        let mut resumed = Fake::new(backend.now + PROBE_DELAY);
        let real_reset = resumed.now + 18_000;
        resumed.zero_snapshot(Some(real_reset));
        resumed.zero_snapshot(Some(real_reset));
        let sent = tick_with(&file.0, None, &settings(), &|| true, &mut resumed)
            .unwrap()
            .unwrap();
        assert!(sent.before_window_idle);
        assert_eq!(sent.request_started_at, Some(resumed.now));
        assert_eq!(sent.verification, Verification::Pending);
        assert_eq!(resumed.posts, 1);

        // A real tiny request can still round to zero: the reset now stays fixed.
        resumed.now += PROBE_DELAY;
        resumed.zero_snapshot(Some(real_reset));
        let verified = tick_with(&file.0, None, &settings(), &|| true, &mut resumed)
            .unwrap()
            .unwrap();
        assert_eq!(verified.verification, Verification::WindowStarted);
        assert_eq!(resumed.posts, 1);
        assert!(
            tick_with(&file.0, Some(&today), &settings(), &|| true, &mut resumed)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_just_started_window_rounded_to_zero_is_not_mistaken_for_idle() {
        let file = TestFile::new();
        let mut backend = Fake::new(1000);
        backend.zero_snapshot(Some(19_000));
        tick_with(&file.0, Some(&slot()), &settings(), &|| true, &mut backend).unwrap();
        backend.now += PROBE_DELAY;
        backend.zero_snapshot(Some(19_001)); // One-second server rounding jitter.
        let waiting = tick_with(&file.0, None, &settings(), &|| true, &mut backend)
            .unwrap()
            .unwrap();
        assert_eq!(waiting.next_check_at, Some(19_001 + RESET_GRACE));
        assert!(waiting.quota_probe.is_none());
        assert_eq!(backend.posts, 0);
    }

    #[test]
    fn positive_use_needs_no_idle_probe_even_with_a_full_window_remaining() {
        let file = TestFile::new();
        let mut backend = Fake::new(1000);
        backend.snapshot(Some(19_000));
        let waiting = tick_with(&file.0, Some(&slot()), &settings(), &|| true, &mut backend)
            .unwrap()
            .unwrap();
        assert_eq!(waiting.next_check_at, Some(19_005));
        assert!(waiting.quota_probe.is_none());
        assert_eq!(backend.posts, 0);
    }

    #[test]
    fn moving_estimates_after_a_completed_post_do_not_confirm_a_window_or_repeat_inference() {
        let file = TestFile::new();
        let mut backend = Fake::new(1000);
        backend.snapshot(None);
        for number in 0..MAX_VERIFICATIONS {
            backend.now = 1000 + number as u64 * VERIFY_RETRY;
            backend.zero_snapshot(Some(backend.now + 18_000));
            let current_slot = slot();
            tick_with(
                &file.0,
                if number == 0 {
                    Some(&current_slot)
                } else {
                    None
                },
                &settings(),
                &|| true,
                &mut backend,
            )
            .unwrap();
        }
        let record = file.record(&slot().key);
        assert_eq!(record.status, Status::Completed);
        assert_eq!(record.verification, Verification::NotConfirmed);
        assert_eq!(backend.posts, 1);
        assert!(record.next_check_at.is_none());
    }

    fn old_waiting(file: &TestFile, now: u64) {
        let mut record = waiting_record(&slot(), &settings(), now);
        record.window_check_version = 0;
        record.quota_before = Some(QuotaSnapshot {
            checked_at: now,
            reset_at: Some(now + 18_000),
            window_seconds: Some(18_000),
            used_percent: Some(0.0),
        });
        record.next_check_at = Some(now + 18_005);
        let mut legacy = serde_json::to_value(History {
            records: BTreeMap::from([(record.slot.clone(), record)]),
            ..Default::default()
        })
        .unwrap();
        for value in legacy["records"].as_object_mut().unwrap().values_mut() {
            let object = value.as_object_mut().unwrap();
            for field in ["window_check_version", "quota_probe", "before_window_idle"] {
                object.remove(field); // Exact v5 shape: new fields did not exist.
            }
        }
        settings_store::save(&file.0, || Some(legacy)).unwrap();
    }

    #[test]
    fn upgrading_rechecks_unsent_v5_jobs_before_their_wrong_five_hour_deadline() {
        let file = TestFile::new();
        old_waiting(&file, 1000);
        let mut backend = Fake::new(1060);
        backend.zero_snapshot(Some(19_060));
        let probe = tick_with(&file.0, None, &settings(), &|| true, &mut backend)
            .unwrap()
            .unwrap();
        assert_eq!(probe.next_check_at, Some(1075));
        assert_eq!(probe.window_check_version, WINDOW_CHECK_VERSION);
        backend.now = 1075;
        backend.zero_snapshot(Some(19_075));
        backend.snapshot(Some(19_075));
        let record = tick_with(&file.0, None, &settings(), &|| true, &mut backend)
            .unwrap()
            .unwrap();
        assert_eq!(record.verification, Verification::WindowStarted);
        assert_eq!(backend.posts, 1);
    }

    #[test]
    fn upgrading_skips_an_old_idle_job_if_other_activity_already_started_the_window() {
        let file = TestFile::new();
        old_waiting(&file, 1000);
        let mut backend = Fake::new(2000);
        backend.snapshot(Some(20_000));
        let record = tick_with(&file.0, None, &settings(), &|| true, &mut backend)
            .unwrap()
            .unwrap();
        assert_eq!(record.status, Status::Skipped);
        assert_eq!(record.verification, Verification::AlreadyActive);
        assert_eq!(backend.posts, 0);
    }

    #[test]
    fn upgrading_never_reopens_claimed_failed_or_completed_model_requests() {
        for status in [Status::Pending, Status::Failed, Status::Completed] {
            let file = TestFile::new();
            old_waiting(&file, 1000);
            let mut history = read_history(&file.0).unwrap();
            let record = history.records.get_mut(&slot().key).unwrap();
            record.status = status.clone();
            record.request_started_at = Some(1001);
            settings_store::save(&file.0, || Some(history)).unwrap();
            let mut backend = Fake::new(1060);
            assert!(
                tick_with(&file.0, Some(&slot()), &settings(), &|| true, &mut backend)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(file.record(&slot().key).status, status);
            assert_eq!(backend.posts, 0);
        }
    }

    #[test]
    fn an_inconsistent_zero_use_reset_is_rechecked_without_sending() {
        let file = TestFile::new();
        let mut backend = Fake::new(1000);
        backend.zero_snapshot(Some(19_000));
        tick_with(&file.0, Some(&slot()), &settings(), &|| true, &mut backend).unwrap();
        backend.now += PROBE_DELAY;
        backend.zero_snapshot(Some(19_060)); // Neither fixed nor following the 15s elapsed.
        let record = tick_with(&file.0, None, &settings(), &|| true, &mut backend)
            .unwrap()
            .unwrap();
        assert_eq!(record.next_check_at, Some(backend.now + CHECK_RETRY));
        assert_eq!(record.quota_check_failures, 1);
        assert_eq!(backend.posts, 0);
    }

    #[test]
    fn a_failed_second_probe_only_retries_the_read_and_keeps_the_idle_baseline() {
        let file = TestFile::new();
        let mut backend = Fake::new(1000);
        backend.zero_snapshot(Some(19_000));
        tick_with(&file.0, Some(&slot()), &settings(), &|| true, &mut backend).unwrap();
        backend.now += PROBE_DELAY;
        backend.error();
        tick_with(&file.0, None, &settings(), &|| true, &mut backend).unwrap();
        assert_eq!(backend.posts, 0);
        backend.now += CHECK_RETRY;
        backend.zero_snapshot(Some(backend.now + 18_000));
        backend.snapshot(Some(backend.now + 18_000));
        let result = tick_with(&file.0, None, &settings(), &|| true, &mut backend)
            .unwrap()
            .unwrap();
        assert_eq!(result.verification, Verification::WindowStarted);
        assert_eq!(backend.posts, 1);
    }
}
