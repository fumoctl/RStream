use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

mod defaults;
mod input_conf;
mod mpv_conf;

pub use defaults::{DEFAULT_INPUT_CONF, DEFAULT_MPV_CONF};
pub use input_conf::{parse_input_conf, parse_key_bind, InputBinding, KeyBind, MenuAction};
pub use mpv_conf::{parse_mpv_conf, ParsedMpvConf};

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShaderFile {
    pub name: String,
    pub path: PathBuf,
    pub description: Option<String>,
}

#[derive(Clone, Debug)]
pub struct MpvConfigDir {
    dir: PathBuf,
}

fn seed_if_missing(path: &Path, contents: &str) -> anyhow::Result<()> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            file.write_all(contents.as_bytes())?;
            Ok(())
        }
        Err(err) if err.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn write_atomic(path: &Path, contents: &str) -> anyhow::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config");
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".{name}.tmp-{}-{seq}", std::process::id()));
    let res = (|| -> anyhow::Result<()> {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

fn read_shader_desc(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    for line in BufReader::new(file.take(16 * 1024))
        .lines()
        .take(12)
        .flatten()
    {
        if let Some(desc) = line.trim().strip_prefix("//!DESC") {
            let desc = desc.trim();
            if !desc.is_empty() {
                return Some(desc.to_string());
            }
        }
    }
    None
}

pub fn list_shaders(mpv_dir: &Path) -> Vec<ShaderFile> {
    let shaders_dir = mpv_dir.join("shaders");
    let Ok(entries) = fs::read_dir(shaders_dir) else {
        return Vec::new();
    };

    let mut shaders = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let is_shader = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|ext| {
                ext.eq_ignore_ascii_case("glsl") || ext.eq_ignore_ascii_case("hook")
            });
        if !is_shader {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            shaders.push(ShaderFile {
                name: name.to_string(),
                description: read_shader_desc(&path),
                path,
            });
        }
    }
    shaders.sort_by(|a, b| a.name.cmp(&b.name));
    shaders
}

