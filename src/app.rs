use std::time::Duration;

use eframe::egui;
use egui::{Align, Color32, FontId, Frame, Label, RichText, ScrollArea, Stroke, Vec2};

use crate::models::{
    CleanReport, CleanStatus, Issue, IssueClass, Issues, OperationProgress, Phase, ScanReport,
    TargetKind, format_bytes, format_duration,
};
use crate::paths::{TargetReport, TargetState, TempRoot, discover_targets};
use crate::worker::{self, RunningWorker, WorkerMessage};

const BG: Color32 = Color32::from_rgb(246, 247, 249);
const CARD: Color32 = Color32::from_rgb(255, 255, 255);
const TEXT_MUTED: Color32 = Color32::from_rgb(100, 106, 115);
const ACCENT: Color32 = Color32::from_rgb(35, 105, 190);
const DANGER: Color32 = Color32::from_rgb(178, 47, 47);
const WARN: Color32 = Color32::from_rgb(164, 111, 12);
const OK: Color32 = Color32::from_rgb(42, 125, 76);
const BORDER: Color32 = Color32::from_rgb(220, 224, 230);

pub struct TempCleanerApp {
    targets: Vec<TargetReport>,
    selected: TargetKind,
    scan: Option<ScanReport>,
    clean: Option<CleanReport>,
    worker: Option<RunningWorker>,
    progress: Option<OperationProgress>,
    confirm_open: bool,
    confirm_checked: bool,
    notice: Option<Notice>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NoticeLevel {
    Info,
    Error,
}

struct Notice {
    level: NoticeLevel,
    text: String,
}

impl TempCleanerApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        configure_style(&cc.egui_ctx);
        let targets = discover_targets();
        let selected = TargetKind::ALL
            .into_iter()
            .find(|kind| {
                targets
                    .iter()
                    .any(|target| target.kind == *kind && target.is_selectable())
            })
            .unwrap_or(TargetKind::UserTemp);

        let mut app = Self {
            targets,
            selected,
            scan: None,
            clean: None,
            worker: None,
            progress: None,
            confirm_open: false,
            confirm_checked: false,
            notice: None,
        };

