# Pheme — Kiến trúc tổng thể

Ngày: 2026-09-21
Trạng thái: đã được duyệt qua brainstorming, chờ review spec
License: GPL-3.0-only

## 1. Mục tiêu

Pheme là ứng dụng chia sẻ bàn phím/chuột kiểu Deskflow (chuyển máy khi
chuột chạm cạnh màn hình) kết hợp chuyển tiếp âm thanh hai chiều:

- **Server**: máy có bàn phím, chuột, loa, mic thật.
- **Client**: máy "câm" — nhận input từ server, âm thanh phát ra ở client
  được nghe trên loa server, mic của server hiện ra như một mic ảo trên
  client.

Ưu tiên theo thứ tự:

1. Độ trễ input thấp nhất có thể trên LAN.
2. Chất lượng audio cao nhất (bit-exact, không codec nén).
3. Cài đặt dễ trên Windows và Linux (X11 lẫn Wayland). macOS để sau.

Một máy bất kỳ có thể đóng vai server hoặc client (một binary, chọn vai
trò khi chạy). v1: 1 server + 1 client trên cùng LAN; protocol và layout
cho phép nhiều client nhưng chưa test.

## 2. Ngoài phạm vi v1

- macOS (trait cho phép thêm backend sau).
- Client kề client (chỉ client kề server).
- Hoạt động qua Internet/NAT.
- Driver âm thanh ảo cho Windows (dùng VB-CABLE, xem §7).
- Clipboard ảnh/file (v1 chỉ text).

## 3. Quyết định kỹ thuật chính

| Vấn đề | Quyết định | Lý do |
|---|---|---|
| Ngôn ngữ | Rust, Cargo workspace | Binary tĩnh, cross-compile dễ, hiệu năng, an toàn bộ nhớ |
| Transport | QUIC (`quinn` + `rustls`) | Datagram không retransmit cho input/audio, stream reliable cho control/clipboard, TLS 1.3 sẵn |
| Audio codec | PCM 48 kHz stereo i16, frame 5 ms, không nén | LAN dư băng thông (~1.5 Mbps/chiều); zero codec latency; bit-exact |
| Serialization | `postcard` | Gói input ~10 byte |
| Phím | USB HID usage code, map sang scancode/evdev ở backend | Layout do client quyết định như bàn phím USB thật |
| Linux inject | uinput | Chạy trên X11, Wayland, mọi compositor; cài = 1 udev rule |
| Linux capture | X11: XInput2 + XTest; Wayland: xdg-portal InputCapture/libei (GNOME/KDE), layer-shell (wlroots) | X11 làm trước; Wayland là sub-project riêng |
| Windows input | LL hooks + Raw Input (capture), SendInput (inject) | Chuẩn, giống Deskflow |
| Audio Linux | PipeWire (`pipewire-rs`): node ảo `Audio/Sink` + `Audio/Source` trên client | Không driver, người dùng chọn trong OS |
| Audio Windows | WASAPI (`wasapi` crate); mic ảo qua VB-CABLE | Windows không có API tạo thiết bị ảo userspace |
| Đồng bộ clock | Jitter buffer thích ứng + resample bù drift (`rubato`) | Giữ độ trễ ổn định, không click |
| GUI | `tray-icon` + `eframe/egui` | Pure Rust, không kéo webkit2gtk |
| Auth | Pairing bằng mã 6 số qua SPAKE2, sau đó pin cert fingerprint (mTLS) | Máy lạ trên LAN bị từ chối ở tầng TLS |
| Discovery | mDNS `_pheme._udp.local` (`mdns-sd`) | Không cần gõ IP |

Tham khảo khi triển khai: `lan-mouse` (Rust, GPLv3 — được phép mượn code),
Deskflow (GPL-2.0-only — chỉ tham khảo cách gọi API, không copy).

## 4. Cấu trúc workspace

```
pheme/
├── Cargo.toml                 workspace
├── crates/
│   ├── pheme-proto/           Msg enum, postcard encode/decode, version
│   ├── pheme-net/             QUIC transport, pairing, mDNS, reconnect
│   ├── pheme-input/           trait InputCapture / InputInject + backend theo OS
│   │   └── src/{keymap,windows,linux_x11,linux_uinput,linux_wayland,mock}/
│   ├── pheme-audio/           trait AudioCapture / AudioPlayback / VirtualSource,
│   │   └── src/{jitter,drift,pipewire,wasapi,mock}/
│   ├── pheme-core/            ScreenLayout, ActiveScreen state machine,
│   │                          shadow key state, clipboard sync — KHÔNG cfg(target_os)
│   └── pheme-app/             binary `pheme`: CLI, config TOML, tray + egui
└── docs/
    ├── superpowers/specs/     spec từng sub-project
    └── testing.md             checklist thủ công cross-OS
```

