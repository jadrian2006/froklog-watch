//! Audio triggers: the dev's trigger engine, with a Linux voice.
//!
//! The matching is entirely his — `froklog::triggers::engine` reads the same
//! `triggers.toml`, does the same regex/glob/variable work, and resolves sound
//! labels through the same sound packages, so a triggers file written for the
//! Windows client behaves identically here. What does not carry over is the
//! output: his overlay speaks through the Windows speech API and plays sound
//! through `PlaySoundW`. Those are the two things this module replaces.
//!
//! Everything is played through the desktop's own audio server, tagged as
//! froklog watch, so it shows up as its own stream in the volume mixer and can
//! be turned down without touching the game.

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

/// Master volume 0–100, applied to every sound and voice (paplay --volume,
/// spd-say -i). Process-wide like the Windows client's audio atomics; the
/// window syncs it from settings each frame.
pub static VOLUME: AtomicU8 = AtomicU8::new(100);
/// Master mute. Auditions (the Sounds tab's ▶ buttons, "Say a phrase")
/// deliberately bypass it via the *_forced variants — you audition BECAUSE
/// things are muted — matching the Windows client's preview behavior.
pub static MUTED: AtomicBool = AtomicBool::new(false);

/// paplay volume scale: 65536 = 100%.
fn paplay_volume() -> String {
    let v = VOLUME.load(Ordering::Relaxed).min(100) as u32;
    format!("--volume={}", v * 65536 / 100)
}

use froklog::triggers::engine::{
    Action, Condition, OverlayEvent, TriggerConfig, TriggerDef, TriggerEngine, VoicePriority,
};

/// What a fired trigger looked like, for the window to show.
#[derive(Clone)]
pub struct Fired {
    pub message: String,
    pub spoke: bool,
    pub played: Option<String>,
}

pub struct Alerts {
    engine: TriggerEngine,
    queue: Arc<Mutex<Vec<OverlayEvent>>>,
    /// what fired recently, newest last, capped
    pub recent: Vec<Fired>,
    pub enabled: bool,
    /// the loaded triggers.toml, kept so the window can show and edit it
    cfg: TriggerConfig,
}

impl Alerts {
    pub fn load(enabled: bool) -> Self {
        let cfg = TriggerConfig::load();
        let queue = Arc::new(Mutex::new(Vec::new()));
        Self {
            engine: TriggerEngine::new(&cfg, Arc::clone(&queue)),
            queue,
            recent: Vec::new(),
            enabled,
            cfg,
        }
    }

    pub fn triggers(&self) -> &[TriggerDef] {
        &self.cfg.triggers
    }

    pub fn count(&self) -> usize {
        self.cfg.triggers.len()
    }

    /// Turn one trigger on or off and write it back. Enabling and disabling is
    /// most of what trigger editing actually is day to day — the rest is text.
    pub fn set_enabled(&mut self, index: usize, on: bool) {
        let Some(t) = self.cfg.triggers.get_mut(index) else {
            return;
        };
        t.enabled = on;
        self.cfg.save();
        self.engine.reload(&self.cfg);
    }

    /// The trigger file as text, for editing in place.
    pub fn read_file() -> String {
        std::fs::read_to_string(froklog::triggers::engine::triggers_path()).unwrap_or_default()
    }

    /// Validate, write, and start using it. Parsing first means a typo shows
    /// up here rather than silently emptying the file the engine reads.
    pub fn write_file(&mut self, text: &str) -> Result<usize, String> {
        let cfg: TriggerConfig = toml::from_str(text).map_err(|e| e.to_string())?;
        std::fs::write(froklog::triggers::engine::triggers_path(), text)
            .map_err(|e| e.to_string())?;
        let n = cfg.triggers.len();
        self.cfg = cfg;
        self.engine.reload(&self.cfg);
        Ok(n)
    }

    /// Append a new trigger and write it to the same file the Windows client
    /// reads, so anything built here is portable back to it.
    pub fn add_trigger(&mut self, name: &str, pattern: &str, sound: &str, say: &str) {
        let mut actions = Vec::new();
        if !sound.is_empty() {
            actions.push(Action::PlaySound {
                sound: Some(sound.to_string()),
                delay_secs: 0.0,
            });
        }
        if !say.is_empty() {
            actions.push(Action::VoiceAlert {
                tts_text: say.to_string(),
                priority: VoicePriority::default(),
            });
        }
        self.cfg.triggers.push(TriggerDef {
            name: name.to_string(),
            enabled: true,
            condition_logic: Default::default(),
            conditions: vec![Condition::Match {
                match_type: froklog::triggers::engine::MatchType::Regex,
                pattern: pattern.to_string(),
            }],
            actions,
        });
        self.cfg.save();
        self.engine.reload(&self.cfg);
    }

