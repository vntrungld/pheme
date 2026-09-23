//! Wire protocol for Pheme: message types and postcard encoding.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Os {
    Linux,
    Windows,
    MacOs,
}

impl Os {
    pub fn current() -> Os {
        if cfg!(target_os = "windows") {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::MacOs
        } else {
            Os::Linux
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenInfo {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub primary: bool,
}

/// USB HID usage code, keyboard/keypad page (0x07).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct KeyCode(pub u16);

impl KeyCode {
    pub const LEFT_CTRL: KeyCode = KeyCode(0xE0);
    pub const LEFT_SHIFT: KeyCode = KeyCode(0xE1);
    pub const LEFT_ALT: KeyCode = KeyCode(0xE2);
    pub const LEFT_GUI: KeyCode = KeyCode(0xE3);
    pub const RIGHT_CTRL: KeyCode = KeyCode(0xE4);
    pub const RIGHT_SHIFT: KeyCode = KeyCode(0xE5);
    pub const RIGHT_ALT: KeyCode = KeyCode(0xE6);
    pub const RIGHT_GUI: KeyCode = KeyCode(0xE7);

    pub fn is_modifier(self) -> bool {
        (0xE0..=0xE7).contains(&self.0)
    }
}

/// Modifier bitmask; left and right variants are merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Modifiers(pub u8);

impl Modifiers {
    pub const SHIFT: u8 = 1;
    pub const CTRL: u8 = 2;
    pub const ALT: u8 = 4;
    pub const META: u8 = 8;

    pub fn from_held(held: impl IntoIterator<Item = KeyCode>) -> Modifiers {
        let mut m = 0;
        for k in held {
            m |= match k {
                KeyCode::LEFT_SHIFT | KeyCode::RIGHT_SHIFT => Self::SHIFT,
                KeyCode::LEFT_CTRL | KeyCode::RIGHT_CTRL => Self::CTRL,
                KeyCode::LEFT_ALT | KeyCode::RIGHT_ALT => Self::ALT,
                KeyCode::LEFT_GUI | KeyCode::RIGHT_GUI => Self::META,
                _ => 0,
            };
        }
        Modifiers(m)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Button {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioStream {
    Playback,
    Mic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioParams {
    pub rate: u32,
    pub channels: u8,
    pub frame_samples: u16,
}

impl AudioParams {
    pub const DEFAULT: AudioParams = AudioParams {
        rate: 48_000,
        channels: 2,
        frame_samples: 240,
    };
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Msg {
    // Control stream (reliable, ordered)
    Hello {
        version: u16,
        name: String,
        os: Os,
        screens: Vec<ScreenInfo>,
        /// What this peer speaks. `HelloAck` carries the server's; this carries the
        /// client's, so each end can refuse a format it does not understand instead of
        /// transmitting into one.
        audio: AudioParams,
    },
    HelloAck {
        version: u16,
        name: String,
        audio: AudioParams,
    },
    Bye {
        reason: String,
    },
    /// Client → server: whether anything on the client is recording from its virtual
    /// microphone. Sent once after the handshake and again on every change.
    ///
    /// Control stream, not a datagram: a lost or reordered demand signal would leave the
    /// microphone stranded open or stranded shut, and it is sent a handful of times per
    /// session.
    MicWanted {
        wanted: bool,
    },
    Ping(u64),
    Pong(u64),
    Key {
        seq: u32,
        code: KeyCode,
        down: bool,
    },
    Button {
        seq: u32,
        btn: Button,
        down: bool,
    },
    Enter {
        seq: u32,
        x: u16,
        y: u16,
        mods: Modifiers,
    },
    Leave {
        seq: u32,
    },
    // Datagrams (unreliable)
    MouseMove {
        seq: u32,
        dx: i16,
        dy: i16,
    },
    MouseAbs {
        seq: u32,
        x: u16,
        y: u16,
    },
    /// Units of 1/120 notch.
    Wheel {
        seq: u32,
        dx: i16,
        dy: i16,
    },
    Audio {
        stream: AudioStream,
        seq: u32,
        ts_us: u64,
        /// Interleaved i16 little-endian PCM bytes (channels × frame_samples × 2).
        samples: Vec<u8>,
    },
    // Clipboard stream
    Clipboard {
        mime: String,
        data: Vec<u8>,
    },
}

impl Msg {
    pub fn is_datagram(&self) -> bool {
        matches!(
            self,
            Msg::MouseMove { .. } | Msg::MouseAbs { .. } | Msg::Wheel { .. } | Msg::Audio { .. }
        )
    }
}

#[derive(Debug, thiserror::Error)]
#[error("protocol decode error: {0}")]
pub struct ProtoError(#[from] postcard::Error);

/// Encodes `m` into `buf`, replacing its contents. Reuses `buf`'s allocation.
pub fn encode(m: &Msg, buf: &mut Vec<u8>) {
    buf.clear();
    let taken = std::mem::take(buf);
    *buf = postcard::to_extend(m, taken).expect("postcard encoding of Msg cannot fail");
}

pub fn decode(bytes: &[u8]) -> Result<Msg, ProtoError> {
    Ok(postcard::from_bytes(bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(m: Msg) -> usize {
        let mut buf = Vec::new();
        encode(&m, &mut buf);
        let back = decode(&buf).expect("decode");
        assert_eq!(back, m);
        buf.len()
    }

    #[test]
    fn every_variant_roundtrips() {
        roundtrip(Msg::Hello {
            version: PROTOCOL_VERSION,
            name: "desk".into(),
            os: Os::Linux,
            screens: vec![ScreenInfo {
                x: 0,
                y: 0,
                w: 2560,
                h: 1440,
                primary: true,
            }],
            audio: AudioParams::DEFAULT,
        });
        roundtrip(Msg::HelloAck {
            version: 1,
            name: "lap".into(),
            audio: AudioParams::DEFAULT,
        });
        roundtrip(Msg::Bye {
            reason: "bye".into(),
        });
        roundtrip(Msg::Ping(7));
        roundtrip(Msg::Pong(7));
        roundtrip(Msg::Key {
            seq: 1,
            code: KeyCode(0x04),
            down: true,
        });
        roundtrip(Msg::Button {
            seq: 2,
            btn: Button::Middle,
            down: false,
        });
        roundtrip(Msg::Enter {
            seq: 3,
            x: 10,
            y: 20,
            mods: Modifiers(Modifiers::SHIFT),
        });
        roundtrip(Msg::Leave { seq: 4 });
        roundtrip(Msg::MouseMove {
            seq: 5,
            dx: -3,
            dy: 4,
        });
        roundtrip(Msg::MouseAbs { seq: 6, x: 1, y: 2 });
        roundtrip(Msg::Wheel {
            seq: 7,
            dx: 0,
            dy: -120,
        });
        roundtrip(Msg::Audio {
            stream: AudioStream::Mic,
            seq: 8,
            ts_us: 123,
            samples: vec![1, 255, 0],
        });
        roundtrip(Msg::Clipboard {
            mime: "text/plain".into(),
            data: b"hi".to_vec(),
        });
    }

    #[test]
    fn input_messages_are_small() {
        assert!(
            roundtrip(Msg::MouseMove {
                seq: 100,
                dx: -5,
                dy: 3
            }) <= 8
        );
        assert!(
            roundtrip(Msg::Key {
                seq: 100,
                code: KeyCode(0xE1),
                down: true
            }) <= 8
        );
    }

    #[test]
    fn audio_frame_fits_a_datagram() {
        let n = roundtrip(Msg::Audio {
            stream: AudioStream::Playback,
            seq: 1,
            ts_us: u64::MAX,
            samples: vec![0xFF; 960],
        });
        assert!(n <= 1200, "audio frame is {n} bytes");
    }

    #[test]
    fn datagram_classification() {
        assert!(Msg::MouseMove {
            seq: 0,
            dx: 0,
            dy: 0
        }
        .is_datagram());
        assert!(Msg::Wheel {
            seq: 0,
            dx: 0,
            dy: 0
        }
        .is_datagram());
        assert!(Msg::MouseAbs { seq: 0, x: 0, y: 0 }.is_datagram());
        assert!(Msg::Audio {
            stream: AudioStream::Mic,
            seq: 0,
            ts_us: 0,
            samples: vec![]
        }
        .is_datagram());
        assert!(!Msg::Key {
            seq: 0,
            code: KeyCode(0),
            down: true
        }
        .is_datagram());
        assert!(!Msg::Enter {
            seq: 0,
            x: 0,
            y: 0,
            mods: Modifiers(0)
        }
        .is_datagram());
    }

    #[test]
    fn modifiers_from_held_keys() {
        let held = [KeyCode::LEFT_SHIFT, KeyCode::RIGHT_ALT, KeyCode(0x04)];
        let m = Modifiers::from_held(held.iter().copied());
        assert_eq!(m.0, Modifiers::SHIFT | Modifiers::ALT);
        assert!(KeyCode::LEFT_GUI.is_modifier());
        assert!(!KeyCode(0x04).is_modifier());
    }

    #[test]
    fn decode_garbage_is_an_error() {
        assert!(decode(&[0xFF, 0xFF, 0xFF]).is_err());
    }

    #[test]
    fn mic_wanted_round_trips_and_is_not_a_datagram() {
        let m = Msg::MicWanted { wanted: true };
        let mut buf = Vec::new();
        encode(&m, &mut buf);
        assert_eq!(decode(&buf).unwrap(), m);
        assert!(
            !m.is_datagram(),
            "a lost demand signal would strand the microphone open or shut"
        );
    }

    #[test]
    fn hello_carries_the_audio_parameters() {
        let m = Msg::Hello {
            version: PROTOCOL_VERSION,
            name: "laptop".into(),
            os: Os::Linux,
            screens: Vec::new(),
            audio: AudioParams::DEFAULT,
        };
        let mut buf = Vec::new();
        encode(&m, &mut buf);
        match decode(&buf).unwrap() {
            Msg::Hello { audio, .. } => assert_eq!(audio, AudioParams::DEFAULT),
            other => panic!("expected Hello, got {other:?}"),
        }
    }

    #[test]
    fn the_protocol_version_is_two() {
        // Bumped when Hello gained `audio`. Note that a version-1 Hello now fails to
        // decode before its `version` field can be read, so a mismatched peer reports a
        // malformed handshake rather than a version mismatch.
        assert_eq!(PROTOCOL_VERSION, 2);
    }

    #[test]
    fn a_mic_audio_frame_round_trips_like_a_playback_one() {
        let m = Msg::Audio {
            stream: AudioStream::Mic,
            seq: 7,
            ts_us: 35_000,
            samples: vec![0u8; 960],
        };
        let mut buf = Vec::new();
        encode(&m, &mut buf);
        assert_eq!(decode(&buf).unwrap(), m);
        assert!(m.is_datagram());
        assert!(
            buf.len() <= 1200,
            "a frame must fit the datagram budget: {} bytes",
            buf.len()
        );
    }
}
