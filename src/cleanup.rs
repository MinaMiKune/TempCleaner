use std::cmp::Reverse;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::error::{describe_io_error, is_not_found, problem_kind_from_io};
use crate::models::{
    CleanReport, CleanStatus, Issue, IssueClass, Issues, OperationProgress, Phase, ProblemKind,
};
use crate::paths::TempRoot;
use crate::scan::relative_display;
use crate::winfs;

#[derive(Debug)]
pub struct CleanError {
    pub message: String,
}

impl fmt::Display for CleanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CleanError {}

struct CleanState {
    deleted_files: u64,
    deleted_directories: u64,
    freed_bytes: u64,
    skipped: u64,
    failed: u64,
    issues: Issues,
}

impl CleanState {
    fn new() -> Self {
        Self {
            deleted_files: 0,
            deleted_directories: 0,
            freed_bytes: 0,
            skipped: 0,
            failed: 0,
            issues: Issues::default(),
        }
    }

    fn skip(&mut self, root: &Path, path: &Path, kind: ProblemKind, detail: String) {
        self.skipped = self.skipped.saturating_add(1);
        self.issues.push(Issue {
            class: IssueClass::Skipped,
            kind,
            relative_path: relative_display(root, path),
            detail,
        });
    }

    fn fail(&mut self, root: &Path, path: &Path, kind: ProblemKind, detail: String) {
        self.failed = self.failed.saturating_add(1);
        self.issues.push(Issue {
            class: IssueClass::Failed,
            kind,
            relative_path: relative_display(root, path),
            detail,
        });
    }
}

/// Re-traverses the selected root and deletes each entry after freshly checking
/// it. The scan report is never used as a deletion list.
pub fn clean(
    root: &TempRoot,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(OperationProgress),
) -> Result<CleanReport, CleanError> {
    let started = Instant::now();
    let root_path = root.path().to_path_buf();

    let guard = winfs::open_root_guard(&root_path).map_err(|error| CleanError {
        message: format!(
            "无法锁定目标目录 {}：{}",
            root_path.display(),
            describe_io_error(&error)
        ),
    })?;
    verify_root(&guard, root)?;

    // Use the path obtained from the live root handle as the safety boundary.
    let boundary = winfs::final_path(&guard).map_err(|error| CleanError {
        message: format!("无法确定目标目录的最终路径：{}", describe_io_error(&error)),
    })?;

    let root_entries = fs::read_dir(&boundary).map_err(|error| CleanError {
        message: format!(
            "无法读取目标目录 {}：{}",
            boundary.display(),
            describe_io_error(&error)
        ),
    })?;

    let mut state = CleanState::new();
    let mut cancelled = false;
    let mut pending_directories: Vec<(usize, PathBuf)> = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = Vec::new();
    let mut last_emit = Instant::now();
    let mut since_emit = 0u64;

    process_directory_entries(
        root_entries,
        &boundary,
        &boundary,
        1,
        cancel,
        &mut stack,
        &mut state,
        on_progress,
        &mut last_emit,
        &mut since_emit,
    );
    if cancel.load(Ordering::Relaxed) {
        cancelled = true;
    }

    'walk: while let Some((directory, depth)) = stack.pop() {
        if cancelled || cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }

        match verify_directory(&directory, &boundary) {
            Ok(()) => {}
            Err(reason) => {
                state.skip(&boundary, &directory, reason.0, reason.1);
                continue;
            }
        }

        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                if is_not_found(&error) {
                    state.skip(
                        &boundary,
                        &directory,
                        ProblemKind::NotFound,
                        describe_io_error(&error),
                    );
                } else {
                    state.fail(
                        &boundary,
                        &directory,
                        problem_kind_from_io(&error),
                        describe_io_error(&error),
                    );
                }
                continue;
            }
        };

        pending_directories.push((depth, directory.clone()));

        process_directory_entries(
            entries,
            &boundary,
            &directory,
            depth + 1,
            cancel,
            &mut stack,
            &mut state,
            on_progress,
            &mut last_emit,
            &mut since_emit,
        );

        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break 'walk;
        }
    }

    if !cancelled {
        pending_directories.sort_by_key(|(depth, _)| Reverse(*depth));
        for (_, directory) in pending_directories {
            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }
            delete_directory(
                &directory,
                &boundary,
                cancel,
                &mut state,
                on_progress,
                &mut last_emit,
                &mut since_emit,
            );
        }
    }

    if cancel.load(Ordering::Relaxed) {
        cancelled = true;
    }

    let status = if cancelled {
        CleanStatus::Cancelled
    } else if state.failed > 0 {
        if state.deleted_files > 0 || state.deleted_directories > 0 {
            CleanStatus::PartialSuccess
        } else {
            CleanStatus::Failed
        }
    } else if state.deleted_files == 0 && state.deleted_directories == 0 {
        CleanStatus::NothingToDo
    } else {
        CleanStatus::Completed
    };

    emit_progress(on_progress, Phase::Cleaning, &boundary, &boundary, &state);

    Ok(CleanReport {
        target: root.kind(),
        root: root_path,
        status,
        deleted_files: state.deleted_files,
        deleted_directories: state.deleted_directories,
        freed_bytes: state.freed_bytes,
        skipped: state.skipped,
        failed: state.failed,
        issues: state.issues,
        cancelled,
        elapsed: started.elapsed(),
    })
}

