use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::mpv_conf::{expand_mpv_paths, is_protected_key, normalize_key, split_unquoted_comment};

const ALLOWED_COMMAND_VERBS: &[&str] = &[
    "cycle",
    "cycle-values",
    "add",
    "set",
    "multiply",
    "seek",
    "frame-step",
    "frame-back-step",
    "change-list",
    "screenshot",
    "sub-step",
    "sub-seek",
    "show-text",
];

const COMMAND_PREFIXES: &[&str] = &[
    "no-osd",
    "osd-auto",
    "osd-bar",
    "osd-msg",
    "osd-msg-bar",
    "raw",
    "expand-properties",
    "repeatable",
    "async",
    "sync",
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyBind {
    pub raw_key: String,
    pub key: String,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MenuAction {
    pub id: usize,
    pub key_hint: String,
    pub label: String,
    pub group: String,
    pub command: String,
    pub touches_shaders: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputBinding {
    pub bind: KeyBind,
    pub action: MenuAction,
}

fn is_mouse_key(base_key: &str) -> bool {
    let upper = base_key.to_ascii_uppercase();
    upper.starts_with("MOUSE_BTN") || upper.starts_with("MBTN_") || upper.starts_with("WHEEL_")
}

pub fn parse_key_bind(raw_key: &str) -> KeyBind {
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut meta = false;

    // Handle trailing `+` key (e.g. `+` or `Ctrl++`).
    let (mod_part, base_raw) = if raw_key == "+" {
        ("", "+")
    } else if let Some(prefix) = raw_key.strip_suffix("++") {
        (prefix, "+")
    } else if let Some((mods, base)) = raw_key.rsplit_once('+') {
        (mods, base)
    } else {
        ("", raw_key)
    };

    if !mod_part.is_empty() {
        for part in mod_part.split('+') {
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => ctrl = true,
                "alt" | "option" => alt = true,
                "shift" => shift = true,
                "meta" | "super" | "cmd" | "win" => meta = true,
                _ => {}
            }
        }
    }

    let key = if base_raw.chars().count() == 1 {
        let ch = base_raw.chars().next().unwrap_or_default();
        if ch.is_uppercase() {
            shift = true;
            ch.to_lowercase().to_string()
        } else {
            base_raw.to_string()
        }
    } else {
        base_raw.to_ascii_uppercase()
    };

    KeyBind {
        raw_key: raw_key.to_string(),
        key,
        ctrl,
        alt,
        shift,
        meta,
    }
}

fn split_unquoted_semicolons(cmd: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for (idx, ch) in cmd.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_double => escaped = true,
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            ';' if !in_single && !in_double => {
                parts.push(cmd[start..idx].trim());
                start = idx + 1;
            }
            _ => {}
        }
    }
    parts.push(cmd[start..].trim());
    parts
}

fn is_safe_mpv_command(command: &str) -> bool {
    if command.contains(['\0', '\r', '\n']) {
        return false;
    }
    let subcommands = split_unquoted_semicolons(command);
    if subcommands.is_empty() || subcommands.iter().any(|s| s.is_empty()) {
        return false;
    }
    for sub in subcommands {
        let mut tokens = sub.split_whitespace();
        let verb = loop {
            let Some(tok) = tokens.next() else {
                return false;
            };
            let lower = tok.to_ascii_lowercase();
            if !COMMAND_PREFIXES.contains(&lower.as_str()) {
                break lower;
            }
        };
        if !ALLOWED_COMMAND_VERBS.contains(&verb.as_str()) {
            return false;
        }
        if matches!(
            verb.as_str(),
            "set" | "change-list" | "cycle" | "cycle-values" | "add" | "multiply"
        ) {
            let Some(raw_prop) = tokens.next() else {
                return false;
            };
            let prop = normalize_key(raw_prop);
            if prop.is_empty() || is_protected_key(&prop) {
                return false;
            }
        }
    }
    true
}

fn infer_group(command: &str) -> &'static str {
    let lower = command.to_ascii_lowercase();
    if lower.contains("glsl-shader") {
        "Shaders"
    } else if lower.contains("sub-") || lower.contains("sub ") {
        "Subtitles"
    } else if lower.contains("audio") || lower.contains("volume") || lower.contains("mute") {
        "Audio"
    } else if lower.contains("deband")
        || lower.contains("panscan")
        || lower.contains("aspect")
        || lower.contains("screenshot")
        || lower.contains("fullscreen")
        || lower.contains("video-")
    {
        "Video"
    } else if lower.contains("pause")
        || lower.contains("seek")
        || lower.contains("speed")
        || lower.contains("frame-")
    {
        "Playback"
    } else {
        "General"
    }
}

