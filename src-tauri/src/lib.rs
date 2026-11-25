use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl};
use tokio::sync::Mutex;

/// 変換ログの1エントリ（AppState に蓄積してポーリングで取得）
#[derive(Serialize, Clone)]
pub struct ConvLogEntry {
    pub seq: u64,
    pub activity_id: String,
    pub ts_ms: u64,
    pub file_name: String,
    pub status: String, // "processing" | "done" | "error" | "skipped"
    pub error: Option<String>,
    /// "allSilent" | "analysisFailed" when silence trimming could not be applied.
    pub notice: Option<String>,
}

/// 中止時は、まだ終端状態の記録がないファイルだけを中止にする。
/// ログは状態遷移ごとに追記されるため、先行する processing 行を一括置換すると、
/// 既に done/error になったファイルまでUI上で中止に見えてしまう。
fn mark_unfinished_logs_cancelled(log: &mut VecDeque<ConvLogEntry>) {
    let completed: HashSet<String> = log
        .iter()
        .filter(|entry| {
            matches!(
                entry.status.as_str(),
                "done" | "error" | "skipped" | "cancelled"
            )
        })
        .map(|entry| entry.activity_id.clone())
        .collect();
    for entry in log.iter_mut() {
        if entry.status == "processing" && !completed.contains(&entry.activity_id) {
            entry.status = "cancelled".to_string();
        }
    }
}

/// A cursor must only acknowledge entries from the same locked snapshot.
fn snapshot_conversion_log(
    log: &VecDeque<ConvLogEntry>,
    after_seq: u64,
    reset: bool,
) -> (Vec<ConvLogEntry>, u64) {
    let cursor = log.back().map_or(after_seq, |entry| entry.seq);
    let entries = log
        .iter()
        .filter(|entry| reset || entry.seq > after_seq)
        .cloned()
        .collect();
    (entries, cursor)
}

mod converter;
mod preview_files;
mod preview_operations;
mod settings;
mod spool;

use converter::{
    run_conversion, BatchOutcome, CompletionPayload, ConversionResources, ConversionRun,
    ConvertRequest, OverwriteChoice, ProgressPayload,
};
use settings::{Settings, SourceFileAction};

pub const INPUT_SPOOL_TARGET_BYTES: usize = 256 * 1024 * 1024;

pub struct JobCancellation {
    cancelled: AtomicBool,
    changed: tokio::sync::Notify,
}

impl JobCancellation {
    pub fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            changed: tokio::sync::Notify::new(),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub fn cancel(&self) {
        if !self.cancelled.swap(true, Ordering::AcqRel) {
            self.changed.notify_waiters();
        }
    }

    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

impl Default for JobCancellation {
    fn default() -> Self {
        Self::new()
    }
}

pub struct SessionPause {
    paused: AtomicBool,
    changed: tokio::sync::Notify,
    clock: std::sync::Mutex<PauseClock>,
    processes: std::sync::Mutex<Vec<i32>>,
}

struct PauseClock {
    started: tokio::time::Instant,
    paused_at: Option<tokio::time::Instant>,
    total_paused: std::time::Duration,
}

impl SessionPause {
    fn new() -> Self {
        Self {
            paused: AtomicBool::new(false),
            changed: tokio::sync::Notify::new(),
            processes: std::sync::Mutex::new(Vec::new()),
            clock: std::sync::Mutex::new(PauseClock {
                started: tokio::time::Instant::now(),
                paused_at: None,
                total_paused: std::time::Duration::ZERO,
            }),
        }
    }

    fn set_paused(&self, paused: bool) -> bool {
        self.set_paused_with(paused, suspend_resume_processes)
    }

    fn set_paused_with(&self, paused: bool, signal: impl FnOnce(&[i32], bool)) -> bool {
        // Registration and every native signal share this lock. In particular,
        // Windows SuspendThread must not increment a new child's count twice.
        let processes = self.processes.lock().unwrap();
        let mut clock = self.clock.lock().unwrap();
        let changed = self.paused.swap(paused, Ordering::AcqRel) != paused;
        if changed {
            let now = tokio::time::Instant::now();
            if paused {
                clock.paused_at = Some(now);
            } else if let Some(started) = clock.paused_at.take() {
                clock.total_paused += now - started;
            }
        }
        drop(clock);
        if changed {
            signal(&processes, paused);
            self.changed.notify_waiters();
        }
        changed
    }

    fn register_process(&self, pid: u32) {
        self.register_process_with(pid, suspend_resume_processes);
    }

    fn register_process_with(&self, pid: u32, signal: impl FnOnce(&[i32], bool)) {
        let mut processes = self.processes.lock().unwrap();
        processes.push(pid as i32);
        if self.paused.load(Ordering::Acquire) {
            signal(&[pid as i32], true);
        }
    }

    fn unregister_process(&self, pid: u32) {
        let mut processes = self.processes.lock().unwrap();
        if let Some(index) = processes.iter().position(|&value| value == pid as i32) {
            processes.swap_remove(index);
        }
    }

    fn active_elapsed(&self) -> std::time::Duration {
        let clock = self.clock.lock().unwrap();
        let end = clock.paused_at.unwrap_or_else(tokio::time::Instant::now);
        (end - clock.started).saturating_sub(clock.total_paused)
    }

    /// Timeout budget measured only while the session is running. Accounting is
    /// stored at each transition, so coalesced pause/resume notifications are safe.
    pub async fn wait_for_active_duration(&self, duration: std::time::Duration) {
        let started = self.active_elapsed();
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let elapsed = self.active_elapsed().saturating_sub(started);
            if elapsed >= duration {
                return;
            }
            tokio::select! {
                _ = &mut changed => {},
                _ = tokio::time::sleep(duration - elapsed), if !self.paused.load(Ordering::Acquire) => {},
            }
        }
    }