#[allow(clippy::too_many_arguments)]
fn process_directory_entries(
    entries: fs::ReadDir,
    boundary: &Path,
    directory: &Path,
    child_depth: usize,
    cancel: &AtomicBool,
    stack: &mut Vec<(PathBuf, usize)>,
    state: &mut CleanState,
    on_progress: &mut dyn FnMut(OperationProgress),
    last_emit: &mut Instant,
    since_emit: &mut u64,
) {
    for entry in entries {
        if cancel.load(Ordering::Relaxed) {
            return;
        }

        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                state.fail(
                    boundary,
                    directory,
                    problem_kind_from_io(&error),
                    describe_io_error(&error),
                );
                continue;
            }
        };
        let path = entry.path();

        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if is_not_found(&error) => {
                state.skip(
                    boundary,
                    &path,
                    ProblemKind::Changed,
                    "条目在清理过程中消失".to_owned(),
                );
                continue;
            }
            Err(error) => {
                state.fail(
                    boundary,
                    &path,
                    problem_kind_from_io(&error),
                    describe_io_error(&error),
                );
                continue;
            }
        };

        if winfs::is_reparse_point(&metadata) {
            state.skip(
                boundary,
                &path,
                ProblemKind::ReparsePoint,
                "不会跟随符号链接、目录联接或其他重解析点".to_owned(),
            );
        } else if metadata.is_dir() {
            stack.push((path.clone(), child_depth));
        } else if metadata.is_file() {
            delete_file(&path, boundary, cancel, state);
        } else {
            state.skip(
                boundary,
                &path,
                ProblemKind::UnsupportedType,
                "既不是普通文件也不是普通目录".to_owned(),
            );
        }

        *since_emit = since_emit.saturating_add(1);
        if *since_emit >= 100 || last_emit.elapsed() >= Duration::from_millis(120) {
            emit_progress(on_progress, Phase::Cleaning, boundary, &path, state);
            *since_emit = 0;
            *last_emit = Instant::now();
        }
    }
}

