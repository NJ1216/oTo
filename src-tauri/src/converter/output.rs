use std::path::{Path, PathBuf};
use anyhow::{anyhow, Result};
use tauri::AppHandle;
use tokio::sync::Semaphore;
use crate::settings::{NameConflict, OutputDest, Settings};

pub async fn ask_overwrite_dialog(app: &AppHandle, filename: &str, lang: &str) -> bool {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
    let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
    let (msg, title, btn_ok, btn_cancel) = if lang.starts_with("en") {
        (
            format!("\"{}\" already exists. Overwrite?", filename),
            "File Conflict",
            "Overwrite",
            "Save As",
        )
    } else {
        (
            format!("\"{}\" はすでに存在します。上書きしますか？", filename),
            "ファイルの競合",
            "上書き",
            "別名保存",
        )
    };
    app.dialog()
        .message(msg)
        .title(title)
        .buttons(MessageDialogButtons::OkCancelCustom(btn_ok.into(), btn_cancel.into()))
        .show(move |result| { let _ = tx.send(result); });
    rx.await.unwrap_or(false)
}

pub async fn resolve_output_path(
    input: &Path,
    format: &str,
    settings: &Settings,
    base_dir: Option<&Path>,
    app: &AppHandle,
    dialog_sem: &Semaphore,
) -> Result<PathBuf> {
    let stem = input
        .file_stem()
        .ok_or_else(|| anyhow!("invalid filename"))?
        .to_string_lossy();

    let mut output_dir = match &settings.output_dest {
        OutputDest::SourceFolder => input
            .parent()
            .ok_or_else(|| anyhow!("no parent dir"))?
            .to_path_buf(),
        OutputDest::Desktop => dirs::desktop_dir().ok_or_else(|| anyhow!("no desktop dir"))?,
        OutputDest::Downloads => {
            dirs::download_dir().ok_or_else(|| anyhow!("no downloads dir"))?
        }
        OutputDest::Custom => {
            let p = settings
                .custom_output_path
                .as_deref()
                .ok_or_else(|| anyhow!("custom path not set"))?;
            PathBuf::from(p)
        }
    };

    if settings.preserve_folder_structure && settings.output_dest != OutputDest::SourceFolder {
        if let (Some(base), Some(parent)) = (base_dir, input.parent()) {
            if let Ok(rel) = parent.strip_prefix(base) {
                output_dir = output_dir.join(rel);
            }
        }
    }

    std::fs::create_dir_all(&output_dir)?;

    // ALAC・AAC は M4A コンテナを使う
    let ext = match format {
        "alac" | "aac" => "m4a",
        other => other,
    };
    let filename = format!("{}.{}", stem, ext);
    let candidate = output_dir.join(&filename);

    if !candidate.exists() || settings.name_conflict == NameConflict::ForceOverwrite {
        return Ok(candidate);
    }

    match settings.name_conflict {
        NameConflict::AutoRename => {
            let mut i = 1u32;
            loop {
                let name = format!("{}_{}.{}", stem, i, ext);
                let path = output_dir.join(&name);
                if !path.exists() { return Ok(path); }
                i += 1;
            }
        }
        NameConflict::ConfirmDialog => {
            let _permit = dialog_sem.acquire().await;
            let display = candidate.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| filename.clone());
            if ask_overwrite_dialog(app, &display, &settings.language).await {
                return Ok(candidate); // 上書き
            }
            // 別名保存
            let mut i = 1u32;
            loop {
                let name = format!("{}_{}.{}", stem, i, ext);
                let path = output_dir.join(&name);
                if !path.exists() { return Ok(path); }
                i += 1;
            }
        }
        NameConflict::ForceOverwrite => Ok(candidate),
    }
}
