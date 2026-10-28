#![cfg_attr(not(windows), allow(unused))]

#[cfg(not(windows))]
compile_error!("TempCleaner 仅支持 Windows 10/11 x64。");

pub mod app;
pub mod cleanup;
pub mod error;
pub mod models;
pub mod paths;
pub mod scan;
pub mod winfs;
pub mod worker;
