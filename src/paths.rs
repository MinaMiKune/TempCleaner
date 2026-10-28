use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::path::{Component, Path, PathBuf, Prefix};

use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;

use crate::error::PathError;
use crate::models::TargetKind;
use crate::winfs::{self, FileId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootSource {
    Temp,
    Tmp,
    SystemRoot,
    Windir,
    Programmatic,
}

impl RootSource {
    pub fn label_zh(self) -> &'static str {
        match self {
            Self::Temp => "环境变量 %TEMP%",
            Self::Tmp => "环境变量 %TMP%（%TEMP% 缺失）",
            Self::SystemRoot => "环境变量 %SystemRoot%",
            Self::Windir => "环境变量 %windir%（%SystemRoot% 缺失）",
            Self::Programmatic => "程序指定（已校验）",
        }
    }
}

#[derive(Clone, Debug)]
pub enum TargetState {
    Ready,
    PermissionLimited { message: String },
    Unavailable { message: String },
    Duplicate { other: TargetKind },
}

#[derive(Clone, Debug)]
pub struct TargetReport {
    pub kind: TargetKind,
    pub source: RootSource,
    pub path: Option<PathBuf>,
    pub root: Option<TempRoot>,
    pub state: TargetState,
}

impl TargetReport {
    pub fn is_selectable(&self) -> bool {
        self.root.is_some()
            && !matches!(
                self.state,
                TargetState::Unavailable { .. } | TargetState::Duplicate { .. }
            )
    }

    pub fn status_text(&self) -> String {
        match &self.state {
            TargetState::Ready => "可扫描、可尝试清理".to_owned(),
            TargetState::PermissionLimited { message } => format!("权限受限：{message}"),
            TargetState::Unavailable { message } => format!("不可用：{message}"),
            TargetState::Duplicate { other } => {
                format!("与“{}”指向同一目录，已禁用以避免重复清理", other.name_zh())
            }
        }
    }
}

/// A Temp root that passed all path checks. Constructing this type is the only
/// way to hand a directory to the scanner or cleaner.
#[derive(Clone, Debug)]
pub struct TempRoot {
    kind: TargetKind,
    path: PathBuf,
    source: RootSource,
    file_id: FileId,
    delete_access: bool,
}

