use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Manager, State, WebviewUrl, WebviewWindowBuilder};
use tokio::sync::Mutex;

mod converter;
mod settings;

use converter::{run_conversion, ConvertRequest};
use settings::Settings;

pub struct JobInfo {
    pub handle: tokio::task::JoinHandle<()>,
    pub pgids: Arc<Mutex<Vec<i32>>>,
    pub paused: AtomicBool,
}

pub struct AppState {
    pub jobs: Mutex<HashMap<String, JobInfo>>,
}

// --- Commands ---

#[tauri::command]
async fn convert_files(
    app: AppHandle,
    state: State<'_, AppState>,
    job_id: String,
    request: ConvertRequest,
) -> Result<(), String> {
    let current_settings = settings::load_settings(&app).map_err(|e| e.to_string())?;
    let pgids: Arc<Mutex<Vec<i32>>> = Arc::new(Mutex::new(vec![]));

    let job_id_clone = job_id.clone();
    let app_clone = app.clone();
    let app_for_cleanup = app.clone();
    let job_id_for_cleanup = job_id.clone();
    let pgids_for_conv = pgids.clone();

    let handle = tokio::spawn(async move {
        run_conversion(app_clone, job_id_clone, request, current_settings, pgids_for_conv).await;
        app_for_cleanup.state::<AppState>().jobs.lock().await.remove(&job_id_for_cleanup);
    });

    state.jobs.lock().await.insert(job_id.clone(), JobInfo { handle, pgids, paused: AtomicBool::new(false) });
    Ok(())
}

#[tauri::command]
async fn cancel_job(
    state: State<'_, AppState>,
    job_id: String,
) -> Result<(), String> {
    let mut jobs = state.jobs.lock().await;
    if let Some(job) = jobs.remove(&job_id) {
        job.handle.abort();
        #[cfg(unix)]
        {
            let pgids = job.pgids.lock().await;
            for &pgid in pgids.iter() {
                unsafe { libc::kill(-pgid, libc::SIGKILL); }
            }
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
            let pids = job.pgids.lock().await;
            for &pid in pids.iter() {
                unsafe {
                    let handle = OpenProcess(PROCESS_TERMINATE, 0, pid as u32);
                    if !handle.is_null() {
                        TerminateProcess(handle, 1);
                        CloseHandle(handle);
                    }
                }
            }
        }
    }
    Ok(())
}

#[tauri::command]
async fn pause_job(
    state: State<'_, AppState>,
    job_id: String,
) -> Result<(), String> {
    let jobs = state.jobs.lock().await;
    if let Some(job) = jobs.get(&job_id) {
        if job.paused.swap(true, Ordering::SeqCst) { return Ok(()); }
        #[cfg(unix)]
        {
            let pgids = job.pgids.lock().await;
            for &pgid in pgids.iter() {
                unsafe { libc::kill(-pgid, libc::SIGSTOP); }
            }
        }
        #[cfg(windows)]
        suspend_resume_windows_processes(&job.pgids, true).await;
    }
    Ok(())
}

#[tauri::command]
async fn resume_job(
    state: State<'_, AppState>,
    job_id: String,
) -> Result<(), String> {
    let jobs = state.jobs.lock().await;
    if let Some(job) = jobs.get(&job_id) {
        if !job.paused.swap(false, Ordering::SeqCst) { return Ok(()); }
        #[cfg(unix)]
        {
            let pgids = job.pgids.lock().await;
            for &pgid in pgids.iter() {
                unsafe { libc::kill(-pgid, libc::SIGCONT); }
            }
        }
        #[cfg(windows)]
        suspend_resume_windows_processes(&job.pgids, false).await;
    }
    Ok(())
}

/// Windows: 対象プロセスの全スレッドを一時停止または再開する
#[cfg(windows)]
async fn suspend_resume_windows_processes(
    pids: &Arc<Mutex<Vec<i32>>>,
    suspend: bool,
) {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, THREADENTRY32, TH32CS_SNAPTHREAD,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, SuspendThread, THREAD_SUSPEND_RESUME};

    let pids_guard = pids.lock().await;
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
                            if suspend { SuspendThread(thread); } else { ResumeThread(thread); }
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

