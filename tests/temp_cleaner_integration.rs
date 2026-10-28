use std::fs;
use std::fs::OpenOptions;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use temp_cleaner::cleanup::{CleanError, clean};
use temp_cleaner::models::{CleanStatus, OperationProgress, ScanReport, TargetKind};
use temp_cleaner::paths::{RootSource, TempRoot};
use temp_cleaner::scan::{ScanError, scan};
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

static NEXT_ID: AtomicU32 = AtomicU32::new(0);

struct Fixture {
    base: PathBuf,
    temp: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "temp-cleaner-integration-{}-{id}",
            std::process::id()
        ));
        let temp = base.join("Temp");
        fs::create_dir_all(&temp).unwrap();
        Self { base, temp }
    }

    fn root(&self) -> TempRoot {
        TempRoot::validate_candidate(&self.temp, TargetKind::UserTemp, RootSource::Programmatic)
            .expect("integration fixture should pass path validation")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn no_progress(_: OperationProgress) {}

fn run_scan(root: &TempRoot) -> Result<ScanReport, ScanError> {
    scan(root, &AtomicBool::new(false), &mut no_progress)
}

fn run_clean(root: &TempRoot) -> Result<temp_cleaner::models::CleanReport, CleanError> {
    clean(root, &AtomicBool::new(false), &mut no_progress)
}

fn create_junction(link: &Path, target: &Path) -> bool {
    matches!(
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .status(),
        Ok(status) if status.success()
    )
}

#[test]
fn complete_cleanup_keeps_root_and_does_not_touch_outside() {
    let fixture = Fixture::new();
    let outside = fixture.base.join("outside.txt");
    fs::write(&outside, b"outside").unwrap();

    fs::write(fixture.temp.join("normal.log"), vec![b'x'; 100]).unwrap();
    fs::create_dir_all(fixture.temp.join("folder with space").join("nested")).unwrap();
    fs::write(
        fixture
            .temp
            .join("folder with space")
            .join("中文 文件 #1.tmp"),
        b"unicode",
    )
    .unwrap();
    fs::write(
        fixture
            .temp
            .join("folder with space")
            .join("nested")
            .join("deep.bin"),
        vec![0u8; 20],
    )
    .unwrap();

    let scan_report = run_scan(&fixture.root()).unwrap();
    assert_eq!(scan_report.counts.files, 3);
    assert_eq!(scan_report.counts.directories, 2);
    assert_eq!(scan_report.counts.bytes, 127);
    assert_eq!(scan_report.counts.skipped_total(), 0);

    let report = run_clean(&fixture.root()).unwrap();
    assert_eq!(report.status, CleanStatus::Completed);
    assert_eq!(report.deleted_files, 3);
    assert_eq!(report.deleted_directories, 2);
    assert_eq!(report.freed_bytes, 127);
    assert_eq!(report.failed, 0);
    assert!(fixture.temp.is_dir(), "Temp root itself must stay");
    assert_eq!(fs::read_dir(&fixture.temp).unwrap().count(), 0);
    assert!(
        outside.exists(),
        "file outside the target must stay untouched"
    );

    let after = run_scan(&fixture.root()).unwrap();
    assert_eq!(after.counts.files, 0);
    assert_eq!(after.counts.directories, 0);

    let second = run_clean(&fixture.root()).unwrap();
    assert_eq!(second.status, CleanStatus::NothingToDo);
    assert_eq!(second.deleted_files, 0);
}

#[test]
fn clean_rechecks_entries_instead_of_using_scan_list() {
    let fixture = Fixture::new();
    fs::write(fixture.temp.join("old.tmp"), b"old").unwrap();

    let scan_report = run_scan(&fixture.root()).unwrap();
    assert_eq!(scan_report.counts.files, 1);

    // Change the directory after scanning: remove the old file and add a new
    // one. Cleanup must traverse again and reflect actual disk state.
    fs::remove_file(fixture.temp.join("old.tmp")).unwrap();
    fs::write(fixture.temp.join("new.tmp"), b"new").unwrap();

    let report = run_clean(&fixture.root()).unwrap();
    assert_eq!(report.deleted_files, 1);
    assert!(!fixture.temp.join("new.tmp").exists());
    assert!(!fixture.temp.join("old.tmp").exists());
}

#[test]
fn locked_file_is_reported_without_false_success() {
    let fixture = Fixture::new();
    fs::write(fixture.temp.join("free.tmp"), b"free").unwrap();
    let locked_path = fixture.temp.join("locked.tmp");
    fs::write(&locked_path, b"locked").unwrap();
    let _lock = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(&locked_path)
        .unwrap();

    let report = run_clean(&fixture.root()).unwrap();

    assert_eq!(report.status, CleanStatus::PartialSuccess);
    assert_eq!(report.deleted_files, 1);
    assert!(report.failed >= 1);
    assert!(locked_path.exists());
    assert!(!fixture.temp.join("free.tmp").exists());
    assert!(
        report
            .issues
            .items
            .iter()
            .any(|issue| issue.relative_path.contains("locked.tmp"))
    );
}

#[test]
fn junction_is_skipped_and_external_target_is_untouched() {
    let fixture = Fixture::new();
    let outside = fixture.base.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("keep.txt"), b"keep").unwrap();

    let link = fixture.temp.join("external-link");
    if !create_junction(&link, &outside) {
        eprintln!("skipping junction integration test: junction creation unavailable");
        return;
    }

    let scan_report = run_scan(&fixture.root()).unwrap();
    assert_eq!(scan_report.counts.files, 0);
    assert!(scan_report.counts.skipped_directories >= 1);

    let report = run_clean(&fixture.root()).unwrap();
    assert!(report.skipped >= 1);
    assert_eq!(report.deleted_files, 0);
    assert!(outside.join("keep.txt").exists());
    assert!(link.exists(), "the junction itself is skipped");

    let _ = fs::remove_dir(&link);
}

