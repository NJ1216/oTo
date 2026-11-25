//! Pure decision logic for trimming leading and trailing silence.

pub const TRIM_MARGIN_SECS: f64 = 0.050;
pub const TRIM_FADE_SECS: f64 = 0.005;
const BOUNDARY_TOLERANCE_SECS: f64 = 0.05;
const MIN_SOUND_SECS: f64 = 1e-6;

#[derive(Debug, Clone, PartialEq)]
pub enum TrimDecision {
    Trim {
        keep_start: f64,
        keep_end: f64,
        cut_head: bool,
        cut_tail: bool,
    },
    NothingToTrim,
    AllSilent,
}

/// Decides what to trim from silence regions as returned by
/// `silence::parse_silence_regions_full` plus the probed duration.
pub fn decide_trim(
    regions: &[(f64, f64)],
    unclosed_tail: Option<f64>,
    decoded_duration: Option<f64>,
    total_duration: f64,
) -> TrimDecision {
    let tol = BOUNDARY_TOLERANCE_SECS;
    let effective_end = decoded_duration.unwrap_or(total_duration);

    // Sound starts where the region touching the head ends.
    let head_end = regions
        .iter()
        .find(|(start, _)| *start <= tol)
        .map(|(_, end)| *end);
    let tail_start = unclosed_tail.or_else(|| {
        regions
            .iter()
            .rev()
            .find(|(_, end)| (effective_end - *end).abs() <= tol)
            .map(|(start, _)| *start)
    });

    if head_end.is_none() && tail_start.is_none() {
        return TrimDecision::NothingToTrim;
    }

    let sound_start = head_end.unwrap_or(0.0);
    let sound_end = tail_start.unwrap_or(effective_end);
    if sound_end - sound_start <= MIN_SOUND_SECS {
        return TrimDecision::AllSilent;
    }

    let keep_start = (sound_start - TRIM_MARGIN_SECS).clamp(0.0, effective_end);
    let keep_end = (sound_end + TRIM_MARGIN_SECS).clamp(0.0, effective_end);
    let cut_head = head_end.is_some() && keep_start > 0.0;
    let cut_tail = tail_start.is_some() && keep_end < effective_end;
    if !cut_head && !cut_tail {
        return TrimDecision::NothingToTrim;
    }
    TrimDecision::Trim {
        keep_start,
        keep_end,
        cut_head,
        cut_tail,
    }
}