    /// Run a line as if the game had just logged it, so a trigger can be heard
    /// before the fight that needs it.
    pub fn test_line(&self, line: &str) {
        self.engine.process_line(line);
    }

    /// Re-read triggers.toml. Editing that file is how triggers are authored,
    /// so there has to be a way to pick up the edit without a restart.
    pub fn reload(&mut self) {
        self.cfg = TriggerConfig::load();
        self.engine.reload(&self.cfg);
    }

    pub fn path() -> std::path::PathBuf {
        froklog::triggers::engine::triggers_path()
    }

    /// Feed one log line through the engine.
    pub fn process_line(&self, line: &str) {
        if self.enabled {
            self.engine.process_line(line);
        }
    }

    /// Fire whatever the engine has queued. Called on the UI tick, which also
    /// drives the engine's own delayed actions.
    pub fn pump(&mut self, package: &str, voice: &Voice) {
        self.engine.tick();
        let events: Vec<OverlayEvent> = {
            let mut q = self.queue.lock().unwrap();
            if q.is_empty() {
                return;
            }
            std::mem::take(&mut *q)
        };
        for ev in events {
            let played = ev.sound.as_deref().and_then(|s| play(s, package));
            let spoke = ev
                .tts_text
                .as_deref()
                .is_some_and(|t| speak(t, &ev.tts_priority, voice));
            self.recent.push(Fired {
                message: if ev.message.is_empty() {
                    ev.tts_text.clone().unwrap_or_default()
                } else {
                    ev.message
                },
                spoke,
                played,
            });
        }
        // a log is long; the interesting alert is the last one
        let overflow = self.recent.len().saturating_sub(20);
        self.recent.drain(..overflow);
    }
}

/// Resolve a trigger's sound and play it. Returns what was played, if anything.
///
/// A trigger names a sound by label ("Ding"), which the active sound package
/// maps to a file — that is what lets one package swap every trigger's sounds
/// at once. An absolute path is taken literally, for a sound that is not in a
/// package at all.
pub fn play(sound: &str, package: &str) -> Option<String> {
    if MUTED.load(Ordering::Relaxed) {
        return None;
    }
    play_forced(sound, package)
}

/// Play regardless of the mute switch (auditions). Volume still applies.
pub fn play_forced(sound: &str, package: &str) -> Option<String> {
    if sound.is_empty() {
        return None;
    }
    let direct = std::path::Path::new(sound);
    let path = if direct.is_absolute() && direct.exists() {
        direct.to_path_buf()
    } else {
        froklog::sound_packages::sound_packages::resolve_label(package, sound)?
    };
    play_file(&path).then(|| path.to_string_lossy().into_owned())
}

/// paplay is the PipeWire/PulseAudio client every desktop ships; aplay is the
/// bare-ALSA fallback for a machine without a sound server. Naming the
/// application is what gives froklog watch its own slider in the mixer.
fn play_file(path: &std::path::Path) -> bool {
    let paplay = Command::new("paplay")
        .arg("--property=application.name=froklog watch")
        .arg("--property=media.role=event")
        .arg(paplay_volume())
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if paplay.is_ok() {
        return true;
    }
    Command::new("aplay")
        .arg("-q")
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}

/// How an alert gets spoken.
///
/// speech-dispatcher is always there, needs no setup and sounds like a robot,
/// because underneath it is espeak-ng — a formant synthesiser from an era when
/// that was the only thing that fit on the machine. piper is a neural voice:
/// it needs a model file on disk, and sounds like a person.
pub enum Voice<'a> {
    SpeechDispatcher,
    Piper { model: &'a str },
}

impl<'a> Voice<'a> {
    pub fn from_settings(engine: &'a str, model: &'a str) -> Self {
        match engine {
            "piper" if !model.is_empty() => Voice::Piper { model },
            _ => Voice::SpeechDispatcher,
        }
    }
}