#[test]
fn a_file_replaced_by_junction_after_scan_is_not_followed() {
    let fixture = Fixture::new();
    let outside = fixture.base.join("replacement-target");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("keep.txt"), b"keep").unwrap();

    let path = fixture.temp.join("race-entry");
    fs::write(&path, b"initial").unwrap();
    let _ = run_scan(&fixture.root()).unwrap();

    // Replace the scanned file with a directory junction. The cleaner must not
    // trust the scanned type or follow the new reparse point.
    fs::remove_file(&path).unwrap();
    if !create_junction(&path, &outside) {
        eprintln!("skipping TOCTOU junction test: junction creation unavailable");
        return;
    }

    let report = run_clean(&fixture.root()).unwrap();
    assert!(report.skipped >= 1);
    assert_eq!(report.deleted_files, 0);
    assert!(outside.join("keep.txt").exists());
    assert!(path.exists());

    let _ = fs::remove_dir(&path);
}

#[test]
fn prefix_sibling_directory_is_outside_the_scope() {
    let fixture = Fixture::new();
    let sibling = fixture.base.join("TempEvil");
    fs::create_dir_all(&sibling).unwrap();
    fs::write(sibling.join("keep.txt"), b"keep").unwrap();
    fs::write(fixture.temp.join("remove.txt"), b"remove").unwrap();

    let report = run_clean(&fixture.root()).unwrap();
    assert_eq!(report.deleted_files, 1);
    assert!(sibling.join("keep.txt").exists());
}

#[test]
#[allow(clippy::permissions_set_readonly_false)]
fn read_only_file_result_matches_filesystem_state() {
    let fixture = Fixture::new();
    let path = fixture.temp.join("readonly.tmp");
    fs::write(&path, b"readonly").unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&path, permissions).unwrap();

    let report = run_clean(&fixture.root()).unwrap();
    let still_exists = path.exists();

    if still_exists {
        assert_eq!(report.deleted_files, 0);
        assert!(report.failed >= 1);
        assert_eq!(report.status, CleanStatus::Failed);
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(false);
        fs::set_permissions(&path, permissions).unwrap();
    } else {
        assert_eq!(report.deleted_files, 1);
        assert_eq!(report.failed, 0);
    }
}
