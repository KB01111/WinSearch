//! Named-pipe server/client with a per-user security descriptor.
//!
//! The pipe is created with an explicit DACL granting access to the current
//! user only and with `PIPE_REJECT_REMOTE_CLIENTS`, so nothing else on the
//! machine — and nothing over the network — can talk to the engine.

use std::io::{self, Read, Write};

use tracing::{error, info, warn};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_PIPE_CONNECTED, GENERIC_ALL, GENERIC_READ,
    GENERIC_WRITE, HANDLE,
};
use windows::Win32::Security::{
    AddAccessAllowedAce, GetLengthSid, GetTokenInformation, InitializeAcl,
    InitializeSecurityDescriptor, SetSecurityDescriptorDacl, ACCESS_ALLOWED_ACE, ACL, ACL_REVISION,
    PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, TOKEN_QUERY, TOKEN_USER,
    TokenUser as TokenUserClass,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    ReadFile, WriteFile,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::PCWSTR;

/// `SECURITY_DESCRIPTOR_REVISION` from winnt.h.
const SECURITY_DESCRIPTOR_REVISION: u32 = 1;

const OUT_BUFFER_BYTES: u32 = 64 * 1024;
const IN_BUFFER_BYTES: u32 = 64 * 1024;

/// One connected pipe endpoint. Implements blocking `Read`/`Write`; the frame
/// codecs from `ws-protocol` operate on top of it.
pub struct PipeStream {
    handle: PipeHandle,
}

/// HANDLE wrapper: created by us, moved across a thread boundary exactly once,
/// closed exactly once on drop (which also disconnects the pipe instance).
struct PipeHandle(HANDLE);
unsafe impl Send for PipeHandle {}

impl Drop for PipeHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

impl PipeStream {
    /// Client side: connects to an existing engine pipe.
    pub fn connect(name: &str) -> io::Result<PipeStream> {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let handle = CreateFileW(
                PCWSTR(wide.as_ptr()),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
            .map_err(win32_io_error)?;
            Ok(PipeStream {
                handle: PipeHandle(handle),
            })
        }
    }
}

impl Read for PipeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut read = 0u32;
        unsafe {
            match ReadFile(self.handle.0, Some(buf), Some(&mut read), None) {
                Ok(()) => Ok(read as usize),
                Err(e) if is_disconnect(&e) => Ok(0),
                Err(e) => Err(win32_io_error(e)),
            }
        }
    }
}

