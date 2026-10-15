//! Windows IFilter COM extraction helper for legacy/unknown document formats
//! (e.g. `.doc`, `.xls`, `.ppt`, or third-party formats with registered shell
//! filters).
//!
//! Uses `LoadIFilter` (`query.dll`) + `IFilter::Init` + `GetChunk` / `GetText`
//! to stream plain text chunks with a strict character cap and wall-clock
//! deadline so buggy or hung third-party COM filters cannot stall extraction.
//! If no filter is registered for the file extension (`E_FAIL` /
//! `FILTER_E_UNKNOWNFORMAT` / `REGDB_E_CLASSNOTREG`), returns
//! [`IFilterOutcome::Unsupported`] so the caller degrades cleanly to
//! exact-name-only indexing without generating garbage embeddings.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::time::Instant;

use windows::core::{HRESULT, Interface, IUnknown, PCWSTR, PWSTR};
use windows::Win32::Foundation::{S_FALSE, S_OK};
use windows::Win32::Storage::IndexServer::{
    IFilter, LoadIFilter, CHUNK_TEXT, FILTER_E_END_OF_CHUNKS, FILTER_E_NO_MORE_TEXT,
    FILTER_E_NO_TEXT, FILTER_E_NO_VALUES, FILTER_S_LAST_TEXT,
    IFILTER_INIT_APPLY_INDEX_ATTRIBUTES, IFILTER_INIT_CANON_HYPHENS,
    IFILTER_INIT_CANON_PARAGRAPHS, IFILTER_INIT_CANON_SPACES, IFILTER_INIT_HARD_LINE_BREAKS,
    STAT_CHUNK,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

/// Outcome of attempting to extract text from a file via Windows IFilter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IFilterOutcome {
    /// Text was extracted via a registered IFilter.
    Extracted(String),
    /// No IFilter is registered for this file format or the format is unrecognized.
    Unsupported,
    /// Deadline expired while streaming chunks from the IFilter.
    DeadlineExceeded,
    /// An IFilter was loaded, but failed during initialization or streaming.
    Failed(String),
}

struct ComGuard {
    should_uninit: bool,
}

impl ComGuard {
    fn init() -> Self {
        // S_OK (0) or S_FALSE (1) means this thread now holds a COM init ref;
        // RPC_E_CHANGED_MODE means STA is already initialized on this thread,
        // which still allows in-proc IFilter calls without uninitializing.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let should_uninit = hr == S_OK || hr == S_FALSE;
        Self { should_uninit }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.should_uninit {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

/// Extracts plain text from `path` via Windows `LoadIFilter`, collecting at
/// most `max_chars` characters and aborting if `Instant::now() >= deadline`.
pub fn extract_text_via_ifilter(
    path: &Path,
    max_chars: usize,
    deadline: Instant,
) -> IFilterOutcome {
    if Instant::now() >= deadline {
        return IFilterOutcome::DeadlineExceeded;
    }
    if !path.is_file() {
        return IFilterOutcome::Unsupported;
    }

    let _com = ComGuard::init();

    let wide_path: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let mut raw_unk: *mut c_void = std::ptr::null_mut();
    let load_res = unsafe {
        LoadIFilter(
            PCWSTR(wide_path.as_ptr()),
            None::<&IUnknown>,
            &mut raw_unk,
        )
    };
    if load_res.is_err() || raw_unk.is_null() {
        return IFilterOutcome::Unsupported;
    }

    let unk: IUnknown = unsafe { IUnknown::from_raw(raw_unk) };
    let filter: IFilter = match unk.cast() {
        Ok(f) => f,
        Err(_) => return IFilterOutcome::Unsupported,
    };

    if Instant::now() >= deadline {
        return IFilterOutcome::DeadlineExceeded;
    }

    let init_flags = (IFILTER_INIT_CANON_PARAGRAPHS.0
        | IFILTER_INIT_HARD_LINE_BREAKS.0
        | IFILTER_INIT_CANON_HYPHENS.0
        | IFILTER_INIT_CANON_SPACES.0
        | IFILTER_INIT_APPLY_INDEX_ATTRIBUTES.0) as u32;
    let mut flags_out = 0u32;
    let init_hr = HRESULT(unsafe { filter.Init(init_flags, &[], &mut flags_out) });
    if init_hr.is_err() {
        // Password-protected, corrupt, or unsupported sub-format.
        return IFilterOutcome::Failed(format!("IFilter::Init failed: {init_hr:?}"));
    }

    let mut out = String::new();
    let mut buf = vec![0u16; 4096];

    loop {
        if Instant::now() >= deadline {
            return IFilterOutcome::DeadlineExceeded;
        }
        if out.len() >= max_chars {
            break;
        }

        let mut stat = STAT_CHUNK::default();
        let chunk_hr = HRESULT(unsafe { filter.GetChunk(&mut stat) });
        if chunk_hr == FILTER_E_END_OF_CHUNKS {
            break;
        }
        if chunk_hr == FILTER_E_NO_TEXT || chunk_hr == FILTER_E_NO_VALUES {
            continue;
        }
        if chunk_hr.is_err() {
            break;
        }

        if (stat.flags.0 & CHUNK_TEXT.0) == 0 {
            continue;
        }

        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }

        loop {
            if Instant::now() >= deadline {
                return IFilterOutcome::DeadlineExceeded;
            }
            if out.len() >= max_chars {
                break;
            }
            let mut cwc = (buf.len() - 1) as u32;
            let text_hr = HRESULT(unsafe { filter.GetText(&mut cwc, PWSTR(buf.as_mut_ptr())) });
            if text_hr == S_OK || text_hr == FILTER_S_LAST_TEXT || text_hr == S_FALSE {
                let count = (cwc as usize).min(buf.len());
                if count > 0 {
                    let decoded = String::from_utf16_lossy(&buf[..count]);
                    out.push_str(&decoded);
                }
                if text_hr == FILTER_S_LAST_TEXT || text_hr == S_FALSE || count == 0 {
                    break;
                }
            } else if text_hr == FILTER_E_NO_MORE_TEXT {
                break;
            } else {
                break;
            }
        }
    }

    let cleaned: String = out
        .chars()
        .filter(|&c| c != '\0')
        .collect::<String>()
        .trim()
        .to_string();

    IFilterOutcome::Extracted(cleaned)
}

/// Alias for [`extract_text_via_ifilter`].
pub use extract_text_via_ifilter as extract_text_ifilter;
