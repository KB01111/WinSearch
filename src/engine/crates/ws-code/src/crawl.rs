//! Repository detection and `.gitignore`-aware source code crawling.
//!
//! Unlike the general filesystem crawler (`winsearchd::crawl`), the code
//! crawler operates on user-configured code folders, detects `.git`
//! repositories, and evaluates `.gitignore` rules (including nested
//! `.gitignore` files and `!negation` patterns) via the `ignore` crate while
//! applying code-specific exclusions (vendor, generated, minified,
//! `node_modules`, `target`, `.git`, binary files, and secret keys).

use std::fs::{self, File};
use std::io::{self, Read};
use std::os::windows::fs::MetadataExt;
use std::path::Path;
use std::time::UNIX_EPOCH;

use ignore::WalkBuilder;

use crate::model::{language_for_file, CodeFileRecord, DetectedRepo};

/// Maximum file size indexed as source code (2 MiB). Larger files are almost
/// always generated dumps, bundled assets, or data fixtures.
pub const MAX_CODE_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Directory names excluded from code indexing even when not listed in `.gitignore`.
pub const EXCLUDED_CODE_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    "vendor",
    "third_party",
    "third-party",
    "generated",
    "__generated__",
    "dist",
    "build",
    "out",
    ".next",
    ".nuxt",
    "coverage",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".tox",
    ".venv",
    "venv",
    ".gradle",
    ".cargo",
    ".rustup",
    "bin",
    "obj",
    "packages",
];

/// Exact file names (case-insensitive) never indexed in `code.db`: secrets,
/// credentials, and lockfiles.
pub const EXCLUDED_CODE_FILES: &[&str] = &[
    ".env",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    "credentials",
    "credentialprovider",
    "master.key",
    "secrets.json",
    "ntuser.dat",
    "ntuser.dat.log",
    "ntuser.ini",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "cargo.lock",
    "poetry.lock",
    "gemfile.lock",
    "composer.lock",
];

/// File extensions (lowercase, without dot) excluded from code indexing:
/// private keys/certificates, compiled artifacts, archives, media, and fonts.
pub const EXCLUDED_CODE_EXTS: &[&str] = &[
    // Keys & certificates
    "pem", "key", "pfx", "p12", "ppk", "kdbx", "asc", "jks", "keystore", "der", "p8", "crt",
    "cer",
    // Compiled / binary / native artifacts
    "exe", "dll", "so", "dylib", "lib", "a", "o", "obj", "pdb", "ilk", "exp", "wasm", "class",
    "pyc", "pyo", "pyd", "rlib", "rmeta",
    // Archives & packages
    "zip", "tar", "gz", "tgz", "bz2", "xz", "7z", "rar", "nupkg", "jar", "war", "whl",
    // Images, audio, video, fonts, documents, DBs
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "tiff", "tif", "heic", "avif", "mp3",
    "wav", "flac", "ogg", "m4a", "mp4", "mkv", "mov", "avi", "webm", "ttf", "otf", "woff",
    "woff2", "eot", "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "sqlite", "sqlite3",
    "db", "db-wal", "db-shm", "lock", "map",
];

/// Generated or minified filename suffixes (matched case-insensitively).
pub const EXCLUDED_CODE_SUFFIXES: &[&str] = &[
    ".min.js",
    ".min.mjs",
    ".min.cjs",
    ".min.css",
    ".bundle.js",
    ".bundle.css",
    ".generated.rs",
    ".generated.ts",
    ".generated.cs",
    ".Designer.cs",
    ".g.cs",
    ".g.i.cs",
    ".pb.go",
    ".pb.cc",
    ".pb.h",
    "_pb2.py",
    "_pb2_grpc.py",
];

/// Detects `.git` repositories under `root`.
///
/// - If `root` itself contains `.git` (directory or worktree `.git` file),
///   returns a single [`DetectedRepo`] with `has_git: true`.
/// - Otherwise walks subdirectories up to `max_depth` looking for `.git`
///   markers. When one or more nested repositories are found, returns them
///   sorted by `root_path`.
/// - If no `.git` is found anywhere under `root`, returns `root` itself with
///   `has_git: false` so standalone source folders configured by the user are
///   still indexed.
pub fn detect_repos(root: &Path) -> io::Result<Vec<DetectedRepo>> {
    detect_repos_with_depth(root, 4)
}

