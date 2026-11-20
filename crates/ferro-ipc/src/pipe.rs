//! Named-pipe transport (SPEC §3.2).
//!
//! Server: overlapped pipe instances, one thread per connected client, every wait also
//! watches a stop event so the service can shut down while clients are idle.
//! Client: a plain blocking connection with impersonation limited to identification.

use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING, ERROR_NO_DATA, ERROR_PIPE_BUSY,
    ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE, HLOCAL, WAIT_OBJECT_0,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE,
    FlushFileBuffers, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, SECURITY_IDENTIFICATION,
    SECURITY_SQOS_PRESENT, WriteFile,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeServerSessionId,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
    PIPE_WAIT, WaitNamedPipeW,
};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects};
use windows::core::{Owned, PCWSTR};

use crate::Error;
use crate::frame::{read_message, write_message};
use crate::protocol::{ErrorCode, PROTOCOL_VERSION, Request, Response};

/// Pipe used by the service.
pub const PIPE_NAME: &str = r"\\.\pipe\ferrofs";
/// Environment variable that moves development servers and clients to another pipe, so
/// they do not meet an installed service (the service itself always uses [`PIPE_NAME`]).
pub const DEV_PIPE_ENV: &str = "FERROFS_DEV_PIPE";

/// The pipe for clients and development servers: `\\.\pipe\<FERROFS_DEV_PIPE>` when that
/// variable is set (a bare name such as `ferrofs-dev`), otherwise [`PIPE_NAME`].
pub fn pipe_name() -> String {
    match std::env::var(DEV_PIPE_ENV) {
        Ok(name) if !name.trim().is_empty() => {
            format!(r"\\.\pipe\{}", name.trim().trim_start_matches(r"\\.\pipe\"))
        }
        _ => PIPE_NAME.to_owned(),
    }
}
/// SYSTEM and administrators: full access; interactive users: read/write (SPEC §3.2).
pub const DEFAULT_SDDL: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)";

const BUFFER_SIZE: u32 = 64 * 1024;

fn win32_code(err: &windows::core::Error) -> Option<u32> {
    #[allow(clippy::cast_sign_loss)]
    let hresult = err.code().0 as u32;
    (hresult & 0xFFFF_0000 == 0x8007_0000).then_some(hresult & 0xFFFF)
}

fn to_io(err: windows::core::Error) -> io::Error {
    match win32_code(&err) {
        // Codes are below 0x10000, so they fit in i32.
        #[allow(clippy::cast_possible_wrap)]
        Some(code) => io::Error::from_raw_os_error(code as i32),
        None => io::Error::other(err),
    }
}

fn wide_nul(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain([0]).collect()
}

/// A manual-reset, unnamed Win32 event.
#[derive(Debug)]
struct Event(Owned<HANDLE>);

// SAFETY: event handles are kernel objects that may be waited on and signalled from any thread.
unsafe impl Send for Event {}
// SAFETY: see above; all operations on the handle are thread-safe system calls.
unsafe impl Sync for Event {}

impl Event {
    fn new() -> Result<Self, Error> {
        // SAFETY: unnamed manual-reset event with default security.
        let handle =
            unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.map_err(|source| {
                Error::Win32 {
                    context: "CreateEventW failed",
                    source,
                }
            })?;
        // SAFETY: the event was just created and is owned only here.
        Ok(Self(unsafe { Owned::new(handle) }))
    }

    fn raw(&self) -> HANDLE {
        *self.0
    }
}

/// Stops a running [`PipeServer::serve`]. Cheap to clone and `Send`.
#[derive(Debug, Clone)]
pub struct StopSignal(Arc<Event>);

impl StopSignal {
    /// Makes `serve` return after its client threads have finished their current I/O.
    pub fn stop(&self) {
        // SAFETY: the event handle is valid for the lifetime of the Arc.
        if let Err(err) = unsafe { SetEvent(self.0.raw()) } {
            tracing::error!("cannot signal the pipe server to stop: {err}");
        }
    }
}

/// Security descriptor allocated by `ConvertStringSecurityDescriptorToSecurityDescriptorW`.
#[derive(Debug)]
struct SecurityDescriptor(Owned<HLOCAL>);

// SAFETY: the descriptor is immutable after creation and only read by CreateNamedPipeW.
unsafe impl Send for SecurityDescriptor {}
// SAFETY: see above.
unsafe impl Sync for SecurityDescriptor {}

impl SecurityDescriptor {
    fn from_sddl(sddl: &str) -> Result<Self, Error> {
        let sddl = wide_nul(sddl);
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `sddl` is NUL-terminated; on success we own the LocalAlloc'd descriptor.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .map_err(|source| Error::Win32 {
            context: "invalid pipe SDDL",
            source,
        })?;
        // SAFETY: the descriptor was allocated with LocalAlloc and is freed by Owned.
        Ok(Self(unsafe { Owned::new(HLOCAL(descriptor.0)) }))
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
            lpSecurityDescriptor: self.0.0,
            bInheritHandle: false.into(),
        }
    }
}

