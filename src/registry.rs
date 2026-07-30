//! Which characters we know about, and the froklog stream each one owns.
//!
//! EverQuest writes one log per character per server, and a froklog stream is
//! likewise one character on one server — so the mapping is 1:1 and permanent.
//! We keep it on disk so a character registered once is never registered twice,
//! and so the view links survive a restart.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Character {
    pub player: String,
    pub server: String,
    pub log_path: String,
    /// Watch this one? Off by default: registering every old alt would spam
    /// the server with streams nobody asked for.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub stream_id: Option<String>,
    #[serde(default)]
    pub stream_token: Option<String>,
    #[serde(default)]
    pub view_token: Option<String>,
    /// History already pushed once — importing twice would duplicate it.
    #[serde(default)]
    pub imported: bool,
    /// Publish a clean, tokenless page at /player/<game>/<server>/<name>.
    /// Off by default: a stream should not become findable without asking.
    #[serde(default)]
    pub public: bool,
}

impl Character {
    pub fn key(&self) -> String {
        format!("{}@{}", self.player, self.server)
    }
    pub fn registered(&self) -> bool {
        self.stream_id.is_some() && self.stream_token.is_some()
    }
    /// The froklog page for this character, the thing worth handing to a human.
    /// The view token is what grants access, so this link is shareable as-is —
    /// but anyone holding it keeps that access until the stream is reset.
    pub fn view_url(&self, server_url: &str) -> Option<String> {
        let id = self.stream_id.as_ref()?;
        let tok = self.view_token.as_ref()?;
        Some(format!("{}/stream/{}?vtok={}", server_url.trim_end_matches('/'), id, tok))
    }

    /// The public page: no token, readable name, and revocable by turning
    /// `public` back off. Only exists while the server has the stream marked
    /// public, which is why it returns None otherwise.
    pub fn public_url(&self, server_url: &str, game: &str) -> Option<String> {
        if !self.public || !self.registered() {
            return None;
        }
        Some(format!(
            "{}/player/{}/{}/{}",
            server_url.trim_end_matches('/'),
            game,
            self.server,
            self.player
        ))
    }

    /// The link to hand someone, preferring the tokenless one.
    pub fn share_url(&self, server_url: &str, game: &str) -> Option<String> {
        self.public_url(server_url, game)
            .or_else(|| self.view_url(server_url))
    }
    pub fn ingest_url(&self, server_url: &str) -> Option<String> {
        let id = self.stream_id.as_ref()?;
        let base = server_url
            .trim_end_matches('/')
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1);
        Some(format!("{}/ingest/{}", base, id))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// froklog-server base URL, e.g. https://froklog.psitronic.net
    pub server_url: String,
    /// Only needed if the server sets `stream_password`; most are open. Kept
    /// locally, and sent as a Bearer token when creating a stream.
    #[serde(default, alias = "admin_token")]
    pub stream_password: String,
    /// Where to look for eqlog_*.txt. More than one install is normal.
    #[serde(default)]
    pub log_dirs: Vec<String>,
    /// Game identifier froklog records against the stream.
    #[serde(default = "default_game")]
    pub game: String,
    /// Planner to hand the feed to, so a character is one click from a build.
    #[serde(default = "default_planner")]
    pub planner_url: String,
    /// Run triggers against every watched log.
    #[serde(default)]
    pub triggers_enabled: bool,
    /// Which sound package trigger labels resolve through. Swapping this
    /// re-themes every trigger's sound at once, which is the whole point of
    /// packages — so it belongs here, not in each trigger.
    #[serde(default = "default_sound_package")]
    pub sound_package: String,
    /// Which text-to-speech engine speaks alerts: "speech-dispatcher" (always
    /// present, but robotic) or "piper" (neural, needs a voice model).
    #[serde(default = "default_voice")]
    pub voice_engine: String,
    /// Path to a piper .onnx voice. Only meaningful when voice_engine=piper.
    #[serde(default)]
    pub piper_model: String,
    /// Show the on-screen DPS meter overlay.
    #[serde(default)]
    pub meter_enabled: bool,
    /// Click-through: the meter ignores the mouse entirely. Like the Windows
    /// client, once locked it can only be unlocked from the main window.
    #[serde(default)]
    pub meter_locked: bool,
    /// Top-left position as layer-shell margins from the screen's top-left.
    #[serde(default = "default_meter_pos")]
    pub meter_x: i32,
    #[serde(default = "default_meter_pos")]
    pub meter_y: i32,
    #[serde(default = "default_meter_width")]
    pub meter_width: u32,
    #[serde(default = "default_meter_max_rows")]
    pub meter_max_rows: usize,
    /// Hide after this many seconds with nothing to show. 0 = never hide.
    #[serde(default = "default_meter_idle")]
    pub meter_idle_secs: u64,
    #[serde(default = "default_meter_font")]
    pub meter_font_size: f32,
}