impl Write for PipeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut written = 0u32;
        unsafe {
            WriteFile(self.handle.0, Some(buf), Some(&mut written), None)
                .map_err(win32_io_error)?;
        }
        Ok(written as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn is_disconnect(e: &windows::core::Error) -> bool {
    e.code() == ERROR_BROKEN_PIPE.to_hresult()
}

fn win32_io_error(e: windows::core::Error) -> io::Error {
    io::Error::from_raw_os_error(e.code().0)
}

/// Runs the accept loop on the calling thread. For each accepted connection a
/// thread is spawned that runs `on_connection` and then disconnects. The loop
/// only ends if a pipe instance cannot be created.
pub fn serve<F>(name: &str, on_connection: F) -> io::Result<()>
where
    F: Fn(PipeStream) + Send + Sync + 'static,
{
    let security = UserOnlySecurity::new()
        .map_err(|e| io::Error::other(e.to_string()))?;
    let wide_name: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let on_connection = std::sync::Arc::new(on_connection);

    info!(pipe = name, "pipe accept loop starting");
    loop {
        let handle = match unsafe { create_instance(&wide_name, &security) } {
            Ok(h) => h,
            Err(e) => {
                error!(error = %e, "failed to create named pipe instance");
                return Err(win32_io_error(e));
            }
        };

        if let Err(e) = unsafe { ConnectNamedPipe(handle, None) } {
            // A client can connect between CreateNamedPipe and ConnectNamedPipe;
            // ERROR_PIPE_CONNECTED means the connection is already established.
            if e.code() != ERROR_PIPE_CONNECTED.to_hresult() {
                unsafe {
                    let _ = CloseHandle(handle);
                }
                warn!(error = %e, "ConnectNamedPipe failed");
                continue;
            }
        }

        let stream = PipeStream {
            handle: PipeHandle(handle),
        };
        let handler = on_connection.clone();
        std::thread::spawn(move || handler(stream));
    }
}

unsafe fn create_instance(
    wide_name: &[u16],
    security: &UserOnlySecurity,
) -> Result<HANDLE, windows::core::Error> {
    let attrs = security.attributes();
    let pipe_mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
    let handle = CreateNamedPipeW(
        PCWSTR(wide_name.as_ptr()),
        PIPE_ACCESS_DUPLEX,
        pipe_mode,
        PIPE_UNLIMITED_INSTANCES,
        OUT_BUFFER_BYTES,
        IN_BUFFER_BYTES,
        0,
        Some(&attrs as *const SECURITY_ATTRIBUTES),
    );
    Ok(handle)
}

/// A DACL granting exactly one ACE: generic-all for the current user's SID.
struct UserOnlySecurity {
    /// Backing storage for TOKEN_USER (the SID points into it).
    _token_user_buf: Vec<u8>,
    /// Backing storage for the ACL (the SD's DACL points into it).
    _acl_buf: Vec<u8>,
    /// Backing storage for the SECURITY_DESCRIPTOR (attrs point into it).
    _sd: Box<SECURITY_DESCRIPTOR>,
}

impl UserOnlySecurity {
    fn new() -> Result<Self, windows::core::Error> {
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;

            let mut needed = 0u32;
            let _ = GetTokenInformation(token, TokenUserClass, None, 0, &mut needed);
            let mut token_user_buf = vec![0u8; needed as usize];
            let filled = GetTokenInformation(
                token,
                TokenUserClass,
                Some(token_user_buf.as_mut_ptr().cast()),
                needed,
                &mut needed,
            );
            let _ = CloseHandle(token);
            filled?;

            let token_user = &*(token_user_buf.as_ptr() as *const TOKEN_USER);
            let sid: PSID = token_user.User.Sid;
            let sid_len = GetLengthSid(sid) as usize;

            // ACL header + one ACCESS_ALLOWED_ACE (ACE header + mask + SID,
            // minus the ACE's SidStart u32 that overlaps the SID bytes).
            let acl_len = std::mem::size_of::<ACL>()
                + std::mem::size_of::<ACCESS_ALLOWED_ACE>()
                + sid_len
                - std::mem::size_of::<u32>();
            let mut acl_buf = vec![0u8; acl_len];
            let acl = acl_buf.as_mut_ptr() as *mut ACL;
            InitializeAcl(acl, acl_len as u32, ACL_REVISION)?;
            AddAccessAllowedAce(acl, ACL_REVISION, GENERIC_ALL.0, sid)?;

            let mut sd = Box::new(std::mem::zeroed::<SECURITY_DESCRIPTOR>());
            let sd_ptr = PSECURITY_DESCRIPTOR((&mut *sd as *mut SECURITY_DESCRIPTOR).cast());
            InitializeSecurityDescriptor(sd_ptr, SECURITY_DESCRIPTOR_REVISION)?;
            SetSecurityDescriptorDacl(sd_ptr, true, Some(acl as *const ACL), false)?;

            Ok(Self {
                _token_user_buf: token_user_buf,
                _acl_buf: acl_buf,
                _sd: sd,
            })
        }
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: &*self._sd as *const SECURITY_DESCRIPTOR as *mut _,
            bInheritHandle: false.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity;
    use ws_protocol::{read_message, write_message, ClientMessage, ServerMessage};

    #[test]
    fn client_server_round_trip_over_real_pipe() {
        let name = format!(r"\\.\pipe\winsearch-test-{}", std::process::id());
        let server_name = name.clone();

        std::thread::spawn(move || {
            serve(&server_name, |mut stream| {
                let msg: ClientMessage = read_message(&mut stream).unwrap().unwrap();
                if let ClientMessage::Ping { id } = msg {
                    write_message(&mut stream, &ServerMessage::Pong { id }).unwrap();
                }
            })
            .ok();
        });

        // Retry briefly while the server thread creates the pipe instance.
        let mut client = None;
        for _ in 0..100 {
            match PipeStream::connect(&name) {
                Ok(c) => {
                    client = Some(c);
                    break;
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        }
        let mut client = client.expect("client failed to connect");

        write_message(&mut client, &ClientMessage::Ping { id: 123 }).unwrap();
        let reply: ServerMessage = read_message(&mut client).unwrap().unwrap();
        assert_eq!(reply, ServerMessage::Pong { id: 123 });
    }

    #[test]
    fn sid_lookup_works() {
        let sid = identity::current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-"));
    }
}