    pub async fn wait_until_resumed(&self, cancellation: &JobCancellation) -> bool {
        loop {
            if cancellation.is_cancelled() {
                return false;
            }
            if !self.paused.load(Ordering::Acquire) {
                return true;
            }
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !self.paused.load(Ordering::Acquire) {
                return true;
            }
            tokio::select! {
                _ = &mut changed => {},
                _ = cancellation.cancelled() => return false,
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ProgressPhase {
    #[default]
    Queueing,
    Exact,
}

#[derive(Default)]
pub struct OverallProgress {
    pub artifact_progress: Vec<f64>,
    /// Sum of monotonic artifact progress; updated by delta under the same lock.
    progress_sum: f64,
    pub artifact_terminal: Vec<bool>,
    pub artifact_input: Vec<usize>,
    pub input_artifact_total: Vec<usize>,
    pub input_terminal_artifacts: Vec<usize>,
    pub percent: f64,
    pub completed_count: usize,
    pub target_total: usize,
    pub enumerated_input_count: usize,
    pub completed_input_count: usize,
    pub phase: ProgressPhase,
    pub scan_complete: bool,
}

impl OverallProgress {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn coarse_percent(&self) -> f64 {
        if self.enumerated_input_count == 0 {
            0.0
        } else {
            (self.completed_input_count as f64 / self.enumerated_input_count as f64 * 100.0)
                .min(99.0)
        }
    }

    fn exact_percent(&self) -> f64 {
        self.progress_sum / self.target_total.max(1) as f64 * 100.0
    }

    fn begin_queueing(&mut self) {
        self.phase = ProgressPhase::Queueing;
        self.scan_complete = false;
        self.percent = self.coarse_percent();
    }

    pub fn add_enumerated_inputs(&mut self, count: usize) {
        self.enumerated_input_count = self.enumerated_input_count.saturating_add(count);
        if self.phase == ProgressPhase::Queueing {
            // 走査中は新しい入力が見つかるたびに分母を増やし、表示率の後退も許可する。
            self.percent = self.coarse_percent();
        }
    }

    pub fn register_input(&mut self, artifact_count: usize) -> usize {
        let first = self.artifact_progress.len();
        let input_index = self.input_artifact_total.len();
        self.artifact_progress.resize(first + artifact_count, 0.0);
        self.artifact_terminal.resize(first + artifact_count, false);
        self.artifact_input
            .resize(first + artifact_count, input_index);
        self.input_artifact_total.push(artifact_count);
        self.input_terminal_artifacts.push(0);
        self.target_total = self.artifact_progress.len();
        first
    }

    pub fn update(&mut self, index: usize, ratio: f64, terminal: bool) {
        if index >= self.artifact_progress.len() {
            return;
        }
        let previous = self.artifact_progress[index];
        let next = if terminal { 1.0 } else { ratio.clamp(0.0, 1.0) };
        self.artifact_progress[index] = previous.max(next);
        self.progress_sum += self.artifact_progress[index] - previous;
        let became_terminal = terminal && !self.artifact_terminal[index];
        if became_terminal {
            self.artifact_terminal[index] = true;
            self.completed_count += 1;
            let input_index = self.artifact_input[index];
            self.input_terminal_artifacts[input_index] += 1;
            if self.input_terminal_artifacts[input_index] == self.input_artifact_total[input_index]
            {
                self.completed_input_count += 1;
            }
        }
        self.percent = match self.phase {
            // キュー登録中はFFmpeg等の部分進捗を全体率へ混ぜない。
            ProgressPhase::Queueing => self.coarse_percent(),
            ProgressPhase::Exact => self.percent.max(self.exact_percent()).min(100.0),
        };
    }

    pub fn finish_queueing(&mut self) {
        self.phase = ProgressPhase::Exact;
        self.scan_complete = true;
        // 切替時だけ、粗い入力率から正確な成果物加重率への後退を許可する。
        self.percent = self.exact_percent().min(100.0);
    }

    pub fn finish_job(&mut self) {
        self.artifact_progress.fill(1.0);
        self.progress_sum = self.artifact_progress.len() as f64;
        self.artifact_terminal.fill(true);
        self.completed_count = self.target_total;
        for (terminal, total) in self
            .input_terminal_artifacts
            .iter_mut()
            .zip(&self.input_artifact_total)
        {
            *terminal = *total;
        }
        self.completed_input_count = self.input_artifact_total.len();
        self.phase = ProgressPhase::Exact;
        self.scan_complete = true;
        self.percent = 100.0;
    }
}

pub struct ConversionSession {
    id: String,
    pending_batches: AtomicUsize,
    next_batch: AtomicU64,
    pause: Arc<SessionPause>,
    cancellation: Arc<JobCancellation>,
    resources: Arc<ConversionResources>,
    outcomes: std::sync::Mutex<Vec<BatchOutcome>>,
    started_at: std::time::Instant,
    finished: tokio::sync::Notify,
}

impl ConversionSession {
    async fn wait_for_batches(&self) {
        loop {
            if self.pending_batches.load(Ordering::Acquire) == 0 {
                return;
            }
            let finished = self.finished.notified();
            tokio::pin!(finished);
            finished.as_mut().enable();
            if self.pending_batches.load(Ordering::Acquire) == 0 {
                return;
            }
            finished.await;
        }
    }
}

pub struct OverwritePrompt {
    pub id: String,
    pub sender: tokio::sync::oneshot::Sender<OverwriteChoice>,
}

pub struct AppState {
    preview_files: preview_files::PreviewFiles,
    preview_operations: Arc<preview_operations::PreviewOperations>,
    pub session: Mutex<Option<Arc<ConversionSession>>>,
    pub is_converting: AtomicBool,
    /// 中止後の子タスクjoinとスプール回収が完了するまで新規ジョブを遮断する。
    pub cleanup_in_progress: AtomicBool,
    /// exit_app が開始済みなら次の ExitRequested を通す。
    pub exiting: AtomicBool,
    pub overwrite_tx: std::sync::Mutex<Option<OverwritePrompt>>,
    /// ネットワーク入力をローカルへ退避した入力スプールの現在使用量（バイト）
    pub temp_cache_used: Arc<AtomicUsize>,
    /// NASへ書き戻す前のローカル出力スプールの現在使用量（バイト）
    pub output_spool_used: Arc<AtomicUsize>,
    /// 入力スプールが上限に達し、次のNAS読み込みを待っている状態
    pub input_spool_waiting: AtomicBool,
    /// 次のFFmpeg開始に必要な出力予約を確保できず待っている状態
    pub output_spool_waiting: AtomicBool,
    /// sysinfo による CPU 監視インスタンス（ポーリングごとに refresh）
    pub sys_monitor: std::sync::Mutex<sysinfo::System>,
    /// 変換ログバッファ（最新10,000件、循環）
    pub conv_log: std::sync::Mutex<VecDeque<ConvLogEntry>>,
    /// 既存ログの状態変更（キャンセル等）をクライアントへ再同期させる世代
    pub log_state_revision: AtomicU64,
    /// 通常ログの単調増加ID
    pub log_sequence: AtomicU64,
    /// Stable session/artifact IDs and progress ratios (0.0–1.0).
    pub active_files: std::sync::Mutex<HashMap<String, f32>>,
    /// Active artifact indexes; unlike filenames these remain unique across batches.
    pub active_artifacts: std::sync::Mutex<HashSet<usize>>,
    /// 最新変換開始時刻（Unix ms）—アクティビティウィンドウの windowOpenTs 基準に使用
    pub conv_start_ts: AtomicU64,
    /// 最新変換の開始から全工程完了までの実測時間（実行中は0）
    pub conversion_elapsed_ms: AtomicU64,
    /// 成果物ごとの全工程進捗と、走査後に確定する対象総数
    pub overall_progress: std::sync::Mutex<OverallProgress>,
    /// Number of batches that are still enumerating/probing inputs.
    pub scanning_batches: AtomicUsize,
    pub queued_batches: AtomicUsize,
    pub successful_count: AtomicUsize,
    pub failed_count: AtomicUsize,
    pub skipped_count: AtomicUsize,
    /// setup中に初期化され、プロセス存続中は排他ロックを保持するスプール管理器。
    pub spool_manager: std::sync::OnceLock<Arc<spool::SpoolManager>>,
}

impl AppState {
    pub fn spool_manager(&self) -> &Arc<spool::SpoolManager> {
        self.spool_manager
            .get()
            .expect("spool manager must be initialized during setup")
    }
}

#[derive(Serialize)]
struct ActivityData {
    cpu_percent: f64,
    input_spool_used_mb: f64,
    input_spool_target_mb: f64,
    output_spool_used_mb: f64,
    input_spool_waiting: bool,
    output_spool_waiting: bool,
    is_converting: bool,
    log: Vec<ConvLogEntry>,
    log_cursor: u64,
    log_state_revision: u64,
    log_reset: bool,
    active_files: HashMap<String, f32>,
    conv_start_ts: u64,
    conversion_elapsed_ms: u64,
    overall_progress_percent: f64,
    completed_count: usize,
    target_total: usize,
    enumerated_input_count: usize,
    completed_input_count: usize,
    progress_phase: ProgressPhase,
    scan_complete: bool,
    scanning_batch_count: usize,
    queued_batch_count: usize,
    waiting_count: usize,
    processing_count: usize,
    successful_count: usize,
    failed_count: usize,
    skipped_count: usize,
}

// --- Commands ---

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EnqueueResponse {
    session_id: String,
    batch_id: String,
    queued_batch_count: usize,
}

#[tauri::command]
async fn convert_files(
    app: AppHandle,
    state: State<'_, AppState>,
    job_id: String,
    request: ConvertRequest,
    settings_snapshot: Option<Settings>,
) -> Result<EnqueueResponse, String> {
    let mut batch_settings = match settings_snapshot {
        Some(settings) => settings,
        None => settings::load_settings(&app).map_err(|e| e.to_string())?,
    };
    batch_settings.validate();
    batch_settings.refresh_runtime_values();

    let mut session_guard = state.session.lock().await;
    let session = if let Some(session) = session_guard.as_ref() {
        if state.cleanup_in_progress.load(Ordering::Acquire) || session.cancellation.is_cancelled()
        {
            return Err("The active conversion session is stopping".to_string());
        }
        if session.id != job_id {
            return Err("The active conversion session ID does not match".to_string());
        }
        session
            .resources
            .update_parallel(batch_settings.parallel_count);
        session.clone()
    } else {
        state.spool_manager().retry_recovery();
        if state.cleanup_in_progress.load(Ordering::Acquire) {
            return Err("Previous conversion cleanup is still in progress".to_string());
        }
        if state.temp_cache_used.load(Ordering::Acquire) != 0
            || state.output_spool_used.load(Ordering::Acquire) != 0
        {
            return Err("Previous spool files could not yet be removed".to_string());
        }
        let session = Arc::new(ConversionSession {
            id: job_id.clone(),
            pending_batches: AtomicUsize::new(0),
            next_batch: AtomicU64::new(0),
            pause: Arc::new(SessionPause::new()),
            cancellation: Arc::new(JobCancellation::new()),
            resources: Arc::new(ConversionResources::new(batch_settings.parallel_count)),
            outcomes: std::sync::Mutex::new(Vec::new()),
            started_at: std::time::Instant::now(),
            finished: tokio::sync::Notify::new(),
        });
        let conv_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        state.conv_start_ts.store(conv_ts, Ordering::SeqCst);
        state.conversion_elapsed_ms.store(0, Ordering::SeqCst);
        state.active_files.lock().unwrap().clear();
        state.active_artifacts.lock().unwrap().clear();
        state.overall_progress.lock().unwrap().reset();
        state.temp_cache_used.store(0, Ordering::Relaxed);
        state.output_spool_used.store(0, Ordering::Relaxed);
        state.input_spool_waiting.store(false, Ordering::Relaxed);
        state.output_spool_waiting.store(false, Ordering::Relaxed);
        state.successful_count.store(0, Ordering::Relaxed);
        state.failed_count.store(0, Ordering::Relaxed);
        state.skipped_count.store(0, Ordering::Relaxed);
        // Clear only the previous session, before any worker can append rows.
        // Activity may be opened after conversion has already started.
        if batch_settings.clear_log_on_convert {
            let mut log = state.conv_log.lock().unwrap();
            log.clear();
            state.log_state_revision.fetch_add(1, Ordering::SeqCst);
        }
        state.is_converting.store(true, Ordering::SeqCst);
        *session_guard = Some(session.clone());
        session
    };

    let batch_order = session.next_batch.fetch_add(1, Ordering::SeqCst) + 1;
    let queued = session.pending_batches.fetch_add(1, Ordering::SeqCst) + 1;
    state.queued_batches.store(queued, Ordering::Release);
    state.scanning_batches.fetch_add(1, Ordering::SeqCst);
    state.overall_progress.lock().unwrap().begin_queueing();
    let batch_id = format!("{}-{batch_order}", session.id);
    drop(session_guard);

    let app_for_batch = app.clone();
    let session_for_batch = session.clone();
    tokio::spawn(async move {
        let outcome = run_conversion(ConversionRun {
            app: app_for_batch.clone(),
            job_id: session_for_batch.id.clone(),
            request,
            settings: batch_settings,
            temp_cache_used: app_for_batch.state::<AppState>().temp_cache_used.clone(),
            cancellation: session_for_batch.cancellation.clone(),
            pause: session_for_batch.pause.clone(),
            resources: session_for_batch.resources.clone(),
            batch_order,
        })
        .await;
        finish_session_batch(app_for_batch, session_for_batch, outcome).await;
    });

    Ok(EnqueueResponse {
        session_id: session.id.clone(),
        batch_id,
        queued_batch_count: queued,
    })
}

fn conversion_profile_key(outcome: &BatchOutcome) -> String {
    let mut value = serde_json::to_value(&outcome.settings).unwrap_or_default();
    if let Some(object) = value.as_object_mut() {
        for key in [
            "outputDest",
            "sourceFileAction",
            "nameConflict",
            "openInFinder",
            "lastMode",
            "lastFormat",
            "lastDecodeFormat",
            "customOutputPath",
            "preserveFolderStructure",
            "language",
            "enabledFormats",
            "enabledDecodeFormats",
            "clearLogOnConvert",
            "autoOpenActivity",
        ] {
            object.remove(key);
        }
    }
    format!("{}:{}:{value}", outcome.mode, outcome.format)
}

fn delete_session_sources(outcomes: &[BatchOutcome], cancelled: bool) {
    struct SourceSummary {
        input: std::path::PathBuf,
        keep: bool,
        all_succeeded: bool,
        outputs: Vec<String>,
    }

    let mut sources: HashMap<std::path::PathBuf, SourceSummary> = HashMap::new();
    for outcome in outcomes {
        for result in &outcome.results {
            let input = std::path::PathBuf::from(&result.input_path);
            let identity = std::fs::canonicalize(&input).unwrap_or_else(|_| input.clone());
            let summary = sources.entry(identity).or_insert_with(|| SourceSummary {
                input,
                keep: false,
                all_succeeded: true,
                outputs: Vec::new(),
            });
            summary.keep |= outcome.settings.source_file_action == SourceFileAction::Keep;
            summary.all_succeeded &= result.success && !result.output_path.is_empty();
            if result.success {
                summary.outputs.push(result.output_path.clone());
            }
        }
    }
    if cancelled {
        return;
    }
    for summary in sources.into_values() {
        let replaces_input = summary.outputs.iter().any(|output| {
            std::fs::canonicalize(&summary.input)
                .ok()
                .zip(std::fs::canonicalize(output).ok())
                .is_some_and(|(input, output)| input == output)
        });
        if !summary.keep && summary.all_succeeded && !replaces_input {
            let _ = std::fs::remove_file(summary.input);
        }
    }
}

fn reveal_in_file_manager(path: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer")
        .arg(format!("/select,{path}"))
        .spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open")
        .arg(
            std::path::Path::new(path)
                .parent()
                .unwrap_or(std::path::Path::new(".")),
        )
        .spawn();
}

async fn finish_session_batch(
    app: AppHandle,
    session: Arc<ConversionSession>,
    outcome: BatchOutcome,
) {
    session.outcomes.lock().unwrap().push(outcome);
    let was_last = session.pending_batches.fetch_sub(1, Ordering::AcqRel) == 1;
    app.state::<AppState>().queued_batches.store(
        session.pending_batches.load(Ordering::Acquire),
        Ordering::Release,
    );
    session.finished.notify_waiters();
    if !was_last {
        return;
    }

    let state = app.state::<AppState>();
    let mut session_guard = state.session.lock().await;
    let is_current = session_guard
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, &session));
    if !is_current || session.pending_batches.load(Ordering::Acquire) != 0 {
        return;
    }
    session_guard.take();

    let mut outcomes = std::mem::take(&mut *session.outcomes.lock().unwrap());
    outcomes.sort_by_key(|outcome| outcome.batch_order);
    let cancelled = session.cancellation.is_cancelled();
    if cancelled {
        let mut log = state.conv_log.lock().unwrap();
        mark_unfinished_logs_cancelled(&mut log);
        state.log_state_revision.fetch_add(1, Ordering::SeqCst);
    }
    delete_session_sources(&outcomes, cancelled);

    if !cancelled {
        if let Some(path) = outcomes
            .iter()
            .rev()
            .filter(|outcome| outcome.settings.open_in_finder)
            .flat_map(|outcome| outcome.results.iter().rev())
            .find(|result| result.success)
            .map(|result| result.output_path.as_str())
        {
            reveal_in_file_manager(path);
        }
    }

    let results = outcomes
        .iter_mut()
        .flat_map(|outcome| std::mem::take(&mut outcome.results))
        .collect::<Vec<_>>();
    let success_count = results.iter().filter(|result| result.success).count();
    let error_count = results
        .iter()
        .filter(|result| !result.success && !result.skipped)
        .count();
    let skipped_count = results.iter().filter(|result| result.skipped).count();
    let mixed_profiles = outcomes
        .first()
        .map(conversion_profile_key)
        .is_some_and(|first| {
            outcomes
                .iter()
                .skip(1)
                .any(|o| conversion_profile_key(o) != first)
        });

    let (final_count, final_percent) = {
        let mut progress = state.overall_progress.lock().unwrap();
        progress.finish_job();
        (progress.target_total, progress.percent)
    };
    state.conversion_elapsed_ms.store(
        session
            .started_at
            .elapsed()
            .as_millis()
            .min(u64::MAX as u128) as u64,
        Ordering::SeqCst,
    );
    state.is_converting.store(false, Ordering::SeqCst);
    state.scanning_batches.store(0, Ordering::Release);
    state.queued_batches.store(0, Ordering::Release);
    state.active_files.lock().unwrap().clear();
    state.active_artifacts.lock().unwrap().clear();
    state.spool_manager().retry_recovery();
    drop(session_guard);

    let _ = app.emit(
        "progress",
        ProgressPayload {
            job_id: session.id.clone(),
            percent: final_percent,
            current_file: String::new(),
            file_index: final_count,
            file_count: final_count,
        },
    );
    let _ = app.emit(
        "conversion_complete",
        CompletionPayload {
            job_id: session.id.clone(),
            results,
            success_count,
            error_count,
            skipped_count,
            mixed_profiles,
        },
    );
    session.finished.notify_waiters();
}

#[tauri::command]
async fn cancel_job(state: State<'_, AppState>, job_id: String) -> Result<(), String> {
    let session = state.session.lock().await.clone();
    if let Some(session) = session.filter(|session| session.id == job_id) {
        state.cleanup_in_progress.store(true, Ordering::Release);
        session.cancellation.cancel();
        session.pause.set_paused(false);
        if let Some(prompt) = state.overwrite_tx.lock().unwrap().take() {
            let _ = prompt.sender.send(OverwriteChoice::CancelAll);
        }
        {
            let mut log = state.conv_log.lock().unwrap();
            mark_unfinished_logs_cancelled(&mut log);
            state.log_state_revision.fetch_add(1, Ordering::SeqCst);
        }
        state.active_files.lock().unwrap().clear();
        state.active_artifacts.lock().unwrap().clear();
        session.wait_for_batches().await;
        state.spool_manager().retry_recovery();
        state.input_spool_waiting.store(false, Ordering::Release);
        state.output_spool_waiting.store(false, Ordering::Release);
        let cleanup_complete = state.temp_cache_used.load(Ordering::Acquire) == 0
            && state.output_spool_used.load(Ordering::Acquire) == 0;
        state.cleanup_in_progress.store(false, Ordering::Release);
        if !cleanup_complete {
            return Err("Some spool files are still waiting for recovery".to_string());
        }
    }
    Ok(())
}

#[tauri::command]
async fn pause_job(state: State<'_, AppState>, job_id: String) -> Result<(), String> {
    let session = state.session.lock().await.clone();
    if let Some(job) = session.filter(|session| session.id == job_id) {
        job.pause.set_paused(true);
    }
    Ok(())
}

#[tauri::command]
async fn resume_job(state: State<'_, AppState>, job_id: String) -> Result<(), String> {
    let session = state.session.lock().await.clone();
    if let Some(job) = session.filter(|session| session.id == job_id) {
        job.pause.set_paused(false);
    }
    Ok(())
}

#[cfg(unix)]
fn suspend_resume_processes(pids: &[i32], suspend: bool) {
    for &pid in pids {
        unsafe {
            libc::kill(
                -pid,
                if suspend {
                    libc::SIGSTOP
                } else {
                    libc::SIGCONT
                },
            );
        }
    }
}

/// Windows: 対象プロセスの全スレッドを一時停止または再開する
#[cfg(windows)]
fn suspend_resume_processes(pids: &[i32], suspend: bool) {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::Threading::{
        OpenThread, ResumeThread, SuspendThread, THREAD_SUSPEND_RESUME,
    };

    for &pid in pids {
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                continue;
            }
            let mut entry: THREADENTRY32 = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            if Thread32First(snapshot, &mut entry) != 0 {
                loop {
                    if entry.th32OwnerProcessID == pid as u32 {
                        let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                        if !thread.is_null() {
                            let ret = if suspend {
                                SuspendThread(thread)
                            } else {
                                ResumeThread(thread)
                            };
                            if ret == u32::MAX {
                                eprintln!(
                                    "{} failed for thread {}",
                                    if suspend {
                                        "SuspendThread"
                                    } else {
                                        "ResumeThread"
                                    },
                                    entry.th32ThreadID
                                );
                            }
                            CloseHandle(thread);
                        }
                    }
                    if Thread32Next(snapshot, &mut entry) == 0 {
                        break;
                    }
                }
            }
            CloseHandle(snapshot);
        }
    }
}

