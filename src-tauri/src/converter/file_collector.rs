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

// 同ディレクトリ内に拡張子違いの同名ファイルが存在する場合、最良ソースを1つ選ぶ。
// 優先度: wav/aiff(PCM) > flac > その他のロスレス(ALAC 等) > 非可逆ファイルの最高ビットレート
// 「.m4a」拡張子は AAC・ALAC 両方で使われるため、ロスレス判定は probe で得た
// codec_name から導出される `info.is_lossless` を使用し、拡張子だけでは判定しない。
pub fn select_best_sources(
    files: Vec<(PathBuf, FileInfo)>,
) -> (Vec<(PathBuf, FileInfo)>, Vec<PathBuf>) {
    use std::collections::HashMap as Map;

    let mut groups: Map<(PathBuf, String), Vec<(PathBuf, FileInfo)>> = Map::new();
    for (path, info) in files {
        let parent = path.parent().unwrap_or(std::path::Path::new("")).to_path_buf();
        let stem = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase();
        groups.entry((parent, stem)).or_default().push((path, info));
    }

    let lossless_score = |path: &Path, info: &FileInfo| -> Option<u8> {
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
    };

    let mut selected = Vec::new();
    let mut rejected = Vec::new();

    for (_, group) in groups {
        if group.len() == 1 {
            selected.push(group.into_iter().next().unwrap());
            continue;
        }

        let best_idx = group
            .iter()
            .enumerate()
            .filter_map(|(i, (path, info))| lossless_score(path, info).map(|s| (s, i)))
            .min_by_key(|(s, _)| *s)
            .map(|(_, i)| i)
            .unwrap_or_else(|| {
                // ロスレスなし → 最高ビットレートの非可逆ファイルを選ぶ
                group
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, (_, info))| info.bit_rate_bps)
                    .map(|(i, _)| i)
                    .unwrap_or(0)
            });

        for (i, (path, info)) in group.into_iter().enumerate() {
            if i == best_idx {
                selected.push((path, info));
            } else {
                rejected.push(path);
            }
        }
    }

    (selected, rejected)
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
        }
    }

    #[test]
    fn single_file_is_always_selected() {
        let files = vec![(PathBuf::from("/music/track.mp3"), make_info(false, 192_000))];
        let (selected, rejected) = select_best_sources(files);
        assert_eq!(selected.len(), 1);
        assert!(rejected.is_empty());
    }

    #[test]
    fn lossless_beats_lossy_same_stem() {
        let files = vec![
            (PathBuf::from("/music/track.mp3"), make_info(false, 320_000)),
            (PathBuf::from("/music/track.flac"), make_info(true, 1_000_000)),
        ];
        let (selected, rejected) = select_best_sources(files);
        assert_eq!(selected.len(), 1);
        assert_eq!(rejected.len(), 1);
        let ext = selected[0].0.extension().unwrap().to_str().unwrap();
        assert_eq!(ext, "flac");
    }

    #[test]
    fn wav_beats_flac_among_lossless() {
        let files = vec![
            (PathBuf::from("/music/track.flac"), make_info(true, 900_000)),
            (PathBuf::from("/music/track.wav"), make_info(true, 1_400_000)),
        ];
        let (selected, rejected) = select_best_sources(files);
        assert_eq!(selected.len(), 1);
        assert_eq!(rejected.len(), 1);
        let ext = selected[0].0.extension().unwrap().to_str().unwrap();
        assert_eq!(ext, "wav");
    }

    #[test]
    fn highest_bitrate_wins_among_all_lossy() {
        let files = vec![
            (PathBuf::from("/music/track.mp3"), make_info(false, 128_000)),
            (PathBuf::from("/music/track.aac"), make_info(false, 256_000)),
        ];
        let (selected, rejected) = select_best_sources(files);
        assert_eq!(selected.len(), 1);
        assert_eq!(rejected.len(), 1);
        let ext = selected[0].0.extension().unwrap().to_str().unwrap();
        assert_eq!(ext, "aac");
    }

    #[test]
    fn different_directories_are_not_grouped() {
        let files = vec![
            (PathBuf::from("/a/track.mp3"), make_info(false, 320_000)),
            (PathBuf::from("/b/track.flac"), make_info(true, 1_000_000)),
        ];
        let (selected, rejected) = select_best_sources(files);
        assert_eq!(selected.len(), 2);
        assert!(rejected.is_empty());
    }

    #[test]
    fn different_stems_not_grouped() {
        let files = vec![
            (PathBuf::from("/music/song1.mp3"), make_info(false, 192_000)),
            (PathBuf::from("/music/song2.mp3"), make_info(false, 192_000)),
        ];
        let (selected, rejected) = select_best_sources(files);
        assert_eq!(selected.len(), 2);
        assert!(rejected.is_empty());
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