impl TempRoot {
    pub fn kind(&self) -> TargetKind {
        self.kind
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn source(&self) -> RootSource {
        self.source
    }

    pub fn file_id(&self) -> FileId {
        self.file_id
    }

    pub fn has_delete_access(&self) -> bool {
        self.delete_access
    }

    /// Validate an explicitly supplied candidate. Production discovery only
    /// passes environment-derived candidates; tests use this with a dedicated
    /// directory below the operating system's temp directory.
    pub fn validate_candidate(
        path: &Path,
        kind: TargetKind,
        source: RootSource,
    ) -> Result<Self, PathError> {
        validate_root(path, kind, source)
    }
}

trait EnvProvider {
    fn var(&self, name: &str) -> Option<OsString>;
}

struct ProcessEnv;

impl EnvProvider for ProcessEnv {
    fn var(&self, name: &str) -> Option<OsString> {
        env::var_os(name)
    }
}

pub fn discover_targets() -> Vec<TargetReport> {
    discover_with(&ProcessEnv)
}

fn discover_with(env_provider: &dyn EnvProvider) -> Vec<TargetReport> {
    let user = resolve_user_temp(env_provider);
    let system = resolve_system_temp(env_provider);

    let mut reports = vec![user, system];
    let duplicate = match (&reports[0].root, &reports[1].root) {
        (Some(a), Some(b)) => a.file_id == b.file_id || path_eq_ci(a.path(), b.path()),
        _ => false,
    };

    if duplicate {
        let system_path = reports[1].path.clone();
        reports[1].root = None;
        reports[1].state = TargetState::Duplicate {
            other: TargetKind::UserTemp,
        };
        reports[1].path = system_path;
    }

    reports
}

fn resolve_user_temp(env_provider: &dyn EnvProvider) -> TargetReport {
    let (source, value) = if let Some(value) = non_empty_env(env_provider, "TEMP") {
        (RootSource::Temp, value)
    } else if let Some(value) = non_empty_env(env_provider, "TMP") {
        (RootSource::Tmp, value)
    } else {
        return unavailable(
            TargetKind::UserTemp,
            RootSource::Temp,
            None,
            "环境变量 TEMP 与 TMP 均缺失或为空；不会回退到其他路径".to_owned(),
        );
    };

    // If TEMP exists but is invalid we deliberately do not fall back to TMP:
    // a broken TEMP must be surfaced instead of silently cleaning another path.
    let raw = match expand_environment(&value) {
        Ok(value) => value,
        Err(error) => {
            return unavailable(
                TargetKind::UserTemp,
                source,
                path_hint(&value),
                error.to_string(),
            );
        }
    };

    match TempRoot::validate_candidate(Path::new(&raw), TargetKind::UserTemp, source) {
        Ok(root) => ready(root),
        Err(error) => unavailable(
            TargetKind::UserTemp,
            source,
            path_hint(&raw),
            error.to_string(),
        ),
    }
}

fn resolve_system_temp(env_provider: &dyn EnvProvider) -> TargetReport {
    let (source, value) = if let Some(value) = non_empty_env(env_provider, "SystemRoot") {
        (RootSource::SystemRoot, value)
    } else if let Some(value) = non_empty_env(env_provider, "windir") {
        (RootSource::Windir, value)
    } else {
        return unavailable(
            TargetKind::SystemTemp,
            RootSource::SystemRoot,
            None,
            "环境变量 SystemRoot 与 windir 均缺失或为空".to_owned(),
        );
    };

    let raw = match expand_environment(&value) {
        Ok(value) => value,
        Err(error) => {
            return unavailable(
                TargetKind::SystemTemp,
                source,
                path_hint(&value),
                error.to_string(),
            );
        }
    };

    let temp_candidate = Path::new(&raw).join("Temp");
    match TempRoot::validate_candidate(&temp_candidate, TargetKind::SystemTemp, source) {
        Ok(root) => ready(root),
        Err(error) => unavailable(
            TargetKind::SystemTemp,
            source,
            Some(temp_candidate),
            error.to_string(),
        ),
    }
}

fn ready(root: TempRoot) -> TargetReport {
    let state = if root.delete_access {
        TargetState::Ready
    } else {
        TargetState::PermissionLimited {
            message: "当前进程没有该目录的删除权限；可以扫描，但清理很可能部分失败，需要以管理员身份运行才有完整权限".to_owned(),
        }
    };
    TargetReport {
        kind: root.kind,
        source: root.source,
        path: Some(root.path.clone()),
        root: Some(root),
        state,
    }
}

fn unavailable(
    kind: TargetKind,
    source: RootSource,
    path: Option<PathBuf>,
    message: String,
) -> TargetReport {
    TargetReport {
        kind,
        source,
        path,
        root: None,
        state: TargetState::Unavailable { message },
    }
}

fn non_empty_env(env_provider: &dyn EnvProvider, name: &str) -> Option<OsString> {
    match env_provider.var(name) {
        Some(value) if !value.is_empty() => Some(value),
        _ => None,
    }
}

fn path_hint(value: &OsStr) -> Option<PathBuf> {
    let path = PathBuf::from(value);
    if path.is_absolute() { Some(path) } else { None }
}

fn expand_environment(value: &OsStr) -> Result<OsString, PathError> {
    let text = value.to_string_lossy();
    if !text.contains('%') {
        return Ok(value.to_os_string());
    }

    let source: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();

    // SAFETY: the input is a NUL-terminated UTF-16 string. The first call has
    // a null output pointer and only asks for the required length.
    let needed = unsafe { ExpandEnvironmentStringsW(source.as_ptr(), std::ptr::null_mut(), 0) };
    if needed == 0 {
        return Err(PathError::EnvironmentExpansion(
            io::Error::last_os_error().to_string(),
        ));
    }

    let mut buffer = vec![0u16; needed as usize];
    // SAFETY: `buffer` has the capacity reported by the first call, and is a
    // valid writable buffer of exactly that size.
    let written = unsafe {
        ExpandEnvironmentStringsW(source.as_ptr(), buffer.as_mut_ptr(), buffer.len() as u32)
    };
    if written == 0 || written > buffer.len() as u32 {
        return Err(PathError::EnvironmentExpansion(
            io::Error::last_os_error().to_string(),
        ));
    }

    let length = written.saturating_sub(1) as usize; // omit terminating NUL
    let expanded = OsString::from_wide(&buffer[..length]);
    let expanded_text = expanded.to_string_lossy();
    if expanded_text.contains('%') {
        return Err(PathError::EnvironmentExpansion(format!(
            "仍有未解析的变量：{expanded_text}"
        )));
    }
    Ok(expanded)
}

fn validate_root(path: &Path, kind: TargetKind, source: RootSource) -> Result<TempRoot, PathError> {
    let absolute = make_absolute_lexical(path)?;

    // Inspect every existing ancestor *before* canonicalize. canonicalize
    // resolves links, so it cannot prove that no link was traversed.
    check_ancestors_for_reparse(&absolute)?;

    let canonical = fs::canonicalize(&absolute).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            PathError::NotFound(absolute.display().to_string())
        } else if error.kind() == io::ErrorKind::PermissionDenied {
            PathError::AccessDenied(format!(
                "{}：{}",
                absolute.display(),
                crate::error::describe_io_error(&error)
            ))
        } else {
            PathError::Io(format!(
                "{}：{}",
                absolute.display(),
                crate::error::describe_io_error(&error)
            ))
        }
    })?;
    let canonical = strip_verbatim(canonical);
    ensure_local_disk_path(&canonical)?;

    if !is_acceptable_temp_path(&canonical, kind) {
        return Err(PathError::InvalidName(format!(
            "目录必须名为 Temp 或位于名为 Temp 的目录下：{}",
            canonical.display()
        )));
    }

    let metadata = fs::symlink_metadata(&canonical)?;
    if !metadata.is_dir() {
        return Err(PathError::NotDirectory(canonical.display().to_string()));
    }
    if winfs::is_reparse_point(&metadata) {
        return Err(PathError::ReparsePoint(canonical.display().to_string()));
    }

    reject_protected_location(&canonical, kind)?;

    // A validated handle gives us the real on-disk path and file identity.
    let guard = winfs::open_root_guard(&canonical).map_err(|error| {
        if error.kind() == io::ErrorKind::PermissionDenied {
            PathError::AccessDenied(format!(
                "{}：{}",
                canonical.display(),
                crate::error::describe_io_error(&error)
            ))
        } else {
            PathError::Io(format!(
                "{}：{}",
                canonical.display(),
                crate::error::describe_io_error(&error)
            ))
        }
    })?;

    let handle_metadata = guard.metadata()?;
    if !handle_metadata.is_dir() {
        return Err(PathError::NotDirectory(canonical.display().to_string()));
    }
    if winfs::is_reparse_point(&handle_metadata) {
        return Err(PathError::ReparsePoint(canonical.display().to_string()));
    }

    let final_path = strip_verbatim(winfs::final_path(&guard)?);
    ensure_local_disk_path(&final_path)?;
    if !is_acceptable_temp_path(&final_path, kind) {
        return Err(PathError::InvalidName(final_path.display().to_string()));
    }
    reject_protected_location(&final_path, kind)?;

    let file_id = winfs::file_id(&guard)?;

    let mut entries = fs::read_dir(&final_path).map_err(|error| {
        if error.kind() == io::ErrorKind::PermissionDenied {
            PathError::AccessDenied(format!(
                "{}：{}",
                final_path.display(),
                crate::error::describe_io_error(&error)
            ))
        } else {
            PathError::Io(format!(
                "{}：{}",
                final_path.display(),
                crate::error::describe_io_error(&error)
            ))
        }
    })?;
    if let Some(Err(error)) = entries.next() {
        return Err(PathError::AccessDenied(format!(
            "{}：{}",
            final_path.display(),
            crate::error::describe_io_error(&error)
        )));
    }

    let delete_access = winfs::open_root_delete_probe(&final_path).is_ok();

    Ok(TempRoot {
        kind,
        path: final_path,
        source,
        file_id,
        delete_access,
    })
}