/// How to say the words a speech engine has never seen.
///
/// Neither piper nor espeak has ever met Cazic-Thule, and both guess from
/// English spelling — usually badly, and always the same badly. Rather than
/// compile espeak pronunciation dictionaries (which are per-engine, need a
/// build step, and would not help speech-dispatcher) this rewrites the text
/// before it is spoken: "Cazic-Thule" in, "Kay-zick Thool" out. It is a
/// spelling hint for a machine, so it is written the way it sounds, not in
/// phonetic notation nobody wants to hand-write.
///
/// Lives at ~/.config/froklog-watch/pronounce.toml as plain key = "value".
pub fn pronounce_path() -> std::path::PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("froklog-watch")
        .join("pronounce.toml")
}

/// The table as the window edits it: insertion-friendly pairs, not a map.
pub fn pronunciations() -> Vec<(String, String)> {
    load_pronunciations()
}

/// Write the table back. Speaking re-reads the file, so an edit takes effect
/// on the next alert without a reload button.
pub fn save_pronunciations(table: &[(String, String)]) -> std::io::Result<()> {
    let mut out = String::from(
        "# How to say what the voice gets wrong. Spelled the way it sounds,\n         # not in phonetic notation. Whole words only, case-insensitive.\n\n",
    );
    for (from, to) in table {
        if from.trim().is_empty() {
            continue;
        }
        out.push_str(&format!("{:?} = {:?}\n", from.trim(), to.trim()));
    }
    let path = pronounce_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, out)
}

/// Apply the table to a line the way an alert would, so the window can show
/// what a voice will actually be handed.
pub fn preview(text: &str) -> String {
    respell(text, &load_pronunciations())
}

fn load_pronunciations() -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(pronounce_path()) else {
        return Vec::new();
    };
    let Ok(table) = toml::from_str::<std::collections::BTreeMap<String, String>>(&text) else {
        return Vec::new();
    };
    let mut v: Vec<(String, String)> = table.into_iter().collect();
    // longest first, so "Plane of Fear" wins over a "Fear" entry
    v.sort_by_key(|(k, _)| std::cmp::Reverse(k.len()));
    v
}

/// Apply the table, matching whole words and ignoring case.
fn respell(text: &str, table: &[(String, String)]) -> String {
    let mut out = text.to_string();
    for (from, to) in table {
        let mut result = String::with_capacity(out.len());
        let lower = out.to_lowercase();
        let needle = from.to_lowercase();
        let mut at = 0;
        while let Some(hit) = lower[at..].find(&needle) {
            let start = at + hit;
            let end = start + needle.len();
            let before_ok = start == 0
                || !lower[..start]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric());
            let after_ok = end == lower.len()
                || !lower[end..].chars().next().is_some_and(|c| c.is_alphanumeric());
            result.push_str(&out[at..start]);
            if before_ok && after_ok {
                result.push_str(to);
            } else {
                result.push_str(&out[start..end]);
            }
            at = end;
        }
        result.push_str(&out[at..]);
        out = result;
    }
    out
}

/// Every piper voice sitting on this machine, newest naming scheme or not.
///
/// A voice is just a pair of files, so "install a voice" means dropping an
/// .onnx and its .json into this directory — no code change, no rebuild. The
/// picker lists whatever is there.
pub fn installed_voices() -> Vec<(String, String)> {
    let dir = dirs::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("piper")
        .join("voices");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension()? != "onnx" {
                return None;
            }
            let stem = path.file_stem()?.to_string_lossy().into_owned();
            Some((stem, path.to_string_lossy().into_owned()))
        })
        .collect();
    out.sort();
    out
}

/// Everything needed to turn a pasted log line into a working pattern.
pub mod builder {
    use std::collections::BTreeSet;

    /// Drop EverQuest's timestamp.
    ///
    /// Every logged line starts with "[Sun Jul 26 19:12:18 2026] ". Leaving it
    /// in a pattern would build a trigger that matches exactly one second of
    /// one day and never fires again, which is the first thing a hand-written
    /// trigger gets wrong.
    pub fn strip_timestamp(line: &str) -> &str {
        let t = line.trim();
        match (t.starts_with('['), t.find(']')) {
            (true, Some(end)) => t[end + 1..].trim_start(),
            _ => t,
        }
    }

