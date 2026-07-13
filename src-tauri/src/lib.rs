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
    pub ts_ms: u64,
    pub file_name: String,
    pub status: String, // "processing" | "done" | "error" | "skipped"
    pub error: Option<String>,
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
        .map(|entry| entry.file_name.clone())
        .collect();
    for entry in log.iter_mut() {
        if entry.status == "processing" && !completed.contains(&entry.file_name) {
            entry.status = "cancelled".to_string();
        }
    }
}

mod converter;
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
}

impl SessionPause {
    fn new() -> Self {
        Self {
            paused: AtomicBool::new(false),
            changed: tokio::sync::Notify::new(),
        }
    }

    fn set_paused(&self, paused: bool) -> bool {
        let changed = self.paused.swap(paused, Ordering::AcqRel) != paused;
        if changed && !paused {
            self.changed.notify_waiters();
        }
        changed
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
        self.artifact_progress.iter().sum::<f64>() / self.target_total.max(1) as f64 * 100.0
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
    pgids: Arc<std::sync::Mutex<Vec<i32>>>,
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
    /// 現在の変換がネットワークフォルダ対象かどうか
    pub is_network_conv: AtomicBool,
    /// 変換中のファイルと進捗比率（0.0–1.0）
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
    system_cpu_percent: f64,
    input_spool_used_mb: f64,
    input_spool_target_mb: f64,
    output_spool_used_mb: f64,
    input_spool_waiting: bool,
    output_spool_waiting: bool,
    is_network: bool,
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
            pgids: Arc::new(std::sync::Mutex::new(Vec::new())),
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
        state.is_network_conv.store(false, Ordering::Relaxed);
        state.successful_count.store(0, Ordering::Relaxed);
        state.failed_count.store(0, Ordering::Relaxed);
        state.skipped_count.store(0, Ordering::Relaxed);
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
            pgids: session_for_batch.pgids.clone(),
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
        }
        state.log_state_revision.fetch_add(1, Ordering::SeqCst);
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
        if !job.pause.set_paused(true) {
            return Ok(());
        }
        #[cfg(unix)]
        {
            let pgids = job.pgids.lock().unwrap();
            for &pgid in pgids.iter() {
                unsafe {
                    libc::kill(-pgid, libc::SIGSTOP);
                }
            }
        }
        #[cfg(windows)]
        suspend_resume_windows_processes(&job.pgids, true);
    }
    Ok(())
}

#[tauri::command]
async fn resume_job(state: State<'_, AppState>, job_id: String) -> Result<(), String> {
    let session = state.session.lock().await.clone();
    if let Some(job) = session.filter(|session| session.id == job_id) {
        if !job.pause.set_paused(false) {
            return Ok(());
        }
        #[cfg(unix)]
        {
            let pgids = job.pgids.lock().unwrap();
            for &pgid in pgids.iter() {
                unsafe {
                    libc::kill(-pgid, libc::SIGCONT);
                }
            }
        }
        #[cfg(windows)]
        suspend_resume_windows_processes(&job.pgids, false);
    }
    Ok(())
}