fn make_absolute_lexical(path: &Path) -> Result<PathBuf, PathError> {
    if path.as_os_str().is_empty() {
        return Err(PathError::NotAbsolute("路径为空".to_owned()));
    }
    if !path.is_absolute() {
        return Err(PathError::NotAbsolute(path.display().to_string()));
    }

    let mut prefix: Option<OsString> = None;
    let mut has_root = false;
    for component in path.components() {
        match component {
            Component::Prefix(value) => {
                if !matches!(value.kind(), Prefix::Disk(_)) {
                    return Err(PathError::UnsupportedLocation(format!(
                        "只接受本地盘符路径：{}",
                        path.display()
                    )));
                }
                prefix = Some(value.as_os_str().to_os_string());
            }
            Component::RootDir => has_root = true,
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(PathError::RelativeComponent(path.display().to_string()));
            }
            Component::Normal(_) => {}
        }
    }

    if prefix.is_none() || !has_root {
        return Err(PathError::NotAbsolute(path.display().to_string()));
    }

    std::path::absolute(path).map_err(|error| PathError::Io(format!("{}：{error}", path.display())))
}

fn ensure_local_disk_path(path: &Path) -> Result<(), PathError> {
    let mut components = path.components();
    match components.next() {
        Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_)) => {}
        _ => {
            return Err(PathError::UnsupportedLocation(path.display().to_string()));
        }
    }
    if !matches!(components.next(), Some(Component::RootDir)) {
        return Err(PathError::NotAbsolute(path.display().to_string()));
    }
    Ok(())
}