/// Owned pipe handle that can be sent to a client thread.
#[derive(Debug)]
struct PipeHandle(Owned<HANDLE>);

// SAFETY: a pipe handle can be used from any thread; each instance is used by one thread.
unsafe impl Send for PipeHandle {}

/// Overlapped I/O on a server pipe instance; every wait is abandoned when `stop` fires.
struct ServerConn<'a> {
    pipe: &'a PipeHandle,
    stop: &'a Event,
    io: Event,
}

/// What a wait for an overlapped request produced.
enum Waited {
    Done(u32),
    Stopped,
}

impl ServerConn<'_> {
    /// Waits for the request tied to `overlapped`, cancelling it if the server stops.
    fn wait(&self, overlapped: &OVERLAPPED) -> io::Result<Waited> {
        let handle = *self.pipe.0;
        // SAFETY: both handles are valid events.
        let which =
            unsafe { WaitForMultipleObjects(&[self.io.raw(), self.stop.raw()], false, INFINITE) };
        let mut transferred = 0u32;
        if which == WAIT_OBJECT_0 {
            // SAFETY: `overlapped` belongs to the completed request on `handle`.
            unsafe { GetOverlappedResult(handle, overlapped, &mut transferred, false) }
                .map_err(to_io)?;
            return Ok(Waited::Done(transferred));
        }
        // SAFETY: cancel only this request, then wait for it so the kernel stops using
        // `overlapped` and the caller's buffer before we return.
        unsafe {
            let _ = CancelIoEx(handle, Some(overlapped));
            let _ = GetOverlappedResult(handle, overlapped, &mut transferred, true);
        }
        Ok(Waited::Stopped)
    }

    fn overlapped(&self) -> OVERLAPPED {
        OVERLAPPED {
            hEvent: self.io.raw(),
            ..Default::default()
        }
    }

    /// Waits for a client. Returns false if the server is stopping.
    fn connect(&self) -> io::Result<bool> {
        let mut overlapped = self.overlapped();
        // SAFETY: `overlapped` lives until `wait` has finished with it.
        match unsafe { ConnectNamedPipe(*self.pipe.0, Some(&mut overlapped)) } {
            Ok(()) => {}
            Err(err) if win32_code(&err) == Some(ERROR_PIPE_CONNECTED.0) => return Ok(true),
            Err(err) if win32_code(&err) == Some(ERROR_IO_PENDING.0) => {}
            Err(err) => return Err(to_io(err)),
        }
        Ok(matches!(self.wait(&overlapped)?, Waited::Done(_)))
    }
}

/// Not `Interrupted`: `read_exact` would silently retry that kind forever.
fn stopped() -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, "pipe server is stopping")
}

impl Read for ServerConn<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = buf.len().min(BUFFER_SIZE as usize);
        let mut overlapped = self.overlapped();
        // SAFETY: `buf` and `overlapped` stay alive until `wait` has completed the request.
        match unsafe {
            ReadFile(
                *self.pipe.0,
                Some(&mut buf[..len]),
                None,
                Some(&mut overlapped),
            )
        } {
            Ok(()) => {}
            Err(err) if win32_code(&err) == Some(ERROR_IO_PENDING.0) => {}
            Err(err) if win32_code(&err) == Some(ERROR_BROKEN_PIPE.0) => return Ok(0),
            Err(err) => return Err(to_io(err)),
        }
        match self.wait(&overlapped) {
            Ok(Waited::Done(n)) => Ok(n as usize),
            Ok(Waited::Stopped) => Err(stopped()),
            Err(err) if err.raw_os_error() == Some(109) => Ok(0), // ERROR_BROKEN_PIPE: client left
            Err(err) => Err(err),
        }
    }
}

