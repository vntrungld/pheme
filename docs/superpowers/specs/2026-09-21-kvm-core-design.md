# Sub-project 1 — KVM core

Ngày: 2026-09-21
Kiến trúc tổng: `2026-09-21-pheme-architecture-design.md`
Kết quả mong đợi: chạy `pheme server` trên máy A, `pheme client <ip>` trên
máy B, đưa chuột qua cạnh màn hình là điều khiển được B bằng bàn phím và
chuột của A; quay lại khi đi ngược. Windows và Linux X11 làm server;
Windows và Linux (uinput) làm client.

## 1. Phạm vi

Có:

- `pheme-proto`: đầy đủ `Msg` (kể cả biến thể Audio/Clipboard để không
  đổi wire format sau — chưa dùng).
- `pheme-net`: QUIC, identity, pairing SPAKE2, pin fingerprint, reconnect.
- `pheme-core`: layout, state machine, shadow key state, lock hotkey.
- `pheme-input`: keymap HID; capture Windows + Linux X11; inject Windows
  (SendInput) + Linux (uinput); backend `mock`.
- `pheme-app`: CLI `server`, `client`, `pair`, `setup`; config TOML; log.

Không: audio, clipboard, mDNS, Wayland capture, tray/GUI, macOS.

## 2. `pheme-proto`

```rust
pub const PROTOCOL_VERSION: u16 = 1;

pub enum Os { Linux, Windows, MacOs }

pub struct ScreenInfo { pub x: i32, pub y: i32, pub w: u32, pub h: u32, pub primary: bool }

pub struct Modifiers(u8);         // bit: Shift, Ctrl, Alt, Meta (trái/phải gộp)
pub struct KeyCode(pub u16);      // USB HID usage (page 0x07 keyboard; 0x0C consumer dùng bit 15)
pub enum Button { Left, Right, Middle, Back, Forward }
pub enum AudioStream { Playback, Mic }
pub struct AudioParams { pub rate: u32, pub channels: u8, pub frame_samples: u16 }

pub enum Msg {
    // Control stream (reliable, ordered)
    Hello    { version: u16, name: String, os: Os, screens: Vec<ScreenInfo> },
    HelloAck { version: u16, name: String, audio: AudioParams },
    Bye      { reason: String },
    Ping(u64), Pong(u64),
    Key      { seq: u32, code: KeyCode, down: bool },
    Button   { seq: u32, btn: Button, down: bool },
    Enter    { seq: u32, x: u16, y: u16, mods: Modifiers },
    Leave    { seq: u32 },
    // Datagram (unreliable)
    MouseMove{ seq: u32, dx: i16, dy: i16 },
    MouseAbs { seq: u32, x: u16, y: u16 },
    Wheel    { seq: u32, dx: i16, dy: i16 },       // 1/120 notch
    Audio    { stream: AudioStream, seq: u32, ts_us: u64, samples: Vec<i16> },
    // Clipboard stream
    Clipboard{ mime: String, data: Vec<u8> },
}

pub fn encode(m: &Msg, buf: &mut Vec<u8>);             // postcard, không alloc nếu buf đủ
pub fn decode(bytes: &[u8]) -> Result<Msg, ProtoError>;
impl Msg { pub fn is_datagram(&self) -> bool; }
```

`seq` là bộ đếm u32 riêng cho mỗi hướng, tăng mỗi message input; dùng để
log mất gói datagram (không dùng để resync). `Enter.x/y` là tọa độ trong
không gian client, đơn vị pixel, gốc góc trên-trái màn hình ảo của client.

Test: round-trip mọi variant; `MouseMove` encode ≤ 8 byte; `Key` ≤ 8 byte.

## 3. `pheme-net`

### Identity & trust

- `identity.key` (Ed25519 PKCS#8) + `identity.crt` (X.509 tự ký, CN =
  `name`) tạo bằng `rcgen` khi chưa có. Fingerprint = SHA-256(DER cert).