fn delete_file(path: &Path, boundary: &Path, cancel: &AtomicBool, state: &mut CleanState) {
    if cancel.load(Ordering::Relaxed) {
        return;
    }

    let file = match winfs::open_for_delete(path) {
        Ok(file) => file,
        Err(error) if is_not_found(&error) => {
            state.skip(
                boundary,
                path,
                ProblemKind::Changed,
                "文件在清理过程中消失".to_owned(),
            );
            return;
        }
        Err(error) => {
            state.fail(
                boundary,
                path,
                problem_kind_from_io(&error),
                describe_io_error(&error),
            );
            return;
        }
    };

    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(error) => {
            state.fail(
                boundary,
                path,
                problem_kind_from_io(&error),
                describe_io_error(&error),
            );
            return;
        }
    };

    if winfs::is_reparse_point(&metadata) {
        state.skip(
            boundary,
            path,
            ProblemKind::ReparsePoint,
            "打开后检测到重解析点，已跳过".to_owned(),
        );
        return;
    }
    if !metadata.is_file() {
        state.skip(
            boundary,
            path,
            ProblemKind::Changed,
            "条目在清理过程中不再是普通文件".to_owned(),
        );
        return;
    }

    match winfs::final_path(&file) {
        Ok(final_path) if winfs::is_strictly_inside(boundary, &final_path) => {}
        Ok(final_path) => {
            state.skip(
                boundary,
                path,
                ProblemKind::PathBoundary,
                format!("句柄最终路径越界：{}", final_path.display()),
            );
            return;
        }
        Err(error) => {
            state.fail(
                boundary,
                path,
                ProblemKind::PathBoundary,
                format!("无法确认最终路径：{}", describe_io_error(&error)),
            );
            return;
        }
    }

    let size = metadata.len();
    let read_only = winfs::is_read_only(&metadata);
    if let Err(error) = winfs::mark_for_delete(&file) {
        let kind = if read_only && error.raw_os_error() == Some(5) {
            ProblemKind::ReadOnly
        } else {
            problem_kind_from_io(&error)
        };
        state.fail(boundary, path, kind, describe_io_error(&error));
        return;
    }

    drop(file);
    state.deleted_files = state.deleted_files.saturating_add(1);
    state.freed_bytes = state.freed_bytes.saturating_add(size);
}

fn delete_directory(
    path: &Path,
    boundary: &Path,
    cancel: &AtomicBool,
    state: &mut CleanState,
    on_progress: &mut dyn FnMut(OperationProgress),
    last_emit: &mut Instant,
    since_emit: &mut u64,
) {
    if cancel.load(Ordering::Relaxed) {
        return;
    }

    let directory = match winfs::open_for_delete(path) {
        Ok(directory) => directory,
        Err(error) if is_not_found(&error) => {
            state.skip(
                boundary,
                path,
                ProblemKind::Changed,
                "目录在清理过程中消失".to_owned(),
            );
            return;
        }
        Err(error) => {
            state.fail(
                boundary,
                path,
                problem_kind_from_io(&error),
                describe_io_error(&error),
            );
            return;
        }
    };

    let metadata = match directory.metadata() {
        Ok(metadata) => metadata,
        Err(error) => {
            state.fail(
                boundary,
                path,
                problem_kind_from_io(&error),
                describe_io_error(&error),
            );
            return;
        }
    };
    if winfs::is_reparse_point(&metadata) {
        state.skip(
            boundary,
            path,
            ProblemKind::ReparsePoint,
            "打开后检测到重解析点，已跳过".to_owned(),
        );
        return;
    }
    if !metadata.is_dir() {
        state.skip(
            boundary,
            path,
            ProblemKind::Changed,
            "条目在清理过程中不再是普通目录".to_owned(),
        );
        return;
    }

    match winfs::final_path(&directory) {
        Ok(final_path) if winfs::is_strictly_inside(boundary, &final_path) => {}
        Ok(final_path) => {
            state.skip(
                boundary,
                path,
                ProblemKind::PathBoundary,
                format!("句柄最终路径越界：{}", final_path.display()),
            );
            return;
        }
        Err(error) => {
            state.fail(
                boundary,
                path,
                ProblemKind::PathBoundary,
                format!("无法确认最终路径：{}", describe_io_error(&error)),
            );
            return;
        }
    }

    if let Err(error) = winfs::mark_for_delete(&directory) {
        state.fail(
            boundary,
            path,
            problem_kind_from_io(&error),
            describe_io_error(&error),
        );
        return;
    }

    drop(directory);
    state.deleted_directories = state.deleted_directories.saturating_add(1);

    *since_emit = since_emit.saturating_add(1);
    if *since_emit >= 50 || last_emit.elapsed() >= Duration::from_millis(120) {
        emit_progress(on_progress, Phase::Cleaning, boundary, path, state);
        *since_emit = 0;
        *last_emit = Instant::now();
    }
}

