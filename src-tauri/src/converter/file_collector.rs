use super::types::FileInfo;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;
#[cfg(test)]
use walkdir::WalkDir;

#[cfg(test)]
pub struct CollectedFiles {
    pub files: Vec<PathBuf>,
    pub errors: Vec<(PathBuf, String)>,
}

/// 同一フォルダ内の候補をまとめて渡す。これにより同ステムの最良ファイル選択を保ったまま、
/// 再帰走査の完了を待たずにフォルダ単位で変換を始められる。
pub struct ScanBatch {
    pub files: Vec<PathBuf>,
    pub errors: Vec<(PathBuf, String)>,
}

fn send_batch(
    tx: &mpsc::Sender<ScanBatch>,
    cancellation: &crate::JobCancellation,
    files: Vec<PathBuf>,
    errors: Vec<(PathBuf, String)>,
) -> bool {
    if cancellation.is_cancelled() {
        return false;
    }
    if files.is_empty() && errors.is_empty() {
        return true;
    }
    tx.blocking_send(ScanBatch { files, errors }).is_ok()
}

fn scan_path_in_batches(
    path: PathBuf,
    tx: &mpsc::Sender<ScanBatch>,
    cancellation: &crate::JobCancellation,
) -> bool {
    if cancellation.is_cancelled() {
        return false;
    }
    if path.is_file() {
        return send_batch(tx, cancellation, vec![path], vec![]);
    }
    if !path.is_dir() {
        return send_batch(
            tx,
            cancellation,
            vec![],
            vec![(path, "path does not exist or is not accessible".to_string())],
        );
    }

    let entries = match std::fs::read_dir(&path) {
        Ok(entries) => entries,
        Err(e) => return send_batch(tx, cancellation, vec![], vec![(path, e.to_string())]),
    };
    let mut files = Vec::new();
    let mut directories = Vec::new();
    let mut errors = Vec::new();
    for entry in entries {
        if cancellation.is_cancelled() {
            return false;
        }
        match entry {
            Ok(entry) => match entry.file_type() {
                Ok(kind) if kind.is_file() => files.push(entry.path()),
                Ok(kind) if kind.is_dir() => directories.push(entry.path()),
                Ok(_) => {} // symlinkなどはWalkDirの従来動作と同様に辿らない
                Err(e) => errors.push((entry.path(), e.to_string())),
            },
            Err(e) => errors.push((path.clone(), e.to_string())),
        }
    }
    files.sort();
    directories.sort();
    if !send_batch(tx, cancellation, files, errors) {
        return false;
    }
    for directory in directories {
        if !scan_path_in_batches(directory, tx, cancellation) {
            return false;
        }
    }
    true
}

pub fn scan_paths_in_batches(
    paths: Vec<String>,
    tx: mpsc::Sender<ScanBatch>,
    cancellation: Arc<crate::JobCancellation>,
) {
    // 個別に選ばれた同一フォルダのファイルも、従来どおり同ステム比較の対象にする。
    let mut direct_files: std::collections::BTreeMap<PathBuf, Vec<PathBuf>> =
        std::collections::BTreeMap::new();
    let mut roots = Vec::new();
    for path in paths {
        if cancellation.is_cancelled() {
            return;
        }
        let path = PathBuf::from(path);
        if path.is_file() {
            direct_files
                .entry(path.parent().unwrap_or(Path::new(".")).to_path_buf())
                .or_default()
                .push(path);
        } else {
            roots.push(path);
        }
    }
    for (_, mut files) in direct_files {
        if cancellation.is_cancelled() {
            return;
        }
        files.sort();
        if !send_batch(&tx, &cancellation, files, vec![]) {
            return;
        }
    }
    for path in roots {
        if !scan_path_in_batches(path, &tx, &cancellation) {
            break;
        }
    }
}

#[cfg(test)]
pub fn collect_audio_files(paths: &[String]) -> CollectedFiles {
    let mut files = Vec::new();
    let mut errors = Vec::new();
    for path_str in paths {
        let path = PathBuf::from(path_str);
        if path.is_dir() {
            for entry in WalkDir::new(&path).into_iter() {
                match entry {
                    Ok(entry) if entry.file_type().is_file() => {
                        files.push(entry.path().to_path_buf())
                    }
                    Ok(_) => {}
                    Err(e) => errors.push((e.path().unwrap_or(&path).to_path_buf(), e.to_string())),
                }
            }
        } else if path.is_file() {
            files.push(path);
        } else {
            errors.push((path, "path does not exist or is not accessible".to_string()));
        }
    }
    files.sort();
    CollectedFiles { files, errors }
}