- `trusted.toml`: `[[peers]] name = "..." fingerprint = "hex"`.
- Verifier rustls tùy biến (server: `ClientCertVerifier`; client:
  `ServerCertVerifier`) chỉ so fingerprint với `trusted.toml`, bỏ qua CA
  chain, tên, thời hạn.

### Pairing

Chạy trên **cùng port QUIC** với ALPN riêng `pheme-pair/1` (ALPN chính
là `pheme/1`). Server chỉ chấp nhận ALPN pair khi đang ở chế độ pairing
(`pheme server --pair` hoặc mục menu tray sau này); chế độ này tự tắt sau
120 s hoặc sau một lần pairing thành công.

1. Server sinh mã 6 chữ số ngẫu nhiên, in ra.
2. Client mở kết nối QUIC với ALPN pair, cert chưa được trust → verifier
   ở chế độ pairing chấp nhận mọi cert nhưng ghi lại fingerprint.
3. Trên stream: SPAKE2 (crate `spake2`, nhóm Ed25519) với password = mã.
   Hai bên tính `K`.
4. Mỗi bên gửi `HMAC(K, fingerprint_của_mình || fingerprint_peer_thấy)`.
   Bên kia kiểm tra. Sai → đóng, server đếm 3 lần sai thì thoát chế độ
   pairing.
5. Đúng → cả hai ghi peer vào `trusted.toml`. Xong.

Mã chỉ dùng cho một handshake, brute-force online bị giới hạn 3 lần,
offline bất khả thi nhờ SPAKE2.

### Transport

```rust
pub struct Endpoint;                        // bọc quinn::Endpoint + config
impl Endpoint {
    pub fn server(cfg: &NetConfig, id: &Identity, trust: &TrustStore) -> Result<Self>;
    pub fn client(cfg: &NetConfig, id: &Identity, trust: &TrustStore) -> Result<Self>;
    pub async fn accept(&self) -> Result<Peer>;                     // server
    pub async fn connect(&self, addr: SocketAddr) -> Result<Peer>;  // client
}

pub struct Peer;
impl Peer {
    pub fn remote_name(&self) -> &str;
    pub async fn send_control(&self, m: &Msg) -> Result<()>;
    pub fn send_datagram(&self, m: &Msg);        // bỏ qua lỗi, log ở mức debug
    pub fn incoming(&self) -> &mpsc::Receiver<Msg>;  // gộp control + datagram
    pub fn rtt(&self) -> Duration;
    pub async fn closed(&self) -> CloseReason;
}
```

Tuning quinn: `max_idle_timeout = 5 s`, `keep_alive_interval = 1 s`,
`initial_rtt = 1 ms`, `datagram_send_buffer_size = 16 KiB`,
`datagram_receive_buffer_size = 64 KiB`, `max_concurrent_bidi_streams =
4`. Control stream: mỗi message = `u16 LE length` + postcard bytes.

Reconnect (client): vòng lặp `connect → run → closed → backoff` với
backoff 0.5 s ×2 tới trần 5 s; reset khi kết nối được ≥ 10 s.

Test: hai `Endpoint` trên `127.0.0.1` cùng process: (a) pairing đúng mã →
trusted có nhau; (b) sai mã → lỗi, không ghi trust; (c) client chưa
trust → server từ chối ở TLS; (d) round-trip control + datagram; (e) ngắt
server → `closed()` trả về trong < 6 s.

## 4. `pheme-core`

### Kiểu