impl Write for ServerConn<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let len = buf.len().min(BUFFER_SIZE as usize);
        let mut overlapped = self.overlapped();
        // SAFETY: `buf` and `overlapped` stay alive until `wait` has completed the request.
        match unsafe { WriteFile(*self.pipe.0, Some(&buf[..len]), None, Some(&mut overlapped)) } {
            Ok(()) => {}
            Err(err) if win32_code(&err) == Some(ERROR_IO_PENDING.0) => {}
            Err(err) => return Err(to_io(err)),
        }
        match self.wait(&overlapped)? {
            Waited::Done(n) => Ok(n as usize),
            Waited::Stopped => Err(stopped()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The service side of the pipe.
#[derive(Debug)]
pub struct PipeServer {
    name: Vec<u16>,
    security: SecurityDescriptor,
    stop: StopSignal,
}

impl PipeServer {
    /// Server for `name` (normally [`PIPE_NAME`]) with the default access rules.
    pub fn new(name: &str) -> Result<Self, Error> {
        Self::with_sddl(name, DEFAULT_SDDL)
    }

    /// Server with custom access rules (tests run unelevated and need to create instances).
    pub fn with_sddl(name: &str, sddl: &str) -> Result<Self, Error> {
        Ok(Self {
            name: wide_nul(name),
            security: SecurityDescriptor::from_sddl(sddl)?,
            stop: StopSignal(Arc::new(Event::new()?)),
        })
    }

    pub fn stop_signal(&self) -> StopSignal {
        self.stop.clone()
    }

    fn create_instance(&self, first: bool) -> Result<PipeHandle, Error> {
        let mut open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
        if first {
            // Fails if another process already owns the name (pipe squatting).
            open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        let attributes = self.security.attributes();
        // SAFETY: the name is NUL-terminated and `attributes` points to a live descriptor.
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(self.name.as_ptr()),
                open_mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                BUFFER_SIZE,
                BUFFER_SIZE,
                0,
                Some(&attributes),
            )
        };
        if handle.is_invalid() {
            return Err(Error::Win32 {
                context: "CreateNamedPipeW failed",
                source: windows::core::Error::from_thread(),
            });
        }
        // SAFETY: a fresh, valid handle that we own.
        Ok(PipeHandle(unsafe { Owned::new(handle) }))
    }

    /// Accepts clients until [`StopSignal::stop`] is called. Each client gets its own
    /// thread; `handler` answers every request after the version handshake.
    pub fn serve<H>(&self, service_version: &str, handler: &H) -> Result<(), Error>
    where
        H: Fn(Request) -> Response + Sync,
    {
        let stop = &*self.stop.0;
        thread::scope(|scope| -> Result<(), Error> {
            let mut first = true;
            loop {
                let pipe = self.create_instance(first)?;
                first = false;
                let conn = ServerConn {
                    pipe: &pipe,
                    stop,
                    io: Event::new()?,
                };
                if !conn.connect()? {
                    return Ok(());
                }
                drop(conn);
                thread::Builder::new()
                    .name("pipe-client".into())
                    .spawn_scoped(scope, move || {
                        if let Err(err) = serve_client(&pipe, stop, service_version, handler) {
                            tracing::debug!("client connection ended: {err}");
                        }
                    })
                    .map_err(Error::Io)?;
            }
        })
    }
}

fn serve_client<H>(
    pipe: &PipeHandle,
    stop: &Event,
    service_version: &str,
    handler: &H,
) -> Result<(), Error>
where
    H: Fn(Request) -> Response + Sync,
{
    let mut conn = ServerConn {
        pipe,
        stop,
        io: Event::new()?,
    };
    match read_message::<Request>(&mut conn)? {
        Request::Hello { protocol, .. } if protocol == PROTOCOL_VERSION => {
            let hello = Response::Hello {
                protocol: PROTOCOL_VERSION,
                service_version: service_version.to_owned(),
            };
            write_message(&mut conn, &hello)?;
        }
        Request::Hello { protocol, .. } => {
            let error = Response::Error {
                code: ErrorCode::ProtocolMismatch,
                message: format!(
                    "service speaks protocol {PROTOCOL_VERSION}, client speaks {protocol}"
                ),
            };
            write_message(&mut conn, &error)?;
            return Ok(());
        }
        _ => {
            let error = Response::Error {
                code: ErrorCode::BadRequest,
                message: "the first message must be Hello".into(),
            };
            write_message(&mut conn, &error)?;
            return Ok(());
        }
    }

    loop {
        let request = match read_message::<Request>(&mut conn) {
            Ok(request) => request,
            Err(Error::Io(err)) if err.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(err) => return Err(err),
        };
        let response = handler(request);
        write_message(&mut conn, &response)?;
    }
    // SAFETY: the pipe is valid; flushing before disconnect lets the client read the tail.
    unsafe {
        let _ = FlushFileBuffers(*pipe.0);
        let _ = DisconnectNamedPipe(*pipe.0);
    }
    Ok(())
}

/// Which pipe servers a client accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerTrust {
    /// Only a server running in session 0, i.e. a Windows service. Unelevated users cannot
    /// start processes there, so a fake server squatting on the pipe name is refused.
    ServiceSession,
    /// Any server, e.g. `ferro-service run --console` while debugging, or tests.
    Any,
}

