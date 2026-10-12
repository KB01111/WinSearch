//! Current-user identity: the user SID, formatted without allocating Win32
//! string APIs (the SID is parsed straight out of the token buffer we own).

use std::sync::OnceLock;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TokenUser, PSID, TOKEN_QUERY, TOKEN_USER};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("windows api error: {0}")]
    Win32(#[from] windows::core::Error),
    #[error("token user buffer was malformed")]
    MalformedSid,
}

/// Returns the current user's SID as an SDDL string, e.g. `S-1-5-21-...`.
/// Cached: the token identity cannot change for a process lifetime.
pub fn current_user_sid() -> Result<String, IdentityError> {
    static SID: OnceLock<String> = OnceLock::new();
    if let Some(s) = SID.get() {
        return Ok(s.clone());
    }
    let sid = read_token_sid()?;
    let _ = SID.set(sid.clone());
    Ok(sid)
}

fn read_token_sid() -> Result<String, IdentityError> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;

        let mut needed = 0u32;
        let probe = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
        debug_assert!(probe.is_err());
        if needed == 0 {
            let _ = CloseHandle(token);
            return Err(IdentityError::MalformedSid);
        }

        let mut buf = vec![0u8; needed as usize];
        let result = GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            needed,
            &mut needed,
        );
        let _ = CloseHandle(token);
        result?;

        let token_user = &*(buf.as_ptr() as *const TOKEN_USER);
        sid_to_string(token_user.User.Sid).ok_or(IdentityError::MalformedSid)
    }
}

/// Formats a SID (`revision`, identifier authority, subauthorities) as
/// `S-<rev>-<authority>-<sub0>-...`.
unsafe fn sid_to_string(sid: PSID) -> Option<String> {
    if sid.0.is_null() {
        return None;
    }
    let s = sid.0 as *const u8;
    let revision = *s;
    let sub_count = *s.add(1) as usize;
    let authority_bytes = std::slice::from_raw_parts(s.add(2), 6);
    let mut authority: u64 = 0;
    for b in authority_bytes {
        authority = (authority << 8) | *b as u64;
    }
    let sub_start = s.add(8) as *const u32;
    let mut out = format!("S-{revision}-{authority}");
    for i in 0..sub_count {
        out.push_str(&format!("-{}", *sub_start.add(i)));
    }
    Some(out)
}

/// The engine's named pipe is suffixed with the user SID so two accounts on one
/// machine never share a pipe.
pub fn pipe_name() -> Result<String, IdentityError> {
    Ok(format!(r"\\.\pipe\winsearch-{}", current_user_sid()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_plausible_current_user_sid() {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-"), "got {sid}");
        assert!(sid.split('-').count() >= 4);
    }

    #[test]
    fn pipe_name_embeds_sid() {
        let name = pipe_name().unwrap();
        assert!(name.starts_with(r"\\.\pipe\winsearch-S-1-"));
    }
}