fn check_ancestors_for_reparse(path: &Path) -> Result<(), PathError> {
    let mut ancestors: Vec<&Path> = path.ancestors().collect();
    ancestors.reverse();

    for ancestor in ancestors {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) => {
                if winfs::is_reparse_point(&metadata) {
                    return Err(PathError::ReparsePoint(ancestor.display().to_string()));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(PathError::NotFound(path.display().to_string()));
            }
            Err(error) => {
                return Err(PathError::Io(format!(
                    "{}：{}",
                    ancestor.display(),
                    crate::error::describe_io_error(&error)
                )));
            }
        }
    }
    Ok(())
}

fn reject_protected_location(path: &Path, kind: TargetKind) -> Result<(), PathError> {
    let mut protected: Vec<PathBuf> = Vec::new();

    for name in [
        "USERPROFILE",
        "LOCALAPPDATA",
        "APPDATA",
        "ProgramData",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "SystemRoot",
        "windir",
        "PUBLIC",
    ] {
        if let Some(value) = env::var_os(name)
            && let Ok(value) = expand_environment(&value)
        {
            let candidate = PathBuf::from(value);
            if candidate.is_absolute() {
                protected.push(candidate);
            }
        }
    }

    if let Some(profile) = env::var_os("USERPROFILE") {
        let profile = PathBuf::from(profile);
        if let Some(parent) = profile.parent() {
            protected.push(parent.to_path_buf()); // usually C:\Users
        }
    }
    if let Some(system_drive) = env::var_os("SystemDrive") {
        protected.push(PathBuf::from(format!(
            "{}\\",
            system_drive.to_string_lossy()
        )));
    }

    for protected_path in protected {
        let protected_path = strip_verbatim(protected_path);
        if path_eq_ci(path, &protected_path) {
            return Err(PathError::ProtectedPath(path.display().to_string()));
        }
        // Reject a root that contains a protected location, e.g. C:\Users
        // as the "Temp" root. Being *inside* a protected location is normal
        // for %TEMP%; the leaf-name check above keeps this narrow.
        if path_is_ancestor_ci(path, &protected_path) {
            return Err(PathError::ProtectedPath(format!(
                "{} 包含受保护位置 {}",
                path.display(),
                protected_path.display()
            )));
        }
    }

    // The Windows directory has special handling: only the exact system
    // target %SystemRoot%\Temp may be cleaned. This prevents a broken or
    // malicious TEMP value from turning, for example,
    // C:\Windows\System32\Temp into a user clean target.
    if let Some(system_root) = env::var_os("SystemRoot")
        .or_else(|| env::var_os("windir"))
        .map(PathBuf::from)
    {
        let system_root = strip_verbatim(system_root);
        if path_is_ancestor_ci(&system_root, path) {
            let system_temp = system_root.join("Temp");
            let is_exact_system_target =
                kind == TargetKind::SystemTemp && path_eq_ci(path, &system_temp);
            if !is_exact_system_target {
                return Err(PathError::ProtectedPath(format!(
                    "{} 位于 Windows 目录内，只允许精确的系统 Temp 目标 {}",
                    path.display(),
                    system_temp.display()
                )));
            }
        }
    }

    Ok(())
}

pub fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.as_os_str().to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else if let Some(rest) = text.strip_prefix(r"\??\") {
        PathBuf::from(rest)
    } else {
        path
    }
}

