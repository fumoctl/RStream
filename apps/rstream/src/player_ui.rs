//! Veylo's player controls, in Dioxus: the same glass over the picture on Linux and Android.
//! The picture itself is under the transparent WebView (video.rs); everything here is the
//! app's own design, lit by the title's accent. Keys arrive through the shell (Shell::player_key).
use crate::{backend::{self, OnlineSubtitle}, engine::{self, Finish, Fit, LiveRef, SegmentKind, State}, icons, Shell};
use crate::player_rules::{clock, details, fallback_with_pref, offer, skip, up_next_due, Fallback, ANDROID, KEYS, ONLINE_SHOWN, SPEEDS};
use dioxus::prelude::*;

/// Subtitles the add-ons offer for this video, fetched the first time the sheet opens.
#[derive(Clone, Debug, PartialEq)]
enum Online { Idle, Loading, Ready(Vec<OnlineSubtitle>), Failed }


#[derive(Clone, Copy, Debug, PartialEq)]
enum Sheet { Audio, Subtitles, Chapters, Speed, Picture, Shaders, Commands, Info, Keys }

fn dev_initial_sheet() -> Option<Sheet> {
    match std::env::var("RSTREAM_DEV_PLAYER_SHEET").ok()?.trim().to_ascii_lowercase().as_str() {
        "audio" => Some(Sheet::Audio),
        "subtitles" => Some(Sheet::Subtitles),
        "chapters" => Some(Sheet::Chapters),
        "speed" => Some(Sheet::Speed),
        "picture" => Some(Sheet::Picture),
        "shaders" => Some(Sheet::Shaders),
        "commands" => Some(Sheet::Commands),
        "info" | "details" => Some(Sheet::Info),
        "keys" | "shortcuts" => Some(Sheet::Keys),
        _ => None,
    }
}