fn title_case_prop(prop: &str) -> String {
    prop.split(['-', '_'])
        .filter(|s| !s.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn basename_from_arg(arg: &str) -> &str {
    let trimmed = arg.trim().trim_matches('"').trim_matches('\'');
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(trimmed)
}

fn humanize_command(command: &str) -> (String, String) {
    let group = infer_group(command).to_string();
    let parts: Vec<&str> = command.split_whitespace().collect();
    let label = match parts.as_slice() {
        ["cycle", prop] => format!("Toggle {}", title_case_prop(prop)),
        ["cycle-values", prop, ..] => format!("Cycle {}", title_case_prop(prop)),
        ["change-list", opt, "toggle", path] if opt.contains("glsl-shader") => {
            format!("Toggle Shader: {}", basename_from_arg(path))
        }
        ["change-list", opt, "append", path] if opt.contains("glsl-shader") => {
            format!("Add Shader: {}", basename_from_arg(path))
        }
        ["change-list", opt, "remove", path] if opt.contains("glsl-shader") => {
            format!("Remove Shader: {}", basename_from_arg(path))
        }
        ["change-list", opt, "clr", ..] if opt.contains("glsl-shader") => {
            "Clear All Shaders".to_string()
        }
        ["add", prop, delta] => format!("{} {}", title_case_prop(prop), delta),
        ["set", prop, val] => format!("Set {} {}", title_case_prop(prop), val),
        ["multiply", prop, factor] => format!("Multiply {} x{}", title_case_prop(prop), factor),
        ["seek", secs, ..] => format!("Seek {secs}s"),
        ["frame-step"] => "Frame Step".to_string(),
        ["frame-back-step"] => "Frame Back Step".to_string(),
        ["screenshot", ..] => "Screenshot".to_string(),
        [cmd, rest @ ..] => {
            if rest.is_empty() {
                title_case_prop(cmd)
            } else {
                format!("{} {}", title_case_prop(cmd), rest.join(" "))
            }
        }
        [] => String::new(),
    };
    (group, label)
}

fn parse_label_and_group(comment: Option<&str>, command: &str) -> (String, String) {
    if let Some(raw_comment) = comment {
        let trimmed = raw_comment.trim();
        if let Some(menu_spec) = trimmed
            .strip_prefix("menu:")
            .or_else(|| trimmed.strip_prefix("MENU:"))
        {
            let spec = menu_spec.trim();
            if let Some((grp, lbl)) = spec.split_once('>') {
                let grp = grp.trim();
                let lbl = lbl.trim();
                if !grp.is_empty() && !lbl.is_empty() {
                    return (grp.to_string(), lbl.to_string());
                }
            }
            if !spec.is_empty() {
                return (infer_group(command).to_string(), spec.to_string());
            }
        } else if !trimmed.is_empty() {
            return (infer_group(command).to_string(), trimmed.to_string());
        }
    }
    humanize_command(command)
}

pub fn parse_input_conf(text: &str, mpv_dir: &Path) -> Vec<InputBinding> {
    let mut out = Vec::new();

    for raw_line in text.lines() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let (code_part, comment) = split_unquoted_comment(trimmed);
        let code_part = code_part.trim();
        let Some(ws_idx) = code_part.find(char::is_whitespace) else {
            continue;
        };

        let raw_key = code_part[..ws_idx].trim();
        let raw_cmd = code_part[ws_idx..].trim();
        if raw_key.is_empty() || raw_cmd.is_empty() {
            continue;
        }

        if raw_cmd.split_whitespace().next() == Some("ignore") || !is_safe_mpv_command(raw_cmd) {
            continue;
        }

        let bind = parse_key_bind(raw_key);
        if is_mouse_key(&bind.key) {
            continue;
        }

        let command = expand_mpv_paths(raw_cmd, mpv_dir);
        let (group, label) = parse_label_and_group(comment, &command);
        let touches_shaders = command.to_ascii_lowercase().contains("glsl-shader");

        let id = out.len();
        out.push(InputBinding {
            bind,
            action: MenuAction {
                id,
                key_hint: raw_key.to_string(),
                label,
                group,
                command,
                touches_shaders,
            },
        });
    }

    out
}
