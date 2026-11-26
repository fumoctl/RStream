#![allow(unused_braces)] // one-line #[component] bodies trip this lint inside the macro
//! Inline 24px stroke icons; they inherit currentColor.
use dioxus::prelude::*;

#[component]
fn Glyph(d: &'static str) -> Element {
    rsx! {
        svg { class: "icon", view_box: "0 0 24 24", width: "20", height: "20", fill: "none", stroke: "currentColor",
            stroke_width: "1.8", stroke_linecap: "round", stroke_linejoin: "round", "aria-hidden": "true",
            path { d } }
    }
}

#[component] pub fn Search() -> Element { rsx! { Glyph { d: "M11 18a7 7 0 1 0 0-14 7 7 0 0 0 0 14Zm5-2 4.5 4.5" } } }
#[component] pub fn Close() -> Element { rsx! { Glyph { d: "M6 6l12 12M18 6 6 18" } } }
#[component] pub fn Download() -> Element { rsx! { Glyph { d: "M12 4v11m-5-5 5 5 5-5M5 20h14" } } }
#[component] pub fn History() -> Element { rsx! { Glyph { d: "M3.5 12a8.5 8.5 0 1 0 2.5-6M3.5 4v4h4M12 8v4l3 2" } } }
#[component] pub fn ThumbUp() -> Element { rsx! { Glyph { d: "M7 10v10H4V10h3Zm0 0 4-7c1.4 0 2.3 1 2 2.4L12.4 9H18a2 2 0 0 1 2 2.3l-1.2 7A2 2 0 0 1 16.8 20H7" } } }
#[component] pub fn ThumbDown() -> Element { rsx! { Glyph { d: "M17 14V4h3v10h-3Zm0 0-4 7c-1.4 0-2.3-1-2-2.4l.6-3.6H6a2 2 0 0 1-2-2.3l1.2-7A2 2 0 0 1 7.2 4H17" } } }
#[component] pub fn Chevron() -> Element { rsx! { Glyph { d: "m7 10 5 5 5-5" } } }
#[component] pub fn Play() -> Element { rsx! { Glyph { d: "M8 5.5v13l10.5-6.5L8 5.5Z" } } }
#[component] pub fn Check() -> Element { rsx! { Glyph { d: "m5 12.5 4.5 4.5L19 7.5" } } }
#[component] pub fn PlayCircle() -> Element { rsx! { Glyph { d: "M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18Zm-2-12.5v7l5.5-3.5L10 8.5Z" } } }
#[component] pub fn Refresh() -> Element { rsx! { Glyph { d: "M20 11a8 8 0 1 0-2.3 5.7M20 4v7h-7" } } }
#[component] pub fn House() -> Element { rsx! { Glyph { d: "M3.5 10.5 12 3.5l8.5 7V20a.5.5 0 0 1-.5.5h-5v-6h-6v6H4a.5.5 0 0 1-.5-.5v-9.5Z" } } }
#[component] pub fn Sparkles() -> Element { rsx! { Glyph { d: "M11 3.5l1.7 4.8 4.8 1.7-4.8 1.7L11 16.5l-1.7-4.8L4.5 10l4.8-1.7L11 3.5Zm7 10 .8 2.2 2.2.8-2.2.8-.8 2.2-.8-2.2-2.2-.8 2.2-.8.8-2.2Z" } } }
#[component] pub fn Stack() -> Element { rsx! { Glyph { d: "M4.5 5h3v14h-3zM10.5 5h3v14h-3zM16 5.8l2.9-.8 3.6 13.5-2.9.8z" } } }
#[component] pub fn Sliders() -> Element { rsx! { Glyph { d: "M4 7h9m4 0h3M4 17h3m4 0h9M15 4.5v5M9 14.5v5" } } }
#[component] pub fn Grid() -> Element { rsx! { Glyph { d: "M4.5 4.5h6v6h-6zM13.5 4.5h6v6h-6zM4.5 13.5h6v6h-6zM13.5 13.5h6v6h-6z" } } }
#[component] pub fn Film() -> Element { rsx! { Glyph { d: "M4.5 4h15v16h-15zM8.5 4v16m7-16v16M4.5 8h4m-4 4h4m-4 4h4m7-8h4m-4 4h4m-4 4h4" } } }
#[component] pub fn Tv() -> Element { rsx! { Glyph { d: "M3.5 7.5h17v12h-17zM8.5 3.5l3.5 4 3.5-4" } } }
#[component] pub fn Blossom() -> Element { rsx! { Glyph { d: "M12 9.5a2.5 2.5 0 1 0 0 5 2.5 2.5 0 0 0 0-5Zm0 0c-2-3.5-1.2-6 0-6s2 2.5 0 6Zm2.4 1.7c2.4-3 5-3.3 5.4-2.1.4 1.2-1.6 2.8-5.4 2.1Zm-.9 2.8c3.8.4 5.2 2.6 4.3 3.4-.9.9-3.3 0-4.3-3.4Zm-3 0c-1 3.4-3.4 4.3-4.3 3.4-.9-.8.5-3 4.3-3.4Zm-.9-2.8C5.8 11.9 3.8 10.3 4.2 9.1c.4-1.2 3-.9 5.4 2.1Z" } } }
#[component] pub fn Tag() -> Element { rsx! { Glyph { d: "M3.5 12V4.5a1 1 0 0 1 1-1H12l8.5 8.5-8.5 8.5L3.5 12ZM8 8h.01" } } }
#[component] pub fn Calendar() -> Element { rsx! { Glyph { d: "M4.5 6h15v14h-15zM4.5 10h15M8.5 3.5v4m7-4v4" } } }
#[component] pub fn Info() -> Element { rsx! { Glyph { d: "M12 20.5a8.5 8.5 0 1 0 0-17 8.5 8.5 0 0 0 0 17ZM12 11v5.5M12 7.8h.01" } } }
#[component] pub fn Star() -> Element { rsx! { Glyph { d: "m12 3.5 2.6 5.3 5.9.9-4.3 4.1 1 5.8L12 16.9l-5.2 2.7 1-5.8-4.3-4.1 5.9-.9L12 3.5Z" } } }
#[component] pub fn Heart() -> Element { rsx! { Glyph { d: "M12 19.5s-7-4.3-8.6-8.6A4.6 4.6 0 0 1 12 7.6a4.6 4.6 0 0 1 8.6 3.3c-1.6 4.3-8.6 8.6-8.6 8.6Z" } } }
#[component] pub fn ArrowRight() -> Element { rsx! { Glyph { d: "M5 12h14m-6-6 6 6-6 6" } } }
#[component] pub fn Undo() -> Element { rsx! { Glyph { d: "M9 13.5 4.5 9 9 4.5M4.5 9H14a5.5 5.5 0 0 1 0 11h-3" } } }
#[component] pub fn Backspace() -> Element { rsx! { Glyph { d: "M20.5 5.5h-11l-6 6.5 6 6.5h11zM11.5 9.5l5 5m0-5-5 5" } } }
#[component] pub fn Trash() -> Element { rsx! { Glyph { d: "M4.5 7h15M9.5 7V4.5h5V7m-8 0 1 12.5h9l1-12.5" } } }
#[component] pub fn Alert() -> Element { rsx! { Glyph { d: "M12 4 21 19.5H3L12 4Zm0 6v4.5m0 2.5h.01" } } }
#[component] pub fn Cpu() -> Element { rsx! { Glyph { d: "M7.5 7.5h9v9h-9zM10 3.5v4m4-4v4m-4 9v4m4-4v4M3.5 10h4m-4 4h4m9-4h4m-4 4h4" } } }