/// Detects repositories under `root` searching up to `max_depth` directory levels.
pub fn detect_repos_with_depth(root: &Path, max_depth: usize) -> io::Result<Vec<DetectedRepo>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    if has_git_marker(root) {
        return Ok(vec![DetectedRepo {
            root_path: root.to_string_lossy().into_owned(),
            name: dir_name(root),
            has_git: true,
        }]);
    }

    let mut repos = Vec::new();
    find_git_repos_recursive(root, 0, max_depth, &mut repos)?;
    if repos.is_empty() {
        repos.push(DetectedRepo {
            root_path: root.to_string_lossy().into_owned(),
            name: dir_name(root),
            has_git: false,
        });
    } else {
        repos.sort_by(|a, b| a.root_path.cmp(&b.root_path));
    }
    Ok(repos)
}

fn find_git_repos_recursive(
    dir: &Path,
    depth: usize,
    max_depth: usize,
    out: &mut Vec<DetectedRepo>,
) -> io::Result<()> {
    if depth >= max_depth {
        return Ok(());
    }
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound || e.kind() == io::ErrorKind::PermissionDenied => {
            return Ok(())
        }
        Err(e) => return Err(e),
    };
    for entry in rd {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !metadata.is_dir() {
            continue;
        }
        let attrs = metadata.file_attributes();
        if attrs & 0x400 != 0 {
            // Skip reparse points / junctions.
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_excluded_code_dir(&name) {
            continue;
        }
        let sub = entry.path();
        if has_git_marker(&sub) {
            out.push(DetectedRepo {
                root_path: sub.to_string_lossy().into_owned(),
                name,
                has_git: true,
            });
        } else {
            find_git_repos_recursive(&sub, depth + 1, max_depth, out)?;
        }
    }
    Ok(())
}

/// Returns `true` if `dir/.git` exists (either a standard `.git/` directory or
/// a git worktree/submodule `.git` file).
pub fn has_git_marker(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// Crawls a single repository (or code folder) root, honoring `.gitignore`
/// (including nested `.gitignore` files and `!negation` patterns) as well as
/// code exclusions (vendor, generated, minified, `node_modules`, `target`,
/// `.git`, binary files, and secret keys).
pub fn crawl_repo(repo_root: &Path) -> io::Result<Vec<CodeFileRecord>> {
    if !repo_root.exists() {
        return Ok(Vec::new());
    }

    let mut builder = WalkBuilder::new(repo_root);
    builder
        // Do not filter all dotfiles automatically so `.github/workflows/*.yml`
        // or `.eslintrc.json` can be indexed while `.git`, `.env*`, etc. are
        // explicitly excluded by our filter.
        .hidden(false)
        .parents(true)
        .ignore(true)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        // `require_git(false)` ensures `.gitignore` rules (including nested and
        // `!negation` rules) work both in `.git` repos, git worktrees, and
        // fixture/standalone trees.
        .require_git(false)
        .follow_links(false)
        .max_depth(Some(64));

    builder.filter_entry(|entry| {
        if entry.depth() == 0 {
            return true;
        }
        let name = entry.file_name().to_string_lossy();
        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        if is_dir {
            !is_excluded_code_dir(&name)
        } else {
            !is_excluded_code_file(&name)
        }
    });

    let mut records = Vec::new();
    for result in builder.build() {
        let entry = match result {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.depth() == 0 {
            continue;
        }
        let ft = match entry.file_type() {
            Some(ft) => ft,
            None => continue,
        };
        if !ft.is_file() {
            continue;
        }
        let path = entry.path();
        if let Some(rec) = record_for_code_path(repo_root, path)? {
            records.push(rec);
        }
    }

    records.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(records)
}

/// Builds a [`CodeFileRecord`] for a single file inside `repo_root`, returning
/// `Ok(None)` if the file vanished, is excluded by policy, exceeds
/// [`MAX_CODE_FILE_BYTES`], or appears to be binary.
pub fn record_for_code_path(
    repo_root: &Path,
    path: &Path,
) -> io::Result<Option<CodeFileRecord>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if !metadata.is_file() {
        return Ok(None);
    }
    let attrs = metadata.file_attributes();
    // Skip reparse points and system files.
    if attrs & (0x400 | 0x4) != 0 {
        return Ok(None);
    }
    let size = metadata.file_size();
    if size > MAX_CODE_FILE_BYTES {
        return Ok(None);
    }

    let name = file_name(path);
    if is_excluded_code_file(&name) {
        return Ok(None);
    }
    // Check ancestor directory components relative to repo_root against excluded dirs.
    if let Ok(rel) = path.strip_prefix(repo_root) {
        if let Some(parent) = rel.parent() {
            for comp in parent.components() {
                let comp_str = comp.as_os_str().to_string_lossy();
                if is_excluded_code_dir(&comp_str) {
                    return Ok(None);
                }
            }
        }
    }

    if is_probably_binary(path)? {
        return Ok(None);
    }

    let ext = extension_of(&name);
    let language = language_for_file(&name, &ext).to_string();
    let rel_path = relative_slash_path(repo_root, path);
    let (volume_serial, file_id) = file_identity(path, false);

    Ok(Some(CodeFileRecord {
        path: path.to_string_lossy().into_owned(),
        rel_path,
        name,
        ext,
        language,
        size,
        mtime: unix_seconds(&metadata),
        volume_serial,
        file_id,
    }))
}

/// True for directory names excluded from code indexing (`node_modules`,
/// `target`, `vendor`, `generated`, `.git`, etc.).
pub fn is_excluded_code_dir(name: &str) -> bool {
    if EXCLUDED_CODE_DIRS
        .iter()
        .any(|d| name.eq_ignore_ascii_case(d))
    {
        return true;
    }
    let lower = name.to_ascii_lowercase();
    if lower.starts_with(".env") {
        return true;
    }
    false
}

/// True for file names excluded from code indexing (secrets/keys, minified
/// bundles, generated code, lockfiles, and binary extensions).
pub fn is_excluded_code_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if EXCLUDED_CODE_FILES.iter().any(|f| lower == *f) {
        return true;
    }
    if lower.starts_with("id_rsa")
        || lower.starts_with("id_dsa")
        || lower.starts_with("id_ecdsa")
        || lower.starts_with("id_ed25519")
    {
        return true;
    }
    if lower.starts_with(".env") {
        return true;
    }
    if EXCLUDED_CODE_SUFFIXES
        .iter()
        .any(|s| lower.ends_with(&s.to_ascii_lowercase()))
    {
        return true;
    }
    let ext = extension_of(&lower);
    if EXCLUDED_CODE_EXTS.contains(&ext.as_str()) {
        return true;
    }
    false
}