fn verify_root(guard: &fs::File, root: &TempRoot) -> Result<(), CleanError> {
    let metadata = guard.metadata().map_err(|error| CleanError {
        message: format!("无法读取目标目录状态：{}", describe_io_error(&error)),
    })?;
    if !metadata.is_dir() || winfs::is_reparse_point(&metadata) {
        return Err(CleanError {
            message: "目标目录已变为非目录或重解析点，已停止清理".to_owned(),
        });
    }
    let identity = winfs::file_id(guard).map_err(|error| CleanError {
        message: format!("无法确认目标目录身份：{}", describe_io_error(&error)),
    })?;
    if identity != root.file_id() {
        return Err(CleanError {
            message: "目标目录在清理开始前发生变化，请重新扫描".to_owned(),
        });
    }
    Ok(())
}

fn verify_directory(directory: &Path, boundary: &Path) -> Result<(), (ProblemKind, String)> {
    let handle = winfs::open_directory_check(directory)
        .map_err(|error| (problem_kind_from_io(&error), describe_io_error(&error)))?;
    let metadata = handle
        .metadata()
        .map_err(|error| (problem_kind_from_io(&error), describe_io_error(&error)))?;
    if !metadata.is_dir() {
        return Err((ProblemKind::Changed, "条目已不再是目录".to_owned()));
    }
    if winfs::is_reparse_point(&metadata) {
        return Err((ProblemKind::ReparsePoint, "不会跟随重解析点".to_owned()));
    }
    let final_path = winfs::final_path(&handle)
        .map_err(|error| (problem_kind_from_io(&error), describe_io_error(&error)))?;
    if !winfs::is_strictly_inside(boundary, &final_path) {
        return Err((
            ProblemKind::PathBoundary,
            format!("句柄最终路径越界：{}", final_path.display()),
        ));
    }
    Ok(())
}