        if app.root_for(app.selected).is_some() {
            app.start_scan(&cc.egui_ctx);
        }
        app
    }

    fn report(&self, kind: TargetKind) -> Option<&TargetReport> {
        self.targets.iter().find(|target| target.kind == kind)
    }

    fn root_for(&self, kind: TargetKind) -> Option<TempRoot> {
        self.report(kind).and_then(|target| target.root.clone())
    }

    fn busy(&self) -> bool {
        self.worker.is_some()
    }

    fn select_target(&mut self, kind: TargetKind) {
        if self.busy() || self.selected == kind {
            return;
        }
        let Some(report) = self.report(kind) else {
            return;
        };
        if !report.is_selectable() {
            return;
        }
        self.selected = kind;
        self.scan = None;
        self.clean = None;
        self.progress = None;
        self.confirm_open = false;
        self.notice = Some(Notice {
            level: NoticeLevel::Info,
            text: "已切换清理目标。请先扫描预览，再决定是否清理。".to_owned(),
        });
    }

    fn start_scan(&mut self, ctx: &egui::Context) {
        if self.busy() {
            return;
        }
        let Some(root) = self.root_for(self.selected) else {
            self.notice = Some(Notice {
                level: NoticeLevel::Error,
                text: "当前目标不可用，无法扫描。".to_owned(),
            });
            return;
        };

        self.scan = None;
        self.clean = None;
        self.progress = Some(OperationProgress {
            phase: Phase::Scanning,
            current: ".".to_owned(),
            files: 0,
            directories: 0,
            bytes: 0,
            skipped: 0,
            failed: 0,
        });

        match worker::spawn_scan(root, ctx.clone()) {
            Ok(worker) => {
                self.worker = Some(worker);
                self.notice = None;
            }
            Err(message) => {
                self.progress = None;
                self.notice = Some(Notice {
                    level: NoticeLevel::Error,
                    text: message,
                });
            }
        }
    }

    fn start_clean(&mut self, ctx: &egui::Context) {
        if self.busy() {
            return;
        }
        let Some(scan) = self.scan.as_ref() else {
            return;
        };
        if scan.cancelled {
            self.notice = Some(Notice {
                level: NoticeLevel::Error,
                text: "上次扫描已取消，请重新扫描后再清理。".to_owned(),
            });
            return;
        }
        let Some(root) = self.root_for(self.selected) else {
            return;
        };

        self.clean = None;
        self.confirm_open = false;
        self.confirm_checked = false;
        self.progress = Some(OperationProgress {
            phase: Phase::Cleaning,
            current: ".".to_owned(),
            files: 0,
            directories: 0,
            bytes: 0,
            skipped: 0,
            failed: 0,
        });

        match worker::spawn_clean(root, ctx.clone()) {
            Ok(worker) => {
                self.worker = Some(worker);
                self.notice = None;
            }
            Err(message) => {
                self.progress = None;
                self.notice = Some(Notice {
                    level: NoticeLevel::Error,
                    text: message,
                });
            }
        }
    }

    fn poll_worker(&mut self, ctx: &egui::Context) {
        let mut finished = false;
        if let Some(worker) = &self.worker {
            loop {
                match worker.receiver.try_recv() {
                    Ok(WorkerMessage::Progress(progress)) => {
                        self.progress = Some(progress);
                    }
                    Ok(WorkerMessage::ScanFinished(result)) => {
                        match result {
                            Ok(report) => {
                                if report.cancelled {
                                    self.notice = Some(Notice {
                                        level: NoticeLevel::Info,
                                        text: "扫描已取消；下方为取消前已经统计到的部分结果。"
                                            .to_owned(),
                                    });
                                }
                                self.scan = Some(report);
                            }
                            Err(message) => {
                                self.notice = Some(Notice {
                                    level: NoticeLevel::Error,
                                    text: format!("扫描失败：{message}"),
                                });
                            }
                        }
                        finished = true;
                        break;
                    }
                    Ok(WorkerMessage::CleanFinished(result)) => {
                        match result {
                            Ok(report) => {
                                // The old preview is no longer a truthful description
                                // of the directory. Force a fresh scan before another
                                // cleanup can be confirmed.
                                self.scan = None;
                                if report.cancelled {
                                    self.notice = Some(Notice {
                                        level: NoticeLevel::Info,
                                        text:
                                            "清理已取消；已经提交给文件系统的单项删除可能已经完成。"
                                                .to_owned(),
                                    });
                                } else if report.failed > 0 {
                                    self.notice = Some(Notice {
                                        level: NoticeLevel::Info,
                                        text: "清理结束，但有部分条目失败；详情见清理结果。"
                                            .to_owned(),
                                    });
                                }
                                self.clean = Some(report);
                            }
                            Err(message) => {
                                self.notice = Some(Notice {
                                    level: NoticeLevel::Error,
                                    text: format!("清理失败：{message}"),
                                });
                            }
                        }
                        finished = true;
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        self.notice = Some(Notice {
                            level: NoticeLevel::Error,
                            text: "后台任务意外结束，未收到完整结果。请重新扫描。".to_owned(),
                        });
                        finished = true;
                        break;
                    }
                }
            }
        }

        if finished {
            if let Some(mut worker) = self.worker.take() {
                worker.join();
            }
            self.progress = None;
            ctx.request_repaint();
        } else if self.worker.is_some() {
            // Progress is count-based, so a periodic repaint is enough.
            ctx.request_repaint_after(Duration::from_millis(120));
        }
    }

    fn clean_enabled(&self) -> bool {
        !self.busy()
            && self.scan.as_ref().is_some_and(|scan| {
                !scan.cancelled && (scan.counts.files > 0 || scan.counts.directories > 0)
            })
            && self.root_for(self.selected).is_some()
    }

    fn draw_header(&self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("Temp 清理工具")
                    .size(28.0)
                    .strong()
                    .color(Color32::from_rgb(28, 34, 42)),
            );
            ui.add_space(6.0);
            ui.label(
                RichText::new("安全优先 · 本地运行 · 无后台服务")
                    .size(13.0)
                    .color(TEXT_MUTED),
            );
        });
        ui.label(
            RichText::new(
                "只清理所选 Temp 目录内部的内容，保留 Temp 根目录；不跟随符号链接、目录联接或其他重解析点。",
            )
            .color(TEXT_MUTED),
        );
        ui.add_space(8.0);
    }

    fn draw_targets(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.label(RichText::new("清理目标").size(17.0).strong());
        ui.add_space(4.0);

        for kind in TargetKind::ALL {
            let Some(report) = self.report(kind).cloned() else {
                continue;
            };
            let selected = self.selected == kind;
            let selectable = report.is_selectable() && !self.busy();

            let frame = Frame::new()
                .fill(if selected {
                    Color32::from_rgb(240, 246, 254)
                } else {
                    CARD
                })
                .stroke(Stroke::new(
                    if selected { 1.5 } else { 1.0 },
                    if selected { ACCENT } else { BORDER },
                ))
                .corner_radius(8)
                .inner_margin(egui::Margin::same(12));

            frame.show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(kind.name_zh()).size(16.0).strong());
                    if selected {
                        ui.label(RichText::new("已选择").small().color(ACCENT).strong());
                    }
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        let button =
                            egui::Button::new(if selected { "当前目标" } else { "选择" }).small();
                        if ui.add_enabled(selectable && !selected, button).clicked() {
                            self.select_target(kind);
                        }
                    });
                });
                ui.label(
                    RichText::new(format!("来源：{}", report.source.label_zh()))
                        .small()
                        .color(TEXT_MUTED),
                );

                match &report.path {
                    Some(path) => {
                        ui.add(
                            Label::new(RichText::new(path.display().to_string()).monospace())
                                .wrap(),
                        );
                    }
                    None => {
                        ui.label(RichText::new("无法解析出绝对路径").color(DANGER));
                    }
                }

                let color = match report.state {
                    TargetState::Ready => OK,
                    TargetState::PermissionLimited { .. } => WARN,
                    TargetState::Unavailable { .. } => DANGER,
                    TargetState::Duplicate { .. } => TEXT_MUTED,
                };
                ui.label(RichText::new(report.status_text()).size(13.0).color(color));

                if kind == TargetKind::SystemTemp {
                    ui.label(
                        RichText::new(
                            "清理系统 Temp 通常需要管理员权限；程序不会自动请求或绕过权限。",
                        )
                        .small()
                        .color(TEXT_MUTED),
                    );
                }
            });
            ui.add_space(8.0);
        }

        let target_ready = self.root_for(self.selected).is_some();
        ui.horizontal(|ui| {
            let scan = egui::Button::new(RichText::new("扫描预览").strong())
                .min_size(Vec2::new(132.0, 34.0));
            if ui.add_enabled(!self.busy() && target_ready, scan).clicked() {
                self.start_scan(ctx);
            }

            let clean = egui::Button::new(RichText::new("清理…").strong().color(Color32::WHITE))
                .fill(DANGER)
                .min_size(Vec2::new(112.0, 34.0));
            if ui.add_enabled(self.clean_enabled(), clean).clicked() {
                self.confirm_open = true;
                self.confirm_checked = false;
            }

            if let Some(worker) = &self.worker
                && ui
                    .add_enabled(!worker.is_cancelled(), egui::Button::new("取消当前任务"))
                    .clicked()
            {
                worker.cancel();
                self.notice = Some(Notice {
                    level: NoticeLevel::Info,
                    text:
                        "已请求取消。已开始的底层文件操作可能需要一点时间结束；不会启动新的删除。"
                            .to_owned(),
                });
            }
        });
        ui.add_space(10.0);
    }

    fn draw_progress(&mut self, ui: &mut egui::Ui) {
        let Some(progress) = self.progress.clone() else {
            return;
        };
        let cancel_requested = self
            .worker
            .as_ref()
            .is_some_and(|worker| worker.is_cancelled());

        Frame::new()
            .fill(Color32::from_rgb(240, 244, 249))
            .corner_radius(6)
            .inner_margin(egui::Margin::same(10))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(progress.phase.label_zh()).strong());
                    if cancel_requested {
                        ui.label(RichText::new("正在取消…").color(WARN));
                    }
                });
                ui.label(
                    RichText::new(format!("当前：{}", progress.current))
                        .small()
                        .color(TEXT_MUTED),
                );
                ui.label(
                    RichText::new(match progress.phase {
                        Phase::Scanning => format!(
                            "已发现 {} 个文件 / {} 个目录，可计算大小 {}；跳过 {}",
                            progress.files,
                            progress.directories,
                            format_bytes(progress.bytes),
                            progress.skipped
                        ),
                        Phase::Cleaning => format!(
                            "已删除 {} 个文件 / {} 个目录，释放 {}；跳过 {}，失败 {}",
                            progress.files,
                            progress.directories,
                            format_bytes(progress.bytes),
                            progress.skipped,
                            progress.failed
                        ),
                    })
                    .small(),
                );
                ui.label(
                    RichText::new(
                        "进度以实际计数显示；无法准确预估剩余时间，因此不显示伪造的百分比。",
                    )
                    .small()
                    .color(TEXT_MUTED),
                );
            });
        ui.add_space(10.0);
    }

    fn draw_notice(&mut self, ui: &mut egui::Ui) {
        let Some(notice) = self.notice.as_ref() else {
            return;
        };
        let color = match notice.level {
            NoticeLevel::Info => Color32::from_rgb(49, 92, 145),
            NoticeLevel::Error => DANGER,
        };
        let text = notice.text.clone();
        let level = notice.level;
        Frame::new()
            .fill(if level == NoticeLevel::Error {
                Color32::from_rgb(253, 240, 240)
            } else {
                Color32::from_rgb(240, 246, 252)
            })
            .stroke(Stroke::new(1.0, color))
            .corner_radius(6)
            .inner_margin(egui::Margin::same(8))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(text).color(color));
                    if ui.small_button("关闭").clicked() {
                        self.notice = None;
                    }
                });
            });
        ui.add_space(10.0);
    }

    fn draw_scan(&mut self, ui: &mut egui::Ui) {
        let Some(scan) = self.scan.clone() else {
            return;
        };
        ui.label(RichText::new("扫描预览").size(17.0).strong());
        ui.label(
            RichText::new(format!(
                "耗时 {} · 扫描只读取元数据，不代表清理时一定能够删除所有条目。",
                format_duration(scan.elapsed)
            ))
            .small()
            .color(TEXT_MUTED),
        );
        ui.add_space(6.0);

        stat_row(
            ui,
            &[
                ("文件", scan.counts.files.to_string()),
                ("目录", scan.counts.directories.to_string()),
                ("可计算大小", format_bytes(scan.counts.bytes)),
                ("跳过条目", scan.counts.skipped_total().to_string()),
            ],
        );

        if scan.cancelled {
            ui.label(RichText::new("扫描已取消，结果不完整。请重新扫描。").color(WARN));
        }

        if scan.counts.files == 0 && scan.counts.directories == 0 && !scan.cancelled {
            ui.label(RichText::new("目录中没有可清理的普通文件或子目录。").color(TEXT_MUTED));
        }

        if scan.counts.skipped_total() > 0 {
            let title = format!(
                "跳过详情（文件 {} / 目录 {} / 其他 {}）",
                scan.counts.skipped_files,
                scan.counts.skipped_directories,
                scan.counts.skipped_other
            );
            egui::CollapsingHeader::new(title)
                .default_open(false)
                .show(ui, |ui| draw_issues(ui, &scan.issues));
        }

        ui.add_space(12.0);
    }

    fn draw_clean_result(&mut self, ui: &mut egui::Ui) {
        let Some(report) = self.clean.clone() else {
            return;
        };
        let color = status_color(report.status);
        ui.horizontal(|ui| {
            ui.label(RichText::new("清理结果").size(17.0).strong());
            ui.label(
                RichText::new(report.status.label_zh())
                    .strong()
                    .color(color),
            );
        });
        ui.label(
            RichText::new(format!(
                "耗时 {} · 目标 {}",
                format_duration(report.elapsed),
                report.root.display()
            ))
            .small()
            .color(TEXT_MUTED),
        );
        ui.add_space(6.0);

        stat_row(
            ui,
            &[
                ("已删除文件", report.deleted_files.to_string()),
                ("已删除目录", report.deleted_directories.to_string()),
                ("实际释放", format_bytes(report.freed_bytes)),
                (
                    "失败 / 跳过",
                    format!("{} / {}", report.failed, report.skipped),
                ),
            ],
        );

        if report.cancelled {
            ui.label(
                RichText::new(
                    "操作被取消；已提交的单项目录/文件删除可能已经完成，未处理的条目仍然保留。",
                )
                .color(WARN),
            );
        }
        if report.failed > 0 {
            ui.label(
                RichText::new(format!(
                    "有 {} 个条目未能删除。常见原因包括文件占用、权限不足、只读属性或目录非空。",
                    report.failed
                ))
                .color(DANGER),
            );
        }
        if report.status == CleanStatus::NothingToDo {
            ui.label(RichText::new("没有成功删除的条目。").color(TEXT_MUTED));
        }

        if report.issues.total > 0 {
            ui.add_space(4.0);
            let title = format!(
                "问题详情（共 {}，失败 {}，跳过 {}）",
                report.issues.total, report.failed, report.skipped
            );
            egui::CollapsingHeader::new(title)
                .default_open(report.failed > 0)
                .show(ui, |ui| draw_issues(ui, &report.issues));
        }

        if ui.button("再次扫描").clicked() {
            self.start_scan(ui.ctx());
        }
        ui.add_space(12.0);
    }

    fn draw_confirm(&mut self, ctx: &egui::Context) {
        if !self.confirm_open {
            return;
        }
        let Some(scan) = self.scan.clone() else {
            self.confirm_open = false;
            return;
        };
        if scan.cancelled {
            self.confirm_open = false;
            return;
        }

        let mut checked = self.confirm_checked;
        let mut close = false;
        let mut proceed = false;

        let modal = egui::Modal::new(egui::Id::unique("confirm-clean-modal")).show(ctx, |ui| {
            ui.set_width(520.0);
            ui.label(
                RichText::new("即将删除所选 Temp 目录内部的内容，此操作不可撤销。")
                    .color(DANGER)
                    .strong(),
            );
            ui.add_space(6.0);
            ui.label("目标：");
            ui.add(Label::new(RichText::new(scan.root.display().to_string()).monospace()).wrap());
            ui.add_space(6.0);
            ui.label(format!(
                "预计：{} 个文件，{} 个目录，可计算大小 {}；扫描时跳过 {} 个条目。",
                scan.counts.files,
                scan.counts.directories,
                format_bytes(scan.counts.bytes),
                scan.counts.skipped_total()
            ));
            ui.label(
                RichText::new(
                    "清理会重新遍历并检查每个条目；文件占用、权限不足或状态变化可能导致部分失败。Temp 根目录本身会保留。",
                )
                .small()
                .color(TEXT_MUTED),
            );
            ui.add_space(8.0);
            ui.checkbox(&mut checked, "我明白此操作不可撤销，并确认清理上述目录内部内容。");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("取消").clicked() {
                    close = true;
                }
                let button = egui::Button::new(
                    RichText::new("确认清理").strong().color(Color32::WHITE),
                )
                .fill(DANGER);
                if ui.add_enabled(checked, button).clicked() {
                    proceed = true;
                }
            });
        });

        if modal.should_close() {
            close = true;
        }
        self.confirm_checked = checked;
        if close {
            self.confirm_open = false;
            self.confirm_checked = false;
        } else if proceed {
            self.start_clean(ctx);
        }
    }

    fn draw_footer(&self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.separator();
        ui.label(
            RichText::new(
                "安全说明：只处理所选 Temp 内部；不删除 Temp 根目录；不跟随重解析点；不通过 cmd.exe / PowerShell 执行删除；不自动提权。关闭窗口会取消尚未开始的操作，已开始的单次文件操作可能完成。",
            )
            .small()
            .color(TEXT_MUTED),
        );
    }
}