/// Inspects the first 8 KiB of `path` and returns `true` if it contains a NUL
/// byte (standard Git/ripgrep binary heuristic).
pub fn is_probably_binary(path: &Path) -> io::Result<bool> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound || e.kind() == io::ErrorKind::PermissionDenied => {
            return Ok(true)
        }
        Err(e) => return Err(e),
    };
    let mut buf = [0u8; 8192];
    let n = file.read(&mut buf)?;
    Ok(buf[..n].contains(&0))
}

/// Windows filesystem identity `(volume_serial, file_id)` used to preserve file
/// identity across renames and moves.
pub fn file_identity(path: &Path, is_dir: bool) -> (u32, u64) {
    use std::ffi::c_void;
    use std::mem::size_of;

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FileIdInfo, GetFileInformationByHandleEx, FILE_FLAGS_AND_ATTRIBUTES,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let wide: Vec<u16> = path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let handle: HANDLE = match CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            if is_dir {
                FILE_FLAG_BACKUP_SEMANTICS
            } else {
                FILE_FLAGS_AND_ATTRIBUTES(0)
            },
            None,
        ) {
            Ok(h) => h,
            Err(_) => return (0, 0),
        };
        let mut info: FILE_ID_INFO = std::mem::zeroed();
        let result = GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            &mut info as *mut FILE_ID_INFO as *mut c_void,
            size_of::<FILE_ID_INFO>() as u32,
        );
        let _ = CloseHandle(handle);
        if result.is_err() {
            return (0, 0);
        }
        let file_id = u64::from_le_bytes(info.FileId.Identifier[..8].try_into().unwrap_or([0; 8]));
        (info.VolumeSerialNumber as u32, file_id)
    }
}