```rust
pub enum Side { Left, Right, Top, Bottom }
pub struct ClientPlacement { pub name: String, pub side: Side, pub span: (f32, f32) }
pub struct Layout { pub server_screens: Vec<ScreenInfo>, pub clients: Vec<ClientPlacement> }

pub enum CaptureEvent {
    MotionAbs { x: i32, y: i32 },           // Observe mode
    MotionRel { dx: i32, dy: i32 },         // Grab mode
    Button { btn: Button, down: bool },
    Wheel { dx: i32, dy: i32 },
    Key { code: KeyCode, down: bool },
}

pub enum Action {
    SendControl(Msg), SendDatagram(Msg),
    Grab, Ungrab, WarpCursor { x: i32, y: i32 },
    SetLocked(bool),                        // để app cập nhật tray/log
}

pub struct ServerCore { .. }
impl ServerCore {
    pub fn new(layout: Layout, hotkeys: Hotkeys) -> Self;
    pub fn client_connected(&mut self, name: &str, screens: Vec<ScreenInfo>) -> Vec<Action>;
    pub fn client_disconnected(&mut self, name: &str) -> Vec<Action>;
    pub fn on_event(&mut self, ev: CaptureEvent) -> Vec<Action>;
    pub fn active(&self) -> Active;         // Local | Remote(name)
}

pub struct ClientCore { .. }                // theo dõi phím đang giữ; release_all khi Leave/disconnect
impl ClientCore {
    pub fn on_msg(&mut self, m: Msg) -> Vec<InjectAction>;
    pub fn on_disconnect(&mut self) -> Vec<InjectAction>;   // release mọi thứ đang giữ
}
```

### Hành vi `ServerCore`

- **Local**: chỉ xử lý `MotionAbs`. Với mỗi client, tính đoạn cạnh
  `[a, b]` trên hình chữ nhật bao của `server_screens` theo `side` và
  `span`. Nếu `x`/`y` nằm đúng biên (`x == min_x` với Left, `x == max_x-1`
  với Right, tương tự Top/Bottom), tọa độ dọc cạnh nằm trong `[a, b]`, và
  sự kiện trước đó có tọa độ *không* nằm trên biên đó (tức là đang tiến
  ra ngoài), và `!locked`, và client đang kết nối → chuyển Remote:
  - Tính vị trí vào phía client: chiếu vị trí dọc cạnh sang cạnh đối diện
    của client theo tỉ lệ `(pos - a) / (b - a)` × kích thước client; tọa
    độ vuông góc = 0 (Right → client x = 0; Left → x = client_w - 1; …).
  - Trả `[Grab, WarpCursor{center}, SendControl(Enter{x,y,mods})]`.
  - `mods` lấy từ shadow key state để client biết modifier đang giữ.
- **Remote(c)**: mọi sự kiện đều chuyển tiếp:
  - `MotionRel` → cập nhật vị trí ảo `(vx, vy)` trong không gian client
    (clamp vào màn hình client); `SendDatagram(MouseMove{dx,dy})`. Nếu vị
    trí ảo vượt cạnh đối diện của client (cạnh nối về server) →
    `[SendControl(Leave), Ungrab, WarpCursor{điểm tương ứng trên cạnh
    server}]`, về Local. Vị trí ảo ở các cạnh khác chỉ clamp, không rời.
  - `Key` → cập nhật shadow set; nếu là hotkey lock → toggle `locked`,
    `SetLocked`, *không* forward; ngược lại `SendControl(Key)`.
  - `Button`/`Wheel` → `SendControl(Button)` / `SendDatagram(Wheel)`.
- **Lock** ở Local: phím lock toggle `locked`; khi `locked`, không bao giờ
  chuyển Remote. Ở Remote, lock nghĩa là "ở lại client" — không rời khi
  vượt cạnh; bấm lại mới thả.
- `client_disconnected` khi đang Remote(c) → `[Ungrab, WarpCursor{center
  server}]`, về Local, shadow set giữ nguyên (phím vẫn đang được giữ vật
  lý; hook LL/XI2 sẽ báo release sau — nhưng vì ở Local ta không forward
  nên chỉ clear shadow khi nhận release).
- Khi về Local vì `Leave`, để tránh modifier kẹt phía server (server đã
  nuốt key-down khi Grab): với mỗi modifier trong shadow set,
  `InjectLocal` **không** cần — lý do: hook LL/XI2 grab chỉ chặn *forward
  tới app*, còn OS vẫn thấy key-up vật lý sau khi Ungrab. Nếu thực tế trên
  OS nào đó modifier vẫn kẹt, thêm `Action::ReleaseLocalModifiers` ở đó
  (ghi nhận là rủi ro cần verify thủ công ở §7).

### Hành vi `ClientCore`