/// Icon-only control: the label is both the tooltip and what screen readers say.
#[component]
pub fn IconButton(
    label: String,
    onclick: EventHandler<MouseEvent>,
    children: Element,
    #[props(default)] class: String,
    #[props(default)] pressed: Option<bool>,
    #[props(default)] disabled: bool,
) -> Element {
    rsx! {
        button {
            class: "orb {class}",
            aria_label: "{label}",
            title: "{label}",
            aria_pressed: pressed.map(|p| p.to_string()),
            disabled,
            onclick: move |e| onclick.call(e),
            {children}
        }
    }
}
#[component] pub fn Magnet() -> Element { rsx! { Glyph { d: "M6 4.5v7a6 6 0 0 0 12 0v-7h-4v7a2 2 0 0 1-4 0v-7H6Zm0 3.5h4m4 0h4" } } }
#[component] pub fn Link() -> Element { rsx! { Glyph { d: "M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1" } } }
#[component] pub fn ArrowLeft() -> Element { rsx! { Glyph { d: "M19 12H5m6-6-6 6 6 6" } } }
#[component] pub fn Bookmark() -> Element { rsx! { Glyph { d: "M6.5 3.5h11v17L12 16.5l-5.5 4v-17Z" } } }
#[component] pub fn Clock() -> Element { rsx! { Glyph { d: "M12 20.5a8.5 8.5 0 1 0 0-17 8.5 8.5 0 0 0 0 17ZM12 7.5V12l3 2" } } }
#[component] pub fn Clapper() -> Element { rsx! { Glyph { d: "M4 10h16v9.5H4zM4 10 3.3 6.6l15.5-3.2.7 3.4L4 10Zm4.5-4.8 2 3.6m3.5-4.6 2 3.6" } } }
#[component] pub fn Users() -> Element { rsx! { Glyph { d: "M9 11a3.5 3.5 0 1 0 0-7 3.5 3.5 0 0 0 0 7Zm-6 9a6 6 0 0 1 12 0M16 4.3a3.5 3.5 0 0 1 0 6.4M18 14.4A6 6 0 0 1 21 20" } } }
#[component] pub fn Pen() -> Element { rsx! { Glyph { d: "M4 20l4-1 11-11-3-3L5 16l-1 4Zm10-13 3 3" } } }
#[component] pub fn Globe() -> Element { rsx! { Glyph { d: "M12 20.5a8.5 8.5 0 1 0 0-17 8.5 8.5 0 0 0 0 17ZM3.5 12h17M12 3.5c2.4 2.6 3.5 5.4 3.5 8.5s-1.1 5.9-3.5 8.5c-2.4-2.6-3.5-5.4-3.5-8.5s1.1-5.9 3.5-8.5Z" } } }
#[component] pub fn Speech() -> Element { rsx! { Glyph { d: "M4 5.5h16v10H10l-4.5 3.5V15.5H4z" } } }
#[component] pub fn Trophy() -> Element { rsx! { Glyph { d: "M7.5 4h9v5a4.5 4.5 0 0 1-9 0V4Zm0 1.5h-3a3 3 0 0 0 3 4m9-4h3a3 3 0 0 1-3 4M12 13.5V17m-4 3.5h8M9.5 17h5v3.5h-5z" } } }
#[component] pub fn List() -> Element { rsx! { Glyph { d: "M8.5 6.5h12M8.5 12h12M8.5 17.5h12M4 6.5h.01M4 12h.01M4 17.5h.01" } } }
#[component] pub fn EyeOff() -> Element { rsx! { Glyph { d: "M3.5 12S6.5 6 12 6s8.5 6 8.5 6-3 6-8.5 6-8.5-6-8.5-6Zm8.5 2.5a2.5 2.5 0 1 0 0-5 2.5 2.5 0 0 0 0 5ZM4.5 19.5l15-15" } } }
#[component] pub fn Camera() -> Element { rsx! { Glyph { d: "M3.5 7.5h12v10h-12zM15.5 11l5-3v8l-5-3" } } }
#[component] pub fn Layers() -> Element { rsx! { Glyph { d: "M12 4 3.5 8.5 12 13l8.5-4.5L12 4Zm-8.5 8L12 16.5l8.5-4.5M3.5 15.5 12 20l8.5-4.5" } } }
#[component] pub fn Next() -> Element { rsx! { Glyph { d: "M5 5.5l9 6.5-9 6.5v-13ZM18.5 5.5v13" } } }
#[component] pub fn Pause() -> Element { rsx! { Glyph { d: "M8 5h2.5v14H8zM13.5 5H16v14h-2.5z" } } }
#[component] pub fn Rewind10() -> Element { rsx! { Glyph { d: "M4.5 12a7.5 7.5 0 1 0 2.2-5.3M4.5 4v4h4M10 10v5M13 10h2a1 1 0 0 1 1 1v3a1 1 0 0 1-1 1h-2v-5Z" } } }
#[component] pub fn Forward10() -> Element { rsx! { Glyph { d: "M19.5 12a7.5 7.5 0 1 1-2.2-5.3M19.5 4v4h-4M8.5 10v5M11.5 10h2a1 1 0 0 1 1 1v3a1 1 0 0 1-1 1h-2v-5Z" } } }
#[component] pub fn Audio() -> Element { rsx! { Glyph { d: "M4 9.5h3.5L12 5.5v13l-4.5-4H4zM15.5 9a4 4 0 0 1 0 6M18 6.5a7.5 7.5 0 0 1 0 11" } } }
#[component] pub fn Muted() -> Element { rsx! { Glyph { d: "M4 9.5h3.5L12 5.5v13l-4.5-4H4zM16 9.5l5 5m0-5-5 5" } } }
#[component] pub fn Subtitles() -> Element { rsx! { Glyph { d: "M3.5 5.5h17v13h-17zM7 12.5h3m2 0h5M7 15.5h6m2 0h2" } } }
#[component] pub fn Gauge() -> Element { rsx! { Glyph { d: "M12 20a8 8 0 1 1 8-8M12 12l4.5-3.5M4.5 12h1m13 0h1M12 4.5v1" } } }
#[component] pub fn Resize() -> Element { rsx! { Glyph { d: "M4 9V4h5M20 9V4h-5M4 15v5h5M20 15v5h-5M9 12h6M12 9v6" } } }
#[component] pub fn Expand() -> Element { rsx! { Glyph { d: "M4 9V4h5M15 4h5v5M20 15v5h-5M9 20H4v-5" } } }