impl MpvConfigDir {
    pub fn open_or_seed(dir: PathBuf) -> anyhow::Result<Self> {
        let shaders_dir = dir.join("shaders");
        fs::create_dir_all(&shaders_dir)?;

        seed_if_missing(&dir.join("mpv.conf"), defaults::DEFAULT_MPV_CONF)?;
        seed_if_missing(&dir.join("input.conf"), defaults::DEFAULT_INPUT_CONF)?;
        seed_if_missing(
            &shaders_dir.join("cas-sharpen.glsl"),
            defaults::SHADER_CAS_SHARPEN,
        )?;
        seed_if_missing(
            &shaders_dir.join("cinema-warm.glsl"),
            defaults::SHADER_CINEMA_WARM,
        )?;

        Ok(Self { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn mpv_conf_path(&self) -> PathBuf {
        self.dir.join("mpv.conf")
    }

    pub fn input_conf_path(&self) -> PathBuf {
        self.dir.join("input.conf")
    }

    pub fn shaders_dir(&self) -> PathBuf {
        self.dir.join("shaders")
    }

    pub fn read_mpv_conf(&self) -> anyhow::Result<String> {
        Ok(fs::read_to_string(self.mpv_conf_path())?)
    }

    pub fn write_mpv_conf(&self, contents: &str) -> anyhow::Result<()> {
        write_atomic(&self.mpv_conf_path(), contents)
    }

    pub fn read_input_conf(&self) -> anyhow::Result<String> {
        Ok(fs::read_to_string(self.input_conf_path())?)
    }

    pub fn write_input_conf(&self, contents: &str) -> anyhow::Result<()> {
        write_atomic(&self.input_conf_path(), contents)
    }

    pub fn parse_mpv_conf(&self) -> anyhow::Result<ParsedMpvConf> {
        Ok(parse_mpv_conf(&self.read_mpv_conf()?, &self.dir))
    }

    pub fn parse_input_conf(&self) -> anyhow::Result<Vec<InputBinding>> {
        Ok(parse_input_conf(&self.read_input_conf()?, &self.dir))
    }

    pub fn list_shaders(&self) -> Vec<ShaderFile> {
        list_shaders(&self.dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("rstream-mpv-config-{tag}-{nanos}"))
    }

    #[test]
    fn open_or_seed_creates_files_and_never_overwrites() {
        let dir = temp_dir("seed");
        let cfg = MpvConfigDir::open_or_seed(dir.clone()).unwrap();

        assert!(cfg.mpv_conf_path().is_file());
        assert!(cfg.input_conf_path().is_file());
        assert!(cfg.shaders_dir().join("cas-sharpen.glsl").is_file());
        assert!(cfg.shaders_dir().join("cinema-warm.glsl").is_file());

        let shaders = cfg.list_shaders();
        assert_eq!(shaders.len(), 2);
        assert_eq!(shaders[0].name, "cas-sharpen.glsl");
        assert_eq!(
            shaders[0].description.as_deref(),
            Some("Contrast Adaptive Sharpening")
        );
        assert_eq!(shaders[1].name, "cinema-warm.glsl");
        assert_eq!(
            shaders[1].description.as_deref(),
            Some("Subtle Cinema Warmth")
        );

        let default_bindings = cfg.parse_input_conf().unwrap();
        assert!(default_bindings.len() >= 15);
        assert!(default_bindings.iter().any(|b| b.action.touches_shaders));
        // Built-in RStream keys must not be hijacked out of the box.
        for reserved in [
            "f", "s", "z", "BS", ",", ".", "0", "1", "2", "SPACE", "LEFT", "RIGHT", "UP", "DOWN",
            "m",
        ] {
            let probe = parse_key_bind(reserved);
            assert!(
                !default_bindings.iter().any(|b| b.bind == probe),
                "DEFAULT_INPUT_CONF must not hijack built-in key {reserved}"
            );
        }

        // Customize files, re-open, and verify user edits survive untouched and leave no .tmp.
        cfg.write_mpv_conf("deband=yes\n").unwrap();
        cfg.write_input_conf("SPACE cycle pause # Custom\n").unwrap();
        fs::write(cfg.shaders_dir().join("cas-sharpen.glsl"), "// custom").unwrap();
        assert!(fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .all(|e| !e.file_name().to_string_lossy().contains(".tmp-")));

        let reopened = MpvConfigDir::open_or_seed(dir.clone()).unwrap();
        assert_eq!(reopened.read_mpv_conf().unwrap(), "deband=yes\n");
        assert_eq!(
            reopened.read_input_conf().unwrap(),
            "SPACE cycle pause # Custom\n"
        );
        assert_eq!(
            fs::read_to_string(reopened.shaders_dir().join("cas-sharpen.glsl")).unwrap(),
            "// custom"
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn mpv_conf_filters_protected_expands_paths_and_ignores_profiles() {
        let mpv_dir = Path::new("/data/rstream/mpv");
        let conf = r#"
# Comment
vo=gpu
wid = 1234
force-window yes
gpu-api=vulkan
gpu-context=wayland
osc=no
config=no
config_dir=/tmp/nope
input-default-bindings=no
input-vo-keyboard=no
terminal=no
idle=yes
include=/etc/passwd
log_file=/tmp/mpv.log
msg-level=all=trace
tls_verify=no
tls-ca-file=/tmp/ca.pem
ytdl=yes
script=/tmp/evil.lua
scripts-append=/tmp/evil2.lua
load_scripts=yes
http-proxy=http://127.0.0.1:8080
network_proxy=http://127.0.0.1:8080
stream-lavf-o=headers="Authorization: secret"

deband
slang = en,eng
sub_font_size 44
glsl-shaders="~~/shaders/cas-sharpen.glsl:~~desktop/shaders/cinema-warm.glsl"
sub-font-provider='C:\rstream\fonts\' # trailing comment after single-quoted backslash

[anime-4k] # inline profile comment must not leak!
glsl-shaders=~~/shaders/ignored.glsl
deband=no

[default] # back to global
hwdec=auto-safe
"#;

        let parsed = parse_mpv_conf(conf, mpv_dir);
        assert_eq!(
            parsed.skipped,
            vec![
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
                "scripts-append",
                "load-scripts",
                "http-proxy",
                "network-proxy",
                "stream-lavf-o",
            ]
        );
        assert!(parsed.has_shaders);
        assert_eq!(
            parsed.options,
            vec![
                ("deband".into(), "yes".into()),
                ("slang".into(), "en,eng".into()),
                ("sub-font-size".into(), "44".into()),
                (
                    "glsl-shaders".into(),
                    "/data/rstream/mpv/shaders/cas-sharpen.glsl:/data/rstream/mpv/shaders/cinema-warm.glsl".into()
                ),
                ("sub-font-provider".into(), r"C:\rstream\fonts\".into()),
                ("hwdec".into(), "auto-safe".into()),
            ]
        );
    }

    #[test]
    fn mpv_conf_empty_or_no_shader_resets_has_shaders() {
        let mpv_dir = Path::new("/tmp/mpv");
        let parsed = parse_mpv_conf(
            "glsl-shaders=~~/shaders/cas-sharpen.glsl\nglsl-shaders=\"\"\n",
            mpv_dir,
        );
        assert!(!parsed.has_shaders);
        let parsed_no = parse_mpv_conf(
            "glsl-shaders=~~/shaders/cas-sharpen.glsl\nglsl-shaders=no\n",
            mpv_dir,
        );
        assert!(!parsed_no.has_shaders);
    }

    #[test]
    fn input_conf_parses_keys_menus_labels_and_blocks_unsafe_commands() {
        let mpv_dir = Path::new("/home/u/.local/share/rstream/mpv");
        let input = r#"
# Header comment
   
MOUSE_BTN0 cycle pause
Shift+MBTN_LEFT ignore
Ctrl+WHEEL_UP add volume 2
q ignore
F1 run "/bin/sh" "-c" "id"
F2 subprocess "/bin/sh"
F3 loadfile "http://evil.example/video.mp4"
F4 set http-proxy "http://127.0.0.1:8080"
F6 set config_dir "/tmp/evil"
F7 change-list scripts append "/tmp/evil.lua"
F8 cycle pause ; run "/bin/sh"
SPACE cycle pause
F5 screenshot #menu: Video > Capture Frame
Ctrl+s change-list glsl-shaders toggle "~~/shaders/cas-sharpen.glsl"
Alt+Shift+k add sub-delay -0.1 # Earlier Subtitles
S add panscan 0.1
Ctrl++ add audio-delay 0.1
é sub-step 1
"#;

        let bindings = parse_input_conf(input, mpv_dir);
        assert_eq!(bindings.len(), 7);

        assert_eq!(
            bindings[0].bind,
            KeyBind {
                raw_key: "SPACE".into(),
                key: "SPACE".into(),
                ctrl: false,
                alt: false,
                shift: false,
                meta: false,
            }
        );
        assert_eq!(bindings[0].action.id, 0);
        assert_eq!(bindings[0].action.label, "Toggle Pause");
        assert_eq!(bindings[0].action.group, "Playback");
        assert!(!bindings[0].action.touches_shaders);

        assert_eq!(bindings[1].bind.key, "F5");
        assert_eq!(bindings[1].action.group, "Video");
        assert_eq!(bindings[1].action.label, "Capture Frame");

        assert!(bindings[2].bind.ctrl);
        assert_eq!(bindings[2].bind.key, "s");
        assert!(!bindings[2].bind.shift);
        assert_eq!(
            bindings[2].action.command,
            "change-list glsl-shaders toggle \"/home/u/.local/share/rstream/mpv/shaders/cas-sharpen.glsl\""
        );
        assert_eq!(bindings[2].action.label, "Toggle Shader: cas-sharpen.glsl");
        assert_eq!(bindings[2].action.group, "Shaders");
        assert!(bindings[2].action.touches_shaders);

        assert!(bindings[3].bind.alt);
        assert!(bindings[3].bind.shift);
        assert_eq!(bindings[3].bind.key, "k");
        assert_eq!(bindings[3].action.label, "Earlier Subtitles");
        assert_eq!(bindings[3].action.group, "Subtitles");

        assert!(bindings[4].bind.shift);
        assert_eq!(bindings[4].bind.key, "s");
        assert_eq!(bindings[4].action.label, "Panscan 0.1");

        assert!(bindings[5].bind.ctrl);
        assert_eq!(bindings[5].bind.key, "+");

        // Multi-byte single-char key preserves lowercase via .chars().count() == 1
        assert_eq!(bindings[6].bind.key, "é");
        assert!(!bindings[6].bind.shift);
    }

    #[test]
    fn list_shaders_filters_extensions_and_handles_missing_dir() {
        let missing = temp_dir("missing");
        assert!(list_shaders(&missing).is_empty());

        let dir = temp_dir("shaders-ext");
        fs::create_dir_all(dir.join("shaders")).unwrap();
        fs::write(dir.join("shaders/a.hook"), "//!DESC Hook A\n").unwrap();
        fs::write(dir.join("shaders/readme.txt"), "ignore").unwrap();
        let shaders = list_shaders(&dir);
        assert_eq!(shaders.len(), 1);
        assert_eq!(shaders[0].name, "a.hook");
        assert_eq!(shaders[0].description.as_deref(), Some("Hook A"));

        let _ = fs::remove_dir_all(dir);
    }
}