#[tauri::command]
async fn get_settings(app: AppHandle) -> Result<Settings, String> {
    settings::load_settings(&app).map_err(|e| e.to_string())
}

#[tauri::command]
async fn save_settings(app: AppHandle, s: Settings) -> Result<(), String> {
    settings::save_settings(&app, &s).map_err(|e| e.to_string())
}

/// Helper to create a dev-mode URL for a given relative path.
#[cfg(dev)]
fn dev_url(path: &str) -> WebviewUrl {
    WebviewUrl::External(
        format!("http://localhost:1420/src/{}", path)
            .parse()
            .unwrap(),
    )
}

/// Helper to create a prod-mode URL for a given relative path.
#[cfg(not(dev))]
fn dev_url(path: &str) -> WebviewUrl {
    WebviewUrl::App(format!("src/{}", path).into())
}

async fn ensure_window(
    app: &AppHandle,
    label: &str,
    url: WebviewUrl,
    title: &str,
    width: f64,
    height: f64,
    resizable: bool,
) -> Result<(), String> {
    if let Some(win) = app.get_webview_window(label) {
        win.show().map_err(|e| e.to_string())?;
        win.set_focus().map_err(|e| e.to_string())?;
    } else {
        tauri::WebviewWindowBuilder::new(app, label, url)
            .title(title)
            .inner_size(width, height)
            .resizable(resizable)
            .build()
            .map_err(|e: tauri::Error| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
async fn open_settings_window(app: AppHandle) -> Result<(), String> {
    ensure_window(
        &app,
        "settings",
        dev_url("settings/settings.html"),
        "oTo - Settings",
        480.0,
        560.0,
        false,
    )
    .await
}

#[tauri::command]
async fn open_about_window(app: AppHandle) -> Result<(), String> {
    ensure_window(
        &app,
        "about",
        dev_url("about/about.html"),
        "oTo - About",
        400.0,
        460.0,
        false,
    )
    .await
}

#[tauri::command]
async fn open_licenses_window(app: AppHandle) -> Result<(), String> {
    ensure_window(
        &app,
        "licenses",
        dev_url("licenses.html"),
        "oTo - Third-Party Licenses",
        720.0,
        760.0,
        true,
    )
    .await
}

#[tauri::command]
async fn pick_folder(app: AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::{DialogExt, FilePath};
    let (tx, rx) = tokio::sync::oneshot::channel::<Option<FilePath>>();
    app.dialog().file().pick_folder(move |path| {
        let _ = tx.send(path);
    });
    let path = rx.await.map_err(|_| "dialog cancelled".to_string())?;
    Ok(path.and_then(|p| match p {
        FilePath::Path(pb) => Some(pb.to_string_lossy().into_owned()),
        _ => None,
    }))
}

#[tauri::command]
fn get_app_version() -> String {
    format!("{} (build {})", env!("CARGO_PKG_VERSION"), env!("GIT_HASH"))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WaveformLevel {
    peaks: Vec<(f32, f32)>,
    rms: Vec<f32>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WaveformData {
    levels: Vec<WaveformLevel>,
    duration_secs: f64,
}

#[tauri::command]
async fn open_silence_preview(app: AppHandle) -> Result<(), String> {
    let label = "silence-preview";
    if let Some(win) = app.get_webview_window(label) {
        win.show().map_err(|e| e.to_string())?;
        win.set_focus().map_err(|e| e.to_string())?;
    } else {
        let win =
            tauri::WebviewWindowBuilder::new(&app, label, dev_url("silence-preview/preview.html"))
                .title("無音トリミング - 詳細設定")
                .inner_size(820.0, 560.0)
                .resizable(true)
                .build()
                .map_err(|e: tauri::Error| e.to_string())?;
        let app_handle = app.clone();
        win.on_window_event(move |event| {
            if matches!(event, tauri::WindowEvent::Destroyed) {
                app_handle.state::<AppState>().preview_files.invalidate();
                app_handle.emit("silence_preview_closed", ()).ok();
            }
        });
    }
    app.emit("silence_preview_opened", ()).ok();
    Ok(())
}

#[tauri::command]
async fn is_silence_preview_visible(app: AppHandle) -> bool {
    if let Some(win) = app.get_webview_window("silence-preview") {
        win.is_visible().unwrap_or(false)
    } else {
        false
    }
}

fn compute_waveform_streaming(
    path: &std::path::Path,
    num_samples: usize,
    resolutions: &[usize],
    cancellation: &JobCancellation,
) -> Result<Vec<WaveformLevel>, String> {
    use std::io::Read;
    type Acc = (f32, f32, f32, u32);
    let mut accs: Vec<Vec<Acc>> = resolutions
        .iter()
        .map(|&res| vec![(f32::INFINITY, f32::NEG_INFINITY, 0.0, 0); res])
        .collect();

    if cancellation.is_cancelled() {
        return Err("application is exiting".into());
    }
    let mut file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let mut buffer = vec![0_u8; 256 * 1024];
    let samples_per_block = buffer.len() / 4;
    for start in (0..num_samples).step_by(samples_per_block) {
        if cancellation.is_cancelled() {
            return Err("application is exiting".into());
        }
        let count = samples_per_block.min(num_samples - start);
        let bytes = &mut buffer[..count * 4];
        file.read_exact(bytes)
            .map_err(|error| format!("waveform read failed: {error}"))?;
        for (offset, sample) in bytes.as_chunks::<4>().0.iter().enumerate() {
            let idx = start + offset;
            let s = f32::from_le_bytes(*sample);
            for (ri, &res) in resolutions.iter().enumerate() {
                let bucket = (idx * res) / num_samples;
                if bucket < res {
                    let a = &mut accs[ri][bucket];
                    if s < a.0 {
                        a.0 = s;
                    }
                    if s > a.1 {
                        a.1 = s;
                    }
                    a.2 += s * s;
                    a.3 += 1;
                }
            }
        }
    }

    Ok(accs
        .into_iter()
        .map(|res_acc| {
            let mut peaks = Vec::with_capacity(res_acc.len());
            let mut rms = Vec::with_capacity(res_acc.len());
            for (mn, mx, sum_sq, count) in res_acc {
                if count == 0 {
                    peaks.push((0.0_f32, 0.0_f32));
                    rms.push(0.0_f32);
                } else {
                    peaks.push((mn.clamp(-1.0, 1.0), mx.clamp(-1.0, 1.0)));
                    rms.push((sum_sq / count as f32).sqrt());
                }
            }
            WaveformLevel { peaks, rms }
        })
        .collect())
}

#[tauri::command]
async fn get_waveform_data(
    state: State<'_, AppState>,
    path: String,
) -> Result<WaveformData, String> {
    let manager = state.spool_manager().clone();
    state
        .preview_operations
        .run(move |cancellation| async move {
            let temp_file = manager.new_temporary_file("waveform", "raw");
            let mut command = tokio::process::Command::new(converter::ffmpeg_path());
            command
                .args(["-y", "-i", &path, "-ar", "4000", "-f", "f32le", "-ac", "1"])
                .arg(temp_file.path());
            preview_operations::run_process(command, &cancellation).await?;
            // Keep the operation and file owners until the blocking reader finishes.
            tokio::task::spawn_blocking(move || {
                let temp = temp_file.path();
                let file_size = std::fs::metadata(temp).map_err(|e| e.to_string())?.len() as usize;
                if file_size < 8 {
                    return Err("no audio data".to_string());
                }
                let num_samples = file_size / 4;
                let duration_secs = num_samples as f64 / 4000.0;
                let levels = compute_waveform_streaming(
                    temp,
                    num_samples,
                    &[800, 8000, 80000],
                    &cancellation,
                )?;
                Ok(WaveformData {
                    levels,
                    duration_secs,
                })
            })
            .await
            .map_err(|e| e.to_string())?
        })
        .await
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
struct TrimPreview {
    /// "trim" | "none" | "allSilent" | "failed"
    outcome: String,
    keep_start: f64,
    keep_end: f64,
    /// Whether the head / tail is actually cut (a margin can swallow a small cut).
    cut_head: bool,
    cut_tail: bool,
    /// Duration the decision was made against (the decoded length when known).
    duration: f64,
    error: Option<String>,
}

fn trim_preview(decision: Result<converter::TrimDecision, String>, duration: f64) -> TrimPreview {
    let (outcome, keep_start, keep_end, cut_head, cut_tail, error) = match decision {
        Ok(converter::TrimDecision::Trim {
            keep_start,
            keep_end,
            cut_head,
            cut_tail,
        }) => ("trim", keep_start, keep_end, cut_head, cut_tail, None),
        Ok(converter::TrimDecision::NothingToTrim) => ("none", 0.0, duration, false, false, None),
        Ok(converter::TrimDecision::AllSilent) => ("allSilent", 0.0, 0.0, false, false, None),
        Err(error) => ("failed", 0.0, 0.0, false, false, Some(error)),
    };
    TrimPreview {
        outcome: outcome.into(),
        keep_start,
        keep_end,
        cut_head,
        cut_tail,
        duration,
        error,
    }
}

/// Preview the trim conversion would apply, using the same scan and decision.
#[tauri::command]
async fn analyze_trim_preview(
    state: State<'_, AppState>,
    path: String,
    db: f64,
    duration_ms: u32,
) -> Result<TrimPreview, String> {
    let (db, duration_ms) = settings::clamp_silence_trim(db, duration_ms);
    let operations = state.preview_operations.clone();
    state
        .preview_operations
        .run_latest(move |cancellation| async move {
            let pause = Arc::new(SessionPause::new());
            let result = converter::analyze_preview_trim(
                std::path::Path::new(&path),
                db,
                duration_ms,
                pause,
                &cancellation,
            )
            .await;
            if cancellation.is_cancelled() {
                return Err(if operations.is_closed() {
                    "application is exiting"
                } else {
                    "preview superseded"
                }
                .to_string());
            }
            Ok(match result {
                Ok((decision, duration)) => trim_preview(Ok(decision), duration),
                Err(error) => trim_preview(Err(error), 0.0),
            })
        })
        .await
}

#[tauri::command]
async fn decode_to_wav(state: State<'_, AppState>, path: String) -> Result<String, String> {
    let generation = state.preview_files.generation();
    let manager = state.spool_manager().clone();
    let temp_file = state
        .preview_operations
        .run(move |cancellation| async move {
            let temp_file = manager.new_temporary_file("preview", "wav");
            let mut command = tokio::process::Command::new(converter::ffmpeg_path());
            command
                .args(["-y", "-i", &path, "-ar", "44100", "-ac", "2", "-f", "wav"])
                .arg(temp_file.path());
            preview_operations::run_process(command, &cancellation).await?;
            Ok(temp_file)
        })
        .await?;
    state.preview_files.retain(temp_file, generation)
}

#[tauri::command]
async fn delete_temp_wav(state: State<'_, AppState>, path: String) -> Result<(), String> {
    state.preview_files.release(&path)
}

#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    // 信頼できる http(s) / mailto スキームのみ許可し、cmd 引数注入を防ぐ
    let lower = url.to_ascii_lowercase();
    let scheme_ok = lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:");
    if !scheme_ok {
        return Err("unsupported url scheme".into());
    }
    if url
        .chars()
        .any(|c| c == '"' || c == '\n' || c == '\r' || c == '\0')
    {
        return Err("invalid characters in url".into());
    }
    #[cfg(target_os = "macos")]
    std::process::Command::new("open")
        .arg(&url)
        .spawn()
        .map_err(|e| e.to_string())?;
    #[cfg(target_os = "windows")]
    {
        // `cmd start` は引数に空白や `&` を含むと壊れるため、URL を引用符で包む。
        // 第 2 引数の `""` は start のウィンドウタイトルプレースホルダ。
        // CREATE_NO_WINDOW で cmd 自体のコンソール表示を抑止する。
        use std::os::windows::process::CommandExt;
        let quoted = format!("\"{}\"", url);
        std::process::Command::new("cmd")
            .args(["/C", "start", "", &quoted])
            .creation_flags(0x08000000)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "linux")]
    std::process::Command::new("xdg-open")
        .arg(&url)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

// --- Activity Monitor commands ---

#[tauri::command]
async fn open_activity_window(app: AppHandle) -> Result<(), String> {
    ensure_window(
        &app,
        "activity",
        dev_url("activity/activity.html"),
        "oTo - Activity",
        480.0,
        540.0,
        true,
    )
    .await
}

#[tauri::command]
fn get_activity_data(
    state: State<'_, AppState>,
    after_seq: Option<u64>,
    known_log_state_revision: Option<u64>,
) -> ActivityData {
    let cpu_percent = {
        let mut sys = state.sys_monitor.lock().unwrap();
        // OS全体の全コア平均。全論理コアが飽和した状態を100%とする値をそのまま使う。
        sys.refresh_cpu_usage();
        (sys.global_cpu_usage() as f64).clamp(0.0, 100.0)
    };
    let input_spool_used_mb = state.temp_cache_used.load(Ordering::Relaxed) as f64 / 1048576.0;
    let output_spool_used_mb = state.output_spool_used.load(Ordering::Relaxed) as f64 / 1048576.0;
    let is_converting = state.is_converting.load(Ordering::SeqCst);
    let conv_start_ts = state.conv_start_ts.load(Ordering::SeqCst);
    let after_seq = after_seq.unwrap_or(0);
    let (log, log_cursor, log_state_revision, log_reset) = {
        let entries = state.conv_log.lock().unwrap();
        let revision = state.log_state_revision.load(Ordering::SeqCst);
        let reset = known_log_state_revision != Some(revision);
        let (log, cursor) = snapshot_conversion_log(&entries, after_seq, reset);
        (log, cursor, revision, reset)
    };
    let active_files: HashMap<String, f32> = state.active_files.lock().unwrap().clone();
    let progress = state.overall_progress.lock().unwrap();
    let processing_count = state.active_artifacts.lock().unwrap().len();
    let unresolved_inputs = progress
        .enumerated_input_count
        .saturating_sub(progress.input_artifact_total.len());
    let waiting_base = progress.target_total.saturating_add(unresolved_inputs);
    ActivityData {
        cpu_percent,
        input_spool_used_mb,
        input_spool_target_mb: INPUT_SPOOL_TARGET_BYTES as f64 / 1048576.0,
        output_spool_used_mb,
        input_spool_waiting: state.input_spool_waiting.load(Ordering::Relaxed),
        output_spool_waiting: state.output_spool_waiting.load(Ordering::Relaxed),
        is_converting,
        log,
        log_cursor,
        log_state_revision,
        log_reset,
        active_files,
        conv_start_ts,
        conversion_elapsed_ms: state.conversion_elapsed_ms.load(Ordering::SeqCst),
        overall_progress_percent: progress.percent,
        completed_count: progress.completed_count,
        target_total: progress.target_total,
        enumerated_input_count: progress.enumerated_input_count,
        completed_input_count: progress.completed_input_count,
        progress_phase: progress.phase,
        scan_complete: progress.scan_complete,
        scanning_batch_count: state.scanning_batches.load(Ordering::Relaxed),
        queued_batch_count: state.queued_batches.load(Ordering::Relaxed),
        waiting_count: waiting_base
            .saturating_sub(progress.completed_count)
            .saturating_sub(processing_count),
        processing_count,
        successful_count: state.successful_count.load(Ordering::Relaxed),
        failed_count: state.failed_count.load(Ordering::Relaxed),
        skipped_count: state.skipped_count.load(Ordering::Relaxed),
    }
}

#[tauri::command]
fn clear_activity_log(state: State<'_, AppState>) {
    let mut log = state.conv_log.lock().unwrap();
    log.clear();
    state.log_state_revision.fetch_add(1, Ordering::SeqCst);
}

// --- App entry ---

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let mut sys = sysinfo::System::new();
    sys.refresh_cpu_usage();

    let state = AppState {
        session: Mutex::new(None),
        is_converting: AtomicBool::new(false),
        cleanup_in_progress: AtomicBool::new(false),
        exiting: AtomicBool::new(false),
        overwrite_tx: std::sync::Mutex::new(None),
        temp_cache_used: Arc::new(AtomicUsize::new(0)),
        output_spool_used: Arc::new(AtomicUsize::new(0)),
        input_spool_waiting: AtomicBool::new(false),
        output_spool_waiting: AtomicBool::new(false),
        sys_monitor: std::sync::Mutex::new(sys),
        conv_log: std::sync::Mutex::new(VecDeque::new()),
        log_state_revision: AtomicU64::new(0),
        log_sequence: AtomicU64::new(0),
        active_files: std::sync::Mutex::new(HashMap::new()),
        active_artifacts: std::sync::Mutex::new(HashSet::new()),
        conv_start_ts: AtomicU64::new(0),
        conversion_elapsed_ms: AtomicU64::new(0),
        overall_progress: std::sync::Mutex::new(OverallProgress::default()),
        scanning_batches: AtomicUsize::new(0),
        queued_batches: AtomicUsize::new(0),
        successful_count: AtomicUsize::new(0),
        failed_count: AtomicUsize::new(0),
        skipped_count: AtomicUsize::new(0),
        spool_manager: std::sync::OnceLock::new(),
        preview_files: preview_files::PreviewFiles::default(),
        preview_operations: Arc::new(preview_operations::PreviewOperations::default()),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            convert_files,
            cancel_job,
            pause_job,
            resume_job,
            get_settings,
            save_settings,
            open_settings_window,
            open_about_window,
            open_licenses_window,
            open_activity_window,
            get_activity_data,
            clear_activity_log,
            pick_folder,
            get_app_version,
            open_url,
            open_silence_preview,
            is_silence_preview_visible,
            get_waveform_data,
            analyze_trim_preview,
            decode_to_wav,
            delete_temp_wav,
            respond_overwrite,
            exit_app,
        ])
        .setup(|app| {
            let app_data_dir = app.path().app_data_dir()?;
            let spool_manager = spool::SpoolManager::initialize(&app_data_dir)?;
            app.state::<AppState>()
                .spool_manager
                .set(spool_manager)
                .map_err(|_| std::io::Error::other("spool manager initialized twice"))?;
            #[cfg(not(target_os = "macos"))]
            if let Some(main_win) = app.get_webview_window("main") {
                let app_handle = app.handle().clone();
                main_win.on_window_event(move |event| {
                    if matches!(event, tauri::WindowEvent::Destroyed) {
                        let app_handle = app_handle.clone();
                        tauri::async_runtime::spawn(async move {
                            shutdown_app(app_handle).await;
                        });
                    }
                });
            }
            #[cfg(target_os = "macos")]
            {
                use tauri::menu::{
                    MenuBuilder, MenuItem, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder,
                };

                let h = app.handle();

                // 「about oTo」クリックでカスタムウィンドウを開くメニュー項目
                let about_item =
                    MenuItem::with_id(h, "open_about", "oTo について", true, None::<&str>)?;
                let quit_item = MenuItemBuilder::with_id("quit", "oTo を終了")
                    .accelerator("CmdOrCtrl+Q")
                    .build(h)?;

                // アプリメニューのみ（File / Edit / View / Window / Help は含めない）
                let app_menu = SubmenuBuilder::new(h, "oTo")
                    .item(&about_item)
                    .separator()
                    .item(&PredefinedMenuItem::services(h, None)?)
                    .separator()
                    .item(&PredefinedMenuItem::hide(h, None)?)
                    .item(&PredefinedMenuItem::hide_others(h, None)?)
                    .item(&PredefinedMenuItem::show_all(h, None)?)
                    .separator()
                    .item(&quit_item)
                    .build()?;

                let menu = MenuBuilder::new(h).item(&app_menu).build()?;
                app.set_menu(menu)?;
            }
            Ok(())
        })
        .on_menu_event(|app, event| {
            if event.id().as_ref() == "open_about" {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let _ = ensure_window(
                        &app,
                        "about",
                        dev_url("about/about.html"),
                        "oTo - About",
                        400.0,
                        460.0,
                        false,
                    )
                    .await;
                });
            } else if event.id() == "quit" {
                let is_conv = app
                    .try_state::<AppState>()
                    .map(|s| s.is_converting.load(std::sync::atomic::Ordering::SeqCst))
                    .unwrap_or(false);
                if is_conv {
                    if let Some(w) = app.get_webview_window("main") {
                        // 非表示状態だとダイアログが見えず詰むため、必ず前面に出す
                        let _ = w.show();
                        let _ = w.unminimize();
                        let _ = w.set_focus();
                        w.emit("quit_requested", ()).ok();
                    }
                } else {
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        shutdown_app(app).await;
                    });
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            if let tauri::RunEvent::ExitRequested { api, .. } = event {
                let exiting = app
                    .try_state::<AppState>()
                    .map(|state| state.exiting.load(Ordering::Acquire))
                    .unwrap_or(false);
                if exiting {
                    return;
                }
                let is_conv = app
                    .try_state::<AppState>()
                    .map(|s| s.is_converting.load(std::sync::atomic::Ordering::SeqCst))
                    .unwrap_or(false);
                if is_conv {
                    api.prevent_exit();
                    if let Some(w) = app.get_webview_window("main") {
                        let _ = w.show();
                        let _ = w.unminimize();
                        let _ = w.set_focus();
                        w.emit("quit_requested", ()).ok();
                    }
                } else {
                    api.prevent_exit();
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        shutdown_app(app).await;
                    });
                }
            }
        });
}