#[tauri::command]
async fn open_settings_window(app: AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("settings") {
        win.show().map_err(|e| e.to_string())?;
        win.set_focus().map_err(|e| e.to_string())?;
    } else {
        WebviewWindowBuilder::new(&app, "settings", WebviewUrl::App("settings/settings.html".into()))
            .title("oTo - Settings")
            .inner_size(480.0, 560.0)
            .resizable(false)
            .build()
            .map_err(|e: tauri::Error| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
async fn open_about_window(app: AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("about") {
        win.show().map_err(|e| e.to_string())?;
        win.set_focus().map_err(|e| e.to_string())?;
    } else {
        WebviewWindowBuilder::new(&app, "about", WebviewUrl::App("about/about.html".into()))
            .title("oTo - About")
            .inner_size(400.0, 460.0)
            .resizable(false)
            .build()
            .map_err(|e: tauri::Error| e.to_string())?;
    }
    Ok(())
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
struct WaveformData {
    peaks_ch0: Vec<(f32, f32)>,
    duration_secs: f64,
}

fn compute_peaks(samples: &[f32], num_buckets: usize) -> Vec<(f32, f32)> {
    if samples.is_empty() { return vec![(0.0, 0.0); num_buckets]; }
    let bucket_size = ((samples.len() + num_buckets - 1) / num_buckets).max(1);
    (0..num_buckets).map(|i| {
        let start = i * bucket_size;
        let end = ((i + 1) * bucket_size).min(samples.len());
        if start >= samples.len() { return (0.0f32, 0.0f32); }
        let chunk = &samples[start..end];
        let mn = chunk.iter().cloned().fold(f32::INFINITY, f32::min).max(-1.0).min(1.0);
        let mx = chunk.iter().cloned().fold(f32::NEG_INFINITY, f32::max).max(-1.0).min(1.0);
        (mn, mx)
    }).collect()
}

#[tauri::command]
async fn open_silence_preview(app: AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("silence-preview") {
        win.show().map_err(|e| e.to_string())?;
        win.set_focus().map_err(|e| e.to_string())?;
    } else {
        WebviewWindowBuilder::new(&app, "silence-preview", WebviewUrl::App("silence-preview/preview.html".into()))
            .title("無音トリミング - 詳細設定")
            .inner_size(820.0, 560.0)
            .resizable(true)
            .build()
            .map_err(|e: tauri::Error| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
async fn get_waveform_data(path: String) -> Result<WaveformData, String> {
    tokio::task::spawn_blocking(move || {
        let ffmpeg = converter::ffmpeg_path();
        let mut temp = std::env::temp_dir();
        temp.push(format!("oto_wave_{}.raw", std::process::id()));

        let mut cmd = std::process::Command::new(&ffmpeg);
        cmd.args(["-y", "-i", &path, "-ac", "1", "-ar", "4000", "-f", "f32le",
                  &temp.to_string_lossy().into_owned()]);
        #[cfg(windows)]
        { use std::os::windows::process::CommandExt; cmd.creation_flags(0x08000000); }
        cmd.stderr(std::process::Stdio::null());
        cmd.output().map_err(|e| e.to_string())?;

        let raw = std::fs::read(&temp).map_err(|_| "decode failed".to_string())?;
        let _ = std::fs::remove_file(&temp);
        if raw.is_empty() { return Err("no audio data".to_string()); }

        let samples: Vec<f32> = raw.chunks_exact(4)
            .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
            .collect();
        let duration_secs = samples.len() as f64 / 4000.0;
        let peaks_ch0 = compute_peaks(&samples, 750);
        Ok(WaveformData { peaks_ch0, duration_secs })
    }).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn get_silence_regions(path: String, db: f64, duration_ms: u32) -> Result<Vec<(f64, f64)>, String> {
    tokio::task::spawn_blocking(move || {
        let ffmpeg = converter::ffmpeg_path();
        let dur_secs = duration_ms as f64 / 1000.0;
        let filter = format!("silencedetect=noise={db}dB:duration={dur_secs:.4}");

        let mut cmd = std::process::Command::new(&ffmpeg);
        cmd.args(["-i", &path, "-af", &filter, "-f", "null", "-"])
           .stderr(std::process::Stdio::piped());
        #[cfg(windows)]
        { use std::os::windows::process::CommandExt; cmd.creation_flags(0x08000000); }

        let out = cmd.output().map_err(|e| e.to_string())?;
        let stderr = String::from_utf8_lossy(&out.stderr);

        let mut regions: Vec<(f64, f64)> = Vec::new();
        let mut cur_start: Option<f64> = None;
        for line in stderr.lines() {
            if let Some(pos) = line.find("silence_start:") {
                if let Ok(t) = line[pos + 14..].trim().parse::<f64>() {
                    cur_start = Some(t.max(0.0));
                }
            } else if let Some(pos) = line.find("silence_end:") {
                if let Some(start) = cur_start.take() {
                    let s = line[pos + 12..].split('|').next().unwrap_or("").trim();
                    if let Ok(end) = s.parse::<f64>() {
                        regions.push((start, end));
                    }
                }
            }
        }
        Ok(regions)
    }).await.map_err(|e| e.to_string())?
}

#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    std::process::Command::new("open").arg(&url).spawn().map_err(|e| e.to_string())?;
    #[cfg(target_os = "windows")]
    std::process::Command::new("cmd").args(["/C", "start", "", &url]).spawn().map_err(|e| e.to_string())?;
    #[cfg(target_os = "linux")]
    std::process::Command::new("xdg-open").arg(&url).spawn().map_err(|e| e.to_string())?;
    Ok(())
}

// --- App entry ---

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let state = AppState {
        jobs: Mutex::new(HashMap::new()),
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
            pick_folder,
            get_app_version,
            open_url,
            open_silence_preview,
            get_waveform_data,
            get_silence_regions,
        ])
        .setup(|app| {
            #[cfg(not(target_os = "macos"))]
            let _ = &app;
            #[cfg(target_os = "macos")]
            {
                use tauri::menu::{MenuBuilder, PredefinedMenuItem, SubmenuBuilder, MenuItem};

                let h = app.handle();

                // 「about oTo」クリックでカスタムウィンドウを開くメニュー項目
                let about_item = MenuItem::with_id(h, "open_about", "oTo について", true, None::<&str>)?;

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
                    .item(&PredefinedMenuItem::quit(h, None)?)
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
                    if let Some(win) = app.get_webview_window("about") {
                        let _ = win.show();
                        let _ = win.set_focus();
                    } else {
                        let _ = WebviewWindowBuilder::new(
                            &app,
                            "about",
                            tauri::WebviewUrl::App("about/about.html".into()),
                        )
                        .title("oTo - About")
                        .inner_size(400.0, 460.0)
                        .resizable(false)
                        .build();
                    }
                });
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
