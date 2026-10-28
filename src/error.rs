use std::fmt;
use std::io;

use crate::models::ProblemKind;

#[derive(Debug)]
pub enum PathError {
    MissingEnvironment(&'static str),
    EmptyEnvironment(&'static str),
    EnvironmentExpansion(String),
    NotAbsolute(String),
    RelativeComponent(String),
    UnsupportedLocation(String),
    NotFound(String),
    NotDirectory(String),
    ReparsePoint(String),
    ProtectedPath(String),
    InvalidName(String),
    AccessDenied(String),
    Io(String),
}

impl PathError {
    pub fn message(&self) -> &str {
        match self {
            Self::MissingEnvironment(var) => var,
            Self::EmptyEnvironment(var) => var,
            Self::EnvironmentExpansion(message)
            | Self::NotAbsolute(message)
            | Self::RelativeComponent(message)
            | Self::UnsupportedLocation(message)
            | Self::NotFound(message)
            | Self::NotDirectory(message)
            | Self::ReparsePoint(message)
            | Self::ProtectedPath(message)
            | Self::InvalidName(message)
            | Self::AccessDenied(message)
            | Self::Io(message) => message,
        }
    }
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingEnvironment(var) => write!(f, "环境变量 {var} 缺失"),
            Self::EmptyEnvironment(var) => write!(f, "环境变量 {var} 为空"),
            Self::EnvironmentExpansion(message) => write!(f, "环境变量展开失败：{message}"),
            Self::NotAbsolute(message) => write!(f, "解析结果不是绝对路径：{message}"),
            Self::RelativeComponent(message) => {
                write!(f, "路径包含不允许的相对路径片段：{message}")
            }
            Self::UnsupportedLocation(message) => {
                write!(f, "路径不是受支持的本地驱动器目录：{message}")
            }
            Self::NotFound(message) => write!(f, "目录不存在：{message}"),
            Self::NotDirectory(message) => write!(f, "路径不是目录：{message}"),
            Self::ReparsePoint(message) => {
                write!(f, "路径包含符号链接、目录联接或其他重解析点：{message}")
            }
            Self::ProtectedPath(message) => write!(f, "目录属于受保护位置，拒绝清理：{message}"),
            Self::InvalidName(message) => write!(f, "目录不符合允许的 Temp 目标规则：{message}"),
            Self::AccessDenied(message) => write!(f, "没有访问目录的权限：{message}"),
            Self::Io(message) => write!(f, "路径检查失败：{message}"),
        }
    }
}

impl std::error::Error for PathError {}

impl From<io::Error> for PathError {
    fn from(error: io::Error) -> Self {
        Self::Io(describe_io_error(&error))
    }
}

pub fn describe_io_error(error: &io::Error) -> String {
    match error.raw_os_error() {
        Some(code) => format!("{error} (Win32 {code})"),
        None => error.to_string(),
    }
}

/// Map common Win32 error codes to user-facing categories.
pub fn problem_kind_from_io(error: &io::Error) -> ProblemKind {
    match error.raw_os_error() {
        Some(5) => ProblemKind::AccessDenied, // ERROR_ACCESS_DENIED
        Some(19) => ProblemKind::ReadOnly,    // ERROR_WRITE_PROTECT
        Some(32 | 33) => ProblemKind::InUse,  // sharing/lock violation
        Some(2 | 3 | 64 | 67) => ProblemKind::NotFound,
        Some(112) => ProblemKind::DiskFull,    // ERROR_DISK_FULL
        Some(145) => ProblemKind::Changed,     // ERROR_DIR_NOT_EMPTY
        Some(206) => ProblemKind::PathTooLong, // ERROR_FILENAME_EXCED_RANGE
        _ => ProblemKind::Io,
    }
}

pub fn is_not_found(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(2 | 3 | 64 | 67))
}