#[tauri::command]
fn respond_overwrite(state: State<'_, AppState>, choice: String, dialog_id: Option<String>) {
    let prompt = state.overwrite_tx.lock().unwrap().take();
    if let Some(prompt) = prompt {
        if dialog_id.as_deref().is_some_and(|id| id != prompt.id) {
            *state.overwrite_tx.lock().unwrap() = Some(prompt);
            return;
        }
        let c = match choice.as_str() {
            "overwrite" => OverwriteChoice::Overwrite,
            "rename" => OverwriteChoice::Rename,
            "skip" => OverwriteChoice::Skip,
            _ => OverwriteChoice::CancelAll,
        };
        prompt.sender.send(c).ok();
    }
}

#[tauri::command]
async fn exit_app(app: AppHandle) {
    shutdown_app(app).await;
}

async fn shutdown_app(app: AppHandle) {
    if let Some(state) = app.try_state::<AppState>() {
        if state.exiting.swap(true, Ordering::AcqRel) {
            return;
        }
        state.cleanup_in_progress.store(true, Ordering::Release);
        state.preview_operations.close();
        state.preview_files.close();
        if let Some(prompt) = state.overwrite_tx.lock().unwrap().take() {
            let _ = prompt.sender.send(OverwriteChoice::CancelAll);
        }
        let session = state.session.lock().await.clone();
        if let Some(session) = session {
            session.cancellation.cancel();
            session.pause.set_paused(false);
            session.wait_for_batches().await;
        }
        state.preview_operations.wait_finished().await;
        state.spool_manager().retry_recovery();
        if let Err(error) = state.spool_manager().cleanup_current_session() {
            eprintln!("failed to clean current spool session: {error}");
        }
        state.input_spool_waiting.store(false, Ordering::Release);
        state.output_spool_waiting.store(false, Ordering::Release);
        state.is_converting.store(false, Ordering::SeqCst);
    }
    app.exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use converter::FileResult;
    use std::io::Write;

    #[test]
    fn trim_preview_maps_decisions() {
        use converter::TrimDecision;
        let trim = trim_preview(
            Ok(TrimDecision::Trim {
                keep_start: 1.0,
                keep_end: 8.0,
                cut_head: true,
                cut_tail: true,
            }),
            10.0,
        );
        assert_eq!(trim.outcome, "trim");
        assert_eq!((trim.keep_start, trim.keep_end), (1.0, 8.0));
        assert_eq!(
            (trim.cut_head, trim.cut_tail, trim.duration),
            (true, true, 10.0)
        );
        let tail_only = trim_preview(
            Ok(TrimDecision::Trim {
                keep_start: 0.0,
                keep_end: 8.0,
                cut_head: false,
                cut_tail: true,
            }),
            10.0,
        );
        assert_eq!((tail_only.cut_head, tail_only.cut_tail), (false, true));
        let none = trim_preview(Ok(TrimDecision::NothingToTrim), 10.0);
        assert_eq!(none.outcome, "none");
        assert_eq!((none.keep_start, none.keep_end), (0.0, 10.0));
        assert_eq!(
            trim_preview(Ok(TrimDecision::AllSilent), 10.0).outcome,
            "allSilent"
        );
        let failed = trim_preview(Err("boom".into()), 10.0);
        assert_eq!(failed.outcome, "failed");
        assert_eq!(failed.error.as_deref(), Some("boom"));
    }

    #[test]
    fn late_process_registration_observes_pause_without_double_suspension() {
        let pause = SessionPause::new();
        let events = std::sync::Mutex::new(Vec::new());
        let signal = |pids: &[i32], stopped| {
            for &pid in pids {
                events.lock().unwrap().push((pid, stopped));
            }
        };
        pause.register_process_with(10, signal);
        assert!(pause.set_paused_with(true, signal));
        pause.register_process_with(20, signal);
        assert!(!pause.set_paused_with(true, signal));
        pause.unregister_process(10);
        assert!(pause.set_paused_with(false, signal));
        assert_eq!(
            *events.lock().unwrap(),
            [(10, true), (20, true), (20, false)]
        );
    }

    #[test]
    fn process_registration_cannot_overtake_pause_signal_delivery() {
        let pause = Arc::new(SessionPause::new());
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let p = pause.clone();
        let transition = std::thread::spawn(move || {
            p.set_paused_with(true, |_, _| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
        });
        entered_rx.recv().unwrap();
        assert!(pause.processes.try_lock().is_err());
        let p = pause.clone();
        let registration = std::thread::spawn(move || {
            let mut stopped = false;
            p.register_process_with(42, |ids, suspend| {
                assert_eq!(ids, [42]);
                stopped = suspend;
            });
            stopped
        });
        release_tx.send(()).unwrap();
        assert!(transition.join().unwrap());
        assert!(registration.join().unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_process_started_during_pause_is_stopped_and_resumed() {
        let pause = Arc::new(SessionPause::new());
        pause.set_paused(true);
        let mut command = tokio::process::Command::new("/bin/sleep");
        command.arg("0.2").kill_on_drop(true).process_group(0);
        let mut child = command.spawn().unwrap();
        let pid = child.id().unwrap();
        pause.register_process(pid);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let mut status = 0;
                let result = unsafe {
                    libc::waitpid(pid as i32, &mut status, libc::WNOHANG | libc::WUNTRACED)
                };
                if result > 0 {
                    assert!(libc::WIFSTOPPED(status));
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        pause.set_paused(false);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(2), child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
        pause.unregister_process(pid);
        assert!(pause.processes.lock().unwrap().is_empty());
    }

    #[test]
    fn incremental_progress_matches_full_sum_across_interleaved_stages_and_batches() {
        let mut progress = OverallProgress::default();
        progress.add_enumerated_inputs(2);
        progress.register_input(1);
        progress.register_input(3);
        progress.update(0, 0.1, false);
        progress.finish_queueing();
        let mut seed = 42_u64;
        for step in 0..10_000 {
            if step == 3_000 {
                progress.begin_queueing();
                progress.add_enumerated_inputs(1);
                progress.register_input(1000);
            }
            if step == 3_010 {
                progress.finish_queueing();
            }
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let index = (seed >> 32) as usize % (progress.target_total + 1);
            let ratio = (seed % 120) as f64 / 100.0 - 0.1;
            let previous_percent = progress.percent;
            progress.update(index, ratio, step % 17 == 0);
            let full_sum: f64 = progress.artifact_progress.iter().sum();
            assert!((progress.progress_sum - full_sum).abs() < 1e-8);
            if progress.phase == ProgressPhase::Exact {
                let expected = full_sum / progress.target_total as f64 * 100.0;
                assert!((progress.percent - expected).abs() < 1e-8);
                assert!(progress.percent >= previous_percent);
            }
        }
        progress.finish_job();
        assert_eq!(progress.progress_sum, progress.target_total as f64);
        assert_eq!(progress.percent, 100.0);
        progress.reset();
        assert_eq!(progress.progress_sum, 0.0);
        progress.register_input(1);
        progress.finish_queueing();
        progress.update(0, 0.5, false);
        assert_eq!(progress.percent, 50.0);
    }

    #[test]
    fn log_cursor_never_skips_entries_appended_after_the_snapshot() {
        let entry = |seq| ConvLogEntry {
            seq,
            activity_id: "session:0".into(),
            ts_ms: 0,
            file_name: "sample.wav".into(),
            status: "processing".into(),
            error: None,
            notice: None,
        };
        let mut log = VecDeque::from([entry(1)]);
        let (first, cursor) = snapshot_conversion_log(&log, 0, false);
        log.push_back(entry(2));
        assert_eq!(first.iter().map(|entry| entry.seq).collect::<Vec<_>>(), [1]);
        assert_eq!(cursor, 1);
        let (next, cursor) = snapshot_conversion_log(&log, cursor, false);
        assert_eq!(next.iter().map(|entry| entry.seq).collect::<Vec<_>>(), [2]);
        assert_eq!(cursor, 2);

        // A clear does not reset the global sequence; the next append is visible.
        log.clear();
        let (empty, cursor) = snapshot_conversion_log(&log, cursor, true);
        assert!(empty.is_empty());
        log.push_back(entry(3));
        let (next, cursor) = snapshot_conversion_log(&log, cursor, false);
        assert_eq!(next[0].seq, 3);
        assert_eq!(cursor, 3);
        assert_eq!(snapshot_conversion_log(&log, cursor, true).0.len(), 1);
    }
    #[tokio::test]
    async fn paused_session_accepts_work_but_does_not_release_it_until_resume() {
        let pause = Arc::new(SessionPause::new());
        let cancellation = Arc::new(JobCancellation::new());
        pause.set_paused(true);
        let waiter = tokio::spawn({
            let pause = pause.clone();
            let cancellation = cancellation.clone();
            async move { pause.wait_until_resumed(&cancellation).await }
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        pause.set_paused(false);
        assert!(waiter.await.unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn active_timeout_excludes_long_and_coalesced_pauses() {
        use std::future::Future;
        use std::task::{Context, Waker};
        use std::time::Duration;
        let pause = SessionPause::new();
        let timeout = pause.wait_for_active_duration(Duration::from_secs(30));
        tokio::pin!(timeout);
        let mut context = Context::from_waker(Waker::noop());
        assert!(timeout.as_mut().poll(&mut context).is_pending());
        tokio::time::advance(Duration::from_secs(5)).await;
        pause.set_paused(true);
        tokio::time::advance(Duration::from_secs(300)).await;
        assert!(timeout.as_mut().poll(&mut context).is_pending());
        assert_eq!(pause.active_elapsed(), Duration::from_secs(5));
        pause.set_paused(false);
        tokio::time::advance(Duration::from_secs(10)).await;
        // Multiple transitions occur without polling the timeout future.
        pause.set_paused(true);
        tokio::time::advance(Duration::from_secs(600)).await;
        assert!(!pause.set_paused(true)); // Repeated pause must not reset accounting.
        pause.set_paused(false);
        tokio::time::advance(Duration::from_secs(14)).await;
        assert_eq!(pause.active_elapsed(), Duration::from_secs(29));
        assert!(timeout.as_mut().poll(&mut context).is_pending());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(timeout.as_mut().poll(&mut context).is_ready());
    }

    #[tokio::test(start_paused = true)]
    async fn paused_timeout_remains_cancellable_and_new_budgets_start_at_zero() {
        use std::future::Future;
        use std::task::{Context, Waker};
        use std::time::Duration;
        let pause = SessionPause::new();
        let cancellation = JobCancellation::new();
        tokio::time::advance(Duration::from_secs(100)).await;
        pause.set_paused(true);
        let result = async {
            tokio::select! {
                _ = pause.wait_for_active_duration(Duration::from_secs(30)) => false,
                _ = cancellation.cancelled() => true,
            }
        };
        tokio::pin!(result);
        let mut context = Context::from_waker(Waker::noop());
        assert!(result.as_mut().poll(&mut context).is_pending());
        tokio::time::advance(Duration::from_secs(1000)).await;
        assert!(result.as_mut().poll(&mut context).is_pending());
        cancellation.cancel();
        assert!(result.await);
        pause.set_paused(false);
        let next = pause.wait_for_active_duration(Duration::from_secs(30));
        tokio::pin!(next);
        assert!(next.as_mut().poll(&mut context).is_pending());
        tokio::time::advance(Duration::from_secs(30)).await;
        assert!(next.as_mut().poll(&mut context).is_ready());
    }

    #[tokio::test]
    async fn cancellation_releases_a_paused_batch_without_starting_it() {
        let pause = Arc::new(SessionPause::new());
        let cancellation = Arc::new(JobCancellation::new());
        pause.set_paused(true);
        let waiter = tokio::spawn({
            let pause = pause.clone();
            let cancellation = cancellation.clone();
            async move { pause.wait_until_resumed(&cancellation).await }
        });
        cancellation.cancel();
        assert!(!waiter.await.unwrap());
    }

    #[test]
    fn reopening_queueing_allows_dynamic_progress_to_move_backwards() {
        let mut progress = OverallProgress::default();
        progress.add_enumerated_inputs(1);
        let first = progress.register_input(1);
        progress.update(first, 1.0, true);
        progress.finish_queueing();
        assert_eq!(progress.percent, 100.0);

        progress.begin_queueing();
        progress.add_enumerated_inputs(1);
        assert_eq!(progress.percent, 50.0);
    }

    fn successful_outcome(
        input: &std::path::Path,
        output: &std::path::Path,
        source_file_action: SourceFileAction,
        batch_order: u64,
    ) -> BatchOutcome {
        BatchOutcome {
            results: vec![FileResult {
                input_path: input.to_string_lossy().into_owned(),
                output_path: output.to_string_lossy().into_owned(),
                success: true,
                skipped: false,
                error: None,
                trim_not_applied: None,
            }],
            settings: Settings {
                source_file_action,
                ..Settings::default()
            },
            mode: "encode".into(),
            format: "mp3".into(),
            batch_order,
        }
    }

    #[test]
    fn source_keep_wins_across_the_whole_session() {
        let dir = std::env::temp_dir().join(format!("oto-source-policy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("source.wav");
        let first_output = dir.join("source.mp3");
        let second_output = dir.join("source_1.mp3");
        std::fs::write(&input, b"source").unwrap();
        std::fs::write(&first_output, b"first").unwrap();
        std::fs::write(&second_output, b"second").unwrap();

        let outcomes = vec![
            successful_outcome(&input, &first_output, SourceFileAction::Delete, 1),
            successful_outcome(&input, &second_output, SourceFileAction::Keep, 2),
        ];
        delete_session_sources(&outcomes, false);
        assert!(input.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn source_delete_is_deferred_until_all_session_references_succeed() {
        let dir = std::env::temp_dir().join(format!("oto-source-delete-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("source.wav");
        let output = dir.join("source.mp3");
        std::fs::write(&input, b"source").unwrap();
        std::fs::write(&output, b"output").unwrap();

        let mut outcomes = vec![successful_outcome(
            &input,
            &output,
            SourceFileAction::Delete,
            1,
        )];
        delete_session_sources(&outcomes, true);
        assert!(
            input.exists(),
            "infrastructure cancellation must retain successful inputs"
        );
        outcomes[0].results.push(FileResult::error(
            input.to_string_lossy(),
            "probe task failed",
        ));
        delete_session_sources(&outcomes, false);
        assert!(input.exists(), "a failed reference must prevent deletion");
        outcomes[0].results.pop();
        delete_session_sources(&outcomes, false);
        assert!(!input.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn queueing_progress_recalculates_when_more_inputs_are_enumerated() {
        let mut progress = OverallProgress::default();
        progress.add_enumerated_inputs(10);
        for _ in 0..10 {
            progress.register_input(1);
        }
        for index in 0..9 {
            progress.update(index, 1.0, true);
        }
        assert_eq!(progress.percent, 90.0);

        progress.add_enumerated_inputs(10);
        assert_eq!(progress.percent, 45.0);
    }

    #[test]
    fn non_media_and_rejected_inputs_only_count_in_the_queueing_denominator() {
        let mut progress = OverallProgress::default();
        progress.add_enumerated_inputs(3);
        let index = progress.register_input(1);
        progress.update(index, 1.0, true);

        assert_eq!(progress.completed_input_count, 1);
        assert!((progress.percent - 100.0 / 3.0).abs() < 0.000_001);

        progress.finish_queueing();
        assert_eq!(progress.percent, 100.0);
        assert_eq!(progress.target_total, 1);
    }

    #[test]
    fn an_input_with_multiple_artifacts_completes_only_after_all_are_terminal() {
        let mut progress = OverallProgress::default();
        progress.add_enumerated_inputs(1);
        let first = progress.register_input(2);

        progress.update(first, 1.0, true);
        assert_eq!(progress.completed_count, 1);
        assert_eq!(progress.completed_input_count, 0);
        assert_eq!(progress.percent, 0.0);

        progress.update(first + 1, 1.0, true);
        progress.update(first + 1, 1.0, true);
        assert_eq!(progress.completed_count, 2);
        assert_eq!(progress.completed_input_count, 1);
        assert_eq!(progress.percent, 99.0);
    }

    #[test]
    fn artifact_partial_progress_does_not_affect_queueing_progress() {
        let mut progress = OverallProgress::default();
        progress.add_enumerated_inputs(1);
        let index = progress.register_input(1);
        progress.update(index, 0.98, false);

        assert_eq!(progress.percent, 0.0);
        assert_eq!(progress.artifact_progress[index], 0.98);
    }

    #[test]
    fn switching_to_exact_progress_recalculates_then_remains_monotonic() {
        let mut progress = OverallProgress::default();
        progress.add_enumerated_inputs(2);
        let completed = progress.register_input(1);
        progress.update(completed, 1.0, true);
        progress.register_input(9);
        assert_eq!(progress.percent, 50.0);

        progress.finish_queueing();
        assert_eq!(progress.percent, 10.0);
        progress.update(1, 0.5, false);
        assert_eq!(progress.percent, 15.0);
        progress.update(1, 0.2, false);
        assert_eq!(progress.percent, 15.0);
    }

    #[test]
    fn queueing_progress_is_capped_until_the_job_finishes() {
        let mut progress = OverallProgress::default();
        progress.add_enumerated_inputs(1);
        let index = progress.register_input(1);
        progress.update(index, 1.0, true);
        assert_eq!(progress.percent, 99.0);

        progress.finish_job();
        assert_eq!(progress.percent, 100.0);
        assert_eq!(progress.phase, ProgressPhase::Exact);
    }

    #[test]
    fn finishing_a_job_makes_failed_or_unfinished_targets_terminal() {
        let mut progress = OverallProgress::default();
        progress.add_enumerated_inputs(1);
        progress.register_input(3);
        progress.update(0, 1.0, true);
        progress.update(1, 0.4, false);
        progress.finish_job();

        assert_eq!(progress.completed_count, 3);
        assert_eq!(progress.target_total, 3);
        assert_eq!(progress.percent, 100.0);
        assert!(progress.scan_complete);
        assert_eq!(progress.completed_input_count, 1);
        assert!(progress.artifact_terminal.iter().all(|terminal| *terminal));
    }

    #[test]
    fn cancelling_keeps_completed_artifacts_even_when_filenames_match() {
        let mut log = VecDeque::from([
            ConvLogEntry {
                seq: 1,
                activity_id: "session:0".into(),
                ts_ms: 0,
                file_name: "sample.wav".into(),
                status: "processing".into(),
                error: None,
                notice: None,
            },
            ConvLogEntry {
                seq: 2,
                activity_id: "session:0".into(),
                ts_ms: 1,
                file_name: "sample.wav".into(),
                status: "done".into(),
                error: None,
                notice: None,
            },
            ConvLogEntry {
                seq: 3,
                activity_id: "session:1".into(),
                ts_ms: 2,
                file_name: "sample.wav".into(),
                status: "processing".into(),
                error: None,
                notice: None,
            },
            ConvLogEntry {
                seq: 4,
                activity_id: "session:1".into(),
                ts_ms: 3,
                file_name: "sample.wav".into(),
                status: "error".into(),
                error: Some("failed".into()),
                notice: None,
            },
            ConvLogEntry {
                seq: 5,
                activity_id: "session:2".into(),
                ts_ms: 4,
                file_name: "sample.wav".into(),
                status: "processing".into(),
                error: None,
                notice: None,
            },
        ]);

        mark_unfinished_logs_cancelled(&mut log);

        assert_eq!(log[1].status, "done");
        assert_eq!(log[3].status, "error");
        assert_eq!(log[4].status, "cancelled");
    }

    /// テスト用一時 RAW ファイルを作成して compute_waveform_streaming を検証
    fn write_raw_f32(samples: &[f32]) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("oto_test_{}.raw", uuid::Uuid::new_v4()));
        let mut f = std::fs::File::create(&path).unwrap();
        for s in samples {
            f.write_all(&s.to_le_bytes()).unwrap();
        }
        path
    }

    #[test]
    fn waveform_block_boundaries_match_reference_peaks_and_rms() {
        let samples: Vec<f32> = (0..65_539)
            .map(|index| ((index * 37 % 1000) as f32 - 500.0) / 500.0)
            .collect();
        let path = write_raw_f32(&samples);
        let resolutions = [800, 8000, 80000];
        let levels =
            compute_waveform_streaming(&path, samples.len(), &resolutions, &JobCancellation::new())
                .unwrap();
        std::fs::remove_file(&path).unwrap();
        for (level, resolution) in levels.iter().zip(resolutions) {
            for bucket in 0..resolution {
                let start = (bucket * samples.len()).div_ceil(resolution);
                let end = ((bucket + 1) * samples.len()).div_ceil(resolution);
                let values = &samples[start..end];
                let expected = if values.is_empty() {
                    ((0.0, 0.0), 0.0)
                } else {
                    let min = values.iter().copied().fold(f32::INFINITY, f32::min);
                    let max = values.iter().copied().fold(f32::NEG_INFINITY, f32::max);
                    let sum = values.iter().map(|value| value * value).sum::<f32>();
                    ((min, max), (sum / values.len() as f32).sqrt())
                };
                assert_eq!(level.peaks[bucket], expected.0);
                assert_eq!(level.rms[bucket], expected.1);
            }
        }
    }

    #[test]
    fn waveform_missing_truncated_and_cancelled_reads_are_errors() {
        let path = write_raw_f32(&[0.5, -0.5]);
        assert!(
            compute_waveform_streaming(&path, 10, &[1], &JobCancellation::new())
                .unwrap_err()
                .contains("waveform read failed")
        );
        let cancellation = JobCancellation::new();
        cancellation.cancel();
        assert!(compute_waveform_streaming(&path, 2, &[1], &cancellation)
            .unwrap_err()
            .contains("exiting"));
        std::fs::remove_file(&path).unwrap();
        assert!(compute_waveform_streaming(&path, 2, &[1], &JobCancellation::new()).is_err());
    }

    #[test]
    fn waveform_streaming_empty_file_returns_zeros() {
        let path = write_raw_f32(&[]);
        let levels = compute_waveform_streaming(&path, 0, &[100], &JobCancellation::new()).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(levels.len(), 1);
        assert!(levels[0]
            .peaks
            .iter()
            .all(|&(mn, mx)| mn == 0.0 && mx == 0.0));
        assert!(levels[0].rms.iter().all(|&r| r == 0.0));
    }

    #[test]
    fn waveform_streaming_constant_signal() {
        // 全サンプル 0.5 の定常信号 → ピーク min/max は 0.5、RMS も 0.5
        let samples: Vec<f32> = vec![0.5; 1000];
        let path = write_raw_f32(&samples);
        let levels =
            compute_waveform_streaming(&path, samples.len(), &[10], &JobCancellation::new())
                .unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(levels.len(), 1);
        for &(mn, mx) in &levels[0].peaks {
            assert!((mn - 0.5).abs() < 1e-4, "min={mn}");
            assert!((mx - 0.5).abs() < 1e-4, "max={mx}");
        }
        for &r in &levels[0].rms {
            assert!((r - 0.5).abs() < 1e-4, "rms={r}");
        }
    }

    #[test]
    fn waveform_streaming_multi_resolution() {
        let samples: Vec<f32> = (0..2000).map(|i| (i as f32 / 2000.0) * 2.0 - 1.0).collect();
        let path = write_raw_f32(&samples);
        let levels = compute_waveform_streaming(
            &path,
            samples.len(),
            &[50, 100, 200],
            &JobCancellation::new(),
        )
        .unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(levels.len(), 3);
        assert_eq!(levels[0].peaks.len(), 50);
        assert_eq!(levels[1].peaks.len(), 100);
        assert_eq!(levels[2].peaks.len(), 200);
    }

    #[test]
    fn waveform_streaming_clamps_to_minus_one_plus_one() {
        // 1.5 や -1.5 など ±1 を超えるサンプルはクランプされる
        let samples = vec![2.0f32, -2.0, 1.5, -1.5];
        let path = write_raw_f32(&samples);
        let levels =
            compute_waveform_streaming(&path, samples.len(), &[1], &JobCancellation::new())
                .unwrap();
        let _ = std::fs::remove_file(&path);
        let (mn, mx) = levels[0].peaks[0];
        assert!(mn >= -1.0, "min {mn} < -1.0");
        assert!(mx <= 1.0, "max {mx} > 1.0");
    }
}
