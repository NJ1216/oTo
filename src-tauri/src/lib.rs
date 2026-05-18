use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl};
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
    pub is_converting: AtomicBool,
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

    state.is_converting.store(true, Ordering::SeqCst);
    let job_id_clone = job_id.clone();
    let app_clone = app.clone();
    let app_for_cleanup = app.clone();
    let job_id_for_cleanup = job_id.clone();
    let pgids_for_conv = pgids.clone();

    let handle = tokio::spawn(async move {
        run_conversion(app_clone, job_id_clone, request, current_settings, pgids_for_conv).await;
        app_for_cleanup.state::<AppState>().jobs.lock().await.remove(&job_id_for_cleanup);
        app_for_cleanup.state::<AppState>().is_converting.store(false, Ordering::SeqCst);
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
                let ret = unsafe { libc::kill(-pgid, libc::SIGKILL) };
                if ret != 0 {
                    eprintln!("kill({}, SIGKILL) failed: {}", -pgid, std::io::Error::last_os_error());
                }
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
                        if TerminateProcess(handle, 1) == 0 {
                            eprintln!("TerminateProcess failed for pid {}", pid);
                        }
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
                            let ret = if suspend { SuspendThread(thread) } else { ResumeThread(thread) };
                            if ret == u32::MAX {
                                eprintln!(
                                    "{} failed for thread {}",
                                    if suspend { "SuspendThread" } else { "ResumeThread" },
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
    WebviewUrl::External(format!("http://localhost:1420/src/{}", path).parse().unwrap())
}

/// Helper to create a prod-mode URL for a given relative path.
#[cfg(not(dev))]
fn dev_url(path: &str) -> WebviewUrl {
    WebviewUrl::App(path.into())
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
            .title(title).inner_size(width, height).resizable(resizable)
            .build().map_err(|e: tauri::Error| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
async fn open_settings_window(app: AppHandle) -> Result<(), String> {
    ensure_window(&app, "settings", dev_url("settings/settings.html"), "oTo - Settings", 480.0, 560.0, false).await
}

#[tauri::command]
async fn open_about_window(app: AppHandle) -> Result<(), String> {
    ensure_window(&app, "about", dev_url("about/about.html"), "oTo - About", 400.0, 460.0, false).await
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
        let win = tauri::WebviewWindowBuilder::new(&app, label, dev_url("silence-preview/preview.html"))
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

fn compute_waveform_streaming(path: &std::path::Path, num_samples: usize, resolutions: &[usize]) -> Vec<WaveformLevel> {
    use std::io::Read;
    type Acc = (f32, f32, f32, u32);
    let mut accs: Vec<Vec<Acc>> = resolutions.iter()
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
                    if s < a.0 { a.0 = s; }
                    if s > a.1 { a.1 = s; }
                    a.2 += s * s;
                    a.3 += 1;
                }
            }
            idx += 1;
        }
    }

    accs.into_iter().map(|res_acc| {
        let mut peaks = Vec::with_capacity(res_acc.len());
        let mut rms   = Vec::with_capacity(res_acc.len());
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
    }).collect()
}

#[tauri::command]
async fn get_waveform_data(path: String) -> Result<WaveformData, String> {
    tokio::task::spawn_blocking(move || {
        let ffmpeg = converter::ffmpeg_path();

        let uuid = uuid::Uuid::new_v4();
        let mut temp = std::env::temp_dir();
        temp.push(format!("oto_wave_{}.raw", uuid));

        let mut cmd = std::process::Command::new(&ffmpeg);
        cmd.args(["-y", "-i", &path, "-ar", "4000", "-f", "f32le", "-ac", "1",
                   &*temp.to_string_lossy()]);
        #[cfg(windows)]
        { use std::os::windows::process::CommandExt; cmd.creation_flags(0x08000000); }
        cmd.stderr(std::process::Stdio::piped());
        let output = cmd.output().map_err(|e| e.to_string())?;
        if !output.status.success() {
            let _ = std::fs::remove_file(&temp);
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("ffmpeg failed: {}", stderr.lines().last().unwrap_or("unknown error")));
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

        Ok(WaveformData { levels, duration_secs })
    }).await.map_err(|e| e.to_string())?
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
        cmd.args(["-y", "-i", &path, "-ar", "44100", "-ac", "2", "-f", "wav", &temp_path]);
        #[cfg(windows)]
        { use std::os::windows::process::CommandExt; cmd.creation_flags(0x08000000); }
        cmd.stderr(std::process::Stdio::piped());
        let output = cmd.output().map_err(|e| e.to_string())?;
        if !output.status.success() {
            let _ = std::fs::remove_file(&temp);
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("decode to wav failed: {}", stderr.lines().last().unwrap_or("unknown error")));
        }

        // Return path; caller uses convertFileSrc() and is responsible for cleanup
        Ok(temp_path)
    }).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn delete_temp_wav(path: String) -> Result<(), String> {
    let p = std::path::Path::new(&path);
    let temp_dir = std::env::temp_dir();
    let name = p.file_name().unwrap_or_default().to_string_lossy();
    if !p.starts_with(&temp_dir) || !name.starts_with("oto_preview_") || !name.ends_with(".wav") {
        return Err("invalid path".to_string());
    }
    tokio::fs::remove_file(&path).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_silence_regions(path: String, db: f64, duration_ms: u32) -> Result<Vec<(f64, f64)>, String> {
    tokio::task::spawn_blocking(move || {
        let dur_secs = duration_ms as f64 / 1000.0;
        let all_regions = converter::run_silence_detect(
            std::path::Path::new(&path), db, dur_secs,
        );

        // Only return the first (beginning) and last (end) silence regions
        if all_regions.is_empty() {
            return Ok(Vec::new());
        }

        let mut result = Vec::new();
        result.push(all_regions[0]);
        if all_regions.len() > 1 {
            let last = all_regions[all_regions.len() - 1];
            if last != all_regions[0] {
                result.push(last);
            }
        }

        Ok(result)
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
        is_converting: AtomicBool::new(false),
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
            is_silence_preview_visible,
            get_waveform_data,
            decode_to_wav,
                delete_temp_wav,
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
                    let _ = ensure_window(&app, "about", dev_url("about/about.html"), "oTo - About", 400.0, 460.0, false).await;
                });
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

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
        assert!(levels[0].peaks.iter().all(|&(mn, mx)| mn == 0.0 && mx == 0.0));
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
        assert!(mx <= 1.0,  "max {mx} > 1.0");
    }
}
