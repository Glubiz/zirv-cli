//! The protocol v1 transport (issue #353): a unix domain socket on unix, a
//! named pipe on Windows, carrying NDJSON in both directions.
//!
//! Deliberately mirrors `ctx::signal`'s platform split rather than inventing
//! a second one -- same `\\.\pipe\` naming discipline, same "the path under
//! the state dir is what a caller passes around" rule, same up-front length
//! check so an over-long state dir fails with a readable message instead of
//! an opaque OS error. Two things differ, both forced by what this endpoint
//! carries:
//!
//! - it is DUPLEX (a turn signal is one-way), so the Windows side creates
//!   `PIPE_ACCESS_DUPLEX` instances and blocks in `ConnectNamedPipe` on the
//!   accept thread instead of running the overlapped dance `signal.rs` needs
//!   to keep a supervisor's main loop free; and
//! - it is OWNER-ONLY by construction: the unix socket is created inside a
//!   0700 directory and chmod'ed 0600, the Windows pipe is created with an
//!   explicit DACL granting only the calling user's own SID, and on unix the
//!   server additionally verifies the peer's uid where the platform exposes
//!   it (`SO_PEERCRED`/`getpeereid`).
//!
//! The endpoint path is ALWAYS derived from the operator-owned state
//! directory ([`Endpoint::for_state`]). Nothing reads it from repository
//! configuration, and there is no flag or environment variable that points
//! the server or the client at an arbitrary path: issue #353's "never accept
//! repository-controlled endpoint paths or server policy", enforced by there
//! being no code that could.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::commands::ctx::CtxResult;
use crate::commands::ctx::state::{self, StateDir};

/// Per line, so one client cannot make the server buffer without bound. A
/// request is a few hundred bytes; a `session.start` prompt is the only
/// field that can be large.
pub const MAX_FRAME_BYTES: u64 = 1024 * 1024;

/// How long a client keeps retrying a Windows pipe that exists but has no
/// free instance (the accept loop is between connections), and how long a
/// unix client retries a socket file whose listener has not bound yet.
const CONNECT_RETRY: std::time::Duration = std::time::Duration::from_secs(2);
const POLL: std::time::Duration = std::time::Duration::from_millis(10);

/// Where the runtime API listens. Constructed only from a [`StateDir`], so
/// the endpoint can never be named by anything a checkout controls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    path: PathBuf,
}

impl Endpoint {
    /// `<state>/s/api.sock` -- a sibling of the per-session turn-signal
    /// sockets `StateDir::socket_for` names, in the same short `s`
    /// directory for the same reason (macOS caps `sun_path` near 104 bytes).
    pub fn for_state(state: &StateDir) -> Self {
        Self {
            path: state.sockets().join("api.sock"),
        }
    }

    /// Test seam: an endpoint at an explicit path. Not reachable from the
    /// CLI, from configuration, or from any deserialized value -- see this
    /// module's own doc comment.
    #[cfg(test)]
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A short, stable, human-readable label for logs and `--json` output.
    pub fn display(&self) -> String {
        state::display_path(&self.path)
    }
}

/// What the server could learn about who connected. `uid` is `None` where
/// the platform does not expose peer credentials on this transport (Windows,
/// or a unix target without `SO_PEERCRED`/`getpeereid`); the endpoint's own
/// permissions are the guarantee there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Peer {
    pub uid: Option<u32>,
}

impl Peer {
    /// Whether this peer may be served. A uid we could read must match the
    /// server's own effective uid; a uid we could not read is accepted,
    /// because the endpoint is already owner-only by permissions and
    /// refusing every connection on a platform without peer credentials
    /// would mean refusing every connection on Windows.
    pub fn is_same_user(&self, server_uid: Option<u32>) -> bool {
        match (self.uid, server_uid) {
            (Some(peer), Some(server)) => peer == server,
            _ => true,
        }
    }
}