fn is_acceptable_temp_path(path: &Path, kind: TargetKind) -> bool {
    match kind {
        // The system target is always built as %SystemRoot%\Temp.
        TargetKind::SystemTemp => path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("Temp")),
        // User TEMP may legitimately be a per-session subdirectory of Temp
        // (for example ...\Local\Temp\2). Require at least one Temp component
        // so an unrelated directory supplied by a broken environment variable
        // is still rejected.
        TargetKind::UserTemp => path.components().any(|component| match component {
            Component::Normal(name) => name.to_string_lossy().eq_ignore_ascii_case("Temp"),
            _ => false,
        }),
    }
}

fn normalized_components(path: &Path) -> Vec<String> {
    strip_verbatim(path.to_path_buf())
        .components()
        .filter_map(|component| match component {
            Component::Prefix(prefix) => Some(prefix.as_os_str().to_string_lossy().to_lowercase()),
            Component::RootDir => Some("\\".to_owned()),
            Component::CurDir => None,
            Component::ParentDir => Some("..".to_owned()),
            Component::Normal(value) => Some(value.to_string_lossy().to_lowercase()),
        })
        .collect()
}

fn path_eq_ci(a: &Path, b: &Path) -> bool {
    normalized_components(a) == normalized_components(b)
}

fn path_is_ancestor_ci(ancestor: &Path, candidate: &Path) -> bool {
    let ancestor = normalized_components(ancestor);
    let candidate = normalized_components(candidate);
    ancestor.len() < candidate.len() && candidate.starts_with(&ancestor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static NEXT_ID: AtomicU32 = AtomicU32::new(0);

    struct Fixture {
        path: PathBuf,
    }

    impl Fixture {
        fn new(leaf: &str) -> Self {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir()
                .join(format!("temp-cleaner-tests-{}-{id}", std::process::id()))
                .join(leaf);
            fs::create_dir_all(&path).expect("create fixture");
            Self { path }
        }

        fn root(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Some(parent) = self.path.parent() {
                let _ = fs::remove_dir_all(parent);
            }
        }
    }

    struct FakeEnv(Vec<(&'static str, OsString)>);

    impl EnvProvider for FakeEnv {
        fn var(&self, name: &str) -> Option<OsString> {
            self.0
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.clone())
        }
    }

    fn fake_env(pairs: &[(&'static str, &Path)]) -> FakeEnv {
        FakeEnv(
            pairs
                .iter()
                .map(|(name, path)| (*name, path.as_os_str().to_os_string()))
                .collect(),
        )
    }

    #[test]
    fn validates_user_temp_candidate() {
        let fixture = Fixture::new("Temp");
        let root = TempRoot::validate_candidate(
            fixture.root(),
            TargetKind::UserTemp,
            RootSource::Programmatic,
        )
        .expect("fixture should validate");
        assert_eq!(
            root.path(),
            strip_verbatim(fs::canonicalize(fixture.root()).unwrap()).as_path()
        );
        assert_eq!(root.kind(), TargetKind::UserTemp);
    }

    #[test]
    fn resolves_user_temp_from_environment() {
        let fixture = Fixture::new("Temp");
        let env = fake_env(&[("TEMP", fixture.root())]);
        let report = resolve_user_temp(&env);
        assert!(report.is_selectable());
        assert_eq!(report.source, RootSource::Temp);
    }

    #[test]
    fn falls_back_to_tmp_only_when_temp_is_missing_or_empty() {
        let fixture = Fixture::new("Temp");
        let missing = fake_env(&[("TMP", fixture.root())]);
        assert_eq!(resolve_user_temp(&missing).source, RootSource::Tmp);

        let mut entries = fake_env(&[("TEMP", fixture.root()), ("TMP", fixture.root())]);
        entries.0[0].1 = OsString::new();
        assert_eq!(resolve_user_temp(&entries).source, RootSource::Tmp);
    }

    #[test]
    fn does_not_fall_back_when_temp_exists_but_is_invalid() {
        let fixture = Fixture::new("Temp");
        let missing = fixture.root().join("does-not-exist");
        let env = fake_env(&[("TEMP", missing.as_path()), ("TMP", fixture.root())]);
        let report = resolve_user_temp(&env);
        assert!(!report.is_selectable());
        assert!(matches!(report.state, TargetState::Unavailable { .. }));
    }

    #[test]
    fn missing_both_temp_variables_is_an_error() {
        let env = FakeEnv(Vec::new());
        let report = resolve_user_temp(&env);
        assert!(!report.is_selectable());
        let TargetState::Unavailable { message } = report.state else {
            panic!("expected unavailable");
        };
        assert!(message.contains("TEMP"));
        assert!(message.contains("TMP"));
    }

    #[test]
    fn resolves_system_temp_from_system_root() {
        let fixture = Fixture::new("Windows");
        fs::create_dir_all(fixture.root().join("Temp")).unwrap();
        let env = fake_env(&[("SystemRoot", fixture.root())]);
        let report = resolve_system_temp(&env);
        assert!(report.is_selectable(), "{:?}", report.state);
        assert_eq!(report.source, RootSource::SystemRoot);
        assert_eq!(
            report.root.as_ref().unwrap().path().file_name().unwrap(),
            OsStr::new("Temp")
        );
    }

    #[test]
    fn duplicate_targets_are_disabled() {
        let fixture = Fixture::new("Windows");
        let temp = fixture.root().join("Temp");
        fs::create_dir_all(&temp).unwrap();
        let env = fake_env(&[("TEMP", temp.as_path()), ("SystemRoot", fixture.root())]);
        let reports = discover_with(&env);
        assert!(reports[0].is_selectable());
        assert!(!reports[1].is_selectable());
        assert!(matches!(
            reports[1].state,
            TargetState::Duplicate {
                other: TargetKind::UserTemp
            }
        ));
    }

    #[test]
    fn only_exact_system_temp_is_allowed_below_windows() {
        let system_root = env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let system_temp = system_root.join("Temp");
        let nested = system_root.join("System32").join("Temp");

        assert!(
            reject_protected_location(&system_temp, TargetKind::SystemTemp).is_ok(),
            "the exact system target must be allowed"
        );
        assert!(matches!(
            reject_protected_location(&system_temp, TargetKind::UserTemp),
            Err(PathError::ProtectedPath(_))
        ));
        assert!(matches!(
            reject_protected_location(&nested, TargetKind::SystemTemp),
            Err(PathError::ProtectedPath(_))
        ));
    }

    #[test]
    fn rejects_system_root_as_a_temp_target() {
        let windows = env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let error = TempRoot::validate_candidate(
            &windows,
            TargetKind::SystemTemp,
            RootSource::Programmatic,
        )
        .unwrap_err();
        assert!(matches!(error, PathError::InvalidName(_)));
    }

    #[test]
    fn rejects_relative_and_parent_paths() {
        let relative = TempRoot::validate_candidate(
            Path::new("Temp"),
            TargetKind::UserTemp,
            RootSource::Programmatic,
        )
        .unwrap_err();
        assert!(matches!(relative, PathError::NotAbsolute(_)));

        let fixture = Fixture::new("Temp");
        let parent = fixture.root().join("..").join("Temp");
        let parent_error =
            TempRoot::validate_candidate(&parent, TargetKind::UserTemp, RootSource::Programmatic)
                .unwrap_err();
        assert!(matches!(parent_error, PathError::RelativeComponent(_)));
    }

    #[test]
    fn rejects_missing_path_and_file() {
        let fixture = Fixture::new("Temp");
        let missing = fixture.root().join("nope");
        let error =
            TempRoot::validate_candidate(&missing, TargetKind::UserTemp, RootSource::Programmatic)
                .unwrap_err();
        assert!(matches!(error, PathError::NotFound(_)));

        let container = fixture.root().join("files");
        fs::create_dir_all(&container).unwrap();
        let file = container.join("Temp");
        fs::write(&file, b"x").unwrap();
        let error =
            TempRoot::validate_candidate(&file, TargetKind::UserTemp, RootSource::Programmatic)
                .unwrap_err();
        assert!(matches!(error, PathError::NotDirectory(_)));
    }

    #[test]
    fn accepts_a_per_session_subdirectory_of_temp() {
        let fixture = Fixture::new("Temp");
        let session = fixture.root().join("session-1");
        fs::create_dir_all(&session).unwrap();
        let root =
            TempRoot::validate_candidate(&session, TargetKind::UserTemp, RootSource::Programmatic)
                .expect("a session directory below Temp is a valid user target");
        assert_eq!(root.path().file_name().unwrap(), OsStr::new("session-1"));
    }

    #[test]
    fn rejects_non_temp_leaf_name() {
        let fixture = Fixture::new("NotTemp");
        let error = TempRoot::validate_candidate(
            fixture.root(),
            TargetKind::UserTemp,
            RootSource::Programmatic,
        )
        .unwrap_err();
        assert!(matches!(error, PathError::InvalidName(_)));
    }
}