- `Enter` → `MoveAbs(x,y)`; ghi `mods` để log (không inject modifier từ
  `mods` — key-down thật đã/đang được forward qua `Key`).
- `Key`/`Button` → cập nhật held set, inject.
- `Leave`, `Bye`, disconnect → `release_all()` cho mọi phím/nút trong
  held set, clear.
- `MouseMove`/`Wheel` → inject thẳng.

### Test (unit, không OS)

1. Right/left/top/bottom, `span` toàn phần và một phần: vào đúng vị trí,
   tọa độ chiếu đúng cả khi độ phân giải client khác server.
2. Con trỏ chạm cạnh mà sự kiện trước đã ở trên cạnh (kéo dọc theo cạnh)
   → không chuyển.
3. Cạnh không có client → không chuyển.
4. Client chưa kết nối → không chuyển.
5. Remote: vượt cạnh về → Leave + Ungrab + Warp đúng điểm; vượt cạnh khác
   → chỉ clamp.
6. Lock: ở Local chặn chuyển; ở Remote chặn rời; phím lock không forward.
7. Disconnect khi Remote → Ungrab, về Local.
8. `ClientCore`: giữ 3 phím + 1 nút rồi `Leave` → 4 release đúng thứ tự
   (nút trước, phím sau, modifier cuối).
9. Property test (`proptest`): chuỗi sự kiện ngẫu nhiên không bao giờ
   phát `Grab` hai lần liên tiếp hoặc `Ungrab` khi đang Local.

## 5. `pheme-input`

### Trait

```rust
pub enum CaptureMode { Observe, Grab }

pub trait InputCapture: Send {
    fn start(&mut self, tx: crossbeam::Sender<CaptureEvent>) -> Result<()>;
    fn set_mode(&mut self, mode: CaptureMode) -> Result<()>;
    fn warp_cursor(&mut self, x: i32, y: i32) -> Result<()>;
    fn screens(&self) -> Vec<ScreenInfo>;
    fn stop(&mut self);
}

pub trait InputInject: Send {
    fn mouse_move_rel(&mut self, dx: i32, dy: i32) -> Result<()>;
    fn mouse_move_abs(&mut self, x: i32, y: i32) -> Result<()>;
    fn button(&mut self, btn: Button, down: bool) -> Result<()>;
    fn wheel(&mut self, dx: i32, dy: i32) -> Result<()>;
    fn key(&mut self, code: KeyCode, down: bool) -> Result<()>;
    fn screens(&self) -> Vec<ScreenInfo>;
}

pub fn detect_capture() -> Result<Box<dyn InputCapture>>;  // theo OS/session
pub fn detect_inject()  -> Result<Box<dyn InputInject>>;
```

`set_mode(Grab)` phải: chặn bàn phím + chuột không tới app local, ẩn con
trỏ, ghim con trỏ (warp về giữa mỗi sự kiện hoặc `ClipCursor` 1×1), và
chuyển sang phát `MotionRel`. `set_mode(Observe)` hoàn tác tất cả.
`set_mode` được gọi từ thread app; backend phải chuyển yêu cầu sang thread
của nó (Windows: `PostThreadMessage`; X11: pipe/eventfd vào event loop).

### Keymap

`keymap/hid.rs`: hằng `KeyCode` cho mọi HID usage phổ biến.
`keymap/evdev.rs`: `hid_to_evdev(KeyCode) -> Option<u16>`,
`evdev_to_hid(u16) -> Option<KeyCode>` — bảng tĩnh sinh từ
`linux/input-event-codes.h`.
`keymap/win.rs`: `hid_to_scancode(KeyCode) -> Option<(u16, bool /*ext*/)>`
và ngược lại — bảng tĩnh từ USB HID Usage Tables §10 + Windows scancode
set 1. X11 keycode = evdev + 8.

Test: mọi entry round-trip; không hai HID map cùng scancode; các phím
khó (Pause, PrintScreen, NumLock, phím mũi tên extended, Right Ctrl/Alt,
Win/Meta, phím media cơ bản) có test riêng.

### Windows

