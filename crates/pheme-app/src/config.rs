//! TOML configuration file.

use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};
use pheme_core::{ClientPlacement, Hotkeys, Side};
use pheme_input::keymap::key_by_name;
use pheme_net::DEFAULT_PORT;
use serde::{Deserialize, Serialize};

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
    #[serde(default)]
    pub hotkeys: HotkeysCfg,
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
            hotkeys: HotkeysCfg::default(),
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

    pub fn hotkeys(&self) -> anyhow::Result<Hotkeys> {
        let lock = match self.hotkeys.lock.as_deref() {
            None | Some("") => None,
            Some(name) => Some(
                key_by_name(name)
                    .ok_or_else(|| anyhow!("unknown key name for hotkeys.lock: {name:?}"))?,
            ),
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
}
