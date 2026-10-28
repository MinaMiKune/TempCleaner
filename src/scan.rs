use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::error::{describe_io_error, problem_kind_from_io};
use crate::models::{
    Issue, IssueClass, Issues, OperationProgress, Phase, ProblemKind, ScanCounts, ScanReport,
};
use crate::paths::TempRoot;
use crate::winfs;

#[derive(Debug)]
pub struct ScanError {
    pub message: String,
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ScanError {}

pub fn scan(
    root: &TempRoot,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(OperationProgress),
) -> Result<ScanReport, ScanError> {
    let started = Instant::now();
    let root_path = root.path().to_path_buf();

    // Keep a non-delete-sharing handle open while scanning. This prevents the
    // selected root itself from being renamed or replaced during the scan.
    let guard = winfs::open_root_guard(&root_path).map_err(|error| ScanError {
        message: format!(
            "无法打开目标目录 {}：{}",
            root_path.display(),
            describe_io_error(&error)
        ),
    })?;
    verify_root(&guard, root)?;

    let mut counts = ScanCounts::default();
    let mut issues = Issues::default();
    let mut cancelled = false;
    let mut stack: Vec<(PathBuf, bool)> = vec![(root_path.clone(), true)];
    let mut last_emit = Instant::now();
    let mut since_emit = 0u64;

    'outer: while let Some((directory, is_root)) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }

        if !is_root && let Err(reason) = verify_directory(&directory, &root_path) {
            record_directory_skip(&mut counts, &mut issues, &root_path, &directory, reason);
            continue;
        }

        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                if is_root {
                    return Err(ScanError {
                        message: format!(
                            "无法读取目标目录 {}：{}",
                            directory.display(),
                            describe_io_error(&error)
                        ),
                    });
                }
                record_directory_skip(
                    &mut counts,
                    &mut issues,
                    &root_path,
                    &directory,
                    (problem_kind_from_io(&error), describe_io_error(&error)),
                );
                continue;
            }
        };

        if !is_root {
            counts.directories = counts.directories.saturating_add(1);
        }

        for entry in entries {
            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
                break 'outer;
            }

            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    counts.skipped_other = counts.skipped_other.saturating_add(1);
                    push_issue(
                        &mut issues,
                        &directory,
                        &root_path,
                        ProblemKind::Io,
                        describe_io_error(&error),
                    );
                    continue;
                }
            };
            let path = entry.path();

            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    counts.skipped_other = counts.skipped_other.saturating_add(1);
                    push_issue(
                        &mut issues,
                        &path,
                        &root_path,
                        problem_kind_from_io(&error),
                        describe_io_error(&error),
                    );
                    continue;
                }
            };

            if winfs::is_reparse_point(&metadata) {
                if winfs::is_directory(&metadata) {
                    counts.skipped_directories = counts.skipped_directories.saturating_add(1);
                } else {
                    counts.skipped_files = counts.skipped_files.saturating_add(1);
                }
                push_issue(
                    &mut issues,
                    &path,
                    &root_path,
                    ProblemKind::ReparsePoint,
                    "不会跟随符号链接、目录联接或其他重解析点".to_owned(),
                );
            } else if metadata.is_dir() {
                stack.push((path.clone(), false));
            } else if metadata.is_file() {
                counts.files = counts.files.saturating_add(1);
                counts.bytes = counts.bytes.saturating_add(metadata.len());
            } else {
                counts.skipped_other = counts.skipped_other.saturating_add(1);
                push_issue(
                    &mut issues,
                    &path,
                    &root_path,
                    ProblemKind::UnsupportedType,
                    "既不是普通文件也不是普通目录".to_owned(),
                );
            }

            since_emit = since_emit.saturating_add(1);
            if since_emit >= 200 || last_emit.elapsed() >= Duration::from_millis(120) {
                emit_progress(on_progress, Phase::Scanning, &root_path, &path, &counts, 0);
                since_emit = 0;
                last_emit = Instant::now();
            }
        }

        if !is_root {
            emit_progress(
                on_progress,
                Phase::Scanning,
                &root_path,
                &directory,
                &counts,
                0,
            );
        }
    }

    emit_progress(
        on_progress,
        Phase::Scanning,
        &root_path,
        &root_path,
        &counts,
        0,
    );

    Ok(ScanReport {
        target: root.kind(),
        root: root_path,
        counts,
        issues,
        cancelled,
        elapsed: started.elapsed(),
    })
}

fn verify_root(guard: &fs::File, root: &TempRoot) -> Result<(), ScanError> {
    let metadata = guard.metadata().map_err(|error| ScanError {
        message: format!("无法读取目标目录状态：{}", describe_io_error(&error)),
    })?;
    if !metadata.is_dir() || winfs::is_reparse_point(&metadata) {
        return Err(ScanError {
            message: "目标目录已变为非目录或重解析点，已停止扫描".to_owned(),
        });
    }
    let identity = winfs::file_id(guard).map_err(|error| ScanError {
        message: format!("无法确认目标目录身份：{}", describe_io_error(&error)),
    })?;
    if identity != root.file_id() {
        return Err(ScanError {
            message: "目标目录在扫描开始前发生变化，请重新选择目标".to_owned(),
        });
    }
    Ok(())
}