fn unix_seconds(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn dir_name(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn extension_of(name: &str) -> String {
    Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

fn relative_slash_path(repo_root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(repo_root).unwrap_or(path);
    rel.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "winsearch-ws-code-crawl-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn detect_repos_finds_single_or_nested_git_repos() {
        let workspace = TempDir::new("detect");
        let repo_a = workspace.0.join("repo-a");
        let repo_b = workspace.0.join("nested").join("repo-b");
        fs::create_dir_all(repo_a.join(".git")).unwrap();
        fs::create_dir_all(&repo_b).unwrap();
        // Worktree-style `.git` file is also recognized.
        fs::write(repo_b.join(".git"), b"gitdir: ../.git/worktrees/repo-b\n").unwrap();

        let detected = detect_repos(&workspace.0).unwrap();
        assert_eq!(detected.len(), 2);
        assert_eq!(detected[0].name, "repo-b");
        assert!(detected[0].has_git);
        assert_eq!(detected[1].name, "repo-a");
        assert!(detected[1].has_git);

        // Direct repo root returns just that repo.
        let direct = detect_repos(&repo_a).unwrap();
        assert_eq!(direct.len(), 1);
        assert_eq!(direct[0].name, "repo-a");
        assert!(direct[0].has_git);
    }

    #[test]
    fn crawl_honors_nested_gitignore_negation_and_code_exclusions() {
        let repo = TempDir::new("gitignore");
        fs::create_dir_all(repo.0.join(".git")).unwrap();
        fs::write(repo.0.join(".git").join("HEAD"), b"ref: refs/heads/main\n").unwrap();

        // Root .gitignore: ignore *.log and build_out/, but un-ignore important.log!
        fs::write(
            repo.0.join(".gitignore"),
            b"*.log\n!important.log\nignored_dir/\n",
        )
        .unwrap();

        // Root files
        fs::write(repo.0.join("debug.log"), b"ignored log").unwrap();
        fs::write(repo.0.join("important.log"), b"kept via negation").unwrap();
        fs::write(repo.0.join("README.md"), b"# Repo").unwrap();

        // Ignored dir from root .gitignore
        fs::create_dir_all(repo.0.join("ignored_dir")).unwrap();
        fs::write(repo.0.join("ignored_dir").join("skip.rs"), b"fn skip() {}").unwrap();

        // Nested directory with its own .gitignore + negation
        let src_dir = repo.0.join("src");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(
            src_dir.join(".gitignore"),
            b"*.tmp.rs\n!keep.tmp.rs\nsub_ignored/\n",
        )
        .unwrap();
        fs::write(src_dir.join("lib.rs"), b"pub fn hello() {}").unwrap();
        fs::write(src_dir.join("drop.tmp.rs"), b"fn drop_me() {}").unwrap();
        fs::write(src_dir.join("keep.tmp.rs"), b"pub fn keep_me() {}").unwrap();
        fs::create_dir_all(src_dir.join("sub_ignored")).unwrap();
        fs::write(
            src_dir.join("sub_ignored").join("hidden.rs"),
            b"fn hidden() {}",
        )
        .unwrap();

        // Excluded code trees (even without .gitignore entries)
        for dir in ["vendor", "generated", "node_modules", "target"] {
            let d = repo.0.join(dir);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("lib.rs"), b"pub fn excluded() {}").unwrap();
        }

        // Excluded minified, generated suffix, secret keys, and binary files
        fs::write(src_dir.join("app.min.js"), b"function a(){}").unwrap();
        fs::write(src_dir.join("styles.min.css"), b"body{}").unwrap();
        fs::write(src_dir.join("schema.generated.rs"), b"struct Gen;").unwrap();
        fs::write(repo.0.join(".env"), b"SECRET=1").unwrap();
        fs::write(repo.0.join(".env.production"), b"SECRET=2").unwrap();
        fs::write(repo.0.join("id_rsa"), b"PRIVATE KEY").unwrap();
        fs::write(repo.0.join("server.pem"), b"CERT").unwrap();
        fs::write(repo.0.join("signing.key"), b"KEY").unwrap();
        fs::write(src_dir.join("blob.dat"), b"hello\0world\0binary").unwrap();

        let files = crawl_repo(&repo.0).unwrap();
        let rel_paths: Vec<&str> = files.iter().map(|f| f.rel_path.as_str()).collect();

        assert_eq!(
            rel_paths,
            vec![
                ".gitignore",
                "README.md",
                "important.log",
                "src/.gitignore",
                "src/keep.tmp.rs",
                "src/lib.rs",
            ]
        );

        let lib_rs = files.iter().find(|f| f.rel_path == "src/lib.rs").unwrap();
        assert_eq!(lib_rs.language, "rust");
        assert_ne!(lib_rs.file_id, 0);
        assert_ne!(lib_rs.volume_serial, 0);
    }
}