/// The UI side of the pipe: a blocking connection to the service.
#[derive(Debug)]
pub struct Client {
    pipe: PipeHandle,
    service_version: String,
}

impl Client {
    /// Connects to `name` (normally [`PIPE_NAME`]) and performs the version handshake.
    /// Waits up to `timeout` while all pipe instances are busy.
    pub fn connect(
        name: &str,
        client: &str,
        timeout: Duration,
        trust: ServerTrust,
    ) -> Result<Self, Error> {
        Self::connect_with_protocol(name, client, timeout, trust, PROTOCOL_VERSION)
    }

    fn connect_with_protocol(
        name: &str,
        client: &str,
        timeout: Duration,
        trust: ServerTrust,
        protocol: u32,
    ) -> Result<Self, Error> {
        let name_w = wide_nul(name);
        let deadline = Instant::now() + timeout;
        let pipe = loop {
            // SAFETY: the name is NUL-terminated. SECURITY_IDENTIFICATION stops a rogue
            // server from impersonating the client beyond identifying it.
            let opened = unsafe {
                CreateFileW(
                    PCWSTR(name_w.as_ptr()),
                    (GENERIC_READ | GENERIC_WRITE).0,
                    FILE_SHARE_NONE,
                    None,
                    OPEN_EXISTING,
                    SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                    None,
                )
            };
            match opened {
                // SAFETY: a fresh handle that we own.
                Ok(handle) => break PipeHandle(unsafe { Owned::new(handle) }),
                Err(err) if win32_code(&err) == Some(ERROR_FILE_NOT_FOUND.0) => {
                    return Err(Error::NotRunning);
                }
                Err(err) if win32_code(&err) == Some(ERROR_PIPE_BUSY.0) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(Error::Timeout);
                    }
                    let ms = u32::try_from(left.as_millis())
                        .unwrap_or(INFINITE - 1)
                        .max(1);
                    // SAFETY: the name is NUL-terminated.
                    let _ = unsafe { WaitNamedPipeW(PCWSTR(name_w.as_ptr()), ms) };
                }
                Err(source) => {
                    return Err(Error::Win32 {
                        context: "cannot open the service pipe",
                        source,
                    });
                }
            }
        };

        if trust == ServerTrust::ServiceSession {
            let mut session = u32::MAX;
            // SAFETY: `pipe` is a connected client handle and `session` a valid out-pointer.
            unsafe { GetNamedPipeServerSessionId(*pipe.0, &mut session) }.map_err(|source| {
                Error::Win32 {
                    context: "cannot query the pipe server's session",
                    source,
                }
            })?;
            if session != 0 {
                return Err(Error::UntrustedServer(format!(
                    "the pipe server runs in session {session}, not as a Windows service"
                )));
            }
        }

        let mut this = Self {
            pipe,
            service_version: String::new(),
        };
        let hello = Request::Hello {
            protocol,
            client: client.to_owned(),
        };
        match this.request(&hello)? {
            Response::Hello {
                service_version, ..
            } => this.service_version = service_version,
            Response::Error {
                code: ErrorCode::ProtocolMismatch,
                message,
            } => return Err(Error::ProtocolMismatch(message)),
            _ => return Err(Error::UnexpectedResponse),
        }
        Ok(this)
    }

    pub fn service_version(&self) -> &str {
        &self.service_version
    }

    /// Sends one request and waits for its response.
    pub fn request(&mut self, request: &Request) -> Result<Response, Error> {
        let mut io = ClientIo(&self.pipe);
        write_message(&mut io, request)?;
        read_message(&mut io)
    }
}

struct ClientIo<'a>(&'a PipeHandle);

impl Read for ClientIo<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut read = 0u32;
        // SAFETY: synchronous read into a live buffer.
        match unsafe { ReadFile(*self.0.0, Some(buf), Some(&mut read), None) } {
            Ok(()) => Ok(read as usize),
            Err(err) if win32_code(&err) == Some(ERROR_BROKEN_PIPE.0) => Ok(0),
            Err(err) => Err(to_io(err)),
        }
    }
}

