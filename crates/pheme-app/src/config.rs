//! TOML configuration file.

use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};
use pheme_core::{ClientPlacement, Hotkeys, Side};
use pheme_input::keymap::key_by_name;
use pheme_net::DEFAULT_PORT;
use serde::{Deserialize, Serialize};

use crate::target::Target;

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("pheme")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    #[default]
    Server,
    Client,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SideCfg {
    Left,
    Right,
    Top,
    Bottom,
}

impl From<SideCfg> for Side {
    fn from(s: SideCfg) -> Side {
        match s {
            SideCfg::Left => Side::Left,
            SideCfg::Right => Side::Right,
            SideCfg::Top => Side::Top,
            SideCfg::Bottom => Side::Bottom,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientCfg {
    pub name: String,
    pub side: SideCfg,
    pub span: Option<[f32; 2]>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HotkeysCfg {
    pub lock: Option<String>,
}

impl Default for HotkeysCfg {
    fn default() -> Self {
        HotkeysCfg {
            lock: Some("ScrollLock".into()),
        }
    }
}

/// Optional device overrides. Audio itself is always on: the user controls it by
/// choosing devices in the OS, which is why there is no enable flag here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct AudioCfg {
    /// Server side: where received audio is played. `None` means the system default.
    /// On Linux this is matched as a PipeWire `target.object`, i.e. a node name (see
    /// `pactl list sinks short`); on Windows it is the endpoint's friendly name.
    pub playback_device: Option<String>,
    /// Client side, Windows only: which output endpoint to record with loopback.
    /// `None` means the default output. Ignored on Linux, where applications select
    /// the "Pheme Speaker" sink instead.
    pub capture_device: Option<String>,
    /// Server side: which microphone to capture and send to the client. `None` means the
    /// platform default. There is no key to turn the microphone on or off: it opens only
    /// while the client reports that something is recording.
    pub mic_device: Option<String>,
}

fn default_name() -> String {
    hostname().unwrap_or_else(|| "pheme".into())
}

fn hostname() -> Option<String> {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty())
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], DEFAULT_PORT))
}

fn default_discovery() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub role: Role,
    #[serde(default = "default_name")]
    pub name: String,
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    #[serde(default)]
    pub connect: Option<String>,
    /// Whether the server publishes itself on the local network. Clients are
    /// unaffected: they look up whatever `connect` names regardless.
    #[serde(default = "default_discovery")]
    pub discovery: bool,
    #[serde(default)]
    pub hotkeys: HotkeysCfg,
    #[serde(default)]
    pub audio: AudioCfg,
    #[serde(default)]
    pub clients: Vec<ClientCfg>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            role: Role::Server,
            name: default_name(),
            listen: default_listen(),
            connect: None,
            discovery: true,
            hotkeys: HotkeysCfg::default(),
            audio: AudioCfg::default(),
            clients: Vec::new(),
        }
    }
}

impl Config {
    pub fn default_path() -> PathBuf {
        config_dir().join("config.toml")
    }

