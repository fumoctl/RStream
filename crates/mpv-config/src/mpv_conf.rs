use std::path::Path;

use serde::{Deserialize, Serialize};

pub(crate) const PROTECTED_KEYS: &[&str] = &[
    "vo",
    "wid",
    "force-window",
    "gpu-api",
    "gpu-context",
    "osc",
    "config",
    "config-dir",
    "input-default-bindings",
    "input-vo-keyboard",
    "terminal",
    "idle",
    "include",
    "log-file",
    "msg-level",
    "tls-verify",
    "tls-ca-file",
    "ytdl",
    "script",
    "scripts",
    "load-scripts",
    "http-proxy",
    "network-proxy",
    "stream-lavf-o",
];

const OPTION_ACTION_SUFFIXES: &[&str] = &[
    "-append",
    "-add",
    "-set",
    "-pre",
    "-post",
    "-remove",
    "-del",
    "-clr",
    "-toggle",
];

const PATH_BEARING_OPTIONS: &[&str] = &[
    "glsl-shaders",
    "glsl-shader",
    "glsl-shaders-append",
    "glsl-shaders-remove",
    "glsl-shaders-set",
    "glsl-shaders-pre",
    "glsl-shaders-post",
    "sub-font-provider",
    "sub-file",
    "sub-files",
    "sub-file-paths",
    "sub-fonts-dir",
    "osd-fonts-dir",
    "audio-file",
    "audio-files",
    "audio-file-paths",
    "icc-profile",
    "lut",
    "screenshot-directory",
    "script-opts",
];

const SHADER_OPTIONS: &[&str] = &[
    "glsl-shaders",
    "glsl-shader",
    "glsl-shaders-append",
    "glsl-shaders-set",
    "glsl-shaders-pre",
    "glsl-shaders-post",
];

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedMpvConf {
    pub options: Vec<(String, String)>,
    pub skipped: Vec<String>,
    pub has_shaders: bool,
}

pub(crate) fn normalize_key(raw_key: &str) -> String {
    let trimmed = raw_key
        .trim()
        .trim_matches('"')
        .trim_matches('\'');
    let without_dashes = trimmed.strip_prefix("--").unwrap_or(trimmed);
    let normalized = without_dashes.to_ascii_lowercase().replace('_', "-");
    let stripped = normalized
        .strip_prefix("options/")
        .or_else(|| normalized.strip_prefix("file-local-options/"))
        .unwrap_or(&normalized);
    stripped.to_string()
}

pub(crate) fn is_protected_key(normalized_key: &str) -> bool {
    let base = normalized_key.strip_prefix("no-").unwrap_or(normalized_key);
    if PROTECTED_KEYS.contains(&base) {
        return true;
    }
    OPTION_ACTION_SUFFIXES.iter().any(|suffix| {
        base.strip_suffix(suffix)
            .is_some_and(|stem| PROTECTED_KEYS.contains(&stem))
    })
}

pub(crate) fn expand_mpv_paths(value: &str, mpv_dir: &Path) -> String {
    let dir_str = mpv_dir.to_string_lossy();
    let prefix = format!("{}/", dir_str.trim_end_matches(['/', '\\']));
    value.replace("~~desktop/", &prefix).replace("~~/", &prefix)
}

/// Strip an unquoted trailing `# ...` comment from a line.
/// Single-quoted strings are literal in mpv (`\` does not escape `'`).
pub(crate) fn split_unquoted_comment(line: &str) -> (&str, Option<&str>) {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for (idx, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_double => escaped = true,
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '#' if !in_single && !in_double => {
                return (line[..idx].trim_end(), Some(line[idx + 1..].trim()));
            }
            _ => {}
        }
    }
    (line.trim_end(), None)
}

fn unquote(value: &str) -> &str {
    let v = value.trim();
    if v.len() >= 2
        && ((v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\'')))
    {
        &v[1..v.len() - 1]
    } else {
        v
    }
}

fn parse_option_line(line: &str) -> Option<(&str, String)> {
    // Prefer `=` if it appears before unquoted whitespace or as `key = val`.
    let mut eq_pos = None;
    let mut ws_pos = None;
    for (idx, ch) in line.char_indices() {
        if ch == '=' && eq_pos.is_none() {
            eq_pos = Some(idx);
            break;
        }
        if ch.is_whitespace() && ws_pos.is_none() {
            ws_pos = Some(idx);
        }
    }

    if let Some(eq) = eq_pos {
        let key = line[..eq].trim();
        if !key.is_empty() && !key.contains(char::is_whitespace) {
            let val = unquote(&line[eq + 1..]).to_string();
            return Some((key, val));
        }
    }

    if let Some(ws) = ws_pos {
        let key = line[..ws].trim();
        let val = unquote(line[ws..].trim()).to_string();
        return Some((key, val));
    }

    let key = line.trim();
    if key.is_empty() {
        None
    } else {
        Some((key, "yes".to_string()))
    }
}

pub fn parse_mpv_conf(text: &str, mpv_dir: &Path) -> ParsedMpvConf {
    let mut parsed = ParsedMpvConf::default();
    let mut in_named_profile = false;

    for raw_line in text.lines() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let (code, _) = split_unquoted_comment(trimmed);
        let code = code.trim();
        if code.is_empty() {
            continue;
        }

        // shortcut: named mpv.conf profiles are ignored; global options only
        if code.starts_with('[') && code.ends_with(']') {
            in_named_profile = code != "[default]";
            continue;
        }
        if in_named_profile {
            continue;
        }

        let Some((raw_key, mut value)) = parse_option_line(code) else {
            continue;
        };
        let key = normalize_key(raw_key);
        if key.is_empty() {
            continue;
        }

        if is_protected_key(&key) {
            parsed.skipped.push(key);
            continue;
        }

        if PATH_BEARING_OPTIONS.contains(&key.as_str()) {
            value = expand_mpv_paths(&value, mpv_dir);
        }

        let trimmed_val = value.trim();
        if (matches!(key.as_str(), "glsl-shaders" | "glsl-shaders-set")
            && (trimmed_val.is_empty() || trimmed_val.eq_ignore_ascii_case("no")))
            || matches!(key.as_str(), "no-glsl-shaders" | "glsl-shaders-clr")
        {
            parsed.has_shaders = false;
        } else if SHADER_OPTIONS.contains(&key.as_str())
            && !trimmed_val.is_empty()
            && !trimmed_val.eq_ignore_ascii_case("no")
        {
            parsed.has_shaders = true;
        }

        parsed.options.push((key, value));
    }

    parsed
}
