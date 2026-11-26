//! What the player decides, without any UI: the subtitle choice that follows the Audio
//! language setting, the skip it offers, the details sheet and the clock. Shared by the
//! Dioxus controls (player_ui.rs).
use crate::{backend, engine::{self, Finish, Segment, SegmentKind, State}};

/// Downloadable subtitles listed before a language is typed.
pub const ONLINE_SHOWN: usize = 60;

pub const SPEEDS: [f64; 6] = [0.5, 0.75, 1.0, 1.25, 1.5, 2.0];
pub const KEYS: [(&[&str], &str); 22] = [
    (&["Enter"], "Skip the intro or recap, when offered"),
    (&["Space", "K", "Enter"], "Play or pause"), (&["←", "→"], "Back or forward 10 seconds"), (&["J", "L"], "Back or forward 10 seconds"),
    (&["0–9"], "Jump to 0–90%"), (&["Home"], "Back to the start"), (&["↑", "↓"], "Volume"), (&["M"], "Mute"),
    (&["A"], "Next audio track"), (&["C"], "Next subtitles, then off"), (&["S"], "Subtitles list"), (&[",", "."], "Subtitle delay −/+ 0.25 s"),
    (&["B"], "Chapters"), (&["Z"], "Fit, fill, stretch"),
    (&["[", "]", "\\"], "Slower, faster, normal speed"), (&["I"], "Details"), (&["H", "?"], "Show or hide shortcuts"),
    (&["N"], "Next episode"), (&["Esc"], "Close panel, leave fullscreen, or close player"), (&["Q"], "Close player"),
    (&["F"], "Fullscreen"), (&["Tab"], "Next control; Shift+Tab goes back"),
];
pub const ANDROID: bool = cfg!(target_os = "android");

/// The skip the player offers for a segment: credits only lead to the next episode.
pub fn offer(segments: &[Segment], position: f64, has_next: bool) -> Option<Segment> {
    engine::skip_offer(segments, position).filter(|g| g.kind != SegmentKind::Credits || has_next)
}

/// Whether the floating "Up Next" auto-play countdown card should appear.
pub fn up_next_due(position: f64, duration: f64, ended: bool, segments: &[Segment], has_next: bool, dismissed: bool) -> bool {
    if !has_next || dismissed { return false; }
    if ended { return true; }
    if offer(segments, position, true).is_some_and(|g| g.kind == SegmentKind::Credits) { return true; }
    duration > 180.0 && (duration - position) <= 25.0 && position > 0.0
}

pub fn skip(live: &engine::Live, g: Segment) {
    if g.kind == SegmentKind::Credits { live.finish(Finish::Next) } else { live.seek_to(g.end) }
}