Capture (`windows` crate):
- Thread riêng chạy message loop. `SetWindowsHookExW(WH_KEYBOARD_LL)` và
  `WH_MOUSE_LL`. Observe: hook chỉ đọc vị trí chuột từ `MSLLHOOKSTRUCT`,
  trả `CallNextHookEx`. Grab: hook trả `1` (nuốt) cho mọi sự kiện; đồng
  thời `RegisterRawInputDevices` (mouse, `RIDEV_INPUTSINK`) trên cửa sổ
  ẩn để lấy `dx/dy` thô (không bị pointer acceleration), `ClipCursor`
  vào hình chữ nhật 1×1 tại giữa màn hình, `ShowCursor(FALSE)` cho tới
  khi đếm < 0 (con trỏ hệ thống ẩn khi hook nuốt move nên thường không
  cần, verify thủ công).
- Bỏ qua sự kiện có `LLKHF_INJECTED` / `LLMHF_INJECTED`.
- Scancode từ `KBDLLHOOKSTRUCT.scanCode` + `LLKHF_EXTENDED`; Pause và
  PrintScreen có scancode đặc biệt, xử lý riêng.
- Wheel: `mouseData` HIWORD (signed, đơn vị 120) → giữ nguyên đơn vị.
- `screens()`: `EnumDisplayMonitors`; DPI-aware (`SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)`) để tọa độ là pixel vật lý.

Inject: `SendInput`. Key: `KEYEVENTF_SCANCODE` (+`EXTENDEDKEY`). Chuột
relative: `MOUSEEVENTF_MOVE` (Windows áp acceleration của client — chấp
nhận, vì bàn phím/chuột USB thật cũng vậy). Abs: `MOUSEEVENTF_ABSOLUTE |
VIRTUALDESK` với tọa độ chuẩn hóa 0..65535 trên virtual desktop. Wheel:
`MOUSEEVENTF_WHEEL`/`HWHEEL` với `mouseData` = giá trị 1/120.

### Linux X11

Capture (`x11rb` với extension `xinput`, `xtest`, `xfixes`, `randr`):
- Thread riêng với kết nối X riêng. `XISelectEvents` trên root với
  `XIAllMasterDevices`: Observe chọn `XI_Motion` (không raw) → đọc
  `root_x/root_y` trực tiếp, phát `MotionAbs`; Grab chọn `XI_RawMotion`,
  `XI_RawButtonPress/Release`, `XI_RawKeyPress/Release` → phát
  `MotionRel` từ `raw_values` (chưa acceleration) và phím/nút.
- Grab: `XIGrabDevice` cho master pointer + master keyboard với
  `owner_events = false` (app không nhận), `XFixesHideCursor` root, mỗi
  RawMotion → `XIWarpPointer` về giữa; delta lấy từ `raw_values` (chưa
  acceleration).
- Keycode X11 − 8 → evdev → HID.
- `screens()`: RandR CRTC.
- Wayland session (`$XDG_SESSION_TYPE == wayland`) → `detect_capture()`
  trả lỗi rõ ràng: "Wayland capture chưa hỗ trợ (sub-project 4); dùng
  session X11 hoặc chạy máy này làm client".

Inject (`evdev` crate, `/dev/uinput`):
- Một thiết bị uinput "Pheme Virtual Input" có `EV_KEY` (toàn bộ keycode
  bàn phím + BTN_LEFT/RIGHT/MIDDLE/SIDE/EXTRA), `EV_REL` (REL_X, REL_Y,
  REL_WHEEL, REL_HWHEEL, REL_WHEEL_HI_RES, REL_HWHEEL_HI_RES), `EV_ABS`
  (ABS_X, ABS_Y với range = màn hình ảo) — libinput chấp nhận thiết bị lai
  nếu có cả `INPUT_PROP_POINTER`; nếu thực tế libinput từ chối, tách
  thành 2 thiết bị (keyboard+rel mouse, abs tablet). Ghi nhận rủi ro.
- Wheel: gửi cả `REL_WHEEL_HI_RES` (1/120) và `REL_WHEEL` khi tích lũy đủ
  120.