Nguyên tắc:

- `pheme-core` là logic thuần: `fn on_event(&mut self, ev) -> Vec<Action>`;
  không gọi OS, test được trên mọi nền tảng.
- Backend OS chỉ hiện thực trait; `pheme-app` chọn backend bằng cfg lúc
  compile và auto-detect lúc runtime (`$WAYLAND_DISPLAY`,
  `$XDG_SESSION_TYPE`).
- Mỗi crate có backend `mock` để test tích hợp không đụng OS.

## 5. Luồng dữ liệu

```
SERVER                                          CLIENT
InputCapture ─► core::Router ─► QUIC ──────────► core::Applier ─► InputInject
AudioPlayback ◄─ Jitter ◄─ QUIC datagram ◄────── AudioCapture (Pheme Speaker / loopback)
AudioCapture(mic) ─► QUIC datagram ─► Jitter ──► VirtualSource (Pheme Mic)
Clipboard ◄──────── QUIC stream (2 chiều) ─────► Clipboard
Control   ◄──────── QUIC bi-stream ────────────► Control
```

Threading: tokio cho network/control. Input capture chạy trên thread của
OS (hook thread / X event loop), đẩy vào channel bounded không khóa, task
network gửi ngay, không batch. Audio callback (realtime) chỉ đọc/ghi ring
buffer lock-free (`rtrb`), không allocate, không chạm tokio.

## 6. Protocol (tóm tắt — chi tiết ở spec sub-project 1)

Kênh trên một kết nối QUIC:

| Kênh | Loại | Nội dung |
|---|---|---|
| Control | bi-stream đầu tiên | Hello, HelloAck, Ping/Pong, Key, Button, Enter, Leave, Bye |
| Input motion | datagram | MouseMove (relative), Wheel |
| Audio Out | datagram | AudioFrame{Playback} |
| Audio Mic | datagram | AudioFrame{Mic} |
| Clipboard | uni-stream mỗi lần thay đổi | ClipboardData |

`Key`/`Button`/`Enter`/`Leave` đi qua stream reliable để không cần cơ
chế resync trạng thái phím; chỉ chuyển động chuột và audio đi datagram
(mất gói chấp nhận được).

## 7. Audio — luôn bật

Audio chạy ngay khi có kết nối, không có cờ bật/tắt. Client hiện ra thiết
bị ảo; người dùng chọn chúng trong cài đặt âm thanh OS. Server dùng
output/input mặc định (override được bằng tên device trong config).

| | Linux (PipeWire) | Windows |
|---|---|---|
| Loa client → server | Node `Pheme Speaker` (`Audio/Sink`) | Loopback WASAPI trên output mặc định (người dùng chọn device bất kỳ; âm thanh vẫn phát tại client nếu device có loa). Tùy chọn: chỉ định `audio.capture_device` (vd. `CABLE-A Output` khi có 2 dây) để client câm hoàn toàn |
| Mic server → client | Node `Pheme Mic` (`Audio/Source`) | Render vào `CABLE Input` (VB-CABLE, miễn phí); app dùng `CABLE Output` làm mic |

Khi mất kết nối, thiết bị ảo vẫn tồn tại (Speaker nuốt, Mic phát im
lặng) để OS không nhảy default; kết nối lại là chảy tiếp.

Pipeline mỗi chiều: capture callback → ring → task network đóng gói 240
mẫu/kênh + seq + ts → datagram → JitterBuffer (mục tiêu 10 ms, tự tăng
tới 40 ms, PLC lặp-fade khi mất gói) → drift resampler (`rubato`, ±0.1 %)
→ ring → playback callback. Độ trễ dự kiến 20–25 ms mỗi chiều.

## 8. Bảo mật