/// Builds the FFmpeg audio filter for a `Trim` decision; `None` otherwise.
pub fn trim_filter(decision: &TrimDecision) -> Option<String> {
    let TrimDecision::Trim {
        keep_start,
        keep_end,
        cut_head,
        cut_tail,
    } = decision
    else {
        return None;
    };
    // Analysis measures from the audio stream's first sample (asetpts=PTS-STARTPTS
    // before silencedetect), so rebase timestamps before cutting; containers whose
    // audio starts late (MP4 with a positive audio start_time) would otherwise be
    // cut at the wrong place.
    let mut atrim = String::from("asetpts=PTS-STARTPTS,atrim=");
    match (cut_head, cut_tail) {
        (true, true) => atrim.push_str(&format!("start={keep_start:.6}:end={keep_end:.6}")),
        (true, false) => atrim.push_str(&format!("start={keep_start:.6}")),
        _ => atrim.push_str(&format!("end={keep_end:.6}")),
    }
    let mut filter = format!("{atrim},asetpts=PTS-STARTPTS");
    if *cut_head {
        filter.push_str(&format!(",afade=t=in:st=0:d={TRIM_FADE_SECS}"));
    }
    if *cut_tail {
        let start = if *cut_head { *keep_start } else { 0.0 };
        let out_start = keep_end - start - TRIM_FADE_SECS;
        filter.push_str(&format!(
            ",afade=t=out:st={out_start:.6}:d={TRIM_FADE_SECS}"
        ));
    }
    Some(filter)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trim(keep_start: f64, keep_end: f64, cut_head: bool, cut_tail: bool) -> TrimDecision {
        TrimDecision::Trim {
            keep_start,
            keep_end,
            cut_head,
            cut_tail,
        }
    }

    fn close(a: &TrimDecision, b: &TrimDecision) {
        match (a, b) {
            (
                TrimDecision::Trim {
                    keep_start: s1,
                    keep_end: e1,
                    cut_head: h1,
                    cut_tail: t1,
                },
                TrimDecision::Trim {
                    keep_start: s2,
                    keep_end: e2,
                    cut_head: h2,
                    cut_tail: t2,
                },
            ) => {
                assert!(
                    (s1 - s2).abs() < 1e-9 && (e1 - e2).abs() < 1e-9,
                    "{a:?} vs {b:?}"
                );
                assert_eq!((h1, t1), (h2, t2));
            }
            _ => assert_eq!(a, b),
        }
    }

    #[test]
    fn head_and_tail_keep_margin() {
        close(
            &decide_trim(&[(0.0, 1.0), (6.0, 7.0)], None, Some(7.0), 7.0),
            &trim(0.95, 6.05, true, true),
        );
    }

    #[test]
    fn internal_region_is_ignored() {
        close(
            &decide_trim(&[(0.0, 1.0), (3.0, 4.0), (6.0, 7.0)], None, Some(7.0), 7.0),
            &trim(0.95, 6.05, true, true),
        );
    }

    #[test]
    fn unclosed_tail_counts_as_tail() {
        close(
            &decide_trim(&[(0.0, 1.0)], Some(6.0), Some(7.0), 7.0),
            &trim(0.95, 6.05, true, true),
        );
    }

    #[test]
    fn no_boundary_silence() {
        assert_eq!(
            decide_trim(&[(3.0, 4.0)], None, Some(7.0), 7.0),
            TrimDecision::NothingToTrim
        );
    }

    #[test]
    fn sound_at_both_edges_has_no_cut() {
        assert_eq!(
            decide_trim(&[], None, Some(7.0), 7.0),
            TrimDecision::NothingToTrim
        );
    }

    #[test]
    fn whole_file_silent() {
        assert_eq!(
            decide_trim(&[], Some(0.0), Some(7.0), 7.0),
            TrimDecision::AllSilent
        );
        assert_eq!(
            decide_trim(&[(0.0, 7.0)], None, Some(7.0), 7.0),
            TrimDecision::AllSilent
        );
    }

    #[test]
    fn sound_start_equal_to_sound_end_is_all_silent() {
        assert_eq!(
            decide_trim(&[(0.0, 1.0)], Some(1.0), Some(3.0), 3.0),
            TrimDecision::AllSilent
        );
    }

    #[test]
    fn short_sound_between_clamped_margins_is_trimmed() {
        close(
            &decide_trim(&[(0.0, 0.01)], Some(0.03), Some(0.2), 0.2),
            &trim(0.0, 0.08, false, true),
        );
    }

    #[test]
    fn head_only() {
        close(
            &decide_trim(&[(0.0, 1.0)], None, Some(7.0), 7.0),
            &trim(0.95, 7.0, true, false),
        );
    }

    #[test]
    fn tail_only_with_closed_region() {
        close(
            &decide_trim(&[(6.0, 7.0)], None, Some(7.0), 7.0),
            &trim(0.0, 6.05, false, true),
        );
    }

    #[test]
    fn decoded_duration_preferred_over_metadata() {
        match decide_trim(&[(0.0, 1.0), (8.0, 9.0)], None, Some(9.0), 10.0) {
            TrimDecision::Trim { cut_tail, .. } => assert!(cut_tail),
            other => panic!("expected Trim, got {other:?}"),
        }
    }

    #[test]
    fn margin_swallowing_cut_is_not_a_cut() {
        assert_eq!(
            decide_trim(&[(0.0, 0.03)], None, Some(7.0), 7.0),
            TrimDecision::NothingToTrim
        );
    }

    #[test]
    fn filter_head_only() {
        assert_eq!(
            trim_filter(&trim(0.95, 7.0, true, false)).as_deref(),
            Some("asetpts=PTS-STARTPTS,atrim=start=0.950000,asetpts=PTS-STARTPTS,afade=t=in:st=0:d=0.005")
        );
    }

    #[test]
    fn filter_tail_only() {
        assert_eq!(
            trim_filter(&trim(0.0, 6.05, false, true)).as_deref(),
            Some("asetpts=PTS-STARTPTS,atrim=end=6.050000,asetpts=PTS-STARTPTS,afade=t=out:st=6.045000:d=0.005")
        );
    }

    #[test]
    fn filter_both() {
        assert_eq!(
            trim_filter(&trim(0.95, 6.05, true, true)).as_deref(),
            Some("asetpts=PTS-STARTPTS,atrim=start=0.950000:end=6.050000,asetpts=PTS-STARTPTS,afade=t=in:st=0:d=0.005,afade=t=out:st=5.095000:d=0.005")
        );
    }

    #[test]
    fn filter_none_unless_trim() {
        assert_eq!(trim_filter(&TrimDecision::NothingToTrim), None);
        assert_eq!(trim_filter(&TrimDecision::AllSilent), None);
    }
}