- `screens()`: X11 → RandR; Wayland → chưa có API chuẩn, dùng
  `wl_output` qua `wayland-client` (chỉ đọc geometry, không cần quyền
  gì).
- Thiếu quyền `/dev/uinput` → lỗi kèm hướng dẫn chạy `pheme setup`.

### Mock

`MockCapture` (đẩy sự kiện từ test) và `MockInject` (ghi lại lời gọi vào
`Vec`) để test tích hợp `pheme-app`.

## 6. `pheme-app`

Runtime server:

```
tokio main
├── task accept: Endpoint::accept → Peer → HelloAck; chỉ 1 peer/client-name
├── thread capture (OS) ──crossbeam──► task router:
│      loop { ev = rx.recv(); for a in core.on_event(ev) { execute(a) } }
│      execute: SendControl → peer.send_control (spawn, không chờ)
│               SendDatagram → peer.send_datagram
│               Grab/Ungrab/Warp → capture.set_mode / warp_cursor
└── task peer reader: incoming → Ping/Pong, Bye → core.client_disconnected
```

Runtime client: `connect loop` → `Hello` → task reader: `core.on_msg` →
inject. Disconnect → `core.on_disconnect` → release_all → backoff.

CLI (`clap`): `server [--config] [--pair]`, `client <host[:port]>
[--config]`, `pair <host[:port]> <code>`, `setup`, `--stats`,
`-v/-vv`. Config TOML như spec tổng, chỉ các khóa dùng trong SP1 (`role`,
`name`, `listen`, `connect`, `hotkeys.lock`, `clients`). Vị trí mặc định
`~/.config/pheme/config.toml` / `%APPDATA%\pheme\config.toml`; không có
file → giá trị mặc định + tham số CLI.

`--stats`: mỗi 1 s log RTT (QUIC), số sự kiện gửi/nhận, datagram mất
(qua `seq` gap).

`pheme setup` (Linux): ghi udev rule, `usermod -aG input`, `udevadm
control --reload` — cần sudo, in lệnh nếu không có quyền. (Windows): SP1
không làm gì ngoài in "OK".

## 7. Tiêu chí hoàn thành

Tự động: `cargo test --workspace` xanh trên Linux và Windows (CI);
test tích hợp mock: server+client cùng process, 10 000 `MouseMove` qua
QUIC localhost, RTT trung bình < 0.5 ms, không mất gói.

Thủ công (`docs/testing.md`), cho cả 4 tổ hợp Linux X11/Windows ×
Linux/Windows:

1. Pair thành công; máy thứ ba không pair thử kết nối → bị từ chối.
2. Chuột qua cạnh phải → điều khiển client, quay lại qua cạnh trái client.
3. Kéo dọc theo cạnh không nhảy máy.
4. Gõ 100 ký tự gồm Shift/Ctrl/Alt combo, phím mũi tên, Numpad,
   PrintScreen, Pause — đúng và không kẹt.
5. Giữ Shift khi kéo qua cạnh, thả bên client → không kẹt ở cả hai máy.
6. Scroll dọc/ngang mượt (hi-res).
7. Lock hotkey: bật → không thể rời; tắt → rời được.
8. Rút mạng client khi đang Remote → server có lại chuột < 5 s; cắm lại →
   client tự kết nối lại < 10 s, không phím kẹt.
9. Client đa màn hình: `Enter` vào đúng màn hình, di chuyển qua tất cả.

## 8. Rủi ro đã biết

- Modifier kẹt phía server sau Ungrab (§4) — verify thủ công; fallback
  `ReleaseLocalModifiers`.
- libinput có thể không chấp nhận thiết bị uinput lai rel+abs — fallback
  tách 2 thiết bị.
- Windows: `ShowCursor` đếm tham chiếu theo thread — phải gọi trên đúng
  thread hook.
- X11 `XIGrabDevice` thất bại nếu app khác đang grab (menu đang mở) →
  retry 3 lần cách 10 ms rồi bỏ qua lần chuyển đó, ở lại Local.