fn verify_directory(directory: &Path, root: &Path) -> Result<(), (ProblemKind, String)> {
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
    if !winfs::is_strictly_inside(root, &final_path) {
        return Err((
            ProblemKind::PathBoundary,
            format!("句柄最终路径越界：{}", final_path.display()),
        ));
    }
    Ok(())
}

fn record_directory_skip(
    counts: &mut ScanCounts,
    issues: &mut Issues,
    root: &Path,
    path: &Path,
    reason: (ProblemKind, String),
) {
    counts.skipped_directories = counts.skipped_directories.saturating_add(1);
    push_issue(issues, path, root, reason.0, reason.1);
}

fn push_issue(issues: &mut Issues, path: &Path, root: &Path, kind: ProblemKind, detail: String) {
    issues.push(Issue {
        class: IssueClass::Skipped,
        kind,
        relative_path: relative_display(root, path),
        detail,
    });
}

fn emit_progress(
    on_progress: &mut dyn FnMut(OperationProgress),
    phase: Phase,
    root: &Path,
    current: &Path,
    counts: &ScanCounts,
    failed: u64,
) {
    on_progress(OperationProgress {
        phase,
        current: relative_display(root, current),
        files: counts.files,
        directories: counts.directories,
        bytes: counts.bytes,
        skipped: counts.skipped_total(),
        failed,
    });
}

pub(crate) fn relative_display(root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(relative) if relative.as_os_str().is_empty() => ".".to_owned(),
        Ok(relative) => relative.display().to_string(),
        Err(_) => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::TargetKind;
    use crate::paths::{RootSource, TempRoot};
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicU32;

    static NEXT_ID: AtomicU32 = AtomicU32::new(0);

    struct Fixture {
        base: PathBuf,
        temp: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let base =
                std::env::temp_dir().join(format!("temp-cleaner-scan-{}-{id}", std::process::id()));
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
    fn scans_files_and_nested_directories() {
        let fixture = Fixture::new();
        fs::write(fixture.temp.join("a.txt"), b"abc").unwrap();
        fs::create_dir_all(fixture.temp.join("sub").join("deeper")).unwrap();
        fs::write(fixture.temp.join("sub").join("b.bin"), vec![0u8; 5]).unwrap();
        fs::write(fixture.temp.join("sub").join("deeper").join("c"), b"").unwrap();

        let report = scan(&fixture.root(), &AtomicBool::new(false), &mut no_progress).unwrap();

        assert_eq!(report.counts.files, 3);
        assert_eq!(report.counts.directories, 2);
        assert_eq!(report.counts.bytes, 8);
        assert_eq!(report.counts.skipped_total(), 0);
        assert!(!report.cancelled);
    }

    #[test]
    fn scans_empty_directory() {
        let fixture = Fixture::new();
        let report = scan(&fixture.root(), &AtomicBool::new(false), &mut no_progress).unwrap();
        assert_eq!(report.counts.files, 0);
        assert_eq!(report.counts.directories, 0);
        assert_eq!(report.counts.bytes, 0);
    }

    #[test]
    fn scan_can_be_cancelled() {
        let fixture = Fixture::new();
        for index in 0..100 {
            fs::write(fixture.temp.join(format!("{index}.tmp")), b"x").unwrap();
        }
        let cancel = AtomicBool::new(true);
        let report = scan(&fixture.root(), &cancel, &mut no_progress).unwrap();
        assert!(report.cancelled);
    }

    #[test]
    fn cancellation_during_scan_stops_further_enumeration() {
        let fixture = Fixture::new();
        for index in 0..400 {
            fs::write(fixture.temp.join(format!("{index}.tmp")), b"x").unwrap();
        }

        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let cancel_from_progress = std::sync::Arc::clone(&cancel);
        let mut progress = |_update: OperationProgress| {
            cancel_from_progress.store(true, Ordering::Relaxed);
        };

        let report = scan(&fixture.root(), &cancel, &mut progress).unwrap();
        assert!(report.cancelled);
        assert!(report.counts.files < 400);
    }

    #[test]
    fn junction_is_skipped_and_not_traversed() {
        let fixture = Fixture::new();
        let outside = fixture.base.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("do-not-touch.txt"), b"outside").unwrap();

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

        let report = scan(&fixture.root(), &AtomicBool::new(false), &mut no_progress).unwrap();
        assert_eq!(report.counts.files, 0);
        assert_eq!(report.counts.skipped_directories, 1);
        assert!(outside.join("do-not-touch.txt").exists());
    }
}