impl eframe::App for TempCleanerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_worker(ui.ctx());
        let ctx = ui.ctx().clone();

        egui::CentralPanel::default_margins().show(ui, |ui| {
            ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    self.draw_header(ui);
                    self.draw_targets(ui, &ctx);
                    self.draw_progress(ui);
                    self.draw_notice(ui);
                    self.draw_scan(ui);
                    self.draw_clean_result(ui);
                    self.draw_footer(ui);
                });
        });

        self.draw_confirm(&ctx);
    }
}

impl Drop for TempCleanerApp {
    fn drop(&mut self) {
        if let Some(worker) = &self.worker {
            worker.cancel();
        }
    }
}

fn configure_style(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::light();
    visuals.panel_fill = BG;
    visuals.window_fill = CARD;
    visuals.faint_bg_color = Color32::from_rgb(250, 251, 252);
    visuals.extreme_bg_color = Color32::WHITE;
    visuals.selection.bg_fill = ACCENT;
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, Color32::from_rgb(50, 56, 64));
    ctx.set_visuals(visuals);
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = Vec2::new(8.0, 6.0);
        style.spacing.button_padding = Vec2::new(10.0, 5.0);
        style
            .text_styles
            .insert(egui::TextStyle::Body, FontId::proportional(14.5));
        style
            .text_styles
            .insert(egui::TextStyle::Button, FontId::proportional(14.5));
    });
}