/// The server's own effective uid, or `None` off unix.
pub fn server_uid() -> Option<u32> {
    #[cfg(unix)]
    {
        // SAFETY: `geteuid` takes no arguments, touches no memory and cannot
        // fail; it is the canonical way to ask for the current effective uid.
        Some(unsafe { libc::geteuid() })
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// One accepted or dialled connection, as NDJSON in both directions.
pub struct Connection {
    reader: BufReader<Box<dyn Read + Send>>,
    writer: Box<dyn Write + Send>,
    peer: Peer,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection").field("peer", &self.peer).finish()
    }
}

impl Connection {
    fn new(reader: Box<dyn Read + Send>, writer: Box<dyn Write + Send>, peer: Peer) -> Self {
        Self {
            reader: BufReader::new(reader),
            writer,
            peer,
        }
    }

    pub fn peer(&self) -> Peer {
        self.peer
    }

    /// Reads one NDJSON line. `Ok(None)` is a clean end of stream. A line
    /// longer than [`MAX_FRAME_BYTES`] is an error, not a silent truncation.
    pub fn read_line(&mut self) -> CtxResult<Option<String>> {
        let mut line = String::new();
        let read = (&mut self.reader)
            .take(MAX_FRAME_BYTES)
            .read_line(&mut line)?;
        if read == 0 {
            return Ok(None);
        }
        if read as u64 == MAX_FRAME_BYTES && !line.ends_with('\n') {
            return Err(format!("frame exceeds {MAX_FRAME_BYTES} bytes").into());
        }
        Ok(Some(line))
    }

    /// Reads one NDJSON line and parses it. A line that is not valid JSON
    /// for `T` is an error; a blank line is skipped.
    pub fn read_frame<T: DeserializeOwned>(&mut self) -> CtxResult<Option<T>> {
        loop {
            let Some(line) = self.read_line()? else {
                return Ok(None);
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            return Ok(Some(serde_json::from_str(trimmed)?));
        }
    }

    pub fn write_frame<T: Serialize>(&mut self, frame: &T) -> CtxResult<()> {
        let line = serde_json::to_string(frame)?;
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// unix
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod imp {
    use super::{CONNECT_RETRY, Connection, CtxResult, Endpoint, POLL, Peer, Path, state};
    use std::os::unix::io::AsRawFd;
    use std::os::unix::net::{UnixListener, UnixStream};

    fn check_len(path: &Path) -> CtxResult<()> {
        let len = path.as_os_str().len();
        let limit = crate::commands::ctx::signal::MAX_SOCKET_PATH;
        if len > limit {
            return Err(format!(
                "runtime API socket path is too long ({len} bytes, limit {limit}): {}",
                path.display()
            )
            .into());
        }
        Ok(())
    }

    /// The peer's uid, where this target exposes it. `None` means "this
    /// platform cannot tell us", never "the peer is somebody else".
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn peer_uid(stream: &UnixStream) -> Option<u32> {
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: `cred` and `len` are live, correctly sized locals; the fd
        // is owned by `stream` for the duration of the call.
        let rc = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                std::ptr::addr_of_mut!(cred).cast(),
                &mut len,
            )
        };
        if rc == 0 { Some(cred.uid) } else { None }
    }

    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    fn peer_uid(stream: &UnixStream) -> Option<u32> {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        // SAFETY: both out-parameters are live locals; the fd is owned by
        // `stream` for the duration of the call.
        let rc = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
        if rc == 0 { Some(uid) } else { None }
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd"
    )))]
    fn peer_uid(_stream: &UnixStream) -> Option<u32> {
        None
    }

    fn connection_from(stream: UnixStream) -> CtxResult<Connection> {
        let peer = Peer {
            uid: peer_uid(&stream),
        };
        let reader = stream.try_clone()?;
        Ok(Connection::new(Box::new(reader), Box::new(stream), peer))
    }

    #[derive(Debug)]
    pub struct Listener {
        listener: UnixListener,
        path: std::path::PathBuf,
    }

    impl Listener {
        pub fn bind(endpoint: &Endpoint) -> CtxResult<Self> {
            let path = endpoint.path();
            check_len(path)?;
            if let Some(parent) = path.parent() {
                // 0700: the whole state directory is already private, and
                // the socket's own directory is what actually gates reaching
                // it on targets that ignore socket permission bits.
                state::create_private_dir_all(parent)?;
            }
            if path.exists() {
                std::fs::remove_file(path)?;
            }
            let listener = UnixListener::bind(path)?;
            // 0600 belt to the 0700 directory's braces: Linux honours
            // permission bits on a socket inode, and the ones a default
            // umask leaves are wider than owner-only.
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            }
            Ok(Self {
                listener,
                path: path.to_path_buf(),
            })
        }

        pub fn accept(&self) -> CtxResult<Connection> {
            let (stream, _) = self.listener.accept()?;
            connection_from(stream)
        }

        /// Wakes a thread blocked in [`Self::accept`] so a server can stop.
        /// Connecting to our own socket is the portable way to do that; the
        /// accept loop notices its own shutdown flag and returns.
        pub fn wake(&self) {
            let _ = UnixStream::connect(&self.path);
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    pub fn connect(endpoint: &Endpoint) -> CtxResult<Connection> {
        let path = endpoint.path();
        check_len(path)?;
        let deadline = std::time::Instant::now() + CONNECT_RETRY;
        loop {
            match UnixStream::connect(path) {
                Ok(stream) => return connection_from(stream),
                Err(error) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(error.into());
                    }
                    std::thread::sleep(POLL);
                }
            }
        }
    }

    /// Whether anything is listening right now, without writing a byte.
    pub fn probe(endpoint: &Endpoint) -> bool {
        UnixStream::connect(endpoint.path()).is_ok()
    }
}

