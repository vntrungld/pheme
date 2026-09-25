//! The platform socket under the IPC protocol.
//!
//! A Unix domain socket on Linux and a named pipe on Windows, both from
//! `tokio::net`, which this crate already depends on. The front-end listens
//! and the core connects, so the core never has to guess when the front-end
//! appeared and the front-end owns the lifetime. Sub-project 6 design §4.

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::proto::{decode_frame, encode_frame, Command, IpcError, Status, MAX_IPC_FRAME};

/// Distinguishes listeners within one process. The pid alone identifies a
/// front-end, which is what the path exists to keep apart — but a test binary
/// is one process with many listeners, and two of those must not land on one
/// socket either.
static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Reads one value, or `None` when the peer has closed the connection.
///
/// The buffer persists across calls because a read can stop mid-frame and a
/// single read can deliver two frames; `decode_frame` reports how much it
/// consumed so the remainder stays for the next call.
async fn recv<T, S>(stream: &mut S, buf: &mut Vec<u8>) -> Result<Option<T>, IpcError>
where
    T: for<'de> serde::Deserialize<'de>,
    S: AsyncReadExt + Unpin,
{
    loop {
        if let Some((value, used)) = decode_frame::<T>(buf)? {
            buf.drain(..used);
            return Ok(Some(value));
        }
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            // A clean close. Anything already buffered was a partial frame and
            // is discarded with the connection.
            return Ok(None);
        }
        if buf.len() + n > MAX_IPC_FRAME * 2 {
            return Err(IpcError::TooLong(buf.len() + n));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

async fn send<T, S>(stream: &mut S, value: &T) -> Result<(), IpcError>
where
    T: serde::Serialize,
    S: AsyncWriteExt + Unpin,
{
    let mut out = Vec::with_capacity(256);
    encode_frame(value, &mut out)?;
    stream.write_all(&out).await?;
    Ok(())
}

/// Listens on the platform socket. Only the front-end constructs this; the
/// core connects to the path it reports.
pub struct IpcListener {
    inner: platform::RawListener,
}

impl IpcListener {
    /// Binds a fresh socket at a path unique to this process and this
    /// listener within it.
    pub async fn bind() -> Result<IpcListener, IpcError> {
        let inner = platform::RawListener::bind().await?;
        Ok(IpcListener { inner })
    }

    /// Binds at an exact path, tolerating a stale socket file already there.
    ///
    /// Separated from `bind()` so the stale-file tolerance can be tested
    /// directly, against a path the test controls, rather than by relying on
    /// two `bind()` calls landing on the same generated path. Not used
    /// outside tests: `bind()` calls this crate's platform-level equivalent
    /// directly rather than going through here.
    #[cfg(all(test, unix))]
    pub(crate) fn bind_at(path: &std::path::Path) -> Result<IpcListener, IpcError> {
        let inner = platform::RawListener::bind_at(path)?;
        Ok(IpcListener { inner })
    }

    /// The path the core should connect to.
    pub fn path(&self) -> &str {
        self.inner.path()
    }

    /// Waits for the core to connect.
    pub async fn accept(&mut self) -> Result<IpcConnection, IpcError> {
        let stream = self.inner.accept().await?;
        Ok(IpcConnection {
            stream,
            buf: Vec::new(),
        })
    }
}

/// The front-end's end of one accepted connection.
pub struct IpcConnection {
    stream: platform::ListenerStream,
    buf: Vec<u8>,
}

impl IpcConnection {
    /// Sends one command to the core.
    pub async fn send_command(&mut self, c: Command) -> Result<(), IpcError> {
        send(&mut self.stream, &c).await
    }

    /// Reads one status from the core, or `None` if it has gone away.
    pub async fn recv_status(&mut self) -> Result<Option<Status>, IpcError> {
        recv(&mut self.stream, &mut self.buf).await
    }
}

/// The core's end of the connection to the front-end.
pub struct CoreLink {
    stream: platform::ClientStream,
    buf: Vec<u8>,
}

impl CoreLink {
    /// Connects to the front-end's listener at `path`.
    pub async fn connect(path: &str) -> Result<CoreLink, IpcError> {
        let stream = platform::connect(path).await?;
        Ok(CoreLink {
            stream,
            buf: Vec::new(),
        })
    }

    /// Sends one status to the front-end.
    pub async fn send_status(&mut self, s: &Status) -> Result<(), IpcError> {
        send(&mut self.stream, s).await
    }

    /// Reads one command from the front-end, or `None` if it has gone away.
    ///
    /// This is the signal the core's lifetime rule depends on: it exits when
    /// the front-end goes away, and can only tell "gone" from "broken" if a
    /// clean close comes back as `Ok(None)` rather than an `Err`.
    pub async fn recv_command(&mut self) -> Result<Option<Command>, IpcError> {
        recv(&mut self.stream, &mut self.buf).await
    }
}

#[cfg(unix)]
mod platform {
    use std::io;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use tokio::net::{UnixListener, UnixStream};

    use super::IpcError;

    /// The stream type handed to `IpcConnection` (the listener side).
    pub(super) type ListenerStream = UnixStream;
    /// The stream type handed to `CoreLink` (the connecting side).
    pub(super) type ClientStream = UnixStream;

    fn socket_path() -> PathBuf {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        base.join("pheme").join(format!(
            "ipc-{}-{}.sock",
            std::process::id(),
            super::next_id()
        ))
    }

    pub(super) struct RawListener {
        inner: UnixListener,
        path: String,
    }

    impl RawListener {
        pub(super) async fn bind() -> Result<Self, IpcError> {
            Self::bind_at(&socket_path())
        }

        /// Binds at an exact path, tolerating a stale socket file already
        /// there — left behind by a front-end that was killed rather than
        /// closed cleanly. Every path this crate generates is unique to one
        /// listener (pid and a per-process counter both feed into it), so
        /// finding a file already there is always a crash's leftovers, never
        /// a live listener; removing it unconditionally is what "tolerate"
        /// means here rather than something to retry around.
        pub(super) fn bind_at(path: &std::path::Path) -> Result<Self, IpcError> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
                // FINDING 7 (final review, security-adjacent): with no
                // `XDG_RUNTIME_DIR`, this directory is `/tmp/pheme`, shared
                // by every user on the machine. Left at the default 0755,
                // another local user can create the front-end's listener
                // path first (or simply watch this one appear) and accept
                // the connection the real core was meant to make, handing
                // that other user the front-end's `Lock`, `Unlock` and
                // `Stop` commands while the real core's own connection is
                // never accepted. 0700 makes the directory reachable only
                // by the user who owns it, closing that off -- applied
                // unconditionally, not only on first creation, so a
                // directory a previous, unpatched run already made at 0755
                // is tightened the next time anything binds under it too.
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
            }
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            let inner = UnixListener::bind(path)?;
            let path = path
                .to_str()
                .ok_or_else(|| io::Error::other("socket path is not valid UTF-8"))?
                .to_owned();
            Ok(Self { inner, path })
        }

        pub(super) fn path(&self) -> &str {
            &self.path
        }

        pub(super) async fn accept(&mut self) -> Result<ListenerStream, IpcError> {
            let (stream, _addr) = self.inner.accept().await?;
            Ok(stream)
        }
    }

    impl Drop for RawListener {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    pub(super) async fn connect(path: &str) -> Result<ClientStream, IpcError> {
        Ok(UnixStream::connect(path).await?)
    }
}

#[cfg(windows)]
mod platform {
    use std::time::Duration;

    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };

    use super::IpcError;

    /// The Windows error code for `ERROR_PIPE_BUSY`: the pipe exists but
    /// every instance currently has a client attached. Pulled in as a raw
    /// constant rather than a `windows-sys` dependency this crate does not
    /// otherwise need.
    const ERROR_PIPE_BUSY: i32 = 231;

    /// The stream type handed to `IpcConnection` (the listener side).
    pub(super) type ListenerStream = NamedPipeServer;
    /// The stream type handed to `CoreLink` (the connecting side).
    pub(super) type ClientStream = NamedPipeClient;

    fn pipe_path() -> String {
        format!(
            r"\\.\pipe\pheme-{}-{}",
            std::process::id(),
            super::next_id()
        )
    }

    pub(super) struct RawListener {
        path: String,
        // The instance waiting for the next client. Always `Some` between
        // calls to `accept`: a new one is created before this one is handed
        // to the caller, so a later connection always has something to land
        // on rather than racing the caller for a fresh instance.
        next: Option<NamedPipeServer>,
    }

    impl RawListener {
        pub(super) async fn bind() -> Result<Self, IpcError> {
            let path = pipe_path();
            let next = ServerOptions::new().create(&path)?;
            Ok(Self {
                path,
                next: Some(next),
            })
        }

        pub(super) fn path(&self) -> &str {
            &self.path
        }

        pub(super) async fn accept(&mut self) -> Result<ListenerStream, IpcError> {
            let server = self
                .next
                .take()
                .expect("a listener instance is always waiting between accepts");
            server.connect().await?;
            self.next = Some(ServerOptions::new().create(&self.path)?);
            Ok(server)
        }
    }

    // No `Drop` impl: unlike a Unix socket, a named pipe leaves nothing on
    // disk to clean up. The OS reclaims the pipe once every handle to it,
    // including the `next` instance still waiting, is closed.

    pub(super) async fn connect(path: &str) -> Result<ClientStream, IpcError> {
        loop {
            match ClientOptions::new().open(path) {
                Ok(client) => return Ok(client),
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    // Every instance is mid-handshake with another client;
                    // the documented pattern is a short retry rather than
                    // failing outright.
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bind_tolerates_a_stale_file_at_its_own_path() {
        // The real requirement, stated directly: a file already sitting at
        // the exact path `bind_at` is given — the kind a front-end killed
        // mid-run leaves behind — must not stop it from binding there.
        //
        // `bind_at` itself does no async work, but binding a tokio
        // `UnixListener` registers it with the reactor, which needs a Tokio
        // runtime in scope — hence `#[tokio::test]` rather than `#[test]`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stale.sock");
        std::fs::write(&path, b"not a socket").unwrap();

        let listener = IpcListener::bind_at(&path);
        assert!(
            listener.is_ok(),
            "a stale file at the target path should not block bind_at: {:?}",
            listener.err()
        );
    }

    /// FINDING 7 (final review, security-adjacent): the directory a socket
    /// is created under must be reachable only by the user who owns it, so
    /// another local user cannot get there first and receive `Lock`,
    /// `Unlock` and `Stop` commands meant for the real core. Uses a
    /// sub-directory `bind_at` has to create itself -- not the tempdir
    /// `tempfile` already made at 0700 on its own -- so this actually
    /// exercises the `create_dir_all` + `set_permissions` path rather than
    /// merely observing a permission this test's own fixture happened to
    /// set.
    #[tokio::test]
    async fn the_socket_directory_is_created_user_only() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("pheme_sockets");
        let path = dir.join("test.sock");

        let _listener = IpcListener::bind_at(&path).expect("bind_at");

        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "the socket directory must be created at 0700, not the default 0755"
        );
    }
}