#[component] pub fn Moon() -> Element { rsx! { Glyph { d: "M19.5 14.5A8 8 0 0 1 9.5 4.5a8 8 0 1 0 10 10Z" } } }
#[component] pub fn Keyboard() -> Element { rsx! { Glyph { d: "M3 6h18v12H3zM6 9h.01M9 9h.01M12 9h.01M15 9h.01M18 9h.01M6 12h.01M9 12h.01M12 12h.01M15 12h.01M18 12h.01M7 15h10" } } }
#[component] pub fn Lock() -> Element { rsx! { Glyph { d: "M6 11h12v9H6zM8.5 11V8a3.5 3.5 0 0 1 7 0v3" } } }
#[component] pub fn Unlock() -> Element { rsx! { Glyph { d: "M6 11h12v9H6zM8.5 11V8a3.5 3.5 0 0 1 6.8-1.2" } } }
#[component] pub fn Pip() -> Element { rsx! { Glyph { d: "M3.5 5.5h17v13h-17zM12.5 12h6v4.5h-6z" } } }
#[component] pub fn Copy() -> Element { rsx! { Glyph { d: "M9 9h10v11H9zM5 15H4.5A1.5 1.5 0 0 1 3 13.5v-8A1.5 1.5 0 0 1 4.5 4h8A1.5 1.5 0 0 1 14 5.5V6" } } }
#[component] pub fn Upload() -> Element { rsx! { Glyph { d: "M12 15V4m-5 5 5-5 5 5M5 20h14" } } }
#[component] pub fn Plus() -> Element { rsx! { Glyph { d: "M12 5v14M5 12h14" } } }
#[component] pub fn Eye() -> Element { rsx! { Glyph { d: "M3.5 12S6.5 6 12 6s8.5 6 8.5 6-3 6-8.5 6-8.5-6-8.5-6Zm8.5 2.5a2.5 2.5 0 1 0 0-5 2.5 2.5 0 0 0 0 5Z" } } }
#[component] pub fn Clipboard() -> Element { rsx! { Glyph { d: "M9 4.5h6v3H9zM9 6H6.5A1.5 1.5 0 0 0 5 7.5v11A1.5 1.5 0 0 0 6.5 20h11a1.5 1.5 0 0 0 1.5-1.5v-11A1.5 1.5 0 0 0 17.5 6H15" } } }
#[component] pub fn External() -> Element { rsx! { Glyph { d: "M14 4h6v6M20 4l-8.5 8.5M11 6H6a2 2 0 0 0-2 2v10a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2v-5" } } }
#[component] pub fn Radar() -> Element { rsx! { Glyph { d: "M12 20a8 8 0 1 0 0-16 8 8 0 0 0 0 16Zm0-3.5a4.5 4.5 0 1 0 0-9 4.5 4.5 0 0 0 0 9Zm0-3.5a1 1 0 1 0 0-2 1 1 0 0 0 0 2Z" } } }
#[component] pub fn StopCircle() -> Element { rsx! { Glyph { d: "M12 20.5a8.5 8.5 0 1 0 0-17 8.5 8.5 0 0 0 0 17ZM9.5 9.5h5v5h-5z" } } }