// ---------------------------------------------------------------------------
// windows
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use super::{CONNECT_RETRY, Connection, CtxResult, Endpoint, POLL, Peer, Path, state};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle};

    use windows_sys::Win32::Foundation::{
        ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
        TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
        PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    const PIPE_PREFIX: &str = r"\\.\pipe\";
    const PIPE_BUFFER_BYTES: u32 = 64 * 1024;

    /// The pipe a state-directory endpoint path names. Derived by hashing
    /// the path rather than reusing its file stem the way `signal::pipe_name`
    /// does: every state directory names its API socket `api.sock`, so a
    /// stem-derived name would make two different state directories (two
    /// tests, or an operator with `ZIRV_CTX_STATE_DIR` set) collide on one
    /// machine-wide pipe.
    pub fn pipe_name(path: &Path) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(path.to_string_lossy().to_lowercase().as_bytes());
        let digest = hasher.finalize();
        let hex: String = digest
            .iter()
            .take(8)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("{PIPE_PREFIX}zirv-api-{hex}")
    }

    fn wide(name: &str) -> Vec<u16> {
        std::ffi::OsStr::new(name)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// A `LocalAlloc`-owned security descriptor, freed on drop.
    struct OwnedDescriptor(PSECURITY_DESCRIPTOR);

    impl Drop for OwnedDescriptor {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: the pointer came from
                // `ConvertStringSecurityDescriptorToSecurityDescriptorW`,
                // which documents `LocalFree` as its release call, and is
                // freed exactly once.
                unsafe { LocalFree(self.0 as _) };
            }
        }
    }

    /// The calling user's own SID in SDDL string form.
    fn current_user_sid() -> CtxResult<String> {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: `GetCurrentProcess` returns a pseudo-handle that needs no
        // release; `token` is a live out-parameter.
        let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
        if ok == 0 {
            return Err(format!(
                "could not open the process token: {}",
                std::io::Error::last_os_error()
            )
            .into());
        }
        // SAFETY: a valid, exclusively owned token handle.
        let token = unsafe { OwnedHandle::from_raw_handle(token as _) };

        let mut needed: u32 = 0;
        // SAFETY: the documented two-call pattern -- a null buffer asks for
        // the required size and is expected to fail.
        unsafe {
            GetTokenInformation(
                token.as_raw_handle() as _,
                TokenUser,
                std::ptr::null_mut(),
                0,
                &mut needed,
            )
        };
        if needed == 0 {
            return Err("could not size the process token user".into());
        }
        let mut buffer = vec![0u8; needed as usize];
        // SAFETY: `buffer` is `needed` bytes long and live for the call.
        let ok = unsafe {
            GetTokenInformation(
                token.as_raw_handle() as _,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        };
        if ok == 0 {
            return Err(format!(
                "could not read the process token user: {}",
                std::io::Error::last_os_error()
            )
            .into());
        }
        // SAFETY: on success the buffer holds a `TOKEN_USER` whose `User.Sid`
        // points into that same buffer.
        let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };

        let mut raw: windows_sys::core::PWSTR = std::ptr::null_mut();
        // SAFETY: `sid` is valid for the call; `raw` is a live
        // out-parameter that receives a `LocalAlloc`ed string.
        let ok = unsafe { ConvertSidToStringSidW(sid, &mut raw) };
        if ok == 0 || raw.is_null() {
            return Err(format!(
                "could not format the process token user: {}",
                std::io::Error::last_os_error()
            )
            .into());
        }
        let mut len = 0usize;
        // SAFETY: `raw` is a NUL-terminated wide string from the call above.
        while unsafe { *raw.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: `raw` is valid for `len` u16s, as just measured.
        let text = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(raw, len) });
        // SAFETY: released exactly once, as `ConvertSidToStringSidW` documents.
        unsafe { LocalFree(raw as _) };
        Ok(text)
    }

    /// An owner-only DACL: a protected (`P`) DACL whose single ACE grants
    /// generic-all to the calling user's own SID. Protected so no inherited
    /// ACE can widen it, and single so nothing else -- not Everyone, not the
    /// anonymous account, both of which the DEFAULT named-pipe descriptor
    /// grants read access to -- can open the endpoint.
    fn owner_only_descriptor() -> CtxResult<OwnedDescriptor> {
        let sddl = format!("D:P(A;;GA;;;{})", current_user_sid()?);
        let wide_sddl = wide(&sddl);
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: `wide_sddl` is NUL-terminated and outlives the call;
        // `descriptor` is a live out-parameter.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide_sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || descriptor.is_null() {
            return Err(format!(
                "could not build the endpoint security descriptor: {}",
                std::io::Error::last_os_error()
            )
            .into());
        }
        Ok(OwnedDescriptor(descriptor))
    }

    fn create_instance(name: &str, descriptor: &OwnedDescriptor) -> CtxResult<OwnedHandle> {
        let wide_name = wide(name);
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        // SAFETY: `wide_name` is NUL-terminated and `attributes` (with the
        // descriptor it points at) outlives the call.
        let handle = unsafe {
            CreateNamedPipeW(
                wide_name.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                PIPE_BUFFER_BYTES,
                PIPE_BUFFER_BYTES,
                0,
                &mut attributes,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(format!(
                "could not create {name}: {}",
                std::io::Error::last_os_error()
            )
            .into());
        }
        // SAFETY: a valid, exclusively owned pipe handle nothing else has.
        Ok(unsafe { OwnedHandle::from_raw_handle(handle as _) })
    }

    fn connection_from(handle: OwnedHandle) -> CtxResult<Connection> {
        // SAFETY: `handle` is a valid pipe handle whose ownership moves into
        // the `File`; `into_raw_handle` gives up this `OwnedHandle`'s claim.
        let file = unsafe { std::fs::File::from_raw_handle(handle.into_raw_handle()) };
        let reader = file.try_clone()?;
        Ok(Connection::new(
            Box::new(reader),
            Box::new(file),
            // Windows named pipes expose the client's token through
            // impersonation rather than a peer-credential socket option; the
            // owner-only DACL above is what keeps another user out, and
            // `Peer::is_same_user` documents the `None` case.
            Peer { uid: None },
        ))
    }

    #[derive(Debug)]
    pub struct Listener {
        name: String,
        path: std::path::PathBuf,
        /// A created-but-not-yet-connected instance, so the pipe NAME exists
        /// from the moment `bind` returns rather than only once some thread
        /// gets around to calling `accept`. Without it a client (or
        /// [`super::probe`]) that tries immediately after `bind` sees
        /// `ERROR_FILE_NOT_FOUND` -- the Windows equivalent of a unix
        /// listener whose socket file does not exist yet, which
        /// `UnixListener::bind` never leaves open.
        spare: std::sync::Mutex<Option<OwnedHandle>>,
    }

    impl Listener {
        pub fn bind(endpoint: &Endpoint) -> CtxResult<Self> {
            let path = endpoint.path();
            let name = pipe_name(path);
            if let Some(parent) = path.parent() {
                state::create_private_dir_all(parent)?;
            }
            // Not the transport, just the same directory entry unix leaves
            // behind, so `zirv ctx api` can say where it listened and a
            // stale file is never mistaken for a live endpoint -- exactly
            // what `signal::SignalServer::bind` already does.
            state::write_private(path, &name)?;
            let descriptor = owner_only_descriptor()?;
            let spare = create_instance(&name, &descriptor)?;
            Ok(Self {
                name,
                path: path.to_path_buf(),
                spare: std::sync::Mutex::new(Some(spare)),
            })
        }

        fn take_or_create(&self) -> CtxResult<OwnedHandle> {
            if let Ok(mut spare) = self.spare.lock()
                && let Some(handle) = spare.take()
            {
                return Ok(handle);
            }
            let descriptor = owner_only_descriptor()?;
            create_instance(&self.name, &descriptor)
        }

        fn replace_spare(&self) {
            let Ok(descriptor) = owner_only_descriptor() else {
                return;
            };
            let Ok(handle) = create_instance(&self.name, &descriptor) else {
                return;
            };
            if let Ok(mut spare) = self.spare.lock() {
                *spare = Some(handle);
            }
        }

        pub fn accept(&self) -> CtxResult<Connection> {
            let instance = self.take_or_create()?;
            // SAFETY: `instance` is live for the call; a null overlapped
            // pointer is the documented blocking form on a synchronous pipe.
            let ok = unsafe { ConnectNamedPipe(instance.as_raw_handle() as _, std::ptr::null_mut()) };
            if ok == 0 {
                let error = std::io::Error::last_os_error();
                // A client that connected between `CreateNamedPipeW` and
                // `ConnectNamedPipe` is already connected, not an error.
                if error.raw_os_error() != Some(ERROR_PIPE_CONNECTED as i32) {
                    self.replace_spare();
                    return Err(format!("accept failed on {}: {error}", self.name).into());
                }
            }
            // Restored before this connection is handed back, so the pipe
            // name never disappears between two accepts.
            self.replace_spare();
            connection_from(instance)
        }

        /// Wakes a thread blocked in [`Self::accept`]. Opening and closing
        /// the client end is the only portable way to release a blocking
        /// `ConnectNamedPipe`.
        pub fn wake(&self) {
            let _ = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&self.name);
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    pub fn connect(endpoint: &Endpoint) -> CtxResult<Connection> {
        use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY};

        let name = pipe_name(endpoint.path());
        let deadline = std::time::Instant::now() + CONNECT_RETRY;
        loop {
            let error = match std::fs::OpenOptions::new().read(true).write(true).open(&name) {
                Ok(file) => {
                    let reader = file.try_clone()?;
                    return Ok(Connection::new(
                        Box::new(reader),
                        Box::new(file),
                        Peer { uid: None },
                    ));
                }
                Err(error) => error,
            };
            let transient = matches!(
                error.raw_os_error(),
                Some(code) if code == ERROR_PIPE_BUSY as i32 || code == ERROR_FILE_NOT_FOUND as i32
            );
            if !transient || std::time::Instant::now() >= deadline {
                return Err(error.into());
            }
            std::thread::sleep(POLL);
        }
    }

    pub fn probe(endpoint: &Endpoint) -> bool {
        use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;

        let name = pipe_name(endpoint.path());
        match std::fs::OpenOptions::new().read(true).write(true).open(&name) {
            Ok(_) => true,
            Err(error) => error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32),
        }
    }
}