    /// Split into runs of word characters and everything between them, so a
    /// name can be picked out without dragging the punctuation around it.
    ///
    /// An apostrophe counts as punctuation, not part of a word: EQ wraps
    /// speech in them ("says, 'Hail'") far more often than names contain
    /// them, and a quote swallowed into a capture would match the wrong text.
    pub fn tokenize(line: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut in_word = false;
        for c in line.chars() {
            let w = c.is_alphanumeric();
            if cur.is_empty() || w == in_word {
                cur.push(c);
            } else {
                out.push(std::mem::take(&mut cur));
                cur.push(c);
            }
            in_word = w;
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        out
    }

    /// True for the tokens worth offering as a variable — the words, not the
    /// spaces and commas between them.
    pub fn is_word(tok: &str) -> bool {
        tok.chars().next().is_some_and(|c| c.is_alphanumeric())
    }

    /// What a chosen token most likely varies over. A number stays a number,
    /// because a digit capture is what makes a threshold testable later; a
    /// word stays a word so punctuation still has to match.
    fn capture_for(tok: &str) -> &'static str {
        if tok.chars().all(|c| c.is_ascii_digit()) {
            "(\\d+)"
        } else if tok.chars().all(|c| c.is_alphanumeric()) {
            "(\\w+)"
        } else {
            "(.+?)"
        }
    }

    /// Build the pattern: chosen tokens become capture groups, everything else
    /// is matched literally and escaped, so a name containing a bracket or a
    /// full stop cannot silently turn into a wildcard.
    pub fn regex(tokens: &[String], chosen: &BTreeSet<usize>) -> String {
        let mut out = String::from("^");
        for (i, tok) in tokens.iter().enumerate() {
            if chosen.contains(&i) {
                out.push_str(capture_for(tok));
            } else {
                out.push_str(&escape(tok));
            }
        }
        out
    }

    /// The capture number a chosen token will have, for showing "{1}" beside
    /// the word it came from.
    pub fn group_of(chosen: &BTreeSet<usize>, index: usize) -> Option<usize> {
        chosen.iter().position(|i| *i == index).map(|n| n + 1)
    }

    fn escape(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            if "\\.+*?()|[]{}^$".contains(c) {
                out.push('\\');
            }
            out.push(c);
        }
        out
    }
}

/// One line describing what a trigger watches for and what it does, so the
/// list is readable without opening the file.
pub fn describe(def: &TriggerDef) -> String {
    let watches: Vec<String> = def
        .conditions
        .iter()
        .map(|c| match c {
            Condition::Match { pattern, .. } => pattern.clone(),
            Condition::Var { var_name, .. } => format!("${var_name}"),
        })
        .collect();
    let does: Vec<&str> = def
        .actions
        .iter()
        .map(|a| match a {
            Action::Overlay { .. } => "message",
            Action::VoiceAlert { .. } => "voice",
            Action::PlaySound { .. } => "sound",
            Action::StoreVar { .. } => "variable",
        })
        .collect();
    format!("{}  →  {}", watches.join(" & "), does.join(", "))
}

/// Sound packages installed on this machine.
pub fn packages() -> Vec<String> {
    froklog::sound_packages::sound_packages::list_packages()
}

/// The labels a package offers. Triggers reference these names, so this is
/// also the list of sounds worth auditioning before wiring one to a trigger.
pub fn labels(package: &str) -> Vec<String> {
    froklog::sound_packages::sound_packages::load_manifest(package)
        .labels
        .into_iter()
        .map(|l| l.name)
        .collect()
}

/// What is currently being said, so a later alert can interrupt it.
static SPEAKING: Mutex<Option<std::process::Child>> = Mutex::new(None);

/// Speak an alert, honouring his three priorities.
///
/// Emergency cuts off whatever is talking — an alert you need now is worth
/// losing the last one for. Ambient gives way instead: it is the trivia, and
/// it should never talk over something that mattered. Operational just queues.
pub fn speak(text: &str, priority: &VoicePriority, voice: &Voice) -> bool {
    if MUTED.load(Ordering::Relaxed) {
        return false;
    }
    speak_forced(text, priority, voice)
}

/// Speak regardless of the mute switch (auditions). Volume still applies.
pub fn speak_forced(text: &str, priority: &VoicePriority, voice: &Voice) -> bool {
    if text.is_empty() {
        return false;
    }
    let table = load_pronunciations();
    let said = respell(text, &table);
    let text = said.as_str();
    match voice {
        Voice::SpeechDispatcher => {
            // speech-dispatcher implements the same three-way policy itself
            let prio = match priority {
                VoicePriority::Emergency => "important",
                VoicePriority::Operational => "message",
                VoicePriority::Ambient => "notification",
            };
            // spd-say volume runs -100 (silent) to +100; the 0-100 slider
            // maps onto the attenuation half so 100 stays the voice default.
            let vol = (VOLUME.load(Ordering::Relaxed).min(100) as i32 - 100).to_string();
            Command::new("spd-say")
                .args(["--priority", prio, "-i", &vol, "--application-name", "froklog-watch", "--", text])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .is_ok()
        }
        Voice::Piper { model } => speak_piper(text, priority, model),
    }
}