fn default_meter_pos() -> i32 {
    40
}
fn default_meter_width() -> u32 {
    300
}
fn default_meter_max_rows() -> usize {
    8
}
fn default_meter_idle() -> u64 {
    30
}
fn default_meter_font() -> f32 {
    14.0
}

fn default_sound_package() -> String {
    "default".into()
}

fn default_voice() -> String {
    "speech-dispatcher".into()
}

fn default_game() -> String {
    "eq".into()
}
fn default_planner() -> String {
    "https://eqp.psitronic.net".into()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            server_url: String::new(),
            stream_password: String::new(),
            log_dirs: Vec::new(),
            game: default_game(),
            planner_url: default_planner(),
            triggers_enabled: false,
            sound_package: default_sound_package(),
            voice_engine: default_voice(),
            piper_model: String::new(),
            meter_enabled: false,
            meter_locked: false,
            meter_x: default_meter_pos(),
            meter_y: default_meter_pos(),
            meter_width: default_meter_width(),
            meter_max_rows: default_meter_max_rows(),
            meter_idle_secs: default_meter_idle(),
            meter_font_size: default_meter_font(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub settings: Settings,
    /// keyed by "Player@server"
    #[serde(default)]
    pub characters: BTreeMap<String, Character>,
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("froklog-watch")
        .join("config.toml")
}

impl Registry {
    pub fn load() -> Self {
        let path = config_path();
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let body = toml::to_string_pretty(self)?;
        std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Fold any eqlog_<Player>_<server>.txt found on disk into the registry,
    /// keeping whatever we already knew about each one.
    pub fn scan(&mut self) -> usize {
        let dirs = self.settings.log_dirs.clone();
        let mut found = 0;
        for dir in dirs {
            for c in scan_dir(Path::new(&dir)) {
                found += 1;
                self.characters
                    .entry(c.key())
                    .and_modify(|existing| existing.log_path = c.log_path.clone())
                    .or_insert(c);
            }
        }
        found
    }
}

/// eqlog_Zarri_halas.txt -> Zarri on halas
pub fn parse_log_name(file: &str) -> Option<(String, String)> {
    let stem = file.strip_prefix("eqlog_")?.strip_suffix(".txt")?;
    let (player, server) = stem.split_once('_')?;
    if player.is_empty() || server.is_empty() {
        return None;
    }
    Some((player.to_string(), server.to_string()))
}

pub fn scan_dir(dir: &Path) -> Vec<Character> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some((player, server)) = parse_log_name(&name) else {
            continue;
        };
        out.push(Character {
            player,
            server,
            log_path: e.path().to_string_lossy().to_string(),
            enabled: false,
            stream_id: None,
            stream_token: None,
            view_token: None,
            imported: false,
            public: false,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_log_filenames() {
        assert_eq!(
            parse_log_name("eqlog_Zarri_halas.txt"),
            Some(("Zarri".into(), "halas".into()))
        );
        assert_eq!(
            parse_log_name("eqlog_Zari_qeynos.txt"),
            Some(("Zari".into(), "qeynos".into()))
        );
        // not a character log
        assert_eq!(parse_log_name("dbg.txt"), None);
        assert_eq!(parse_log_name("eqlog_broken.txt"), None);
    }

    #[test]
    fn builds_links_only_when_registered() {
        let mut c = Character {
            player: "Zarri".into(),
            server: "halas".into(),
            log_path: "/tmp/eqlog_Zarri_halas.txt".into(),
            enabled: true,
            stream_id: None,
            stream_token: None,
            view_token: None,
            imported: false,
            public: false,
        };
        assert!(c.view_url("https://froklog.example").is_none());
        c.stream_id = Some("abc123".into());
        c.view_token = Some("tok".into());
        assert_eq!(
            c.view_url("https://froklog.example/").as_deref(),
            Some("https://froklog.example/stream/abc123?vtok=tok")
        );
        assert_eq!(
            c.ingest_url("https://froklog.example").as_deref(),
            Some("wss://froklog.example/ingest/abc123")
        );
    }
}
