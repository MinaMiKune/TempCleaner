use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};

use eframe::egui;

use crate::cleanup;
use crate::models::{CleanReport, OperationProgress, ScanReport};
use crate::paths::TempRoot;
use crate::scan;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerKind {
    Scan,
    Clean,
}

#[derive(Debug)]
pub enum WorkerMessage {
    Progress(OperationProgress),
    ScanFinished(Result<ScanReport, String>),
    CleanFinished(Result<CleanReport, String>),
}

pub struct RunningWorker {
    pub kind: WorkerKind,
    pub receiver: Receiver<WorkerMessage>,
    cancel: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl RunningWorker {
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn join(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for RunningWorker {
    fn drop(&mut self) {
        self.cancel();
        // The thread observes the cancellation flag at its next safe point.
        // It is intentionally not joined here: closing the window must not
        // block on a slow file-system call. Process exit ends the thread.
    }
}

pub fn spawn_scan(root: TempRoot, ctx: egui::Context) -> Result<RunningWorker, String> {
    spawn_worker(WorkerKind::Scan, root, ctx)
}

pub fn spawn_clean(root: TempRoot, ctx: egui::Context) -> Result<RunningWorker, String> {
    spawn_worker(WorkerKind::Clean, root, ctx)
}

fn spawn_worker(
    kind: WorkerKind,
    root: TempRoot,
    ctx: egui::Context,
) -> Result<RunningWorker, String> {
    let (sender, receiver) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let thread_cancel = Arc::clone(&cancel);

    let handle = thread::Builder::new()
        .name(match kind {
            WorkerKind::Scan => "temp-scan".to_owned(),
            WorkerKind::Clean => "temp-clean".to_owned(),
        })
        .spawn(move || {
            let progress_sender = sender.clone();
            let progress_ctx = ctx.clone();
            let mut on_progress = move |progress: OperationProgress| {
                if progress_sender
                    .send(WorkerMessage::Progress(progress))
                    .is_ok()
                {
                    progress_ctx.request_repaint();
                }
            };

            let outcome = catch_unwind(AssertUnwindSafe(|| match kind {
                WorkerKind::Scan => match scan::scan(&root, &thread_cancel, &mut on_progress) {
                    Ok(report) => WorkerMessage::ScanFinished(Ok(report)),
                    Err(error) => WorkerMessage::ScanFinished(Err(error.to_string())),
                },
                WorkerKind::Clean => {
                    match cleanup::clean(&root, &thread_cancel, &mut on_progress) {
                        Ok(report) => WorkerMessage::CleanFinished(Ok(report)),
                        Err(error) => WorkerMessage::CleanFinished(Err(error.to_string())),
                    }
                }
            }));

            let message = match outcome {
                Ok(message) => message,
                Err(_) => match kind {
                    WorkerKind::Scan => WorkerMessage::ScanFinished(Err(
                        "扫描线程发生内部错误，操作已中止".to_owned(),
                    )),
                    WorkerKind::Clean => WorkerMessage::CleanFinished(Err(
                        "清理线程发生内部错误，操作已中止".to_owned(),
                    )),
                },
            };

            let _ = sender.send(message);
            ctx.request_repaint();
        })
        .map_err(|error| format!("无法启动后台任务：{error}"))?;

    Ok(RunningWorker {
        kind,
        receiver,
        cancel,
        handle: Some(handle),
    })
}