/// piper reads text on stdin and writes raw samples on stdout, so it pipes
/// straight into the same audio server the trigger sounds use.
fn speak_piper(text: &str, priority: &VoicePriority, model: &str) -> bool {
    use std::io::Write;

    {
        let mut cur = SPEAKING.lock().unwrap();
        let busy = cur
            .as_mut()
            .is_some_and(|c| matches!(c.try_wait(), Ok(None)));
        match (busy, priority) {
            (true, VoicePriority::Emergency) => {
                if let Some(c) = cur.as_mut() {
                    let _ = c.kill();
                }
                *cur = None;
            }
            (true, VoicePriority::Ambient) => return false, // never talk over a real alert
            _ => {}
        }
    }

    let rate = piper_sample_rate(model);
    let Ok(mut piper) = Command::new("piper")
        .args(["--model", model, "--output_raw"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    if let Some(mut stdin) = piper.stdin.take() {
        let _ = writeln!(stdin, "{text}");
    }
    let Some(audio) = piper.stdout.take() else {
        return false;
    };
    let played = Command::new("paplay")
        .args([
            "--property=application.name=froklog watch",
            "--raw",
            "--format=s16le",
            "--channels=1",
            &format!("--rate={rate}"),
            &paplay_volume(),
        ])
        .stdin(audio)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match played {
        Ok(child) => {
            *SPEAKING.lock().unwrap() = Some(child);
            true
        }
        Err(_) => false,
    }
}

/// Each piper voice ships a JSON sidecar naming its sample rate; playing at
/// the wrong rate is what makes a voice sound chipmunked or drunk.
fn piper_sample_rate(model: &str) -> u32 {
    let sidecar = format!("{model}.json");
    let Ok(text) = std::fs::read_to_string(sidecar) else {
        return 22050;
    };
    text.split("\"sample_rate\"")
        .nth(1)
        .and_then(|rest| {
            rest.trim_start_matches([':', ' '])
                .split(|c: char| !c.is_ascii_digit())
                .find(|s| !s.is_empty())
                .and_then(|n| n.parse().ok())
        })
        .unwrap_or(22050)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_pattern_from_a_pasted_line() {
        use builder::*;
        let raw = "[Sun Jul 26 19:12:18 2026] Zarri says, 'Hail, Icestorm'";
        // the timestamp must go, or the trigger matches one second of one day
        let line = strip_timestamp(raw);
        assert_eq!(line, "Zarri says, 'Hail, Icestorm'");

        let tokens = tokenize(line);
        // pick the two names
        let chosen: std::collections::BTreeSet<usize> = tokens
            .iter()
            .enumerate()
            .filter(|(_, t)| t.as_str() == "Zarri" || t.as_str() == "Icestorm")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(chosen.len(), 2);

        let pattern = regex(&tokens, &chosen);
        // captures where we pointed, literal everywhere else
        assert!(pattern.starts_with(r"^(\w+) says, "), "{pattern}");
        assert!(pattern.ends_with(r"(\w+)'"), "{pattern}");

        // and it has to actually match the line it was built from
        let re = ::regex::Regex::new(&pattern).expect("valid regex");
        let caps = re.captures(line).expect("matches its own line");
        assert_eq!(&caps[1], "Zarri");
        assert_eq!(&caps[2], "Icestorm");
    }

    /// Adding a trigger writes it and it starts matching — the path the
    /// window's Create button takes.
    #[test]
    #[ignore] // rewrites HOME; run alone: cargo test adds_a_trigger -- --ignored
    fn adds_a_trigger_and_it_fires() {
        let tmp = std::env::temp_dir().join("froklog-watch-addtrigger");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("HOME", &tmp);

        let line = builder::strip_timestamp("[Sun Jul 26 19:12:18 2026] Zarri has merged an item to +4");
        let tokens = builder::tokenize(line);
        let chosen: std::collections::BTreeSet<usize> = tokens
            .iter()
            .enumerate()
            .filter(|(_, t)| t.as_str() == "Zarri" || t.as_str() == "4")
            .map(|(i, _)| i)
            .collect();
        let pattern = builder::regex(&tokens, &chosen);

        let mut alerts = Alerts::load(true);
        alerts.add_trigger("merge", &pattern, "Ding", "{1} merged to plus {2}");
        assert_eq!(alerts.count(), 1, "trigger should be in the config");

        // it must survive the round trip through the file
        let reread = TriggerConfig::load();
        assert_eq!(reread.triggers.len(), 1, "trigger should be on disk");

        alerts.test_line(line);
        alerts.pump("default", &Voice::SpeechDispatcher);
        assert!(!alerts.recent.is_empty(), "the trigger should have fired");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The full path a user walks: paste a line, click two words, write a
    /// spoken template referring to them, and hear the words come back.
    #[test]
    fn captures_come_back_as_numbered_placeholders() {
        let line = builder::strip_timestamp("[Sun Jul 26 19:12:18 2026] Zarri has merged an item to +4");
        let tokens = builder::tokenize(line);
        let chosen: std::collections::BTreeSet<usize> = tokens
            .iter()
            .enumerate()
            .filter(|(_, t)| t.as_str() == "Zarri" || t.as_str() == "4")
            .map(|(i, _)| i)
            .collect();
        let pattern = builder::regex(&tokens, &chosen);

        let cfg: TriggerConfig = toml::from_str(&format!(
            r#"
            [[trigger]]
            name = "merge"
            [[trigger.condition]]
            type = "match"
            match_type = "regex"
            pattern = {pattern:?}
            [[trigger.action]]
            type = "voice_alert"
            tts_text = "{{1}} merged an item to plus {{2}}"
            "#
        ))
        .expect("config parses");

        let queue = Arc::new(Mutex::new(Vec::new()));
        let engine = TriggerEngine::new(&cfg, Arc::clone(&queue));
        engine.process_line(line);
        engine.tick();

        let fired = queue.lock().unwrap();
        assert_eq!(
            fired.first().and_then(|e| e.tts_text.as_deref()),
            Some("Zarri merged an item to plus 4")
        );
    }

    #[test]
    fn respelling_matches_whole_words_only() {
        let table = vec![
            ("Cazic-Thule".to_string(), "Kay-zick Thool".to_string()),
            ("Erudin".to_string(), "Air-oo-din".to_string()),
        ];
        assert_eq!(
            respell("Cazic-Thule hits you", &table),
            "Kay-zick Thool hits you"
        );
        // case-insensitive
        assert_eq!(respell("erudin guard", &table), "Air-oo-din guard");
        // but not inside a longer word, or every name containing it breaks
        assert_eq!(respell("Erudinite", &table), "Erudinite");
    }

    /// Makes a noise, so it only runs when asked for:
    ///   cargo test -- --ignored
    #[test]
    #[ignore]
    fn plays_the_ding_label_from_the_default_package() {
        let played = play("Ding", "default");
        assert!(played.is_some(), "no sound package installed?");
        assert!(played.unwrap().ends_with("ding.wav"));
        assert!(speak(
            "trigger test",
            &VoicePriority::Operational,
            &Voice::SpeechDispatcher
        ));
    }

    /// The whole point of reusing his engine is that a triggers.toml written
    /// for the Windows client behaves the same here — matching, captures and
    /// templates included. This asserts that end of the contract; the audio
    /// side is a process spawn and is verified by hand.
    #[test]
    fn a_matching_line_produces_a_message_and_speech() {
        let cfg: TriggerConfig = toml::from_str(
            r#"
            [[trigger]]
            name = "hail"
            [[trigger.condition]]
            type = "match"
            match_type = "regex"
            pattern = "^(\\w+) says, 'Hail, (\\w+)'"
            [[trigger.action]]
            type = "overlay"
            message = "{1} hailed {2}"
            [[trigger.action]]
            type = "voice_alert"
            tts_text = "{1} is hailing you"
            "#,
        )
        .expect("test config parses");

        let queue = Arc::new(Mutex::new(Vec::new()));
        let engine = TriggerEngine::new(&cfg, Arc::clone(&queue));
        engine.process_line("Zarri says, 'Hail, Icestorm'");
        engine.tick();

        let fired = queue.lock().unwrap();
        assert_eq!(fired.len(), 2, "one overlay action and one voice action");
        assert_eq!(fired[0].message, "Zarri hailed Icestorm");
        assert_eq!(fired[1].tts_text.as_deref(), Some("Zarri is hailing you"));
    }
}