    /// Loads `path` (or the default path). A missing file yields defaults.
    pub fn load(path: Option<&Path>) -> anyhow::Result<Config> {
        let path = path
            .map(Path::to_path_buf)
            .unwrap_or_else(Config::default_path);
        if !path.exists() {
            return Ok(Config::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// The hotkeys as key codes.
    ///
    /// `hotkeys.lock` is a name from pheme's key table. A value containing `+` is
    /// instead a shortcut *trigger* in the syntax
    /// `org.freedesktop.portal.GlobalShortcuts` uses — `"CTRL+ALT+l"` — which names no
    /// single key and so produces no `KeyCode`. That is only meaningful on a Wayland
    /// server, where the lock hotkey is bound through that portal rather than by
    /// watching the keyboard; anywhere else it leaves the machine with no lock hotkey
    /// at all, which is an error rather than a silent loss.
    pub fn hotkeys(&self) -> anyhow::Result<Hotkeys> {
        let lock = match self.hotkeys.lock.as_deref() {
            None | Some("") => None,
            Some(name) => match key_by_name(name) {
                Some(code) => Some(code),
                None if name.contains('+') && pheme_input::is_wayland_session() => {
                    tracing::info!(
                        trigger = %name,
                        "hotkeys.lock is a portal shortcut trigger, not a key name: it is \
                         sent to the GlobalShortcuts portal as written, and no key is \
                         watched for it"
                    );
                    None
                }
                None if name.contains('+') => {
                    bail!(
                        "hotkeys.lock: {name:?} is a portal shortcut trigger, which only a \
                         Wayland server can use. Name a single key from pheme's key table \
                         instead (for example \"ScrollLock\")"
                    )
                }
                None => bail!("unknown key name for hotkeys.lock: {name:?}"),
            },
        };
        Ok(Hotkeys { lock })
    }

    /// The configured client placements; fails on a `span` outside `[0, 1]` or that is
    /// empty/reversed, which would otherwise silently make an edge unreachable.
    pub fn placements(&self) -> anyhow::Result<Vec<ClientPlacement>> {
        self.clients
            .iter()
            .map(|c| {
                let span = c.span.map(|s| (s[0], s[1])).unwrap_or((0.0, 1.0));
                if !(0.0..=1.0).contains(&span.0)
                    || !(0.0..=1.0).contains(&span.1)
                    || span.0 >= span.1
                {
                    bail!(
                        "client {:?}: span must satisfy 0 <= start < end <= 1, got [{}, {}]",
                        c.name,
                        span.0,
                        span.1
                    );
                }
                Ok(ClientPlacement {
                    name: c.name.clone(),
                    side: c.side.into(),
                    span,
                })
            })
            .collect()
    }

    /// Resolves the server address from the CLI override or `connect`, adding the default port.
    pub fn connect_addr(&self, override_host: Option<&str>) -> anyhow::Result<SocketAddr> {
        let host = override_host.or(self.connect.as_deref()).ok_or_else(|| {
            anyhow!("no server address: pass HOST or set `connect` in the config")
        })?;
        let with_port = if host.contains(':') {
            host.to_string()
        } else {
            format!("{host}:{DEFAULT_PORT}")
        };
        let mut addrs = with_port
            .to_socket_addrs()
            .with_context(|| format!("resolving {with_port}"))?;
        let v4 = addrs.find(|a| a.is_ipv4());
        match v4 {
            Some(a) => Ok(a),
            None => bail!("{with_port} did not resolve to an IPv4 address"),
        }
    }

    /// The server to connect to, as a target that is resolved on every attempt.
    ///
    /// `connect_addr` resolves once and returns an address; this returns the
    /// *question*, so the reconnect loop can ask it again after the answer
    /// changes. §4.3.
    pub fn connect_target(&self, override_host: Option<&str>) -> anyhow::Result<Target> {
        let host = override_host.or(self.connect.as_deref()).unwrap_or("");
        Target::parse(host)
    }

    /// Writes this configuration to `path`.
    ///
    /// Atomically: a temporary file in the same directory, then a rename. The
    /// rename is what makes it safe — writing in place means a crash or a full
    /// disk between truncating and finishing leaves a file that no longer
    /// parses, and the next start has no configuration at all. That is the one
    /// failure this path cannot afford, because its whole purpose is to keep
    /// the user out of a text editor.
    ///
    /// Comments in a hand-written file do not survive. The window warns before
    /// its first write; there is nothing to do about it here, since a TOML
    /// serializer has no comments to preserve.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let text = toml::to_string_pretty(self).context("serializing the configuration")?;
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        match std::fs::rename(&tmp, path) {
            Ok(()) => Ok(()),
            Err(e) => {
                // Leave nothing behind on the way out: a stray .tmp beside a
                // config file invites someone to wonder which one is real.
                let _ = std::fs::remove_file(&tmp);
                Err(anyhow::Error::from(e).context(format!("replacing {}", path.display())))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pheme_core::Side;
    use pheme_proto::KeyCode;

    const SAMPLE: &str = r#"
role = "server"
name = "desk"
listen = "0.0.0.0:24800"

[hotkeys]
lock = "ScrollLock"

[[clients]]
name = "lap"
side = "right"
span = [0.25, 0.75]

[[clients]]
name = "tv"
side = "top"
"#;

    #[test]
    fn parses_sample() {
        let c: Config = toml::from_str(SAMPLE).unwrap();
        assert_eq!(c.role, Role::Server);
        assert_eq!(c.name, "desk");
        assert_eq!(c.hotkeys().unwrap().lock, Some(KeyCode(0x47)));
        let p = c.placements().unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].side, Side::Right);
        assert_eq!(p[0].span, (0.25, 0.75));
        assert_eq!(p[1].side, Side::Top);
        assert_eq!(p[1].span, (0.0, 1.0));
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let c = Config::load(Some(&dir.path().join("nope.toml"))).unwrap();
        assert_eq!(c.role, Role::Server);
        assert!(!c.name.is_empty());
        assert_eq!(c.listen.port(), 24800);
        assert_eq!(c.hotkeys().unwrap().lock, Some(KeyCode(0x47)));
        assert!(c.placements().unwrap().is_empty());
    }

    #[test]
    fn unknown_key_and_bad_side_are_rejected() {
        assert!(toml::from_str::<Config>("bogus = 1").is_err());
        assert!(
            toml::from_str::<Config>("[[clients]]\nname = \"x\"\nside = \"diagonal\"").is_err()
        );
    }

    #[test]
    fn invalid_span_is_an_error() {
        for span in [
            "[0.5, 0.5]",
            "[0.75, 0.25]",
            "[-0.1, 1.0]",
            "[0.0, 1.5]",
            "[1.0, 1.2]",
        ] {
            let c: Config = toml::from_str(&format!(
                "[[clients]]\nname = \"x\"\nside = \"left\"\nspan = {span}"
            ))
            .unwrap();
            let err = c.placements().unwrap_err().to_string();
            assert!(err.contains("span"), "{span}: {err}");
        }
        let c: Config =
            toml::from_str("[[clients]]\nname = \"x\"\nside = \"left\"\nspan = [0.0, 0.5]")
                .unwrap();
        assert_eq!(c.placements().unwrap()[0].span, (0.0, 0.5));
    }

    #[test]
    fn a_portal_trigger_is_not_a_key_name() {
        // Only a Wayland server can use one, and this test binary is not guaranteed to
        // be running in a Wayland session -- so assert the half that holds either way:
        // it never becomes a KeyCode, and off Wayland it is refused outright.
        let c: Config = toml::from_str("[hotkeys]\nlock = \"CTRL+ALT+l\"").unwrap();
        match c.hotkeys() {
            Ok(h) => {
                assert!(pheme_input::is_wayland_session());
                assert_eq!(h.lock, None, "a trigger names no single key");
            }
            Err(e) => {
                assert!(!pheme_input::is_wayland_session());
                assert!(
                    e.to_string().contains("only a Wayland server can use"),
                    "unhelpful error: {e}"
                );
            }
        }
    }

    #[test]
    fn unknown_hotkey_name_is_an_error() {
        let c: Config = toml::from_str("[hotkeys]\nlock = \"NoSuchKey\"").unwrap();
        assert!(c.hotkeys().is_err());
        let c: Config = toml::from_str("[hotkeys]\nlock = \"\"").unwrap();
        assert_eq!(
            c.hotkeys().unwrap().lock,
            None,
            "empty string disables the hotkey"
        );
    }

    #[test]
    fn connect_addr_resolution() {
        let c: Config = toml::from_str("role = \"client\"\nconnect = \"127.0.0.1\"").unwrap();
        assert_eq!(
            c.connect_addr(None).unwrap(),
            "127.0.0.1:24800".parse().unwrap()
        );
        assert_eq!(
            c.connect_addr(Some("127.0.0.2:5000")).unwrap(),
            "127.0.0.2:5000".parse().unwrap()
        );
        let c: Config = toml::from_str("role = \"client\"").unwrap();
        assert!(c.connect_addr(None).is_err(), "no host anywhere");
    }

    #[test]
    fn an_absent_audio_section_means_defaults() {
        let cfg: Config = toml::from_str("role = \"server\"").unwrap();
        assert_eq!(cfg.audio, AudioCfg::default());
        assert_eq!(cfg.audio.playback_device, None);
        assert_eq!(cfg.audio.capture_device, None);
    }

    #[test]
    fn audio_devices_are_read_from_the_config() {
        let cfg: Config = toml::from_str(
            r#"
            role = "server"

            [audio]
            playback_device = "Speakers (Realtek)"
            capture_device = "CABLE-A Output"
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.audio.playback_device.as_deref(),
            Some("Speakers (Realtek)")
        );
        assert_eq!(cfg.audio.capture_device.as_deref(), Some("CABLE-A Output"));
    }

    #[test]
    fn a_partial_audio_section_is_valid() {
        let cfg: Config = toml::from_str("[audio]\nplayback_device = \"x\"\n").unwrap();
        assert_eq!(cfg.audio.playback_device.as_deref(), Some("x"));
        assert_eq!(cfg.audio.capture_device, None);
    }

    #[test]
    fn an_unknown_audio_key_is_rejected() {
        let e = toml::from_str::<Config>("[audio]\nenabled = true\n").unwrap_err();
        assert!(
            e.to_string().contains("enabled"),
            "audio is always on; there is no enable flag: {e}"
        );
    }

    #[test]
    fn the_microphone_device_is_read_from_the_config() {
        let cfg: Config = toml::from_str(
            r#"
            role = "server"
            name = "desk"
            [audio]
            mic_device = "alsa_input.usb-Blue_Yeti"
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.audio.mic_device.as_deref(),
            Some("alsa_input.usb-Blue_Yeti")
        );
    }

    #[test]
    fn an_absent_microphone_device_means_the_platform_default() {
        let cfg: Config = toml::from_str("role = \"server\"\nname = \"desk\"\n").unwrap();
        assert_eq!(cfg.audio.mic_device, None);
    }

    #[test]
    fn discovery_is_on_unless_it_is_turned_off() {
        let c: Config = toml::from_str("role = \"server\"").unwrap();
        assert!(c.discovery);
        let c: Config = toml::from_str("role = \"server\"\ndiscovery = false").unwrap();
        assert!(!c.discovery);
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn a_saved_config_loads_back_equal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut c = Config::default();
        c.name = "desk-linux".into();
        c.connect = Some("laptop-win".into());
        c.clients.push(ClientCfg {
            name: "laptop-win".into(),
            side: SideCfg::Right,
            span: Some([0.25, 0.75]),
        });
        c.save(&path).unwrap();
        assert_eq!(Config::load(Some(&path)).unwrap(), c);
    }

    #[test]
    fn saving_creates_a_missing_directory() {
        // The first run has no config directory at all, and that is the one
        // run this path exists for.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("deeper").join("config.toml");
        Config::default().save(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn saving_leaves_no_temporary_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save(&path).unwrap();
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left.len(), 1, "stray files: {left:?}");
    }

    #[test]
    fn a_failed_save_leaves_no_temporary_file_behind() {
        // The rename is what makes the write atomic, and when it fails the
        // temporary file must go with it — a stray .tmp beside a real config
        // invites someone to wonder which one is live. Simulated by making the
        // destination a directory, so the write succeeds and the rename fails.
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("config.toml");
        std::fs::create_dir(&blocked).unwrap();
        assert!(Config::default().save(&blocked).is_err());
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            left.len(),
            1,
            "a temporary file survived a failed save: {left:?}"
        );
    }
}