// ---------------------------------------------------------------------------
// neither
// ---------------------------------------------------------------------------

#[cfg(not(any(unix, windows)))]
mod imp {
    use super::{Connection, CtxResult, Endpoint};

    #[derive(Debug)]
    pub struct Listener;

    impl Listener {
        pub fn bind(_endpoint: &Endpoint) -> CtxResult<Self> {
            Err("the runtime API needs a unix domain socket or a Windows named pipe".into())
        }

        pub fn accept(&self) -> CtxResult<Connection> {
            Err("the runtime API needs a unix domain socket or a Windows named pipe".into())
        }

        pub fn wake(&self) {}
    }

    pub fn connect(_endpoint: &Endpoint) -> CtxResult<Connection> {
        Err("the runtime API needs a unix domain socket or a Windows named pipe".into())
    }

    pub fn probe(_endpoint: &Endpoint) -> bool {
        false
    }
}

pub use imp::{Listener, connect, probe};

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #353: "never accept repository-controlled endpoint paths". The
    /// production constructor takes a [`StateDir`] and nothing else, so the
    /// endpoint always lands inside the operator-owned state directory.
    #[test]
    fn the_endpoint_always_sits_inside_the_state_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let endpoint = Endpoint::for_state(&state);
        assert!(
            endpoint.path().starts_with(state.root()),
            "{:?} escaped {:?}",
            endpoint.path(),
            state.root()
        );
        assert_eq!(endpoint.path(), state.sockets().join("api.sock"));
    }

    /// A uid we could read must match the server's; a uid the platform does
    /// not expose is not treated as a mismatch, or Windows could never
    /// connect to its own endpoint.
    #[test]
    fn peer_credentials_refuse_another_user_and_tolerate_an_unknown_one() {
        assert!(Peer { uid: Some(501) }.is_same_user(Some(501)));
        assert!(!Peer { uid: Some(502) }.is_same_user(Some(501)));
        assert!(Peer { uid: None }.is_same_user(Some(501)));
        assert!(Peer { uid: Some(502) }.is_same_user(None));
    }

    /// A round trip over the REAL transport of whichever platform this is
    /// running on: bind, connect, write a line each way, read it back.
    #[test]
    fn a_frame_round_trips_over_the_platform_transport() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let endpoint = Endpoint::at(tmp.path().join("s").join("api.sock"));
        let listener = Listener::bind(&endpoint).expect("bind");
        let server = std::thread::spawn(move || {
            let mut connection = listener.accept().expect("accept");
            let line: serde_json::Value = connection
                .read_frame()
                .expect("read")
                .expect("a frame, not EOF");
            connection
                .write_frame(&serde_json::json!({"echo": line}))
                .expect("write");
            // Keeping the listener alive until the client is done: dropping
            // it removes the endpoint file.
            drop(connection);
            drop(listener);
        });

        let mut client = connect(&endpoint).expect("connect");
        client
            .write_frame(&serde_json::json!({"hello": "there"}))
            .expect("write");
        let reply: serde_json::Value = client.read_frame().expect("read").expect("a frame");
        assert_eq!(reply["echo"]["hello"], serde_json::json!("there"));
        drop(client);
        server.join().expect("server thread");
    }

    /// `probe` answers "is anyone listening" without writing anything, and
    /// says no once the listener is gone.
    #[test]
    fn probe_reports_a_live_endpoint_and_then_a_dead_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let endpoint = Endpoint::at(tmp.path().join("s").join("api.sock"));
        assert!(!probe(&endpoint), "nothing is bound yet");
        let listener = Listener::bind(&endpoint).expect("bind");
        let accepting = std::thread::spawn(move || {
            let _ = listener.accept();
            listener
        });
        assert!(probe(&endpoint), "a bound endpoint answers");
        let listener = accepting.join().expect("accept thread");
        drop(listener);
        assert!(!probe(&endpoint), "a dropped listener stops answering");
    }

    /// On unix the endpoint must be owner-only on the filesystem too, not
    /// merely by peer credentials. Cannot run on Windows (there is no mode
    /// to read), where the owner-only DACL in `imp::owner_only_descriptor`
    /// is the equivalent guarantee.
    #[cfg(unix)]
    #[test]
    fn the_unix_endpoint_is_owner_only_on_disk() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let endpoint = Endpoint::at(tmp.path().join("s").join("api.sock"));
        let listener = Listener::bind(&endpoint).expect("bind");
        let socket_mode = std::fs::metadata(endpoint.path())
            .expect("stat socket")
            .permissions()
            .mode()
            & 0o777;
        let dir_mode = std::fs::metadata(
            endpoint
                .path()
                .parent()
                .expect("the socket has a parent directory"),
        )
        .expect("stat directory")
        .permissions()
        .mode()
            & 0o777;
        drop(listener);
        assert_eq!(socket_mode, 0o600, "the socket must be owner-only");
        assert_eq!(dir_mode, 0o700, "its directory must be owner-only");
    }

    /// The peer-credential check runs against a REAL connection from this
    /// same process, so the uid the platform reports must be our own. Cannot
    /// run on Windows, which exposes no peer uid on a named pipe.
    #[cfg(unix)]
    #[test]
    fn a_unix_peer_is_recognised_as_the_same_user() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let endpoint = Endpoint::at(tmp.path().join("s").join("api.sock"));
        let listener = Listener::bind(&endpoint).expect("bind");
        let server = std::thread::spawn(move || {
            let connection = listener.accept().expect("accept");
            let peer = connection.peer();
            drop(connection);
            drop(listener);
            peer
        });
        let client = connect(&endpoint).expect("connect");
        drop(client);
        let peer = server.join().expect("server thread");
        assert!(
            peer.is_same_user(server_uid()),
            "our own connection must pass the same-user check, got {peer:?}"
        );
    }
}