- Identity: cert Ed25519 tự ký, tạo lần đầu chạy, lưu
  `~/.config/pheme/identity.*` (Windows: `%APPDATA%\pheme\`).
- Pairing: server hiện mã 6 số; client `pheme pair <host> <code>`; SPAKE2
  với mã → shared key → xác nhận fingerprint bằng HMAC. Mã dùng một lần.
- Sau pairing: fingerprint peer lưu `trusted.toml`; rustls pin
  fingerprint hai chiều (mTLS). Kết nối từ cert lạ bị từ chối.

## 9. Lộ trình sub-project

Mỗi sub-project có spec + plan riêng, xây trên nền của cái trước.

1. **KVM core** — proto, net (QUIC, pairing, reconnect), core state
   machine, lock hotkey, input Windows + Linux X11 capture + uinput
   inject, CLI. Kết quả: dùng được bàn phím/chuột qua cạnh màn hình.
2. **Audio out** — client → server: PipeWire sink / WASAPI loopback,
   jitter buffer, drift, playback trên server.
3. **Mic ảo** — server → client: PipeWire source / VB-CABLE render.
4. **Wayland capture** — portal InputCapture + libei (GNOME/KDE),
   layer-shell (Hyprland/Sway).
5. **Clipboard text + mDNS.**
6. **Tray + egui config GUI**, installer Windows (Inno Setup), systemd
   user unit, CI release.

## 10. Kiểm thử

| Tầng | Cách |
|---|---|
| proto | round-trip mọi variant; gói input ≤ 16 B, audio ≤ 1200 B |
| core | unit test state machine: vào/ra cạnh với `span`, đổi tọa độ, lock, release phím khi disconnect, không nhảy khi vector không hướng ra ngoài |
| input keymap | round-trip HID ↔ evdev ↔ Windows scancode, không trùng |
| net | 2 endpoint quinn localhost: pairing đúng/sai mã, từ chối cert lạ, reconnect |
| audio | JitterBuffer với chuỗi mất/đảo/trễ mô phỏng; drift resampler giữ độ trễ ổn định với clock lệch 0.1 % |
| tích hợp | server + client cùng process với backend `mock` qua QUIC thật; đo RTT input |

**Ma trận cross-OS bắt buộc trước mỗi release** (thủ công, `docs/testing.md`):

| Server → Client | Input | Audio out | Mic | Clipboard |
|---|---|---|---|---|
| Linux X11 → Windows | ✔ | ✔ | ✔ | ✔ |
| Windows → Linux (session X11 và Wayland) | ✔ | ✔ | ✔ | ✔ |
| Linux → Linux | ✔ | ✔ | ✔ | ✔ |
| Windows → Windows | ✔ | ✔ | ✔ | ✔ |

Tiêu chí đạt (ví dụ): gõ 100 phím không kẹt; giữ Shift kéo qua cạnh không
dính; rút mạng client → server lấy lại chuột < 5 s; audio không click,
không drift sau 30 phút; RTT input < 1 ms trên cáp.

Đo lường: `tracing`; cờ `--stats` in mỗi 1 s RTT, loss, jitter depth, CPU.

## 11. Cài đặt & phân phối

- Linux: binary (link động libpipewire, libX11), tarball; `pheme setup`
  ghi udev rule `/etc/udev/rules.d/80-pheme.rules`
  (`KERNEL=="uinput", MODE="0660", GROUP="input", TAG+="uaccess"`) và
  thêm user vào group `input`; systemd `--user` unit mẫu. AUR/Flatpak sau.
- Windows: `.exe` + Inno Setup, tùy chọn khởi động cùng Windows, link cài
  VB-CABLE; `pheme setup` kiểm tra VB-CABLE có mặt.
- CI: GitHub Actions build + test Linux/Windows, release theo tag.

## 12. CLI & config

```
pheme server [--config path]
pheme client <host|name> [--config]
pheme pair <host> <code>
pheme discover
pheme setup
pheme devices
pheme                       # tray + config, vai trò từ config (SP6)
```

```toml
role = "server"                # "server" | "client"
name = "desk-linux"
listen = "0.0.0.0:24800"       # server
connect = "laptop-win"         # client: tên mDNS hoặc IP

[hotkeys]
lock = "ScrollLock"

[[clients]]                    # server
name = "laptop-win"
side = "right"                 # left | right | top | bottom
span = [0.0, 1.0]              # tùy chọn: đoạn cạnh

[audio]                        # tùy chọn; rỗng = device mặc định OS
capture_device = ""            # Windows client: rỗng = loopback default output
virtual_mic_device = "CABLE Input"
playback_device = ""           # server
mic_device = ""                # server
```