pub fn common_ancestor(paths: &[PathBuf]) -> Option<PathBuf> {
    let dirs: Vec<&Path> = paths.iter().filter_map(|p| p.parent()).collect();
    if dirs.is_empty() {
        return None;
    }
    let mut ancestor = dirs[0].to_path_buf();
    for dir in &dirs[1..] {
        while !dir.starts_with(&ancestor) {
            ancestor = ancestor.parent()?.to_path_buf();
        }
    }
    Some(ancestor)
}

/// (parent, lowercase_stem) キーを計算する（重複検出・ストリーミング処理で使用）
pub fn stem_key(path: &Path) -> (PathBuf, String) {
    let parent = path.parent().unwrap_or(Path::new("")).to_path_buf();
    let stem = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    (parent, stem)
}

fn lossless_score(path: &Path, info: &FileInfo) -> Option<u8> {
    if !info.is_lossless {
        return None;
    }
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    Some(match ext.as_str() {
        "wav" | "aiff" => 0,
        "flac" => 1,
        _ => 2, // alac (.m4a / .alac) など
    })
}

/// グループ内（同ディレクトリ・同ステム）から最良ファイルを1つ選ぶ
/// 優先度: wav/aiff(PCM) > flac > その他のロスレス > 非可逆の最高ビットレート
pub fn select_best_from_group(
    group: Vec<(PathBuf, FileInfo)>,
) -> ((PathBuf, FileInfo), Vec<PathBuf>) {
    if group.len() == 1 {
        return (group.into_iter().next().unwrap(), vec![]);
    }
    let best_idx = group
        .iter()
        .enumerate()
        .filter_map(|(i, (path, info))| lossless_score(path, info).map(|s| (s, i)))
        .min_by_key(|(s, _)| *s)
        .map(|(_, i)| i)
        .unwrap_or_else(|| {
            group
                .iter()
                .enumerate()
                .max_by_key(|(_, (_, info))| info.bit_rate_bps)
                .map(|(i, _)| i)
                .unwrap_or(0)
        });
    let mut best = None;
    let mut rejected = Vec::new();
    for (i, item) in group.into_iter().enumerate() {
        if i == best_idx {
            best = Some(item);
        } else {
            rejected.push(item.0);
        }
    }
    (best.unwrap(), rejected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_info(is_lossless: bool, bit_rate_bps: u64) -> super::super::types::FileInfo {
        super::super::types::FileInfo {
            duration_secs: 100.0,
            tags: HashMap::new(),
            bits_per_sample: 16,
            cover_art_stream_idx: None,
            has_media: true,
            is_lossless,
            bit_rate_bps,
            audio_tracks: Vec::new(),
        }
    }

    #[test]
    fn single_file_is_always_selected() {
        let group = vec![(PathBuf::from("/music/track.mp3"), make_info(false, 192_000))];
        let (best, rejected) = select_best_from_group(group);
        assert_eq!(best.0.extension().unwrap(), "mp3");
        assert!(rejected.is_empty());
    }

    #[test]
    fn lossless_beats_lossy_same_stem() {
        let group = vec![
            (PathBuf::from("/music/track.mp3"), make_info(false, 320_000)),
            (
                PathBuf::from("/music/track.flac"),
                make_info(true, 1_000_000),
            ),
        ];
        let (best, rejected) = select_best_from_group(group);
        assert_eq!(best.0.extension().unwrap(), "flac");
        assert_eq!(rejected.len(), 1);
    }

    #[test]
    fn wav_beats_flac_among_lossless() {
        let group = vec![
            (PathBuf::from("/music/track.flac"), make_info(true, 900_000)),
            (
                PathBuf::from("/music/track.wav"),
                make_info(true, 1_400_000),
            ),
        ];
        let (best, rejected) = select_best_from_group(group);
        assert_eq!(best.0.extension().unwrap(), "wav");
        assert_eq!(rejected.len(), 1);
    }

    #[test]
    fn highest_bitrate_wins_among_all_lossy() {
        let group = vec![
            (PathBuf::from("/music/track.mp3"), make_info(false, 128_000)),
            (PathBuf::from("/music/track.aac"), make_info(false, 256_000)),
        ];
        let (best, rejected) = select_best_from_group(group);
        assert_eq!(best.0.extension().unwrap(), "aac");
        assert_eq!(rejected.len(), 1);
    }

    /// 異なるディレクトリの同名ファイルは stem_key が異なるグループになる
    #[test]
    fn different_directories_have_different_stem_keys() {
        let key_a = stem_key(Path::new("/a/track.mp3"));
        let key_b = stem_key(Path::new("/b/track.flac"));
        assert_ne!(key_a, key_b);
    }

    /// 異なるステム名のファイルは stem_key が異なるグループになる
    #[test]
    fn different_stems_have_different_stem_keys() {
        let key1 = stem_key(Path::new("/music/song1.mp3"));
        let key2 = stem_key(Path::new("/music/song2.mp3"));
        assert_ne!(key1, key2);
    }

    /// stem_key は拡張子を無視して同ステムを同一グループとみなす
    #[test]
    fn same_stem_different_ext_same_key() {
        let key_mp3 = stem_key(Path::new("/music/track.mp3"));
        let key_flac = stem_key(Path::new("/music/track.flac"));
        assert_eq!(key_mp3, key_flac);
    }

    /// stem_key は大文字小文字を区別しない
    #[test]
    fn stem_key_is_case_insensitive() {
        let key_lower = stem_key(Path::new("/music/Track.mp3"));
        let key_upper = stem_key(Path::new("/music/TRACK.flac"));
        assert_eq!(key_lower, key_upper);
    }

    #[test]
    fn common_ancestor_single_file() {
        let paths = vec![PathBuf::from("/a/b/c.mp3")];
        assert_eq!(common_ancestor(&paths), Some(PathBuf::from("/a/b")));
    }

    #[test]
    fn common_ancestor_sibling_files() {
        let paths = vec![PathBuf::from("/a/b/x.mp3"), PathBuf::from("/a/b/y.flac")];
        assert_eq!(common_ancestor(&paths), Some(PathBuf::from("/a/b")));
    }

    #[test]
    fn common_ancestor_nested_dirs() {
        let paths = vec![PathBuf::from("/a/b/c/x.mp3"), PathBuf::from("/a/b/y.flac")];
        assert_eq!(common_ancestor(&paths), Some(PathBuf::from("/a/b")));
    }

    #[test]
    fn common_ancestor_empty_is_none() {
        assert_eq!(common_ancestor(&[]), None);
    }

    #[test]
    fn missing_input_path_is_reported_as_error() {
        let path = format!("/tmp/oto-missing-{}", uuid::Uuid::new_v4());
        let collected = collect_audio_files(&[path]);
        assert!(collected.files.is_empty());
        assert_eq!(collected.errors.len(), 1);
    }

    #[test]
    fn scanner_emits_parent_files_before_descending_into_child_directory() {
        let root = std::env::temp_dir().join(format!("oto-scan-test-{}", uuid::Uuid::new_v4()));
        let child = root.join("child");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(root.join("first.mp3"), b"").unwrap();
        std::fs::write(child.join("second.mp3"), b"").unwrap();
        let (tx, mut rx) = mpsc::channel(4);
        let scan_root = root.clone();
        let cancellation = Arc::new(crate::JobCancellation::new());
        std::thread::spawn(move || {
            scan_paths_in_batches(
                vec![scan_root.to_string_lossy().into_owned()],
                tx,
                cancellation,
            )
        });
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let first = runtime.block_on(rx.recv()).unwrap();
        let second = runtime.block_on(rx.recv()).unwrap();
        assert_eq!(first.files, vec![root.join("first.mp3")]);
        assert_eq!(second.files, vec![child.join("second.mp3")]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scanner_stops_before_starting_when_job_is_cancelled() {
        let (tx, mut rx) = mpsc::channel(1);
        let cancellation = Arc::new(crate::JobCancellation::new());
        cancellation.cancel();
        scan_paths_in_batches(vec!["/".to_string()], tx, cancellation);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        assert!(runtime.block_on(rx.recv()).is_none());
    }
}