fn emit_progress(
    on_progress: &mut dyn FnMut(OperationProgress),
    phase: Phase,
    boundary: &Path,
    current: &Path,
    state: &CleanState,
) {
    on_progress(OperationProgress {
        phase,
        current: relative_display(boundary, current),
        files: state.deleted_files,
        directories: state.deleted_directories,
        bytes: state.freed_bytes,
        skipped: state.skipped,
        failed: state.failed,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::TargetKind;
    use crate::paths::{RootSource, TempRoot};
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    use std::sync::atomic::AtomicU32;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

    static NEXT_ID: AtomicU32 = AtomicU32::new(0);

    struct Fixture {
        base: PathBuf,
        temp: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let base = std::env::temp_dir()
                .join(format!("temp-cleaner-clean-{}-{id}", std::process::id()));
            let temp = base.join("Temp");
            fs::create_dir_all(&temp).unwrap();
            Self { base, temp }
        }

        fn root(&self) -> TempRoot {
            TempRoot::validate_candidate(&self.temp, TargetKind::UserTemp, RootSource::Programmatic)
                .unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    fn no_progress(_: OperationProgress) {}

    #[test]
    fn deletes_files_and_nested_directories_but_keeps_root() {
        let fixture = Fixture::new();
        fs::write(fixture.temp.join("one.tmp"), b"abc").unwrap();
        fs::create_dir_all(fixture.temp.join("a").join("b")).unwrap();
        fs::write(fixture.temp.join("a").join("two.tmp"), b"12345").unwrap();
        fs::write(fixture.temp.join("a").join("b").join("three"), b"z").unwrap();

        let report = clean(&fixture.root(), &AtomicBool::new(false), &mut no_progress).unwrap();

        assert_eq!(report.status, CleanStatus::Completed);
        assert_eq!(report.deleted_files, 3);
        assert_eq!(report.deleted_directories, 2);
        assert_eq!(report.freed_bytes, 9);
        assert_eq!(report.failed, 0);
        assert!(fixture.temp.is_dir());
        assert_eq!(fs::read_dir(&fixture.temp).unwrap().count(), 0);
    }

    #[test]
    fn deletes_empty_directory() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.temp.join("empty")).unwrap();
        let report = clean(&fixture.root(), &AtomicBool::new(false), &mut no_progress).unwrap();
        assert_eq!(report.deleted_directories, 1);
        assert_eq!(report.status, CleanStatus::Completed);
        assert!(!fixture.temp.join("empty").exists());
    }

    #[test]
    fn does_not_delete_outside_files() {
        let fixture = Fixture::new();
        let outside = fixture.base.join("outside.txt");
        fs::write(&outside, b"keep").unwrap();
        fs::write(fixture.temp.join("inside.txt"), b"remove").unwrap();

        clean(&fixture.root(), &AtomicBool::new(false), &mut no_progress).unwrap();

        assert!(outside.exists());
        assert!(!fixture.temp.join("inside.txt").exists());
    }

    #[test]
    fn locked_file_is_reported_as_failure() {
        let fixture = Fixture::new();
        let path = fixture.temp.join("locked.bin");
        fs::write(&path, b"locked").unwrap();
        let _lock = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&path)
            .unwrap();

        let report = clean(&fixture.root(), &AtomicBool::new(false), &mut no_progress).unwrap();

        assert_eq!(report.deleted_files, 0);
        assert!(report.failed >= 1);
        assert!(report.status == CleanStatus::Failed);
        assert!(path.exists());
    }

    #[test]
    fn cancellation_before_start_deletes_nothing() {
        let fixture = Fixture::new();
        fs::write(fixture.temp.join("keep.tmp"), b"x").unwrap();
        let report = clean(&fixture.root(), &AtomicBool::new(true), &mut no_progress).unwrap();
        assert!(report.cancelled);
        assert_eq!(report.status, CleanStatus::Cancelled);
        assert_eq!(report.deleted_files, 0);
        assert!(fixture.temp.join("keep.tmp").exists());
    }

    #[test]
    fn cancellation_during_cleanup_stops_subsequent_deletions() {
        let fixture = Fixture::new();
        for index in 0..400 {
            fs::write(fixture.temp.join(format!("{index}.tmp")), b"x").unwrap();
        }

        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let cancel_from_progress = std::sync::Arc::clone(&cancel);
        let mut progress = |_update: OperationProgress| {
            cancel_from_progress.store(true, Ordering::Relaxed);
        };

        let report = clean(&fixture.root(), &cancel, &mut progress).unwrap();
        assert!(report.cancelled);
        assert_eq!(report.status, CleanStatus::Cancelled);
        assert!(report.deleted_files < 400);
        assert!(fs::read_dir(&fixture.temp).unwrap().count() > 0);
    }

    #[test]
    fn junction_is_not_followed() {
        let fixture = Fixture::new();
        let outside = fixture.base.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep.txt"), b"keep").unwrap();

        let link = fixture.temp.join("link");
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&outside)
            .status();
        if !matches!(status, Ok(status) if status.success()) {
            eprintln!("skipping junction test: could not create junction");
            return;
        }

        let report = clean(&fixture.root(), &AtomicBool::new(false), &mut no_progress).unwrap();

        assert!(report.skipped >= 1);
        assert!(outside.join("keep.txt").exists());
        assert!(link.exists());
    }
}