impl Write for ClientIo<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut written = 0u32;
        // SAFETY: synchronous write from a live buffer.
        match unsafe { WriteFile(*self.0.0, Some(buf), Some(&mut written), None) } {
            Ok(()) => Ok(written as usize),
            Err(err) if win32_code(&err) == Some(ERROR_NO_DATA.0) => {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }
            Err(err) => Err(to_io(err)),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::protocol::{SearchRequest, SearchResponse, Stats};

    /// Unelevated tests are not in BUILTIN\Administrators, so let interactive users create
    /// further instances too.
    const TEST_SDDL: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;IU)";

    fn unique_name() -> String {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        format!(
            r"\\.\pipe\ferrofs-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn echo(request: Request) -> Response {
        match request {
            Request::Search(search) => Response::Search(SearchResponse {
                rows: Vec::new(),
                total: u64::from(search.limit),
                took_us: 0,
                complete: true,
            }),
            Request::Stats => Response::Stats(Stats {
                volumes: 1,
                entries: 2,
                memory_bytes: 3,
                uptime_secs: 4,
            }),
            _ => Response::Error {
                code: ErrorCode::BadRequest,
                message: "unsupported".into(),
            },
        }
    }

    /// Runs a server on a background thread for the duration of `body`.
    fn with_server(body: impl FnOnce(&str)) {
        let name = unique_name();
        let server = PipeServer::with_sddl(&name, TEST_SDDL).unwrap();
        let stop = server.stop_signal();
        thread::scope(|scope| {
            let handle = scope.spawn(|| server.serve("9.9.9", &echo));
            // Wait until the first instance exists.
            let deadline = Instant::now() + Duration::from_secs(5);
            while matches!(
                Client::connect(&name, "probe", Duration::from_millis(50), ServerTrust::Any),
                Err(Error::NotRunning)
            ) {
                assert!(Instant::now() < deadline, "server did not start");
                thread::sleep(Duration::from_millis(10));
            }
            body(&name);
            stop.stop();
            handle.join().unwrap().unwrap();
        });
    }

    #[test]
    fn handshake_and_requests() {
        with_server(|name| {
            let mut client =
                Client::connect(name, "test", Duration::from_secs(2), ServerTrust::Any).unwrap();
            assert_eq!(client.service_version(), "9.9.9");
            for limit in [1, 50, 1000] {
                let response = client
                    .request(&Request::Search(SearchRequest::new("x", limit)))
                    .unwrap();
                assert!(matches!(response, Response::Search(r) if r.total == u64::from(limit)));
            }
            assert!(matches!(
                client.request(&Request::Stats).unwrap(),
                Response::Stats(Stats { entries: 2, .. })
            ));
        });
    }

    #[test]
    fn concurrent_clients() {
        with_server(|name| {
            thread::scope(|scope| {
                for i in 0..8u32 {
                    scope.spawn(move || {
                        let mut client =
                            Client::connect(name, "test", Duration::from_secs(5), ServerTrust::Any)
                                .unwrap();
                        for _ in 0..20 {
                            let response = client
                                .request(&Request::Search(SearchRequest::new("x", i)))
                                .unwrap();
                            assert!(
                                matches!(response, Response::Search(r) if r.total == u64::from(i))
                            );
                        }
                    });
                }
            });
        });
    }

    #[test]
    fn stops_while_a_client_is_idle() {
        let mut idle = None;
        with_server(|name| {
            idle = Some(
                Client::connect(name, "idle", Duration::from_secs(2), ServerTrust::Any).unwrap(),
            );
        });
        // The server returned although `idle` never disconnected; it now sees a closed pipe.
        let mut idle = idle.unwrap();
        assert!(idle.request(&Request::Stats).is_err());
    }

    #[test]
    fn rejects_protocol_mismatch() {
        with_server(|name| {
            let result = Client::connect_with_protocol(
                name,
                "old",
                Duration::from_secs(2),
                ServerTrust::Any,
                PROTOCOL_VERSION + 1,
            );
            assert!(matches!(result, Err(Error::ProtocolMismatch(m)) if m.contains("protocol")));
        });
    }

    #[test]
    fn refuses_server_outside_session_zero() {
        with_server(|name| {
            // Tests run in the interactive session, so this server is not a service.
            let result = Client::connect(
                name,
                "test",
                Duration::from_secs(2),
                ServerTrust::ServiceSession,
            );
            assert!(matches!(result, Err(Error::UntrustedServer(_))));
        });
    }

    #[test]
    fn reports_not_running() {
        let result = Client::connect(
            &unique_name(),
            "test",
            Duration::from_millis(100),
            ServerTrust::Any,
        );
        assert!(matches!(result, Err(Error::NotRunning)));
    }

    #[test]
    fn second_server_on_same_name_is_refused() {
        with_server(|name| {
            let squatter = PipeServer::with_sddl(name, TEST_SDDL).unwrap();
            assert!(squatter.create_instance(true).is_err());
        });
    }
}