/// Windows: 対象プロセスの全スレッドを一時停止または再開する
#[cfg(windows)]
fn suspend_resume_windows_processes(pids: &Arc<std::sync::Mutex<Vec<i32>>>, suspend: bool) {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::Threading::{
        OpenThread, ResumeThread, SuspendThread, THREAD_SUSPEND_RESUME,
    };

    let pids_guard = pids.lock().unwrap();
    for &pid in pids_guard.iter() {
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

#[derive(Serialize)]
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
) -> Vec<WaveformLevel> {
    use std::io::Read;
    type Acc = (f32, f32, f32, u32);
    let mut accs: Vec<Vec<Acc>> = resolutions
        .iter()
        .map(|&res| vec![(f32::INFINITY, f32::NEG_INFINITY, 0.0, 0); res])
        .collect();

    if let Ok(file) = std::fs::File::open(path) {
        let mut reader = std::io::BufReader::with_capacity(262144, file);
        let mut buf = [0u8; 4];
        let mut idx = 0usize;
        while reader.read_exact(&mut buf).is_ok() {
            let s = f32::from_le_bytes(buf);
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
            idx += 1;
        }
    }

    accs.into_iter()
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
        .collect()
}

#[tauri::command]
async fn get_waveform_data(path: String) -> Result<WaveformData, String> {
    tokio::task::spawn_blocking(move || {
        let ffmpeg = converter::ffmpeg_path();

        let uuid = uuid::Uuid::new_v4();
        let mut temp = std::env::temp_dir();
        temp.push(format!("oto_wave_{}.raw", uuid));

        let mut cmd = std::process::Command::new(&ffmpeg);
        cmd.args([
            "-y",
            "-i",
            &path,
            "-ar",
            "4000",
            "-f",
            "f32le",
            "-ac",
            "1",
            &*temp.to_string_lossy(),
        ]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000);
        }
        cmd.stderr(std::process::Stdio::piped());
        let output = cmd.output().map_err(|e| e.to_string())?;
        if !output.status.success() {
            let _ = std::fs::remove_file(&temp);
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "ffmpeg failed: {}",
                stderr.lines().last().unwrap_or("unknown error")
            ));
        }

        let file_size = std::fs::metadata(&temp).map_err(|e| e.to_string())?.len() as usize;
        if file_size < 8 {
            let _ = std::fs::remove_file(&temp);
            return Err("no audio data".to_string());
        }
        let num_samples = file_size / 4;
        let duration_secs = num_samples as f64 / 4000.0;

        let resolutions = [800_usize, 8000, 80000];
        let levels = compute_waveform_streaming(&temp, num_samples, &resolutions);
        let _ = std::fs::remove_file(&temp);

        Ok(WaveformData {
            levels,
            duration_secs,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn decode_to_wav(path: String) -> Result<String, String> {
    tokio::task::spawn_blocking(move || {
        let ffmpeg = converter::ffmpeg_path();
        let uuid = uuid::Uuid::new_v4();
        let mut temp = std::env::temp_dir();
        temp.push(format!("oto_preview_{}.wav", uuid));
        let temp_path = temp.to_string_lossy().into_owned();

        let mut cmd = std::process::Command::new(&ffmpeg);
        cmd.args([
            "-y", "-i", &path, "-ar", "44100", "-ac", "2", "-f", "wav", &temp_path,
        ]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000);
        }
        cmd.stderr(std::process::Stdio::piped());
        let output = cmd.output().map_err(|e| e.to_string())?;
        if !output.status.success() {
            let _ = std::fs::remove_file(&temp);
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "decode to wav failed: {}",
                stderr.lines().last().unwrap_or("unknown error")
            ));
        }

        // Return path; caller uses convertFileSrc() and is responsible for cleanup
        Ok(temp_path)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn delete_temp_wav(path: String) -> Result<(), String> {
    let p = std::path::Path::new(&path);
    let temp_dir = std::env::temp_dir();
    let name = p.file_name().unwrap_or_default().to_string_lossy();
    if !p.starts_with(&temp_dir) || !name.starts_with("oto_preview_") || !name.ends_with(".wav") {
        return Err("invalid path".to_string());
    }
    tokio::fs::remove_file(&path)
        .await
        .map_err(|e| e.to_string())
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
    let is_network = state.is_network_conv.load(Ordering::Relaxed);
    let is_converting = state.is_converting.load(Ordering::SeqCst);
    let conv_start_ts = state.conv_start_ts.load(Ordering::SeqCst);
    let after_seq = after_seq.unwrap_or(0);
    let log_state_revision = state.log_state_revision.load(Ordering::SeqCst);
    // キャンセルのように既存行のstatusが変わった場合だけ、次回は全体を送り直す。
    let log_reset = known_log_state_revision != Some(log_state_revision);
    let log: Vec<ConvLogEntry> = state
        .conv_log
        .lock()
        .unwrap()
        .iter()
        .filter(|entry| log_reset || entry.seq > after_seq)
        .cloned()
        .collect();
    let log_cursor = state.log_sequence.load(Ordering::SeqCst);
    let active_files: HashMap<String, f32> = state.active_files.lock().unwrap().clone();
    let progress = state.overall_progress.lock().unwrap();
    let processing_count = state.active_artifacts.lock().unwrap().len();
    let unresolved_inputs = progress
        .enumerated_input_count
        .saturating_sub(progress.input_artifact_total.len());
    let waiting_base = progress.target_total.saturating_add(unresolved_inputs);
    ActivityData {
        cpu_percent,
        system_cpu_percent: cpu_percent,
        input_spool_used_mb,
        input_spool_target_mb: INPUT_SPOOL_TARGET_BYTES as f64 / 1048576.0,
        output_spool_used_mb,
        input_spool_waiting: state.input_spool_waiting.load(Ordering::Relaxed),
        output_spool_waiting: state.output_spool_waiting.load(Ordering::Relaxed),
        is_network,
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
    state.conv_log.lock().unwrap().clear();
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
        is_network_conv: AtomicBool::new(false),
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
            // 過去セッションが残した一時ファイル (プレビュー、進捗、波形、ネットワークキャッシュ) を掃除
            cleanup_stale_temp_files();
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
        if let Some(prompt) = state.overwrite_tx.lock().unwrap().take() {
            let _ = prompt.sender.send(OverwriteChoice::CancelAll);
        }
        let session = state.session.lock().await.clone();
        if let Some(session) = session {
            session.cancellation.cancel();
            session.pause.set_paused(false);
            session.wait_for_batches().await;
        }
        state.spool_manager().retry_recovery();
        if let Err(error) = state.spool_manager().cleanup_current_session() {
            eprintln!("failed to clean current spool session: {error}");
        }
        state.input_spool_waiting.store(false, Ordering::Release);
        state.output_spool_waiting.store(false, Ordering::Release);
        state.is_converting.store(false, Ordering::SeqCst);
    }
    cleanup_stale_temp_files();
    app.exit(0);
}

/// アプリ起動時・終了時に呼び、temp ディレクトリに残った oTo 用一時ファイルを除去する。
/// クラッシュやキャンセル時の SIGKILL で削除されなかったファイルもここで掃除する。
fn cleanup_stale_temp_files() {
    let dir = std::env::temp_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let is_target = (name.starts_with("oto_preview_") && name.ends_with(".wav"))
            || (name.starts_with("oto_p") && name.ends_with(".txt"))
            || (name.starts_with("oto_wave_") && name.ends_with(".raw"));
        if is_target {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use converter::FileResult;
    use std::io::Write;

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

        let outcomes = vec![successful_outcome(
            &input,
            &output,
            SourceFileAction::Delete,
            1,
        )];
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
    fn cancelling_keeps_completed_log_entries_unchanged() {
        let mut log = VecDeque::from([
            ConvLogEntry {
                seq: 1,
                ts_ms: 0,
                file_name: "done.mp3".into(),
                status: "processing".into(),
                error: None,
            },
            ConvLogEntry {
                seq: 2,
                ts_ms: 1,
                file_name: "done.mp3".into(),
                status: "done".into(),
                error: None,
            },
            ConvLogEntry {
                seq: 3,
                ts_ms: 2,
                file_name: "failed.mp3".into(),
                status: "processing".into(),
                error: None,
            },
            ConvLogEntry {
                seq: 4,
                ts_ms: 3,
                file_name: "failed.mp3".into(),
                status: "error".into(),
                error: Some("failed".into()),
            },
            ConvLogEntry {
                seq: 5,
                ts_ms: 4,
                file_name: "running.mp3".into(),
                status: "processing".into(),
                error: None,
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
    fn waveform_streaming_empty_file_returns_zeros() {
        let path = write_raw_f32(&[]);
        let levels = compute_waveform_streaming(&path, 0, &[100]);
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
        let levels = compute_waveform_streaming(&path, samples.len(), &[10]);
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
        let levels = compute_waveform_streaming(&path, samples.len(), &[50, 100, 200]);
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
        let levels = compute_waveform_streaming(&path, samples.len(), &[1]);
        let _ = std::fs::remove_file(&path);
        let (mn, mx) = levels[0].peaks[0];
        assert!(mn >= -1.0, "min {mn} < -1.0");
        assert!(mx <= 1.0, "max {mx} > 1.0");
    }
}
