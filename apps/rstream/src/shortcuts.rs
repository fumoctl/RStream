//! The shortcuts sheet (?): only the keys that apply where you are, then the app's navigation.
use crate::{icons, Shell};
use dioxus::prelude::*;

/// The same discoverable control in the top bar and Search; player uses its own sheet.
#[component]
pub fn ShortcutButton() -> Element {
    let mut shell = use_context::<Shell>();
    rsx! { icons::IconButton { label: "Page shortcuts (?)", class: "glass", onclick: move |_| shell.shortcuts_open.set(true), icons::Keyboard {} } }
}

/// Only commands that apply here, followed by shared app navigation.
#[component]
pub fn AppShortcuts() -> Element {
    let mut shell = use_context::<Shell>();
    let mut rows: Vec<(&[&str], &str)> = Vec::new();
    let context = if shell.pending().is_some() {
        rows.push((&["Esc"], "Cancel opening")); "Opening"
    } else if let Some(t) = (shell.trailer)() {
        rows.extend([(&["Space", "K"][..], "Play or pause"), (&["J", "L"][..], "10 seconds back or on"), (&["M"][..], "Mute or unmute"), (&["F"][..], "Full screen")]);
        if t.clips.len() > 1 { rows.extend([(&["←", "→"][..], "Previous or next video"), (&["1–9"][..], "Jump to a video")]); }
        rows.extend([(&["Y"][..], "Open on YouTube"), (&["Esc"][..], "Close the videos")]); "Videos"
    } else if (shell.search_open)() {
        rows.extend([(&["Enter"][..], "Search"), (&["Esc"][..], "Close search")]); "Search"
    } else if let Some(media) = shell.opened.read().as_ref() {
        rows.extend([(&["P"][..], "Play or resume"), (&["S"][..], "Save"), (&["T"][..], "Trailer")]);
        if media.kind == "movie" { rows.extend([(&["W"][..], "Watched"), (&["O"][..], "Choose source")]); }
        rows.push((&["Esc"], "Back")); "Title"
    } else {
        (shell.tab)().title()
    };
    if shell.pending().is_none() {
        rows.extend([(&["Ctrl", "K"][..], "Search"), (&["Ctrl", "1–3"][..], "Home / Library / Settings")]);
        if !(shell.search_open)() { rows.push((&["/"], "Search")); }
    }
    rows.extend([(&["Tab"][..], "Next control; Shift+Tab goes back"), (&["Enter", "Space"][..], "Activate a focused button"), (&["?"][..], "Show or hide shortcuts")]);
    rsx! {
        div { class: "scrim shortcuts-scrim", onclick: move |_| shell.shortcuts_open.set(false),
            section { class: "shortcuts glass", role: "dialog", aria_modal: "true", aria_label: "{context} shortcuts", tabindex: "-1", onmounted: move |e| async move { let _ = e.set_focus(true).await; }, onclick: move |e| e.stop_propagation(),
                div { class: "sheet-head",
                    span { class: "group-icon", icons::Keyboard {} }
                    span { class: "group-name", "{context} shortcuts" }
                    icons::IconButton { label: "Close (Esc)", class: "glass", onclick: move |_| shell.shortcuts_open.set(false), icons::Close {} }
                }
                div { class: "shortcut-list",
                    for (keys, what) in rows {
                        div { class: "shortcut",
                            span { class: "caps", for k in keys.iter() { kbd { class: "keycap", "{k}" } } }
                            span { "{what}" }
                        }
                    }
                }
            }
        }
    }
}
