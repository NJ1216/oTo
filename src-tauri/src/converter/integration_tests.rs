use super::*;
use std::sync::Mutex;

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("oto-ffmpeg-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    #[cfg(windows)]
    fn progress_file(&self) -> crate::spool::LocalSpoolFile {
        crate::spool::SpoolManager::initialize(&self.0.join("progress-state"))
            .unwrap()
            .new_temporary_file("progress", "txt")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn ffmpeg_output(args: &[&std::ffi::OsStr]) -> Vec<u8> {
    let mut command = tokio::process::Command::new(ffmpeg_path());
    command.args(args).stdin(Stdio::null());
    configure_ffmpeg_command(&mut command);
    let output = command.output().await.expect("FFmpeg must be installed");
    assert!(
        output.status.success(),
        "FFmpeg fixture/check failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

async fn pcm_hash(path: &Path) -> Vec<u8> {
    ffmpeg_output(&[
        "-v".as_ref(),
        "error".as_ref(),
        "-i".as_ref(),
        path.as_os_str(),
        "-map".as_ref(),
        "0:a:0".as_ref(),
        "-c:a".as_ref(),
        "pcm_s24le".as_ref(),
        "-f".as_ref(),
        "hash".as_ref(),
        "-hash".as_ref(),
        "sha256".as_ref(),
        "-".as_ref(),
    ])
    .await
}

// Explicit opt-in: normal unit-test environments need not install FFmpeg.
// cargo test --offline --locked real_ffmpeg -- --ignored --nocapture
#[tokio::test]
#[ignore = "requires installed FFmpeg and FFprobe"]
async fn real_ffmpeg_wav_roundtrip_preserves_pcm_metadata_and_progress() {
    let directory = TestDirectory::new();
    let original = directory.0.join("音声 sample.wav");
    ffmpeg_output(&[
        "-v".as_ref(),
        "error".as_ref(),
        "-f".as_ref(),
        "lavfi".as_ref(),
        "-i".as_ref(),
        "sine=frequency=440:sample_rate=48000:duration=30".as_ref(),
        "-c:a".as_ref(),
        "pcm_s24le".as_ref(),
        "-metadata".as_ref(),
        "title=oTo regression sample".as_ref(),
        original.as_os_str(),
    ])
    .await;
    let original_hash = pcm_hash(&original).await;
    let mut input = original.clone();
    for format in ["flac", "wav"] {
        let output = directory.0.join(format!("converted.{format}"));
        let cancellation = Arc::new(crate::JobCancellation::new());
        let info = probe_file(&input, &cancellation).await.unwrap();
        assert!(info.has_media);
        assert_eq!(info.bits_per_sample, 24);
        let callbacks = Arc::new(Mutex::new(Vec::new()));
        let active_pids = Arc::new(Mutex::new(Vec::new()));
        let started_pids = active_pids.clone();
        let exited_pids = active_pids.clone();
        let settings = Settings::default();
        convert_one(
            ConvertOneRequest {
                input: &input,
                output: &output,
                format,
                settings: &settings,
                info: &info,
                audio_stream_index: None,
                duration_secs: info.duration_secs,
                network_input: None,
            },
            ConversionContext {
                #[cfg(windows)]
                progress_file: directory.progress_file(),
                threads_per_job: 1,
                pause: Arc::new(crate::SessionPause::new()),
                cancellation: cancellation.clone(),
                on_progress: {
                    let callbacks = callbacks.clone();
                    Arc::new(move |ratio| callbacks.lock().unwrap().push(ratio))
                },
                processes: ProcessTracker::new(
                    Arc::new(move |pid| started_pids.lock().unwrap().push(pid)),
                    Arc::new(move |pid| {
                        exited_pids.lock().unwrap().retain(|active| *active != pid)
                    }),
                ),
            },
        )
        .await
        .unwrap();
        let progress = callbacks.lock().unwrap().clone();
        assert_eq!(progress.first(), Some(&0.0));
        assert_eq!(progress.last(), Some(&1.0));
        assert!(progress
            .iter()
            .all(|ratio| ratio.is_finite() && (0.0..=1.0).contains(ratio)));
        assert!(progress.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(active_pids.lock().unwrap().is_empty());
        let converted = probe_file(&output, &cancellation).await.unwrap();
        assert_eq!(converted.bits_per_sample, 24);
        assert!((converted.duration_secs - info.duration_secs).abs() < 0.001);
        assert_eq!(
            converted.tags.get("title").map(String::as_str),
            Some("oTo regression sample")
        );
        assert_eq!(
            pcm_hash(&output).await,
            original_hash,
            "PCM changed in {format}"
        );
        assert!(original.exists(), "conversion must keep the source");
        eprintln!(
            "{format}: {} progress callbacks, first={:?}, last={:?}",
            progress.len(),
            progress.first(),
            progress.last()
        );
        input = output;
    }
}

#[tokio::test]
#[ignore = "requires installed FFmpeg and FFprobe"]
async fn real_ffmpeg_aac_and_alac_spools_use_the_m4a_container() {
    let directory = TestDirectory::new();
    let input = directory.0.join("source.wav");
    ffmpeg_output(&[
        "-v".as_ref(),
        "error".as_ref(),
        "-f".as_ref(),
        "lavfi".as_ref(),
        "-i".as_ref(),
        "sine=duration=0.2".as_ref(),
        input.as_os_str(),
    ])
    .await;
    let manager = crate::spool::SpoolManager::initialize(&directory.0.join("state")).unwrap();
    for format in ["aac", "alac"] {
        let usage = Arc::new(AtomicUsize::new(0));
        let spool = manager.new_spool_file("output", output::output_extension(format), usage);
        let cancellation = Arc::new(crate::JobCancellation::new());
        let info = probe_file(&input, &cancellation).await.unwrap();
        let settings = Settings::default();
        let callbacks = Arc::new(Mutex::new(Vec::new()));
        convert_one(
            ConvertOneRequest {
                input: &input,
                output: spool.path(),
                format,
                settings: &settings,
                info: &info,
                audio_stream_index: None,
                duration_secs: info.duration_secs,
                network_input: None,
            },
            ConversionContext {
                #[cfg(windows)]
                progress_file: directory.progress_file(),
                threads_per_job: 1,
                pause: Arc::new(crate::SessionPause::new()),
                cancellation,
                on_progress: {
                    let callbacks = callbacks.clone();
                    Arc::new(move |ratio| callbacks.lock().unwrap().push(ratio))
                },
                processes: ProcessTracker::new(Arc::new(|_| {}), Arc::new(|_| {})),
            },
        )
        .await
        .unwrap();
        let mut probe = tokio::process::Command::new(binary::ffprobe_path());
        probe
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=format_name",
                "-of",
                "json",
            ])
            .arg(spool.path());
        configure_ffmpeg_command(&mut probe);
        let result = probe.output().await.unwrap();
        assert!(result.status.success());
        let json: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert!(json["format"]["format_name"]
            .as_str()
            .unwrap()
            .split(',')
            .any(|name| name == "m4a"));
        assert_eq!(callbacks.lock().unwrap().last(), Some(&1.0));
    }
}

#[tokio::test]
#[ignore = "requires installed FFmpeg"]
async fn real_ffmpeg_file_progress_reports_intermediate_updates_and_completion() {
    let directory = TestDirectory::new();
    let manager = crate::spool::SpoolManager::initialize(&directory.0.join("state")).unwrap();
    let file = manager.new_temporary_file("progress", "txt");
    let mut command = tokio::process::Command::new(ffmpeg_path());
    command
        .args([
            "-v",
            "error",
            "-re",
            "-f",
            "lavfi",
            "-i",
            "sine=duration=2",
            "-progress",
        ])
        .arg(file.path())
        .args(["-nostats", "-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_ffmpeg_command(&mut command);
    let mut child = command.spawn().unwrap();
    let mut reader = progress_file::ProgressFileReader::default();
    let mut samples = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            let exited = child.try_wait().unwrap();
            if let Ok(progress) = reader.poll(file.path()).await {
                samples.push(progress);
            }
            if let Some(status) = exited {
                assert!(status.success());
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("FFmpeg file progress test timed out");
    assert!(samples
        .iter()
        .any(|p| p.out_time_us > 0 && p.out_time_us < 2_000_000));
    assert!(samples
        .windows(2)
        .all(|p| p[0].out_time_us <= p[1].out_time_us));
    assert!(samples.last().unwrap().ended);
    assert!(samples.last().unwrap().out_time_us >= 2_000_000);
    eprintln!(
        "file progress: {} polls, intermediate timestamps and end received",
        samples.len()
    );
    drop(file);
    manager.cleanup_current_session().unwrap();
}

#[tokio::test]
#[ignore = "requires installed FFmpeg and FFprobe"]
async fn real_ffmpeg_silence_trim_preserves_internal_pause_and_reports_analysis() {
    let directory = TestDirectory::new();
    let input = directory.0.join("silence-tone-pause-tone-silence.wav");
    let output = directory.0.join("trimmed.wav");
    ffmpeg_output(&[
        "-v".as_ref(),
        "error".as_ref(),
        "-f".as_ref(),
        "lavfi".as_ref(),
        "-i".as_ref(),
        "aevalsrc=if(between(t\\,1\\,3)+between(t\\,4\\,6)\\,0.25*sin(2*PI*440*t)\\,0):s=48000:d=7"
            .as_ref(),
        "-c:a".as_ref(),
        "pcm_s16le".as_ref(),
        input.as_os_str(),
    ])
    .await;
    let cancellation = Arc::new(crate::JobCancellation::new());
    let info = probe_file(&input, &cancellation).await.unwrap();
    let settings = Settings {
        silence_trim_enabled: true,
        silence_trim_db: -60.0,
        silence_trim_duration_ms: 200,
        ..Settings::default()
    };
    let callbacks = Arc::new(Mutex::new(Vec::new()));
    convert_one(
        ConvertOneRequest {
            input: &input,
            output: &output,
            format: "wav",
            settings: &settings,
            info: &info,
            audio_stream_index: None,
            duration_secs: info.duration_secs,
            network_input: None,
        },
        ConversionContext {
            #[cfg(windows)]
            progress_file: directory.progress_file(),
            threads_per_job: 1,
            pause: Arc::new(crate::SessionPause::new()),
            cancellation: cancellation.clone(),
            on_progress: {
                let callbacks = callbacks.clone();
                Arc::new(move |ratio| callbacks.lock().unwrap().push(ratio))
            },
            processes: ProcessTracker::new(Arc::new(|_| {}), Arc::new(|_| {})),
        },
    )
    .await
    .unwrap();
    let progress = callbacks.lock().unwrap().clone();
    assert_eq!(progress.first(), Some(&0.0));
    assert_eq!(progress.last(), Some(&1.0));
    assert!(progress.iter().any(|ratio| *ratio > 0.0 && *ratio <= 0.2));
    assert!(progress.windows(2).all(|pair| pair[0] <= pair[1]));
    let converted = probe_file(&output, &cancellation).await.unwrap();
    assert!(
        (5.05..5.15).contains(&converted.duration_secs),
        "duration: {}",
        converted.duration_secs
    );
    let pcm = ffmpeg_output(&[
        "-v".as_ref(),
        "error".as_ref(),
        "-i".as_ref(),
        output.as_os_str(),
        "-f".as_ref(),
        "s16le".as_ref(),
        "-acodec".as_ref(),
        "pcm_s16le".as_ref(),
        "-".as_ref(),
    ])
    .await;
    let mut runs = Vec::new();
    let mut silent_samples = 0;
    for sample in pcm
        .as_chunks::<2>()
        .0
        .iter()
        .map(|s| i16::from_le_bytes(*s))
        .chain(std::iter::once(i16::MAX))
    {
        if sample.unsigned_abs() <= 2 {
            silent_samples += 1;
        } else {
            if silent_samples > 1200 {
                runs.push(silent_samples as f64 / 48000.0);
            }
            silent_samples = 0;
        }
    }
    assert_eq!(runs.len(), 3, "head, internal pause, tail: {runs:?}");
    assert!((runs[0] - 0.05).abs() < 0.03, "head margin: {runs:?}");
    assert!(
        (runs[1] - 1.0).abs() < 0.003,
        "internal pause must stay intact"
    );
    assert!((runs[2] - 0.05).abs() < 0.03, "tail margin: {runs:?}");
    let samples = pcm
        .as_chunks::<2>()
        .0
        .iter()
        .map(|s| i16::from_le_bytes(*s))
        .collect::<Vec<_>>();
    assert!(samples[0].unsigned_abs() <= 2, "fade-in starts at silence");
    // A 0.25-amplitude 440 Hz tone at 48 kHz never steps more than about 480 per sample.
    assert!(samples
        .windows(2)
        .all(|pair| (i32::from(pair[1]) - i32::from(pair[0])).abs() <= 500));
    eprintln!("silence trim: progress={progress:?}, silent runs={runs:?}");

    let direct_progress = Arc::new(Mutex::new(Vec::new()));
    let processes = ProcessTracker::new(Arc::new(|_| {}), Arc::new(|_| {}));
    let decision = silence::analyze_trim(
        &input,
        &SilenceConfig {
            db: -60.0,
            min_duration_secs: 0.2,
            total_duration_secs: 7.0,
            audio_stream_index: None,
        },
        &SilenceContext {
            pause: &crate::SessionPause::new(),
            cancellation: &cancellation,
            processes: &processes,
            on_progress: {
                let progress = direct_progress.clone();
                Arc::new(move |ratio| progress.lock().unwrap().push(ratio))
            },
        },
    )
    .await
    .unwrap();
    let TrimDecision::Trim {
        keep_start,
        keep_end,
        cut_head: true,
        cut_tail: true,
    } = decision
    else {
        panic!("expected a head and tail trim: {decision:?}");
    };
    assert!((keep_start - 0.95).abs() < 0.003, "{keep_start}");
    assert!((keep_end - 6.05).abs() < 0.003, "{keep_end}");
    let progress = direct_progress.lock().unwrap().clone();
    assert_eq!(progress.first(), Some(&0.0));
    assert_eq!(progress.last(), Some(&1.0));
    assert!(progress.windows(2).all(|pair| pair[0] <= pair[1]));

    let cancelled_output = directory.0.join("cancelled.wav");
    let cancelled = Arc::new(crate::JobCancellation::new());
    let result = convert_one(
        ConvertOneRequest {
            input: &input,
            output: &cancelled_output,
            format: "wav",
            settings: &settings,
            info: &info,
            audio_stream_index: None,
            duration_secs: info.duration_secs,
            network_input: None,
        },
        ConversionContext {
            #[cfg(windows)]
            progress_file: directory.progress_file(),
            threads_per_job: 1,
            pause: Arc::new(crate::SessionPause::new()),
            cancellation: cancelled.clone(),
            on_progress: {
                let cancelled = cancelled.clone();
                Arc::new(move |ratio| {
                    if ratio > 0.0 && ratio <= 0.2 {
                        cancelled.cancel();
                    }
                })
            },
            processes,
        },
    )
    .await;
    assert!(result.unwrap_err().to_string().contains(CANCELLED_ERROR));
    assert!(
        !cancelled_output.exists(),
        "analysis cancellation must not leave partial output"
    );
}

#[tokio::test]
#[ignore = "requires installed FFmpeg and FFprobe"]
async fn real_ffmpeg_high_resolution_pcm_fits_output_reservation() {
    let directory = TestDirectory::new();
    let input = directory.0.join("192k-six-channel.wav");
    ffmpeg_output(&[
        "-v".as_ref(),
        "error".as_ref(),
        "-f".as_ref(),
        "lavfi".as_ref(),
        "-i".as_ref(),
        "anullsrc=r=192000:cl=5.1:d=10".as_ref(),
        "-c:a".as_ref(),
        "pcm_s24le".as_ref(),
        input.as_os_str(),
    ])
    .await;
    let cancellation = crate::JobCancellation::new();
    let info = probe_file(&input, &cancellation).await.unwrap();
    let actual = std::fs::metadata(&input).unwrap().len();
    let reserved = estimate_output_bytes("wav", &Settings::default(), &info);
    eprintln!("192kHz/6ch PCM: actual={actual} bytes, reserved={reserved} bytes");
    assert!(
        reserved >= actual,
        "NAS output reservation must cover high-resolution PCM"
    );
    let compressed = directory.0.join("192k-six-channel.flac");
    ffmpeg_output(&[
        "-v".as_ref(),
        "error".as_ref(),
        "-i".as_ref(),
        input.as_os_str(),
        "-c:a".as_ref(),
        "flac".as_ref(),
        compressed.as_os_str(),
    ])
    .await;
    let compressed_info = probe_file(&compressed, &cancellation).await.unwrap();
    assert!(compressed_info.bit_rate_bps < info.bit_rate_bps);
    for format in ["wav", "aiff", "flac", "alac"] {
        assert!(
            estimate_output_bytes(format, &Settings::default(), &compressed_info) >= actual,
            "{format} reservation must account for decoded PCM, not compressed silence bitrate"
        );
    }
}

#[tokio::test]
#[ignore = "requires installed FFmpeg and FFprobe"]
async fn real_ffmpeg_failed_silence_analysis_preserves_existing_output() {
    let directory = TestDirectory::new();
    let input = directory.0.join("invalid.wav");
    let output = directory.0.join("existing.wav");
    std::fs::write(&input, b"invalid audio").unwrap();
    std::fs::write(&output, b"original output").unwrap();
    let pause = Arc::new(crate::SessionPause::new());
    let cancellation = Arc::new(crate::JobCancellation::new());
    let info = FileInfo {
        duration_secs: 1.0,
        tags: HashMap::new(),
        bits_per_sample: 16,
        cover_art_stream_idx: None,
        has_media: true,
        is_lossless: true,
        bit_rate_bps: 768_000,
        max_pcm_bit_rate_bps: 1_536_000,
        audio_tracks: Vec::new(),
    };
    let settings = Settings {
        silence_trim_enabled: true,
        ..Settings::default()
    };
    let progress = Arc::new(Mutex::new(Vec::new()));
    let record = progress.clone();
    let error = convert_one(
        ConvertOneRequest {
            input: &input,
            output: &output,
            format: "wav",
            settings: &settings,
            info: &info,
            audio_stream_index: None,
            duration_secs: 1.0,
            network_input: None,
        },
        ConversionContext {
            #[cfg(windows)]
            progress_file: directory.progress_file(),
            pause: pause.clone(),
            cancellation,
            threads_per_job: 1,
            on_progress: Arc::new(move |ratio| record.lock().unwrap().push(ratio)),
            processes: ProcessTracker::for_session(pause.clone()),
        },
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("FFmpeg failed"),
        "conversion itself fails on invalid input: {error}"
    );
    assert_eq!(std::fs::read(&output).unwrap(), b"original output");
    assert_eq!(std::fs::read(&input).unwrap(), b"invalid audio");
    assert!(pause.processes.lock().unwrap().is_empty());
}

#[tokio::test]
#[ignore = "requires installed FFmpeg and FFprobe"]
async fn real_ffmpeg_preview_processes_produce_readable_waveforms_and_wav() {
    let directory = TestDirectory::new();
    let manager =
        crate::spool::SpoolManager::initialize(&directory.0.join("preview-state")).unwrap();
    let operations = Arc::new(crate::preview_operations::PreviewOperations::default());
    for kind in ["raw", "wav"] {
        let manager = manager.clone();
        let file = operations
            .run(move |cancellation| async move {
                let file = manager.new_temporary_file("preview-check", kind);
                let mut command = tokio::process::Command::new(ffmpeg_path());
                command.args([
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:duration=1:sample_rate=48000",
                ]);
                if kind == "raw" {
                    command.args(["-ar", "4000", "-ac", "1", "-f", "f32le"]);
                } else {
                    command.args(["-ar", "44100", "-ac", "2", "-f", "wav"]);
                }
                command.arg(file.path());
                crate::preview_operations::run_process(command, &cancellation).await?;
                Ok(file)
            })
            .await
            .unwrap();
        let path = file.path().to_path_buf();
        let cancellation = crate::JobCancellation::new();
        if kind == "raw" {
            assert_eq!(std::fs::metadata(&path).unwrap().len(), 4000 * 4);
            let levels =
                crate::compute_waveform_streaming(&path, 4000, &[800, 8000, 80000], &cancellation)
                    .unwrap();
            assert_eq!(
                levels
                    .iter()
                    .map(|level| level.peaks.len())
                    .collect::<Vec<_>>(),
                [800, 8000, 80000]
            );
            assert!(levels[0].rms.iter().any(|level| *level > 0.01));
        } else {
            let info = probe_file(&path, &cancellation).await.unwrap();
            assert!(info.has_media);
            assert_eq!(info.bits_per_sample, 16);
            assert_eq!(info.max_pcm_bit_rate_bps, 44_100 * 2 * 32);
            assert!((info.duration_secs - 1.0).abs() < 0.001);
        }
        drop(file);
        assert!(!path.exists());
    }
    operations.close();
    operations.wait_finished().await;
    manager.cleanup_current_session().unwrap();
}

fn s16_samples(pcm: &[u8]) -> Vec<i16> {
    pcm.as_chunks::<2>()
        .0
        .iter()
        .map(|s| i16::from_le_bytes(*s))
        .collect()
}

async fn decode_s16(path: &Path) -> Vec<i16> {
    s16_samples(
        &ffmpeg_output(&[
            "-v".as_ref(),
            "error".as_ref(),
            "-i".as_ref(),
            path.as_os_str(),
            "-map".as_ref(),
            "0:a:0".as_ref(),
            "-f".as_ref(),
            "s16le".as_ref(),
            "-acodec".as_ref(),
            "pcm_s16le".as_ref(),
            "-".as_ref(),
        ])
        .await,
    )
}

async fn convert_with_trim(
    directory: &TestDirectory,
    input: &Path,
    output: &Path,
    settings: &Settings,
    audio_stream_index: Option<usize>,
) -> Result<Option<(TrimNotice, Option<String>)>> {
    convert_with_trim_network(directory, input, output, settings, audio_stream_index, None).await
}

async fn convert_with_trim_network(
    directory: &TestDirectory,
    input: &Path,
    output: &Path,
    settings: &Settings,
    audio_stream_index: Option<usize>,
    network_input: Option<NetworkInput>,
) -> Result<Option<(TrimNotice, Option<String>)>> {
    let _ = directory;
    let cancellation = Arc::new(crate::JobCancellation::new());
    let info = probe_file(input, &cancellation).await.unwrap();
    convert_one(
        ConvertOneRequest {
            input,
            output,
            format: "wav",
            settings,
            info: &info,
            audio_stream_index,
            duration_secs: info.duration_secs,
            network_input,
        },
        ConversionContext {
            #[cfg(windows)]
            progress_file: directory.progress_file(),
            threads_per_job: 1,
            pause: Arc::new(crate::SessionPause::new()),
            cancellation,
            on_progress: Arc::new(|_| {}),
            processes: ProcessTracker::new(Arc::new(|_| {}), Arc::new(|_| {})),
        },
    )
    .await
}

fn trim_settings(db: f64, duration_ms: u32) -> Settings {
    Settings {
        silence_trim_enabled: true,
        silence_trim_db: db,
        silence_trim_duration_ms: duration_ms,
        ..Settings::default()
    }
}

async fn lavfi_wav(path: &Path, source: &str) {
    ffmpeg_output(&[
        "-v".as_ref(),
        "error".as_ref(),
        "-f".as_ref(),
        "lavfi".as_ref(),
        "-i".as_ref(),
        source.as_ref(),
        "-c:a".as_ref(),
        "pcm_s16le".as_ref(),
        path.as_os_str(),
    ])
    .await;
}

#[tokio::test]
#[ignore = "requires installed FFmpeg and FFprobe"]
async fn real_ffmpeg_trim_at_minus_30db_has_no_click() {
    let directory = TestDirectory::new();
    let input = directory.0.join("noise-floor-tone.wav");
    let output = directory.0.join("trimmed.wav");
    // Uniform noise at -40 dBFS peak with a 2 s tone in the middle.
    lavfi_wav(
        &input,
        "aevalsrc=0.01*(2*random(0)-1)+if(between(t\\,1\\,3)\\,0.25*sin(2*PI*440*t)\\,0):s=48000:d=4",
    )
    .await;
    let notice = convert_with_trim(&directory, &input, &output, &trim_settings(-30.0, 50), None)
        .await
        .unwrap();
    assert_eq!(notice, None);
    let samples = decode_s16(&output).await;
    let seconds = samples.len() as f64 / 48000.0;
    assert!((2.05..2.2).contains(&seconds), "duration: {seconds}");
    let noise_peak = (0.01_f64 * 32768.0) as i32;
    // 1 ms envelopes across the 5 ms fades: the quiet end of each fade stays well
    // below the noise floor's peak and the other end approaches it.
    let envelope = |chunk: &[i16]| chunk.iter().map(|s| i32::from(*s).abs()).max().unwrap();
    let head: Vec<i32> = samples[..240].chunks(48).map(envelope).collect();
    let mut tail: Vec<i32> = samples[samples.len() - 240..]
        .chunks(48)
        .map(envelope)
        .collect();
    tail.reverse();
    for ramp in [&head, &tail] {
        assert!(ramp[0] <= noise_peak / 4, "ramp starts near zero: {ramp:?}");
        assert!(ramp[0] < ramp[4], "ramp grows toward the sound: {ramp:?}");
        // Non-decreasing across the whole ramp, allowing for the random noise's peak jitter.
        let slack = noise_peak / 4;
        assert!(
            ramp.windows(2).all(|pair| pair[1] + slack >= pair[0]),
            "ramp never falls back: {ramp:?}"
        );
    }
    assert!(samples[0].unsigned_abs() <= 2);
    assert!(samples[samples.len() - 1].unsigned_abs() <= 2);
}

#[tokio::test]
#[ignore = "requires installed FFmpeg and FFprobe"]
async fn real_ffmpeg_analyze_preview_trim_matches_conversion_decision() {
    let directory = TestDirectory::new();
    let input = directory.0.join("silence-tone-pause-tone-silence.wav");
    lavfi_wav(
        &input,
        "aevalsrc=if(between(t\\,1\\,3)+between(t\\,4\\,6)\\,0.25*sin(2*PI*440*t)\\,0):s=48000:d=7",
    )
    .await;
    let cancellation = Arc::new(crate::JobCancellation::new());
    let info = probe_file(&input, &cancellation).await.unwrap();
    let pause = Arc::new(crate::SessionPause::new());
    let processes = ProcessTracker::for_session(pause.clone());
    let config = SilenceConfig {
        db: -60.0,
        min_duration_secs: 0.2,
        total_duration_secs: info.duration_secs,
        audio_stream_index: None,
    };
    let context = SilenceContext {
        pause: &pause,
        cancellation: &cancellation,
        processes: &processes,
        on_progress: Arc::new(|_| {}),
    };
    let expected = analyze_trim(&input, &config, &context).await.unwrap();
    let (actual, duration) = analyze_preview_trim(&input, -60.0, 200, pause.clone(), &cancellation)
        .await
        .unwrap();
    assert_eq!(actual, expected);
    assert!(matches!(actual, TrimDecision::Trim { .. }));
    assert_eq!(duration, info.duration_secs);
}

#[tokio::test]
#[ignore = "requires installed FFmpeg and FFprobe"]
async fn real_ffmpeg_all_silent_file_is_converted_untrimmed() {
    let directory = TestDirectory::new();
    let input = directory.0.join("silent.wav");
    let output = directory.0.join("out.wav");
    lavfi_wav(&input, "anullsrc=r=48000:cl=mono:d=2").await;
    let notice = convert_with_trim(
        &directory,
        &input,
        &output,
        &trim_settings(-60.0, 200),
        None,
    )
    .await
    .unwrap();
    assert_eq!(notice, Some((TrimNotice::AllSilent, None)));
    let cancellation = crate::JobCancellation::new();
    let converted = probe_file(&output, &cancellation).await.unwrap();
    assert!(
        (converted.duration_secs - 2.0).abs() <= 0.01,
        "duration: {}",
        converted.duration_secs
    );
}

#[tokio::test]
#[ignore = "requires installed FFmpeg and FFprobe"]
async fn real_ffmpeg_analysis_failure_converts_untrimmed() {
    let directory = TestDirectory::new();
    let input = directory.0.join("tone.wav");
    let output = directory.0.join("out.wav");
    lavfi_wav(
        &input,
        "aevalsrc=if(between(t\\,1\\,2)\\,0.25*sin(2*PI*440*t)\\,0):s=48000:d=3",
    )
    .await;
    FORCE_ANALYSIS_FAILURE.with(|flag| flag.set(true));
    let result = convert_with_trim(
        &directory,
        &input,
        &output,
        &trim_settings(-60.0, 200),
        None,
    )
    .await;
    FORCE_ANALYSIS_FAILURE.with(|flag| flag.set(false));
    let notice = result.unwrap();
    assert!(
        matches!(&notice, Some((TrimNotice::AnalysisFailed, Some(detail))) if !detail.is_empty()),
        "{notice:?}"
    );
    let cancellation = crate::JobCancellation::new();
    let converted = probe_file(&output, &cancellation).await.unwrap();
    assert!(
        (converted.duration_secs - 3.0).abs() <= 0.01,
        "duration: {}",
        converted.duration_secs
    );
}

#[tokio::test]
#[ignore = "requires installed FFmpeg and FFprobe"]
async fn real_ffmpeg_multitrack_decides_per_track() {
    let directory = TestDirectory::new();
    let input = directory.0.join("two-tracks.mka");
    ffmpeg_output(&[
        "-v".as_ref(),
        "error".as_ref(),
        "-f".as_ref(),
        "lavfi".as_ref(),
        "-i".as_ref(),
        "aevalsrc=if(between(t\\,1\\,4)\\,0.25*sin(2*PI*440*t)\\,0):s=48000:d=5".as_ref(),
        "-f".as_ref(),
        "lavfi".as_ref(),
        "-i".as_ref(),
        "aevalsrc=if(between(t\\,2\\,3)\\,0.25*sin(2*PI*660*t)\\,0):s=48000:d=5".as_ref(),
        "-map".as_ref(),
        "0:a".as_ref(),
        "-map".as_ref(),
        "1:a".as_ref(),
        "-c:a".as_ref(),
        "pcm_s16le".as_ref(),
        input.as_os_str(),
    ])
    .await;
    let cancellation = crate::JobCancellation::new();
    let info = probe_file(&input, &cancellation).await.unwrap();
    assert_eq!(info.audio_tracks.len(), 2);
    for (track, expected) in info.audio_tracks.iter().zip([3.1, 1.1]) {
        let output = directory
            .0
            .join(format!("track-{}.wav", track.stream_index));
        let notice = convert_with_trim(
            &directory,
            &input,
            &output,
            &trim_settings(-60.0, 200),
            Some(track.stream_index),
        )
        .await
        .unwrap();
        assert_eq!(notice, None);
        let converted = probe_file(&output, &cancellation).await.unwrap();
        assert!(
            (converted.duration_secs - expected).abs() < 0.03,
            "track {}: {} vs {expected}",
            track.stream_index,
            converted.duration_secs
        );
    }
}

/// MP4 whose audio starts about 0.5 s after the video (container timestamps are
/// offset from the first audio sample): 1 s silence, 2 s tone, 1 s silence, AAC.
async fn delayed_audio_mp4(path: &Path) {
    ffmpeg_output(&[
        "-v".as_ref(),
        "error".as_ref(),
        "-f".as_ref(),
        "lavfi".as_ref(),
        "-i".as_ref(),
        "testsrc=d=4:s=64x64:r=25".as_ref(),
        "-itsoffset".as_ref(),
        "0.5".as_ref(),
        "-f".as_ref(),
        "lavfi".as_ref(),
        "-i".as_ref(),
        "aevalsrc=if(between(t\\,1\\,3)\\,0.25*sin(2*PI*440*t)\\,0):s=48000:d=4".as_ref(),
        "-c:v".as_ref(),
        "mpeg4".as_ref(),
        "-c:a".as_ref(),
        "aac".as_ref(),
        "-b:a".as_ref(),
        "128k".as_ref(),
        path.as_os_str(),
    ])
    .await;
}

/// Leading and trailing quiet run lengths in seconds (|sample| below about -40 dBFS).
fn quiet_edges(samples: &[i16]) -> (f64, f64) {
    let loud = |s: &i16| s.unsigned_abs() > 300;
    let head = samples.iter().position(loud).unwrap_or(samples.len());
    let tail = samples.iter().rev().position(loud).unwrap_or(samples.len());
    (head as f64 / 48000.0, tail as f64 / 48000.0)
}

#[tokio::test]
#[ignore = "requires installed FFmpeg and FFprobe"]
async fn real_ffmpeg_trim_of_delayed_audio_mp4_keeps_the_whole_tone() {
    let directory = TestDirectory::new();
    let input = directory.0.join("delayed-audio.mp4");
    let output = directory.0.join("trimmed.wav");
    delayed_audio_mp4(&input).await;
    let notice = convert_with_trim(
        &directory,
        &input,
        &output,
        &trim_settings(-60.0, 200),
        None,
    )
    .await
    .unwrap();
    assert_eq!(notice, None);
    let samples = decode_s16(&output).await;
    let (head, tail) = quiet_edges(&samples);
    let seconds = samples.len() as f64 / 48000.0;
    // 50 ms margins within the AAC frame size (about 21 ms) of tolerance.
    assert!((0.03..0.09).contains(&head), "head silence: {head}");
    assert!((0.03..0.09).contains(&tail), "tail silence: {tail}");
    assert!((2.05..2.2).contains(&seconds), "duration: {seconds}");
}

#[tokio::test]
#[ignore = "requires installed FFmpeg and FFprobe"]
async fn real_ffmpeg_cached_and_direct_network_inputs_trim_identically() {
    let directory = TestDirectory::new();
    let input = directory.0.join("silence-tone-silence.wav");
    lavfi_wav(
        &input,
        "aevalsrc=if(between(t\\,1\\,3)\\,0.25*sin(2*PI*440*t)\\,0):s=48000:d=4",
    )
    .await;
    let settings = trim_settings(-60.0, 200);

    let cache_used = Arc::new(AtomicUsize::new(0));
    let spool_manager = crate::spool::SpoolManager::initialize(&directory.0.join("spool")).unwrap();
    let cached = copy_network_input_to_temp(
        &input,
        cache_used.clone(),
        spool_manager,
        None,
        Arc::new(crate::JobCancellation::new()),
        Arc::new(|_| {}),
    )
    .await
    .unwrap();
    assert!(cache_used.load(Ordering::Acquire) > 0);

    let cached_output = directory.0.join("cached.wav");
    let notice = convert_with_trim_network(
        &directory,
        &input,
        &cached_output,
        &settings,
        None,
        Some(NetworkInput::Cached(cached.clone())),
    )
    .await
    .unwrap();
    assert_eq!(notice, None);
    let direct_output = directory.0.join("direct.wav");
    let notice = convert_with_trim_network(
        &directory,
        &input,
        &direct_output,
        &settings,
        None,
        Some(NetworkInput::Direct(input.clone())),
    )
    .await
    .unwrap();
    assert_eq!(notice, None);

    let cached_samples = decode_s16(&cached_output).await;
    let direct_samples = decode_s16(&direct_output).await;
    let seconds = cached_samples.len() as f64 / 48000.0;
    assert!((2.05..2.2).contains(&seconds), "duration: {seconds}");
    assert_eq!(cached_samples, direct_samples);

    drop(cached);
    assert_eq!(cache_used.load(Ordering::Acquire), 0);
}
