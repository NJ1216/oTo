use std::path::{Path, PathBuf};
use walkdir::WalkDir;
use super::types::FileInfo;

pub fn collect_audio_files(paths: &[String]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for path_str in paths {
        let path = PathBuf::from(path_str);
        if path.is_dir() {
            for entry in WalkDir::new(&path)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
            {
                files.push(entry.path().to_path_buf());
            }
        } else if path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    files
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
    let stem = path.file_stem().unwrap_or_default().to_string_lossy().to_lowercase();
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
            format_name: String::new(),
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
            (PathBuf::from("/music/track.flac"), make_info(true, 1_000_000)),
        ];
        let (best, rejected) = select_best_from_group(group);
        assert_eq!(best.0.extension().unwrap(), "flac");
        assert_eq!(rejected.len(), 1);
    }

    #[test]
    fn wav_beats_flac_among_lossless() {
        let group = vec![
            (PathBuf::from("/music/track.flac"), make_info(true, 900_000)),
            (PathBuf::from("/music/track.wav"), make_info(true, 1_400_000)),
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
        let paths = vec![
            PathBuf::from("/a/b/x.mp3"),
            PathBuf::from("/a/b/y.flac"),
        ];
        assert_eq!(common_ancestor(&paths), Some(PathBuf::from("/a/b")));
    }

    #[test]
    fn common_ancestor_nested_dirs() {
        let paths = vec![
            PathBuf::from("/a/b/c/x.mp3"),
            PathBuf::from("/a/b/y.flac"),
        ];
        assert_eq!(common_ancestor(&paths), Some(PathBuf::from("/a/b")));
    }

    #[test]
    fn common_ancestor_empty_is_none() {
        assert_eq!(common_ancestor(&[]), None);
    }
}