pub fn clock(seconds: f64) -> String {
    let t = seconds.max(0.0) as u64;
    let (h, m, s) = (t / 3600, t / 60 % 60, t % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// What to do when a video starts, for the audio-language setting (`None` is "Any language").
#[derive(Debug, PartialEq)]
pub enum Fallback {
    /// No audio tracks, or untagged audio that could be yours: leave the file as it plays.
    Nothing,
    /// Your language's audio is there (this track, when it was not already on), or the setting is
    /// Any: subtitles off.
    Off(Option<i64>),
    /// No audio in your language: this embedded subtitle track on.
    Embedded(i64),
    /// No audio and no embedded subtitles in your language: download them.
    Download,
}

/// The Audio language setting decides subtitles: off when its language is in the main audio (a
/// file can flag a default subtitle regardless), otherwise full subtitles in that language (text
/// before image, never forced-only signs), else a download. Untagged audio says nothing, so the
/// file's own choice stays.
#[cfg(test)]
pub fn fallback(tracks: &[engine::Track], key: Option<&str>) -> Fallback {
    fallback_with_pref(tracks, key, None).1
}

/// Per-series remembered audio/subtitle language preference takes priority over the global
/// Audio-language setting when matching tracks exist in the current episode.
/// Returns `(explicit_audio_id, subtitle_action)`.
pub fn fallback_with_pref(tracks: &[engine::Track], global_key: Option<&str>, pref: Option<&backend::TrackPref>) -> (Option<i64>, Fallback) {
    let audio_tracks: Vec<&engine::Track> = tracks.iter().filter(|t| t.audio && !engine::extra_audio(t)).collect();
    if let Some(p) = pref {
        let mut chosen_audio = None;
        if let Some(al) = p.audio_lang.as_deref() {
            if let Some(t) = audio_tracks.iter().find(|t| lang_matches(&t.lang, al)) {
                chosen_audio = Some(t.id);
            }
        }
        if p.sub_off {
            return (chosen_audio, Fallback::Off(chosen_audio));
        }
        if let Some(sl) = p.sub_lang.as_deref() {
            let full = |t: &&engine::Track| !t.audio && !t.forced && lang_matches(&t.lang, sl);
            if let Some(t) = tracks.iter().filter(full).find(|t| !t.image).or_else(|| tracks.iter().find(full)) {
                return (chosen_audio, Fallback::Embedded(t.id));
            }
            return (chosen_audio, Fallback::Download);
        }
        if chosen_audio.is_some() {
            return (chosen_audio, Fallback::Off(chosen_audio));
        }
    }
    let Some(key) = global_key else { return (None, Fallback::Off(None)) };
    let in_language = |t: &engine::Track| backend::track_in_language(&t.lang, key);
    if audio_tracks.is_empty() { return (None, Fallback::Nothing); }
    if let Some(t) = audio_tracks.iter().find(|t| in_language(t)) { return (Some(t.id), Fallback::Off(Some(t.id))); }
    if audio_tracks.iter().any(|t| t.lang.is_empty()) { return (None, Fallback::Nothing); }
    if tracks.iter().any(|t| !t.audio && t.selected && !t.forced && in_language(t)) { return (None, Fallback::Nothing); }
    let full = |t: &&engine::Track| !t.audio && !t.forced && in_language(t);
    match tracks.iter().filter(full).find(|t| !t.image).or_else(|| tracks.iter().find(full)) {
        Some(t) => (None, Fallback::Embedded(t.id)),
        None => (None, Fallback::Download),
    }
}

fn lang_matches(track_lang: &str, wanted: &str) -> bool {
    if track_lang.is_empty() || wanted.is_empty() { return false; }
    if track_lang.eq_ignore_ascii_case(wanted) { return true; }
    let base_t = track_lang.split(['-', '_']).next().unwrap_or(track_lang);
    let base_w = wanted.split(['-', '_']).next().unwrap_or(wanted);
    if base_t.eq_ignore_ascii_case(base_w) { return true; }
    crate::addons::LANGUAGES.iter().any(|l| {
        (l.key.eq_ignore_ascii_case(wanted) || l.codes.iter().any(|c| c.eq_ignore_ascii_case(base_w)))
            && l.codes.iter().any(|c| c.eq_ignore_ascii_case(base_t))
    })
}

/// File details, plainly: what is decoded, how, and what the screen shows.
pub fn details(live: &engine::Live, s: &State, facts: &crate::player::Facts, hw: Option<&str>) -> Vec<(&'static str, String)> {
    let mut rows = Vec::new();
    if s.width > 0 { rows.push(("Resolution", format!("{}×{}{}", s.width, s.height, if facts.resolution.is_empty() { String::new() } else { format!(" ({})", facts.resolution) }))); }
    if !s.codec.is_empty() { rows.push(("Video", engine::codec_name(&s.codec))); }
    if !facts.picture.is_empty() { rows.push(("Picture", facts.picture.clone())); }
    rows.push(("Decoding", hw.map(|h| format!("Hardware, {h}")).unwrap_or_else(|| if s.loaded { "Software".into() } else { "Starting".into() })));
    rows.push(("Output", if ANDROID {
        if s.direct { "Direct to the display (MediaCodec)".into() } else if hw.is_none() && s.dv_profile == 5 { "Rendered by mpv, Dolby Vision reshaping".into() } else { "Rendered by mpv (gpu-next)".into() }
    } else if cfg!(target_os = "windows") { "In the app, rendered by mpv (Direct3D 11)".into() } else { "In the app, rendered by mpv (OpenGL)".into() }));
    // What the screen declared when this video started (Linux: always SDR).
    rows.push(("Screen", crate::display::summary(live.screen, backend::hdr_output()).0));
    if !s.target_transfer.is_empty() && !ANDROID { rows.push(("Sent to the screen", if s.target_transfer == "pq" { "HDR10".into() } else { "SDR".into() })); }
    if let Some(label) = &live.request.hdr { rows.push(("Source says", label.clone())); }
    if s.dv_profile > 0 { rows.push(("Dolby Vision", format!("Profile {}", s.dv_profile))); }
    if s.loaded { rows.push(("Dropped frames", s.dropped.to_string())); }
    if !facts.size.is_empty() { rows.push(("File size", facts.size.clone())); }
    if let Some(a) = s.tracks.iter().find(|t| t.audio && t.selected) { rows.push(("Audio", a.label.clone())); }
    rows
}

#[cfg(test)]
mod tests {
    use super::{fallback, Fallback};
    use crate::engine::Track;
    fn track(id: i64, audio: bool, lang: &str) -> Track { Track { id, audio, lang: lang.into(), ..Default::default() } }

    #[test]
    fn subtitles_follow_the_audio_language_setting() {
        let it = Some("italian");
        // MULTI with Italian audio: mpv's alang picks it, and a flagged subtitle is turned off.
        assert_eq!(fallback(&[track(1, true, "eng"), track(2, true, "ita")], it), Fallback::Off(Some(2)));
        assert_eq!(fallback(&[track(1, true, "eng"), track(2, true, "ita"), track(3, false, "eng")], it), Fallback::Off(Some(2)));
        assert_eq!(fallback(&[track(1, true, "eng"), track(2, true, "ita"), Track { selected: true, ..track(3, false, "eng") }], it), Fallback::Off(Some(2)));
        // "Any language": the file's default audio, no subtitles even when one is flagged.
        assert_eq!(fallback(&[track(1, true, "eng"), track(2, true, "ita"), track(3, false, "eng")], None), Fallback::Off(None));
        // Untagged audio can't be told apart, alone or next to another language: left alone.
        assert_eq!(fallback(&[track(1, true, "")], it), Fallback::Nothing);
        assert_eq!(fallback(&[track(1, true, ""), track(2, true, "eng")], it), Fallback::Nothing);
        // ...but it does not hide your language when that track is there too.
        assert_eq!(fallback(&[track(1, true, ""), track(2, true, "ita")], it), Fallback::Off(Some(2)));
        // Forced Italian (signs only) is skipped for the full track; an image track only if nothing else.
        let forced = Track { forced: true, ..track(5, false, "ita") };
        let pgs = Track { image: true, ..track(6, false, "ita") };
        assert_eq!(fallback(&[track(1, true, "eng"), forced.clone(), pgs.clone(), track(7, false, "ita")], it), Fallback::Embedded(7));
        assert_eq!(fallback(&[track(1, true, "eng"), forced.clone(), pgs], it), Fallback::Embedded(6));
        assert_eq!(fallback(&[track(1, true, "eng"), forced], it), Fallback::Download);
        // English only, with Italian subtitles in the file: those.
        assert_eq!(fallback(&[track(1, true, "eng"), track(3, false, "eng"), track(4, false, "ita")], it), Fallback::Embedded(4));
        // English only, no Italian subtitles in the file: download.
        assert_eq!(fallback(&[track(1, true, "eng"), track(3, false, "eng")], it), Fallback::Download);
        // Commentary in your language does not suppress subtitles over the main foreign audio.
        let commentary = Track { label: "English director commentary".into(), ..track(2, true, "eng") };
        assert_eq!(fallback(&[track(1, true, "jpn"), commentary], Some("english")), Fallback::Download);
        // Italian subtitles already on (forced by the file): nothing.
        assert_eq!(fallback(&[track(1, true, "eng"), Track { selected: true, ..track(4, false, "it") }], it), Fallback::Nothing);
        // A subtitle language tag with a region still counts.
        assert_eq!(fallback(&[track(1, true, "eng"), track(2, false, "it-IT")], it), Fallback::Embedded(2));
    }
    #[test]
    fn clock_reads_like_a_player() {
        assert_eq!(super::clock(5.0), "0:05");
        assert_eq!(super::clock(605.4), "10:05");
        assert_eq!(super::clock(3725.0), "1:02:05");
        assert_eq!(super::clock(-3.0), "0:00");
    }

    #[test]
    fn per_series_track_memory_overrides_global_language() {
        let tracks = vec![track(1, true, "eng"), track(2, true, "jpn"), track(3, false, "eng"), track(4, false, "ita")];
        let pref = crate::backend::TrackPref { audio_lang: Some("jpn".into()), sub_lang: Some("eng".into()), sub_off: false };
        assert_eq!(super::fallback_with_pref(&tracks, Some("italian"), Some(&pref)), (Some(2), Fallback::Embedded(3)));
        let off = crate::backend::TrackPref { audio_lang: Some("eng".into()), sub_lang: None, sub_off: true };
        assert_eq!(super::fallback_with_pref(&tracks, Some("italian"), Some(&off)), (Some(1), Fallback::Off(Some(1))));
    }

    #[test]
    fn up_next_triggers_on_credits_tail_or_eof_until_dismissed() {
        use crate::engine::{Segment, SegmentKind};
        let credits = [Segment { kind: SegmentKind::Credits, start: 1300.0, end: 1380.0 }];
        assert!(!super::up_next_due(600.0, 1400.0, false, &credits, true, false));
        assert!(super::up_next_due(1310.0, 1400.0, false, &credits, true, false));
        assert!(super::up_next_due(1380.0, 1400.0, false, &[], true, false));
        assert!(super::up_next_due(1400.0, 1400.0, true, &[], true, false));
        assert!(!super::up_next_due(1310.0, 1400.0, false, &credits, false, false));
        assert!(!super::up_next_due(1310.0, 1400.0, false, &credits, true, true));
    }
}