#[component]
pub fn PlayerScreen(live: LiveRef) -> Element {
    let mut shell = use_context::<Shell>();
    let mut state = use_signal(|| live.0.state());
    let mut shown = use_signal(|| true);
    let mut hide_at = use_signal(|| None::<std::time::Instant>);
    let mut timer_running = use_signal(|| false);
    let mut last_move = use_signal(|| None::<std::time::Instant>);
    let mut sheet = use_signal(dev_initial_sheet);
    let mut scrub = use_signal(|| None::<f64>);
    let mut fullscreen = use_signal(|| false);
    let mut rotation_locked = use_signal(|| false);
    let mut up_next_dismissed = use_signal(|| false);
    let mut up_next_secs = use_signal(|| None::<u8>);
    let mut up_next_task = use_signal(|| false);
    let auto_next_on = use_hook(|| backend::load_settings().auto_next);
    let mpv_cfg = use_hook(|| {
        let dir = rstream_mpv_config::MpvConfigDir::open_or_seed(backend::mpv_config_dir()).ok();
        let bindings = dir.as_ref().and_then(|d| d.parse_input_conf().ok()).unwrap_or_default();
        let shaders = dir.as_ref().map(|d| d.list_shaders()).unwrap_or_default();
        let shaders_dir = dir.as_ref().map(|d| d.shaders_dir().to_string_lossy().into_owned())
            .unwrap_or_else(|| backend::mpv_config_dir().join("shaders").to_string_lossy().into_owned());
        (bindings, shaders, shaders_dir)
    });
    // Touch gesture state (Android phone/tablet): double-tap seek, vertical brightness/volume drag, long-press 2×.
    #[cfg(target_os = "android")]
    let mut touch_start = use_signal(|| None::<(f64, f64, std::time::Instant, f32, f32, u64)>);
    #[cfg(target_os = "android")]
    let mut touch_drag = use_signal(|| None::<bool>); // Some(true)=brightness(left), Some(false)=volume(right)
    #[cfg(target_os = "android")]
    let mut touch_boost = use_signal(|| None::<f64>); // previous speed while long-pressing 2×
    #[cfg(target_os = "android")]
    let mut last_tap = use_signal(|| None::<(std::time::Instant, f64)>);
    #[cfg(target_os = "android")]
    let mut suppress_click = use_signal(|| false);
    let mut online = use_signal(|| Online::Idle);
    let sub_query = use_signal(String::new);
    let mut automatic_language = use_signal(|| None::<String>);
    // The download in progress, and what each downloaded subtitle became (its track id).
    let mut sub_busy = use_signal(|| None::<String>);
    let mut sub_added = use_signal(std::collections::HashMap::<String, i64>::new);
    // The video whose subtitles can be downloaded: the episode, or the film itself.
    let wanted = live.0.request.record.as_ref().map(|(m, v)| (m.kind.clone(), v.clone().unwrap_or_else(|| m.id.clone())));
    let media_key = live.0.request.record.as_ref().map(|(m, _)| m.key());
    use_effect(move || {
        // Reopening the sheet retries a failed lookup, without a retry loop while it stays open.
        if sheet() == Some(Sheet::Subtitles) && *online.peek() == Online::Failed { online.set(Online::Idle); }
    });
    let target = wanted.clone();
    use_effect(move || {
        if sheet() != Some(Sheet::Subtitles) && automatic_language().is_none() { return; }
        if *online.peek() != Online::Idle { return; }
        let Some((kind, video)) = target.clone() else { return };
        online.set(Online::Loading);
        spawn(async move {
            online.set(match backend::online_subtitles(&kind, &video).await { Ok(list) => Online::Ready(list), Err(_) => Online::Failed });
        });
    });

    // Intro times from IntroDB or AniSkip, asked once the episode's length is known (AniSkip
    // matches its entries to it). Films and previews have no episode to look up.
    let episode = live.0.request.record.as_ref().and_then(|(_, v)| v.clone());
    let mut looked_up = use_signal(|| false);
    let ask = live.clone();
    use_effect(move || {
        let duration = state().duration;
        if duration <= 0.0 || *looked_up.peek() { return; }
        let Some(video) = episode.clone() else { return };
        looked_up.set(true);
        let live = ask.0.clone();
        spawn(async move { live.set_online_segments(backend::online_segments(&video, duration).await); });
    });

    // The engine publishes its state; the controls follow it.
    let feed = live.clone();
    use_future(move || {
        let live = feed.clone();
        async move {
            let mut rx = live.0.subscribe();
            state.set(rx.borrow_and_update().clone());
            while rx.changed().await.is_ok() { let s = rx.borrow_and_update().clone(); state.set(s); }
        }
    });
    // The picture goes under the page while this screen exists.
    let session_id = std::sync::Arc::as_ptr(&live.0) as usize;
    let attach = live.clone();
    use_hook(move || crate::video::attach(&attach.0, session_id));
    use_drop(move || crate::video::detach(session_id));

    // Controls fade three seconds after the last touch, key or pointer movement (one timer task,
    // never spawning per mousemove pixel).
    let mut poke = move || {
        if !*shown.peek() { shown.set(true); }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        hide_at.set(Some(deadline));
        if !*timer_running.peek() {
            timer_running.set(true);
            spawn(async move {
                loop {
                    let Some(target) = *hide_at.peek() else { break };
                    let now = std::time::Instant::now();
                    if now >= target {
                        shown.set(false);
                        break;
                    }
                    backend::sleep((target - now).as_millis().max(50) as u64).await;
                }
                timer_running.set(false);
            });
        }
    };
    use_effect(move || {
        let _ = sheet();
        poke();
    });

    // Developer hook: RSTREAM_DEV_PLAYER_PAUSED=1 pauses once a frame is decoded so headless UI checks
    // can inspect static player controls and open sheets deterministically.
    let dev_pause = use_hook(|| matches!(std::env::var("RSTREAM_DEV_PLAYER_PAUSED").ok().as_deref(), Some("1" | "true" | "yes")));
    let mut dev_paused = use_signal(|| false);
    let pause_live = live.clone();
    use_effect(move || {
        if !dev_pause || *dev_paused.peek() { return; }
        let s = state.read();
        if s.loaded && s.width > 0 {
            dev_paused.set(true);
            let live = pause_live.0.clone();
            spawn(async move {
                backend::sleep(180).await;
                live.set_paused(true);
                shown.set(true);
            });
        }
    });

    // Prefetch the next episode's sources once playback enters the last quarter (or last 3 minutes)
    // so clicking "Next episode" or reaching credits starts almost instantly.
    let next_ep = live.0.request.next_video.clone();
    let mut next_prefetched = use_signal(|| false);
    use_effect(move || {
        if *next_prefetched.peek() { return; }
        let Some((kind, id)) = next_ep.as_ref() else { return };
        let s = state.read();
        if s.duration > 60.0 && (s.position >= s.duration * 0.75 || s.duration - s.position <= 180.0) {
            next_prefetched.set(true);
            backend::prefetch_sources(kind, id);
        }
    });

    let s: State = state();
    let l = live.0.clone();
    let req = &l.request;
    let visible = shown() || s.paused || sheet().is_some() || scrub().is_some() || s.error.is_some() || !s.loaded;
    let position = scrub().unwrap_or(s.position);
    let duration = s.duration.max(0.0);
    let pct = |v: f64| if duration > 0.0 { (v / duration * 100.0).clamp(0.0, 100.0) } else { 0.0 };
    let (played, buffered) = (pct(position), pct(position + s.buffered));
    let facts = engine::facts(&l, &s);
    let hw = engine::hardware(&s);
    // The facts line says plainly when HDR does not reach the panel ("HDR10+ → SDR").
    let mapped = facts.picture.contains("tone-mapped");
    let short_picture = { let name = facts.picture.split(',').next().unwrap_or_default();
        if facts.picture.contains("to SDR") { format!("{name} → SDR") } else { name.to_owned() } };
    let hdr = facts.picture.starts_with("HDR") || facts.picture.starts_with("HLG") || facts.picture.starts_with("Dolby");
    let has_subs = s.tracks.iter().any(|t| !t.audio) || wanted.is_some();
    let sub_on = s.tracks.iter().any(|t| !t.audio && t.selected);
    // Direct output: mpv can't draw into the picture, so text subtitles are drawn here.
    let caption = (s.direct && sub_on && !s.subtitle.trim().is_empty()).then(|| s.subtitle.clone());
    let up_next_active = auto_next_on
        && up_next_due(s.position, s.duration, s.ended, &s.skips(), req.has_next, up_next_dismissed())
        && sheet().is_none() && s.error.is_none() && scrub().is_none();
    let skippable = offer(&s.skips(), s.position, req.has_next)
        .filter(|g| sheet().is_none() && s.error.is_none() && scrub().is_none() && !(up_next_active && g.kind == SegmentKind::Credits));

    // Up Next auto-play countdown (5s at end of episode, 10s when credits begin).
    let next_live = live.clone();
    use_effect(move || {
        let s = state.read();
        let active = auto_next_on
            && up_next_due(s.position, s.duration, s.ended, &s.skips(), next_live.0.request.has_next, up_next_dismissed())
            && sheet().is_none() && s.error.is_none() && scrub().is_none();
        if !active {
            if up_next_secs.peek().is_some() { up_next_secs.set(None); }
            return;
        }
        if !*up_next_task.peek() {
            let start = if s.ended { 5 } else { 10 };
            up_next_secs.set(Some(start));
            up_next_task.set(true);
            let live = next_live.0.clone();
            spawn(async move {
                loop {
                    backend::sleep(1000).await;
                    let cur = *up_next_secs.peek();
                    let Some(rem) = cur else { break };
                    if *up_next_dismissed.peek() { up_next_secs.set(None); break; }
                    let st = state.peek().clone();
                    if st.paused && !st.ended { continue; }
                    if sheet.peek().is_some() { continue; }
                    if rem <= 1 {
                        up_next_secs.set(None);
                        live.finish(Finish::Next);
                        break;
                    }
                    up_next_secs.set(Some(rem - 1));
                }
                up_next_task.set(false);
            });
        }
    });

    // Keys routed from the shell.
    // A short word on screen for what a key just did ("Fill", "Speed 1.25×").
    let mut osd = use_signal(|| None::<(String, u64)>);
    let mut say = move |text: String| {
        let n = osd.peek().as_ref().map(|o| o.1 + 1).unwrap_or(0);
        osd.set(Some((text, n)));
        spawn(async move { backend::sleep(1400).await; if osd.peek().as_ref().is_some_and(|o| o.1 == n) { osd.set(None); } });
    };

    // Tap a downloadable subtitle: download it, add it as a track and turn it on.
    let get = live.clone();
    let dl_key = media_key.clone();
    // `tapped`: chosen in the sheet (which then closes), not added on its own by the fallback.
    let add_online = use_callback(move |(sub, tapped): (OnlineSubtitle, bool)| {
        let live = get.0.clone();
        if let Some(id) = sub_added.peek().get(&sub.id).copied() {
            live.select(false, Some(id));
            if tapped {
                if let Some(mk) = &dl_key {
                    let code = sub.code.clone();
                    backend::save_track_pref(mk, |p| { p.sub_lang = Some(code); p.sub_off = false; });
                }
                sheet.set(None);
            }
            return;
        }
        if sub_busy.peek().is_some() { return; }
        sub_busy.set(Some(sub.id.clone()));
        let mut say = say;
        let mk_save = dl_key.clone();
        spawn(async move {
            let file = backend::fetch_subtitle(&sub.url).await;
            let (title, code) = (sub.language.clone(), sub.code.clone());
            let code_for_pref = code.clone();
            let added = backend::on_runtime(async move { tokio::task::spawn_blocking(move || live.add_subtitle(&file, &title, &code)).await }).await;
            sub_busy.set(None);
            match added {
                Ok(Ok(Ok(id))) => {
                    sub_added.write().insert(sub.id.clone(), id);
                    if tapped {
                        if let Some(mk) = &mk_save {
                            backend::save_track_pref(mk, |p| { p.sub_lang = Some(code_for_pref); p.sub_off = false; });
                        }
                        sheet.set(None);
                        say(format!("Subtitles: {}", sub.language));
                    } else {
                        say(format!("No {0} audio: {0} subtitles on", sub.language));
                    }
                }
                _ => say("Those subtitles couldn’t be loaded".into()),
            }
        });
    });
    let download = use_callback(move |sub: OnlineSubtitle| add_online.call((sub, true)));

    // Subtitles follow per-series remembered preference first, then the global Audio language
    // setting, once per video: off when that language is in the audio, otherwise the file's own
    // full subtitles, else the best download.
    let mut fallback_done = use_signal(|| false);
    // Re-runs only when the video becomes ready, not on every position tick.
    let ready = use_memo(move || { let s = state.read(); s.loaded && !s.tracks.is_empty() });
    let (fb_live, fb_wanted, fb_mkey) = (live.clone(), wanted.clone(), media_key.clone());
    use_effect(move || {
        if !ready() || *fallback_done.peek() { return; }
        fallback_done.set(true);
        let s = state.peek().clone();
        let key = backend::stream_language();
        let pref = fb_mkey.as_deref().and_then(backend::load_track_pref);
        let name = key.as_deref().map(backend::subtitle_language_of).unwrap_or_default();
        let mut say = say;
        let (pref_audio, found) = fallback_with_pref(&s.tracks, key.as_deref(), pref.as_ref());
        let langs: Vec<&str> = s.tracks.iter().filter(|t| t.audio).map(|t| if t.lang.is_empty() { "?" } else { t.lang.as_str() }).collect();
        backend::dev_log(&format!("audio tracks [{}]{}", langs.join(" "), if matches!(found, Fallback::Embedded(_) | Fallback::Download) { ": none in your language" } else { "" }));
        if pref.is_none() && matches!(found, Fallback::Embedded(_) | Fallback::Download) && fb_live.0.request.language_check && s.position < 120.0 {
            say(format!("No {name} audio in this file: trying another source"));
            fb_live.0.finish(Finish::OtherLanguage);
            return;
        }
        if let Some(id) = pref_audio {
            if !s.tracks.iter().any(|t| t.audio && t.selected && t.id == id) { fb_live.0.select(true, Some(id)); }
        }
        match found {
            Fallback::Off(audio) => {
                if let Some(id) = audio {
                    if !s.tracks.iter().any(|t| t.audio && t.selected && t.id == id) { fb_live.0.select(true, Some(id)); }
                }
                if s.tracks.iter().any(|t| !t.audio && t.selected) { fb_live.0.select(false, None); }
            }
            Fallback::Embedded(id) => {
                fb_live.0.select(false, Some(id));
                if pref.is_none() { say(format!("No {name} audio: {name} subtitles on")); }
            }
            Fallback::Download => {
                let want_lang = pref.as_ref().and_then(|p| p.sub_lang.clone()).or(key);
                let Some(lang) = want_lang else { return };
                if fb_wanted.is_none() { return; }
                automatic_language.set(Some(lang));
            }
            Fallback::Nothing => {}
        }
    });
    // One lookup feeds both the sheet and the automatic fallback, whichever starts first.
    use_effect(move || {
        let Some(key) = automatic_language() else { return };
        match online() {
            Online::Ready(list) => {
                automatic_language.set(None);
                // A user's download takes precedence over automatic selection.
                if sub_busy.peek().is_some() { return; }
                let name = backend::subtitle_language_of(&key);
                match list.into_iter().find(|s| backend::track_in_language(&s.code, &key) || s.language == name) {
                    Some(sub) => add_online.call((sub, false)),
                    None => say(format!("No {name} audio or subtitles for this video")),
                }
            }
            Online::Failed => { automatic_language.set(None); say("Subtitles couldn’t be looked up right now".into()); }
            _ => {}
        }
    });

    // Keys routed from the shell; `input.conf` bindings run first, then Veylo's built-in keys.
    let keys = live.clone();
    let key_bindings = mpv_cfg.0.clone();
    use_effect(move || {
        let Some(key) = (shell.player_key)() else { return };
        shell.player_key.set(None);
        let live = &keys.0;
        let now = state.peek().clone();
        let speed = |v: f64| { let v = (v * 100.0).round() / 100.0; live.set_speed(v); if v == 1.0 { "Normal speed".to_string() } else { format!("Speed {v}×") } };
        // Reading shortcuts must not seek, change tracks or close playback accidentally.
        if sheet() == Some(Sheet::Keys) && !matches!(key.as_str(), "h" | "H" | "?" | "Escape" | "Backspace") { return; }
        if let Some(binding) = match_input_binding(&key_bindings, &key) {
            if is_cycle_fullscreen(&binding.action.command) {
                if !ANDROID {
                    let on = !fullscreen();
                    fullscreen.set(on);
                    crate::video::set_fullscreen(on);
                }
            } else {
                match live.run_mpv_command(&binding.action.command) {
                    Ok(()) => say(binding.action.label.clone()),
                    Err(e) => say(format!("{e}")),
                }
            }
            poke();
            return;
        }
        match key.as_str() {
            "Enter" if offer(&now.skips(), now.position, live.request.has_next).is_some() => {
                if let Some(g) = offer(&now.skips(), now.position, live.request.has_next) { skip(live, g); poke(); }
            }
            " " | "k" | "K" | "Enter" | "MediaPlayPause" => { live.toggle_pause(); poke(); }
            "MediaPlay" => { if now.paused { live.toggle_pause(); } poke(); }
            "MediaPause" => { if !now.paused { live.toggle_pause(); } poke(); }
            "ArrowLeft" | "j" | "J" | "MediaRewind" => { live.seek_by(-10.0); say("−10 s".into()); poke(); }
            "ArrowRight" | "l" | "L" | "MediaFastForward" => { live.seek_by(10.0); say("+10 s".into()); poke(); }
            "Home" => { live.seek_to(0.0); say("Start".into()); poke(); }
            d if d.len() == 1 && d.as_bytes()[0].is_ascii_digit() => { let p = (d.as_bytes()[0] - b'0') as f64 * 10.0; live.seek_percent(p); say(format!("{p}%")); poke(); }
            "ArrowUp" | "AudioVolumeUp" => { let v = (now.volume + 5.0).min(130.0); live.set_volume(v); say(format!("Volume {v:.0}")); poke(); }
            "ArrowDown" | "AudioVolumeDown" => { let v = (now.volume - 5.0).max(0.0); live.set_volume(v); say(format!("Volume {v:.0}")); poke(); }
            "m" | "M" | "AudioVolumeMute" => { live.toggle_mute(); say(if now.muted { "Sound on".into() } else { "Muted".into() }); poke(); }
            "f" | "F" if !ANDROID => { let on = !fullscreen(); fullscreen.set(on); crate::video::set_fullscreen(on); poke(); }
            "a" | "A" => { let t = live.cycle_track(true); say(format!("Audio: {t}")); poke(); }
            "c" | "C" => { let t = live.cycle_track(false); say(format!("Subtitles: {t}")); poke(); }
            "s" | "S" => { sheet.set(if sheet() == Some(Sheet::Subtitles) { None } else { Some(Sheet::Subtitles) }); }
            "," => { let d = now.sub_delay - 0.25; live.set_sub_delay(d); say(format!("Subtitle delay {:+.2} s", d)); poke(); }
            "." => { let d = now.sub_delay + 0.25; live.set_sub_delay(d); say(format!("Subtitle delay {:+.2} s", d)); poke(); }
            "b" | "B" if !now.chapters.is_empty() => sheet.set(if sheet() == Some(Sheet::Chapters) { None } else { Some(Sheet::Chapters) }),
            "z" | "Z" => {
                let next = Fit::ALL[(Fit::ALL.iter().position(|f| *f == now.fit).unwrap_or(0) + 1) % Fit::ALL.len()];
                live.set_fit(next); say(next.name().into()); poke();
            }
            "i" | "I" => sheet.set(if sheet() == Some(Sheet::Info) { None } else { Some(Sheet::Info) }),
            "h" | "H" | "?" => sheet.set(if sheet() == Some(Sheet::Keys) { None } else { Some(Sheet::Keys) }),
            "n" | "N" | "MediaTrackNext" if live.request.has_next => live.finish(Finish::Next),
            "[" => { say(speed((now.speed - 0.25).max(0.25))); poke(); }
            "]" => { say(speed((now.speed + 0.25).min(3.0))); poke(); }
            "\\" => { say(speed(1.0)); poke(); }
            "Escape" | "Backspace" if sheet().is_some() => sheet.set(None),
            "Escape" if fullscreen() => { fullscreen.set(false); crate::video::set_fullscreen(false); poke(); }
            "Escape" | "Backspace" | "q" | "Q" | "MediaStop" => live.finish(Finish::Close),
            _ => {}
        }
    });

    let lc = || live.0.clone();
    rsx! {
        div { class: if visible { "player" } else { "player calm" },
            onfocusin: move |_| poke(),
            // A tap also sends a synthetic mousemove first: on touch screens only the tap counts.
            // Throttle desktop mousemove to avoid reactive churn while the cursor glides.
            onmousemove: move |_| if !ANDROID {
                let now = std::time::Instant::now();
                let due = last_move.peek().is_none_or(|t| now.duration_since(t) >= std::time::Duration::from_millis(250));
                if due || !*shown.peek() {
                    last_move.set(Some(now));
                    poke();
                }
            },
            // The picture: a tap shows the controls; with them up, a click pauses (desktop) or hides them (touch).
            // On Android phones/tablets: double-tap left/right seeks ±10s, vertical drag left/right
            // adjusts brightness/volume, and long-press temporarily boosts to 2× speed.
            div { class: "player-surface",
                ontouchstart: { let live_ts = lc(); move |e: TouchEvent| {
                    #[cfg(target_os = "android")]
                    {
                        let touches = e.data.touches();
                        if let Some(t) = touches.first() {
                            let pt = t.client_coordinates();
                            let b = crate::android::video_get_brightness();
                            let v = crate::android::video_get_volume();
                            let seq = (*touch_start.peek()).map(|x| x.5 + 1).unwrap_or(1);
                            touch_start.set(Some((pt.x, pt.y, std::time::Instant::now(), b, v, seq)));
                            touch_drag.set(None);
                            let live_lp = live_ts.clone();
                            let mut say_lp = say;
                            spawn(async move {
                                backend::sleep(450).await;
                                let still = (*touch_start.peek()).is_some_and(|x| x.5 == seq);
                                if still && touch_drag.peek().is_none() && sheet.peek().is_none() {
                                    let prev = live_lp.state().speed;
                                    touch_boost.set(Some(prev));
                                    suppress_click.set(true);
                                    live_lp.set_speed(2.0);
                                    say_lp("2× speed".into());
                                }
                            });
                        }
                    }
                    #[cfg(not(target_os = "android"))]
                    let _ = (e, &live_ts);
                } },
                ontouchmove: move |e: TouchEvent| {
                    #[cfg(target_os = "android")]
                    {
                        let Some((sx, sy, _, sb, sv, _)) = *touch_start.peek() else { return };
                        let touches = e.data.touches();
                        let Some(t) = touches.first() else { return };
                        let pt = t.client_coordinates();
                        let dx = pt.x - sx;
                        let dy = sy - pt.y; // swipe up = positive
                        let cur_drag = *touch_drag.peek();
                        if cur_drag.is_none() {
                            if dy.abs() > 22.0 && dy.abs() > dx.abs() * 1.4 && touch_boost.peek().is_none() {
                                // Rough midpoint: left half controls brightness, right half volume.
                                let left = sx < 480.0;
                                touch_drag.set(Some(left));
                                suppress_click.set(true);
                            } else {
                                return;
                            }
                        }
                        if let Some(left) = *touch_drag.peek() {
                            let delta = (dy / 260.0) as f32;
                            if left {
                                let lvl = (sb + delta).clamp(0.05, 1.0);
                                let _ = crate::android::video_set_brightness(lvl);
                                say(format!("Brightness {:.0}%", lvl * 100.0));
                            } else {
                                let lvl = (sv + delta).clamp(0.0, 1.0);
                                let _ = crate::android::video_set_volume(lvl);
                                say(format!("Volume {:.0}%", lvl * 100.0));
                            }
                        }
                    }
                    #[cfg(not(target_os = "android"))]
                    let _ = e;
                },
                ontouchend: { let live_te = lc(); move |_e: TouchEvent| {
                    #[cfg(target_os = "android")]
                    {
                        if let Some(prev) = touch_boost.take() {
                            live_te.set_speed(prev);
                            say(if (prev - 1.0).abs() < 0.01 { "Normal speed".into() } else { format!("Speed {prev}×") });
                            touch_start.set(None);
                            touch_drag.set(None);
                            return;
                        }
                        let dragged = touch_drag.take().is_some();
                        let start = touch_start.take();
                        if !dragged {
                            if let Some((sx, _, _, _, _, _)) = start {
                                let now = std::time::Instant::now();
                                let prev_tap = *last_tap.peek();
                                if let Some((prev_t, prev_x)) = prev_tap {
                                    if now.duration_since(prev_t) < std::time::Duration::from_millis(320) && (sx - prev_x).abs() < 120.0 {
                                        last_tap.set(None);
                                        if sx < 320.0 {
                                            suppress_click.set(true);
                                            live_te.seek_by(-10.0);
                                            say("−10 s".into());
                                            return;
                                        } else if sx > 580.0 {
                                            suppress_click.set(true);
                                            live_te.seek_by(10.0);
                                            say("+10 s".into());
                                            return;
                                        }
                                    }
                                }
                                last_tap.set(Some((now, sx)));
                            }
                        }
                    }
                    #[cfg(not(target_os = "android"))]
                    let _ = &live_te;
                } },
                ontouchcancel: { let live_tc = lc(); move |_| {
                    #[cfg(target_os = "android")]
                    {
                        if let Some(prev) = touch_boost.take() { live_tc.set_speed(prev); }
                        touch_start.set(None);
                        touch_drag.set(None);
                    }
                    #[cfg(not(target_os = "android"))]
                    let _ = &live_tc;
                } },
                onclick: { let live = lc(); move |_| {
                    #[cfg(target_os = "android")]
                    if *suppress_click.peek() { suppress_click.set(false); return; }
                    if sheet().is_some() { sheet.set(None); }
                    else if !visible { poke(); }
                    else if ANDROID { shown.set(false); }
                    else { live.toggle_pause(); poke(); }
                } },
                ondoubleclick: move |_| if !ANDROID { let on = !fullscreen(); fullscreen.set(on); crate::video::set_fullscreen(on); },
            }
            if let Some(text) = caption {
                {
                    let sub_bottom = (107 - s.sub_pos).clamp(5, 35);
                    let cls = format!("player-caption{}{}", if visible { " lifted" } else { "" }, if s.sub_bg { " bg" } else { "" });
                    rsx! {
                        div { class: "{cls}", style: "--sub-scale: {s.sub_scale}; --sub-bottom: {sub_bottom}%", aria_live: "polite",
                            for line in text.lines().map(str::to_owned).collect::<Vec<_>>() { span { "{line}" } }
                        }
                    }
                }
            }
            if let Some((text, n)) = osd() { div { key: "{n}", class: "player-osd", role: "status", "{text}" } }
            // Floating "Up Next" auto-play countdown card during credits or end of episode.
            if up_next_active {
                div { class: if visible { "player-upnext lifted" } else { "player-upnext" }, role: "region", aria_label: "Up next",
                    span { class: "player-upnext-meta",
                        match up_next_secs() {
                            Some(n) => format!("Up next in {n} s"),
                            None => "Up next".into(),
                        }
                    }
                    span { class: "player-upnext-title display", "{req.next_label.as_deref().unwrap_or(\"Next episode\")}" }
                    div { class: "player-upnext-actions",
                        button { class: "pill accent", onclick: { let live = lc(); move |_| live.finish(Finish::Next) }, icons::Play {} span { "Play now" } }
                        button { class: "pill glass", onclick: move |_| { up_next_dismissed.set(true); up_next_secs.set(None); }, span { "Stay" } }
                    }
                }
            }
            // Over the picture even when the controls have faded, as long as the segment lasts.
            if let Some(g) = skippable {
                button { class: if visible { "pill glass player-skip lifted" } else { "pill glass player-skip" }, title: "Enter",
                    onclick: { let live = lc(); move |_| skip(&live, g) },
                    span { match g.kind { SegmentKind::Intro => "Skip intro", SegmentKind::Recap => "Skip recap", SegmentKind::Credits => "Next episode" } }
                    icons::Next {}
                }
            }
            if let Some(error) = s.error.clone() {
                div { class: "player-center",
                    div { class: "glass panel player-error", role: "alert",
                        span { class: "warn-icon big-icon", icons::Alert {} }
                        p { class: "body-strong", "{error}" }
                        p { class: "dim", "Another source may play; this one can also be tried again." }
                        div { class: "player-error-actions",
                            button { class: "pill accent", onclick: { let live = lc(); move |_| live.finish(Finish::Another) }, icons::Next {} span { "Try another source" } }
                            button { class: "pill glass", onclick: { let live = lc(); move |_| live.retry() }, icons::Refresh {} span { "Retry" } }
                            button { class: "pill glass", onclick: { let live = lc(); move |_| live.finish(Finish::Close) }, icons::Close {} span { "Close" } }
                        }
                    }
                }
            } else if s.buffering || !s.loaded {
                div { class: "player-center", aria_label: "Loading", div { class: "player-spinner" } }
            }

            div { class: "player-chrome", inert: (!visible || sheet() == Some(Sheet::Keys)).then_some("true"),
                onkeydown: move |e: KeyboardEvent| {
                    poke();
                    if matches!(e.key(), Key::Enter | Key::Tab) || e.key() == Key::Character(" ".into()) { e.stop_propagation(); }
                },
                div { class: "player-top",
                    button { class: "bare", aria_label: "Back (Esc)", title: "Back (Esc)", onclick: { let live = lc(); move |_| live.finish(Finish::Close) }, icons::ArrowLeft {} }
                    div { class: "player-heading",
                        span { class: "player-title display", "{req.title}" }
                        // One quiet line of facts: what is decoded, how, and what the panel shows.
                        span { class: "player-facts-line",
                            if !req.subtitle.is_empty() { span { "{req.subtitle}" } }
                            if !facts.resolution.is_empty() { span { "{facts.resolution}" } }
                            if !facts.size.is_empty() { span { class: "tnum", title: "File size", "{facts.size}" } }
                            if !facts.picture.is_empty() { span { class: if hdr && !mapped { "lit" } else { "" }, title: "{facts.picture}", "{short_picture}" } }
                            if s.loaded && !s.codec.is_empty() {
                                span { class: if hw.is_some() { "" } else { "warn" }, title: if hw.is_some() { "Hardware decoding" } else { "Software decoding" },
                                    "{hw.clone().unwrap_or_else(|| \"Software\".into())}" }
                            }
                        }
                    }
                    span { class: "topbar-spacer" }
                    if ANDROID && !crate::is_tv() {
                        button { class: "bare",
                            aria_label: if rotation_locked() { "Unlock orientation" } else { "Lock orientation" },
                            title: if rotation_locked() { "Unlock orientation" } else { "Lock orientation" },
                            aria_pressed: "{rotation_locked()}",
                            onclick: move |_| {
                                let next = !rotation_locked();
                                rotation_locked.set(next);
                                #[cfg(target_os = "android")]
                                let _ = crate::android::video_set_rotation_lock(next);
                                say(if next { "Rotation locked".into() } else { "Rotation unlocked".into() });
                            },
                            if rotation_locked() { icons::Lock {} } else { icons::Unlock {} }
                        }
                        button { class: "bare", aria_label: "Picture-in-picture", title: "Picture-in-picture",
                            onclick: move |_| {
                                #[cfg(target_os = "android")]
                                {
                                    shown.set(false);
                                    let st = state.peek();
                                    let ok = crate::android::video_enter_pip(st.width as i32, st.height as i32);
                                    if !ok { shown.set(true); say("Picture-in-picture isn't available".into()); }
                                }
                            },
                            icons::Pip {}
                        }
                    }
                    button { class: "bare", aria_label: "Player shortcuts (H / ?)", title: "Player shortcuts (H / ?)", aria_pressed: "{sheet() == Some(Sheet::Keys)}",
                        onclick: move |_| sheet.set(if sheet() == Some(Sheet::Keys) { None } else { Some(Sheet::Keys) }), icons::Keyboard {} }
                    button { class: "bare", aria_label: "Details (I)", title: "Details (I)", aria_pressed: "{sheet() == Some(Sheet::Info)}",
                        onclick: move |_| sheet.set(if sheet() == Some(Sheet::Info) { None } else { Some(Sheet::Info) }), icons::Info {} }
                }

                if s.loaded && s.error.is_none() {
                    div { class: "player-mid",
                        button { class: "bare", aria_label: "Back 10 seconds (←)", title: "Back 10 seconds (←)", onclick: { let live = lc(); move |_| { live.seek_by(-10.0); poke(); } }, icons::Rewind10 {} }
                        button { class: "bare player-big", aria_label: if s.paused { "Play (Space)" } else { "Pause (Space)" }, title: if s.paused { "Play (Space)" } else { "Pause (Space)" },
                            onclick: { let live = lc(); move |_| { live.toggle_pause(); poke(); } },
                            if s.paused { icons::Play {} } else { icons::Pause {} } }
                        button { class: "bare", aria_label: "Forward 10 seconds (→)", title: "Forward 10 seconds (→)", onclick: { let live = lc(); move |_| { live.seek_by(10.0); poke(); } }, icons::Forward10 {} }
                    }
                }

                div { class: "player-bottom",
                    span { class: "player-time tnum", "{clock(position)}" }
                    div { class: "scrubber-wrap",
                        input { class: "scrubber", r#type: "range", aria_label: "Position", min: "0", max: "{duration.max(1.0)}", step: "0.5",
                            value: "{position}", disabled: duration <= 0.0, style: "--played: {played}%; --buffered: {buffered}%",
                            oninput: move |e| { scrub.set(e.value().parse().ok()); poke(); },
                            onchange: { let live = lc(); move |e: FormEvent| { if let Ok(v) = e.value().parse::<f64>() { live.seek_to(v); } scrub.set(None); } },
                        }
                        if duration > 0.0 {
                            for ch in s.chapters.iter().filter(|c| c.start > 1.0 && c.start < duration - 1.0) {
                                span { key: "ch-{ch.start:.1}", class: "chapter-tick", style: "left: {pct(ch.start)}%", title: "{ch.title}" }
                            }
                        }
                    }
                    span { class: "player-time tnum dim", if duration > 0.0 { "-{clock(duration - position)}" } }
                    div { class: "player-actions",
                        if !s.chapters.is_empty() {
                            button { class: "bare", aria_label: "Chapters (B)", title: "Chapters (B)", aria_pressed: "{sheet() == Some(Sheet::Chapters)}",
                                onclick: move |_| sheet.set(if sheet() == Some(Sheet::Chapters) { None } else { Some(Sheet::Chapters) }), icons::List {} }
                        }
                        button { class: "bare", aria_label: "Audio tracks (A cycles them)", title: "Audio tracks (A cycles them)", aria_pressed: "{sheet() == Some(Sheet::Audio)}",
                            onclick: move |_| sheet.set(if sheet() == Some(Sheet::Audio) { None } else { Some(Sheet::Audio) }), icons::Audio {} }
                        button { class: "bare", aria_label: "Subtitles (S)", title: "Subtitles (S)", disabled: !has_subs, aria_pressed: "{sheet() == Some(Sheet::Subtitles)}",
                            onclick: move |_| sheet.set(if sheet() == Some(Sheet::Subtitles) { None } else { Some(Sheet::Subtitles) }), icons::Subtitles {} }
                        button { class: "bare", aria_label: "Speed ([ ])", title: "Speed ([ ])", aria_pressed: "{sheet() == Some(Sheet::Speed)}",
                            onclick: move |_| sheet.set(if sheet() == Some(Sheet::Speed) { None } else { Some(Sheet::Speed) }), icons::Gauge {} }
                        button { class: "bare", aria_label: "Picture (Z)", title: "Picture (Z)", aria_pressed: "{sheet() == Some(Sheet::Picture)}",
                            onclick: move |_| sheet.set(if sheet() == Some(Sheet::Picture) { None } else { Some(Sheet::Picture) }), icons::Resize {} }
                        button { class: "bare", aria_label: "GLSL Shaders", title: "GLSL Shaders", aria_pressed: "{sheet() == Some(Sheet::Shaders)}",
                            onclick: move |_| sheet.set(if sheet() == Some(Sheet::Shaders) { None } else { Some(Sheet::Shaders) }), icons::Sparkles {} }
                        button { class: "bare", aria_label: "Commands (input.conf)", title: "Commands (input.conf)", aria_pressed: "{sheet() == Some(Sheet::Commands)}",
                            onclick: move |_| sheet.set(if sheet() == Some(Sheet::Commands) { None } else { Some(Sheet::Commands) }), icons::Layers {} }
                        if !ANDROID {
                            button { class: "bare", aria_label: if s.muted { "Unmute (M)" } else { "Mute (M)" }, title: if s.muted { "Unmute (M)" } else { "Mute (M)" },
                                onclick: { let live = lc(); move |_| live.toggle_mute() }, if s.muted || s.volume <= 0.0 { icons::Muted {} } else { icons::Audio {} } }
                            input { class: "scrubber volume", r#type: "range", aria_label: "Volume", min: "0", max: "100", step: "1", value: "{s.volume.min(100.0)}",
                                style: "--played: {s.volume.min(100.0)}%; --buffered: 0%",
                                oninput: { let live = lc(); move |e: FormEvent| if let Ok(v) = e.value().parse::<f64>() { live.set_volume(v); } } }
                            button { class: "bare", aria_label: if fullscreen() { "Leave full screen (F)" } else { "Full screen (F)" }, title: if fullscreen() { "Leave full screen (F)" } else { "Full screen (F)" },
                                onclick: move |_| { let on = !fullscreen(); fullscreen.set(on); crate::video::set_fullscreen(on); }, icons::Expand {} }
                        }
                        if req.has_next {
                            button { class: "bare player-next", title: "Next episode (N)", onclick: { let live = lc(); move |_| live.finish(Finish::Next) }, span { "Next" } icons::Next {} }
                        }
                    }
                }
            }

            if let Some(which) = sheet() {
                section { class: "player-sheet glass", role: "dialog", aria_label: sheet_name(which), aria_modal: (which == Sheet::Keys).then_some("true"),
                    tabindex: (which == Sheet::Keys).then_some("-1"), onmounted: move |e| async move { if which == Sheet::Keys { let _ = e.set_focus(true).await; } },
                    onkeydown: move |e: KeyboardEvent| {
                        poke();
                        if matches!(e.key(), Key::Enter | Key::Tab) || e.key() == Key::Character(" ".into()) { e.stop_propagation(); }
                    },
                    div { class: "sheet-head",
                        span { class: "group-name", "{sheet_name(which)}" }
                        icons::IconButton { label: "Close (Esc)", class: "glass", onclick: move |_| sheet.set(None), icons::Close {} }
                    }
                    div { class: "player-options",
                        match which {
                            Sheet::Audio | Sheet::Subtitles => {
                                let audio = which == Sheet::Audio;
                                let tracks: Vec<engine::Track> = s.tracks.iter().filter(|t| t.audio == audio).cloned().collect();
                                let mk_sheet = media_key.clone();
                                rsx! {
                                    if audio {
                                        div { class: "player-stepper",
                                            span { class: "player-stepper-label", "Audio sync" }
                                            div { class: "player-stepper-controls",
                                                button { class: "player-step-btn", onclick: { let live = lc(); let d = s.audio_delay; move |_| live.set_audio_delay(d - 0.1) }, "−0.1s" }
                                                button { class: "player-step-btn player-step-val tnum", title: "Reset sync", onclick: { let live = lc(); move |_| live.set_audio_delay(0.0) }, "{s.audio_delay:+.2} s" }
                                                button { class: "player-step-btn", onclick: { let live = lc(); let d = s.audio_delay; move |_| live.set_audio_delay(d + 0.1) }, "+0.1s" }
                                            }
                                        }
                                    } else {
                                        div { class: "player-stepper",
                                            span { class: "player-stepper-label", "Subtitle sync" }
                                            div { class: "player-stepper-controls",
                                                button { class: "player-step-btn", onclick: { let live = lc(); let d = s.sub_delay; move |_| live.set_sub_delay(d - 0.25) }, "−0.25s" }
                                                button { class: "player-step-btn player-step-val tnum", title: "Reset sync", onclick: { let live = lc(); move |_| live.set_sub_delay(0.0) }, "{s.sub_delay:+.2} s" }
                                                button { class: "player-step-btn", onclick: { let live = lc(); let d = s.sub_delay; move |_| live.set_sub_delay(d + 0.25) }, "+0.25s" }
                                            }
                                        }
                                        div { class: "player-stepper",
                                            span { class: "player-stepper-label", "Size" }
                                            div { class: "player-stepper-controls",
                                                button { class: "player-step-btn", onclick: { let live = lc(); let v = s.sub_scale; move |_| live.set_sub_scale(v - 0.1) }, "−" }
                                                button { class: "player-step-btn player-step-val tnum", title: "Reset size", onclick: { let live = lc(); move |_| live.set_sub_scale(1.05) }, "{s.sub_scale * 100.0:.0}%" }
                                                button { class: "player-step-btn", onclick: { let live = lc(); let v = s.sub_scale; move |_| live.set_sub_scale(v + 0.1) }, "+" }
                                            }
                                        }
                                        div { class: "player-stepper",
                                            span { class: "player-stepper-label", "Position & box" }
                                            div { class: "player-stepper-controls",
                                                button { class: "player-step-btn", onclick: { let live = lc(); let v = s.sub_pos; move |_| live.set_sub_pos(v - 4) }, "Up" }
                                                button { class: "player-step-btn", onclick: { let live = lc(); let v = s.sub_pos; move |_| live.set_sub_pos(v + 4) }, "Down" }
                                                button { class: "player-step-btn", aria_pressed: "{s.sub_bg}", onclick: { let live = lc(); let bg = s.sub_bg; move |_| live.set_sub_bg(!bg) }, if s.sub_bg { "Box on" } else { "Box off" } }
                                            }
                                        }
                                    }
                                    if !audio && !tracks.is_empty() { p { class: "player-options-head", "In this video" } }
                                    if !audio {
                                        OptionRow { on: !sub_on, label: "Off", about: String::new(), pick: { let live = lc(); let mk = mk_sheet.clone(); move |_| {
                                            live.select(false, None);
                                            if let Some(mk) = &mk { backend::save_track_pref(mk, |p| { p.sub_off = true; p.sub_lang = None; }); }
                                            sheet.set(None);
                                        } } }
                                    }
                                    for t in tracks {
                                        OptionRow { key: "{t.id}", on: t.selected, label: t.label.clone(),
                                            about: if audio || !s.direct { String::new() } else if t.image { "Image subtitles: drawn by mpv, off the direct path".to_string() }
                                                else if t.styled { "Styled: full look on SDR, plain text over HDR".to_string() } else { String::new() },
                                            pick: { let live = lc(); let id = t.id; let lang = t.lang.clone(); let mk = mk_sheet.clone(); move |_| {
                                                live.select(audio, Some(id));
                                                if let Some(mk) = &mk {
                                                    let lang = lang.clone();
                                                    backend::save_track_pref(mk, |p| {
                                                        if audio { if !lang.is_empty() { p.audio_lang = Some(lang); } }
                                                        else { p.sub_off = false; if !lang.is_empty() { p.sub_lang = Some(lang); } }
                                                    });
                                                }
                                                sheet.set(None);
                                            } } }
                                    }
                                    if audio && s.tracks.iter().all(|t| !t.audio) { p { class: "dim player-empty", "No audio tracks." } }
                                    if !audio && wanted.is_some() {
                                        OnlineSubtitles { online: online(), query: sub_query, busy: sub_busy(), added: sub_added(),
                                            selected: s.tracks.iter().find(|t| !t.audio && t.selected).map(|t| t.id), pick: download }
                                    }
                                }
                            }
                            Sheet::Chapters => rsx! {
                                for (idx, ch) in s.chapters.iter().enumerate() {
                                    {
                                        let next_start = s.chapters.get(idx + 1).map(|c| c.start).unwrap_or(f64::INFINITY);
                                        let active = position >= ch.start && position < next_start;
                                        let start = ch.start;
                                        rsx! {
                                            OptionRow { key: "{idx}-{start:.1}", on: active, label: ch.title.clone(), about: clock(start),
                                                pick: { let live = lc(); move |_| { live.seek_to(start); sheet.set(None); } } }
                                        }
                                    }
                                }
                            },
                            Sheet::Speed => rsx! {
                                for v in SPEEDS {
                                    OptionRow { key: "{v}", on: (s.speed - v).abs() < 0.01, label: if v == 1.0 { "Normal".to_string() } else { format!("{v}×") }, about: String::new(),
                                        pick: { let live = lc(); move |_| { live.set_speed(v); sheet.set(None); } } }
                                }
                            },
                            Sheet::Picture => rsx! {
                                for f in Fit::ALL {
                                    OptionRow { key: "{f.name()}", on: s.fit == f, label: f.name().to_string(), about: f.about().to_string(),
                                        pick: { let live = lc(); move |_| live.set_fit(f) } }
                                }
                                if ANDROID {
                                    p { class: "player-options-head", "Output" }
                                    OptionRow { on: s.direct, label: "Direct to the display".to_string(), about: "MediaCodec straight to the screen: real HDR10, HLG and HDR10+, lowest power".to_string(),
                                        pick: { let live = lc(); move |_| live.set_direct(true) } }
                                    OptionRow { on: !s.direct, label: "Rendered by mpv".to_string(), about: "Full ASS and image subtitles; HDR is tone-mapped to SDR".to_string(),
                                        pick: { let live = lc(); move |_| live.set_direct(false) } }
                                }
                            },
                            Sheet::Shaders => {
                                let shaders = mpv_cfg.1.clone();
                                let shaders_dir = mpv_cfg.2.clone();
                                rsx! {
                                    if shaders.is_empty() {
                                        p { class: "dim player-empty", "Drop .glsl or .hook shaders into {shaders_dir}" }
                                    }
                                    for sh in shaders {
                                        {
                                            let path = sh.path.clone();
                                            let path_str = path.to_string_lossy().into_owned();
                                            let on = s.active_shaders.iter().any(|p| p == &path_str);
                                            let name = sh.name.clone();
                                            let about = sh.description.clone().unwrap_or_default();
                                            let live = lc();
                                            let mut say = say;
                                            rsx! {
                                                OptionRow { key: "{path_str}", on, label: name.clone(), about,
                                                    pick: move |_| {
                                                        match live.toggle_shader(&path) {
                                                            Ok(enabled) => say(format!("{name}: {}", if enabled { "on" } else { "off" })),
                                                            Err(e) => say(format!("{e}")),
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            },
                            Sheet::Commands => {
                                let bindings = mpv_cfg.0.clone();
                                let mut groups: Vec<(String, Vec<rstream_mpv_config::MenuAction>)> = Vec::new();
                                for b in bindings {
                                    if let Some((_, list)) = groups.iter_mut().find(|(g, _)| g == &b.action.group) {
                                        list.push(b.action);
                                    } else {
                                        groups.push((b.action.group.clone(), vec![b.action]));
                                    }
                                }
                                rsx! {
                                    if groups.is_empty() {
                                        p { class: "dim player-empty", "No input.conf commands defined yet. Edit input.conf in Settings → Player." }
                                    }
                                    for (group, actions) in groups {
                                        p { key: "grp-{group}", class: "player-options-head", "{group}" }
                                        for act in actions {
                                            {
                                                let cmd = act.command.clone();
                                                let lbl = act.label.clone();
                                                let hint = act.key_hint.clone();
                                                let live = lc();
                                                let mut say = say;
                                                rsx! {
                                                    button { key: "cmd-{act.id}", class: "player-option", role: "menuitem",
                                                        onclick: move |_| {
                                                            if is_cycle_fullscreen(&cmd) {
                                                                if !ANDROID {
                                                                    let on = !fullscreen();
                                                                    fullscreen.set(on);
                                                                    crate::video::set_fullscreen(on);
                                                                }
                                                            } else {
                                                                match live.run_mpv_command(&cmd) {
                                                                    Ok(()) => say(lbl.clone()),
                                                                    Err(e) => say(format!("{e}")),
                                                                }
                                                            }
                                                            sheet.set(None);
                                                        },
                                                        span { class: "player-option-text",
                                                            span { class: "player-option-label", "{act.label}" }
                                                            span { class: "player-option-about dim", "{act.command}" }
                                                        }
                                                        if !hint.is_empty() { kbd { class: "keycap", "{hint}" } }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            },
                            Sheet::Keys => rsx! {
                                div { class: "shortcut-list player-keys",
                                    for (k, what) in KEYS.into_iter().filter(|(k,_)| !(ANDROID && k == &["F"]) && (req.has_next || k != &["N"])) { div { class: "shortcut", span { class: "caps", for c in k.iter() { kbd { class: "keycap", "{c}" } } } span { "{what}" } } }
                                }
                            },
                            Sheet::Info => {
                                let rows = details(&l, &s, &facts, hw.as_deref());
                                rsx! {
                                    dl { class: "player-facts",
                                        for (k, v) in rows { div { key: "{k}", dt { "{k}" } dd { "{v}" } } }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}


fn sheet_name(s: Sheet) -> &'static str {
    match s {
        Sheet::Audio => "Audio",
        Sheet::Subtitles => "Subtitles",
        Sheet::Chapters => "Chapters",
        Sheet::Speed => "Speed",
        Sheet::Picture => "Picture",
        Sheet::Shaders => "Shaders",
        Sheet::Commands => "Commands",
        Sheet::Info => "Details",
        Sheet::Keys => "Shortcuts",
    }
}

fn is_cycle_fullscreen(command: &str) -> bool {
    let mut parts = command.split_whitespace();
    parts.next().is_some_and(|a| a.eq_ignore_ascii_case("cycle"))
        && parts.next().is_some_and(|b| b.eq_ignore_ascii_case("fullscreen"))
        && parts.next().is_none()
}

fn normalize_player_key(raw: &str) -> rstream_mpv_config::KeyBind {
    let mut b = rstream_mpv_config::parse_key_bind(raw);
    b.key = match b.key.as_str() {
        " " => "SPACE".into(),
        "ARROWLEFT" => "LEFT".into(),
        "ARROWRIGHT" => "RIGHT".into(),
        "ARROWUP" => "UP".into(),
        "ARROWDOWN" => "DOWN".into(),
        "ESCAPE" => "ESC".into(),
        "BACKSPACE" => "BS".into(),
        "PAGEUP" => "PGUP".into(),
        "PAGEDOWN" => "PGDWN".into(),
        _ => return b,
    };
    b
}

fn match_input_binding<'a>(bindings: &'a [rstream_mpv_config::InputBinding], raw: &str) -> Option<&'a rstream_mpv_config::InputBinding> {
    let k = normalize_player_key(raw);
    bindings.iter().find(|b| {
        b.bind.key == k.key && b.bind.ctrl == k.ctrl && b.bind.alt == k.alt && b.bind.shift == k.shift && b.bind.meta == k.meta
    })
}


/// "Download": the add-ons' subtitles for this video, narrowed by language as you type.
#[component]
fn OnlineSubtitles(online: Online, query: Signal<String>, busy: Option<String>, added: std::collections::HashMap<String, i64>,
                   selected: Option<i64>, pick: EventHandler<OnlineSubtitle>) -> Element {
    let q = query();
    let list = match &online { Online::Ready(list) => list.clone(), _ => Vec::new() };
    // The languages on offer, in the list's order (preferred first), as quick filters.
    let mut languages: Vec<String> = Vec::new();
    for s in &list { if !languages.contains(&s.language) { languages.push(s.language.clone()); } }
    let matching: Vec<OnlineSubtitle> = list.iter().filter(|s| backend::subtitle_matches(s, &q)).cloned().collect();
    let more = matching.len().saturating_sub(ONLINE_SHOWN);
    // "Italian 1", "Italian 2": the add-ons give no other name worth showing.
    let mut counts = std::collections::HashMap::<String, usize>::new();
    let rows: Vec<(OnlineSubtitle, usize)> = matching.into_iter().take(ONLINE_SHOWN)
        .map(|s| { let n = counts.entry(s.language.clone()).or_default(); *n += 1; let n = *n; (s, n) }).collect();
    rsx! {
        p { class: "player-options-head", "Download" }
        div { class: "sub-search",
            span { class: "sub-search-icon", aria_hidden: "true", icons::Search {} }
            input { r#type: "search", placeholder: "Search by language", aria_label: "Search subtitles by language", value: "{q}",
                autocomplete: "off", spellcheck: "false",
                oninput: move |e| query.set(e.value()),
                // Typing stays in the field; Esc still closes the sheet.
                onkeydown: move |e: KeyboardEvent| if e.key() != Key::Escape { e.stop_propagation(); },
                onkeyup: move |e: KeyboardEvent| e.stop_propagation(),
            }
            if !q.is_empty() { button { class: "bare sub-search-clear", aria_label: "Clear", onclick: move |_| query.set(String::new()), icons::Close {} } }
        }
        if languages.len() > 1 {
            div { class: "sub-langs", role: "group", aria_label: "Languages",
                for l in languages.into_iter().take(10) {
                    button { key: "{l}", class: if q.eq_ignore_ascii_case(&l) { "sub-lang on" } else { "sub-lang" }, aria_pressed: "{q.eq_ignore_ascii_case(&l)}",
                        onclick: { let l = l.clone(); move |_| { let same = query.peek().eq_ignore_ascii_case(&l); query.set(if same { String::new() } else { l.clone() }); } },
                        "{l}" }
                }
            }
        }
        match online {
            Online::Idle | Online::Loading => rsx! { div { class: "player-empty sub-loading dim", span { class: "spinner", aria_hidden: "true" } span { "Looking for subtitles" } } },
            Online::Failed => rsx! { p { class: "dim player-empty", "Subtitles couldn’t be found right now." } },
            Online::Ready(all) if all.is_empty() => rsx! { p { class: "dim player-empty", "No add-on has subtitles for this video." } },
            Online::Ready(_) => rsx! {
                if rows.is_empty() { p { class: "dim player-empty", "No subtitles in “{q}”." } }
                for (sub, n) in rows {
                    {
                        let track = added.get(&sub.id).copied();
                        let loading = busy.as_deref() == Some(sub.id.as_str());
                        let about = if loading { "Downloading…".to_string() } else if track.is_some() { "Downloaded".to_string() } else { sub.release.clone().unwrap_or_else(|| sub.code.clone()) };
                        rsx! {
                            button { key: "{sub.id}", class: if track.is_some() && track == selected { "player-option on" } else { "player-option" },
                                role: "menuitemradio", aria_checked: "{track.is_some() && track == selected}", disabled: busy.is_some() && !loading,
                                onclick: { let sub = sub.clone(); move |_| pick.call(sub.clone()) },
                                span { class: "player-option-text",
                                    span { class: "player-option-label", "{sub.language} {n}" }
                                    span { class: "player-option-about dim sub-release", title: "{about}", "{about}" }
                                }
                                if loading { span { class: "spinner", aria_hidden: "true" } }
                                else if track.is_some() && track == selected { span { class: "player-option-check", icons::Check {} } }
                                else if track.is_none() { span { class: "player-option-check dim", icons::Download {} } }
                            }
                        }
                    }
                }
                if more > 0 { p { class: "dim player-empty", "{more} more: type a language to narrow the list." } }
            },
        }
    }
}

/// The player before a source is found: it opens with the tap, and Back stops the search.
#[component]
pub fn PendingPlayer(title: String, step: String) -> Element {
    let mut shell = use_context::<Shell>();
    rsx! {
        div { class: "player pending", role: "dialog", aria_label: "{title}",
            div { class: "player-top", onkeydown: move |e: KeyboardEvent| if matches!(e.key(), Key::Enter | Key::Tab) || e.key() == Key::Character(" ".into()) { e.stop_propagation(); },
                button { class: "bare", aria_label: "Back (Esc)", title: "Back (Esc)", onclick: move |_| crate::cancel_finding(shell), icons::ArrowLeft {} }
                div { class: "player-heading",
                    span { class: "player-title display", "{title}" }
                    span { class: "player-facts-line", span { "{step}" } }
                }
                span { class: "topbar-spacer" }
                button { class: "bare", aria_label: "Page shortcuts (?)", title: "Page shortcuts (?)", onclick: move |_| shell.shortcuts_open.set(true), icons::Keyboard {} }
            }
            div { class: "player-center", role: "status", aria_label: "{step}",
                div { class: "player-pending-note", div { class: "player-spinner" } span { class: "dim", "{step}" } }
            }
        }
    }
}

#[component]
fn OptionRow(on: bool, label: String, about: String, pick: EventHandler<MouseEvent>) -> Element {
    rsx! {
        button { class: if on { "player-option on" } else { "player-option" }, role: "menuitemradio", aria_checked: "{on}", onclick: move |e| pick.call(e),
            span { class: "player-option-text",
                span { class: "player-option-label", "{label}" }
                if !about.is_empty() { span { class: "player-option-about dim", "{about}" } }
            }
            if on { span { class: "player-option-check", icons::Check {} } }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_dom_keys_and_matches_mpv_bindings() {
        let conf = "SPACE cycle pause\nShift+RIGHT seek 60\nS screenshot\nCtrl++ add audio-delay 0.1\nf cycle fullscreen\n";
        let bindings = rstream_mpv_config::parse_input_conf(conf, std::path::Path::new("/tmp/mpv"));
        assert_eq!(match_input_binding(&bindings, " ").unwrap().action.command, "cycle pause");
        assert_eq!(match_input_binding(&bindings, "Shift+ArrowRight").unwrap().action.command, "seek 60");
        assert_eq!(match_input_binding(&bindings, "S").unwrap().action.command, "screenshot");
        assert_eq!(match_input_binding(&bindings, "Ctrl++").unwrap().action.command, "add audio-delay 0.1");
        assert!(is_cycle_fullscreen(&match_input_binding(&bindings, "f").unwrap().action.command));
        assert!(!is_cycle_fullscreen("set fullscreen yes"));
    }
}
