use std::collections::HashMap;
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
}

pub struct AppState {
    pub jobs: Mutex<HashMap<String, JobInfo>>,
}

// --- Commands ---

#[tauri::command]
async fn convert_files(
    app: AppHandle,
    state: State<'_, AppState>,
    request: ConvertRequest,
) -> Result<String, String> {
    let current_settings = settings::load_settings(&app).map_err(|e| e.to_string())?;
    let job_id = uuid::Uuid::new_v4().to_string();
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

    state.jobs.lock().await.insert(job_id.clone(), JobInfo { handle, pgids });
    Ok(job_id)
}

#[tauri::command]
async fn cancel_job(
    state: State<'_, AppState>,
    job_id: String,
) -> Result<(), String> {
    let jobs = state.jobs.lock().await;
    if let Some(job) = jobs.get(&job_id) {
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
