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
    state: State<'_, Arc<AppState>>,
    request: ConvertRequest,
) -> Result<String, String> {
    let current_settings = settings::load_settings(&app).map_err(|e| e.to_string())?;
    let job_id = uuid::Uuid::new_v4().to_string();
    let pgids: Arc<Mutex<Vec<i32>>> = Arc::new(Mutex::new(vec![]));

    let job_id_clone = job_id.clone();
    let app_clone = app.clone();
    let state_arc = state.inner().clone();
    let pgids_for_conv = pgids.clone();

    let handle = tokio::spawn(async move {
        run_conversion(app_clone, job_id_clone.clone(), request, current_settings, pgids_for_conv).await;
        state_arc.jobs.lock().await.remove(&job_id_clone);
    });

    state.jobs.lock().await.insert(job_id.clone(), JobInfo { handle, pgids });
    Ok(job_id)
}

#[tauri::command]
async fn cancel_job(
    state: State<'_, Arc<AppState>>,
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
    }
    Ok(())
}

#[tauri::command]
async fn pause_job(
    state: State<'_, Arc<AppState>>,
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
    }
    Ok(())
}

#[tauri::command]
async fn resume_job(
    state: State<'_, Arc<AppState>>,
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
    }
    Ok(())
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
    format!("2026.5.5 (build {})", env!("GIT_HASH"))
}

// --- App entry ---

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let state = Arc::new(AppState {
        jobs: Mutex::new(HashMap::new()),
    });

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
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
