use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

/// The two cleanup targets exposed by the application.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetKind {
    UserTemp,
    SystemTemp,
}

impl TargetKind {
    pub const ALL: [Self; 2] = [Self::UserTemp, Self::SystemTemp];

    pub fn name_zh(self) -> &'static str {
        match self {
            Self::UserTemp => "当前用户 Temp",
            Self::SystemTemp => "Windows 系统 Temp",
        }
    }

    pub fn source_hint(self) -> &'static str {
        match self {
            Self::UserTemp => "%TEMP%（缺失时参考 %TMP%）",
            Self::SystemTemp => "%SystemRoot%\\Temp",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IssueClass {
    Skipped,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ProblemKind {
    AccessDenied,
    InUse,
    NotFound,
    ReparsePoint,
    PathBoundary,
    UnsupportedType,
    Changed,
    ReadOnly,
    DiskFull,
    PathTooLong,
    Io,
    Internal,
}

impl ProblemKind {
    pub fn label_zh(self) -> &'static str {
        match self {
            Self::AccessDenied => "访问被拒绝",
            Self::InUse => "文件被占用",
            Self::NotFound => "条目已不存在",
            Self::ReparsePoint => "符号链接/重解析点",
            Self::PathBoundary => "超出允许的目录边界",
            Self::UnsupportedType => "不支持的文件类型",
            Self::Changed => "扫描后状态已变化",
            Self::ReadOnly => "只读属性",
            Self::DiskFull => "磁盘空间或系统资源不足",
            Self::PathTooLong => "路径过长",
            Self::Io => "文件系统错误",
            Self::Internal => "内部错误",
        }
    }
}

impl fmt::Display for ProblemKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label_zh())
    }
}

#[derive(Clone, Debug)]
pub struct Issue {
    pub class: IssueClass,
    pub kind: ProblemKind,
    /// Path relative to the selected Temp root, for display.
    pub relative_path: String,
    pub detail: String,
}

/// A bounded issue list: totals stay exact, while UI detail is capped.
#[derive(Clone, Debug, Default)]
pub struct Issues {
    pub items: Vec<Issue>,
    pub total: u64,
    pub truncated: bool,
}

pub const MAX_RECORDED_ISSUES: usize = 240;

impl Issues {
    pub fn push(&mut self, issue: Issue) {
        self.total = self.total.saturating_add(1);
        if self.items.len() < MAX_RECORDED_ISSUES {
            self.items.push(issue);
        } else {
            self.truncated = true;
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanCounts {
    pub files: u64,
    pub directories: u64,
    pub bytes: u64,
    pub skipped_files: u64,
    pub skipped_directories: u64,
    pub skipped_other: u64,
}

impl ScanCounts {
    pub fn skipped_total(self) -> u64 {
        self.skipped_files
            .saturating_add(self.skipped_directories)
            .saturating_add(self.skipped_other)
    }
}

#[derive(Clone, Debug)]
pub struct ScanReport {
    pub target: TargetKind,
    pub root: PathBuf,
    pub counts: ScanCounts,
    pub issues: Issues,
    pub cancelled: bool,
    pub elapsed: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CleanStatus {
    Completed,
    NothingToDo,
    PartialSuccess,
    Failed,
    Cancelled,
}

impl CleanStatus {
    pub fn label_zh(self) -> &'static str {
        match self {
            Self::Completed => "清理完成",
            Self::NothingToDo => "没有可清理的内容",
            Self::PartialSuccess => "部分成功",
            Self::Failed => "清理失败",
            Self::Cancelled => "已取消",
        }
    }
}

#[derive(Clone, Debug)]
pub struct CleanReport {
    pub target: TargetKind,
    pub root: PathBuf,
    pub status: CleanStatus,
    pub deleted_files: u64,
    pub deleted_directories: u64,
    pub freed_bytes: u64,
    pub skipped: u64,
    pub failed: u64,
    pub issues: Issues,
    pub cancelled: bool,
    pub elapsed: Duration,
}

impl CleanReport {
    pub fn deleted_total(&self) -> u64 {
        self.deleted_files.saturating_add(self.deleted_directories)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Scanning,
    Cleaning,
}

impl Phase {
    pub fn label_zh(self) -> &'static str {
        match self {
            Self::Scanning => "正在扫描",
            Self::Cleaning => "正在清理",
        }
    }
}

/// Progress is deliberately count-based. No percentage is invented.
#[derive(Clone, Debug)]
pub struct OperationProgress {
    pub phase: Phase,
    pub current: String,
    /// Files found (scan) or deleted (clean).
    pub files: u64,
    /// Directories found (scan) or deleted (clean).
    pub directories: u64,
    /// Bytes found (scan) or actually freed (clean).
    pub bytes: u64,
    pub skipped: u64,
    pub failed: u64,
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

pub fn format_duration(duration: Duration) -> String {
    if duration.as_secs() >= 1 {
        format!("{:.1} 秒", duration.as_secs_f64())
    } else {
        format!("{} 毫秒", duration.as_millis())
    }
}