fn stat_row(ui: &mut egui::Ui, stats: &[(&str, String)]) {
    ui.horizontal_wrapped(|ui| {
        for (label, value) in stats {
            Frame::new()
                .fill(CARD)
                .stroke(Stroke::new(1.0, BORDER))
                .corner_radius(6)
                .inner_margin(egui::Margin::same(9))
                .show(ui, |ui| {
                    ui.set_min_width(145.0);
                    ui.label(RichText::new(value).size(19.0).strong());
                    ui.label(RichText::new(*label).small().color(TEXT_MUTED));
                });
        }
    });
}

fn draw_issues(ui: &mut egui::Ui, issues: &Issues) {
    if issues.items.is_empty() {
        ui.label(RichText::new("没有可显示的问题详情。").color(TEXT_MUTED));
        return;
    }
    if issues.truncated {
        ui.label(
            RichText::new(format!(
                "详情最多展示 {} 条；总数与分类统计仍然准确。",
                crate::models::MAX_RECORDED_ISSUES
            ))
            .small()
            .color(TEXT_MUTED),
        );
    }

    for issue in &issues.items {
        draw_issue(ui, issue);
    }
}

fn draw_issue(ui: &mut egui::Ui, issue: &Issue) {
    let (class_label, class_color) = match issue.class {
        IssueClass::Skipped => ("跳过", WARN),
        IssueClass::Failed => ("失败", DANGER),
    };
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(format!("[{class_label}]"))
                .small()
                .strong()
                .color(class_color),
        );
        ui.label(RichText::new(issue.kind.label_zh()).small().strong());
        let path = if issue.relative_path == "." {
            "(Temp 根目录)".to_owned()
        } else {
            issue.relative_path.clone()
        };
        ui.add(Label::new(RichText::new(path).small().monospace()).wrap());
    });
    ui.add_space(1.0);
    ui.add(Label::new(RichText::new(&issue.detail).small().color(TEXT_MUTED)).wrap());
    ui.add_space(4.0);
}

fn status_color(status: CleanStatus) -> Color32 {
    match status {
        CleanStatus::Completed => OK,
        CleanStatus::NothingToDo => TEXT_MUTED,
        CleanStatus::PartialSuccess => WARN,
        CleanStatus::Failed => DANGER,
        CleanStatus::Cancelled => WARN,
    }
}
