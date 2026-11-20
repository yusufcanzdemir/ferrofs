//! The compression tool (SPEC §5.3): suggests large, rarely updated program folders from
//! the index, analyzes a folder (candidates, already compressed, compressed formats),
//! estimates the savings for an algorithm from a sample, and compresses or decompresses
//! after a preview.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use eframe::egui::{self, Align2, FontId, Key, RichText, ScrollArea, Sense, TextEdit, pos2, vec2};
use ferro_ipc::{Request, Response, SizeTreeRequest};

use crate::actions::ActionLog;
use crate::compressor::{self, Estimate, Progress, Scan, Summary};
use crate::elevation;
use crate::format;
use crate::paint::{fit_text, row_background};
use crate::tree;
use crate::wof::Algorithm;
use crate::worker::{Job, JobKind, Outcome as JobOutcome, ServiceState};

const ROW_HEIGHT: f32 = 24.0;
/// Program folders smaller than this are not suggested.
const MIN_SUGGESTION: u64 = 200 << 20;
/// Folders unchanged for this long count as rarely updated.
const COLD_DAYS: u64 = 90;

/// A program folder worth looking at.
#[derive(Debug, Clone)]
struct Suggestion {
    path: String,
    size: u64,
    alloc_size: u64,
    mtime: u64,
}

enum TaskResult {
    Scanned(Scan, Estimate),
    Estimated(Estimate),
    Compressed(Summary),
    Decompressed(Summary),
}

enum Stage {
    Idle,
    Running {
        what: &'static str,
        progress: Arc<Progress>,
        result: Receiver<TaskResult>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PreviewKind {
    Compress,
    Decompress,
}

pub struct Compression {
    jobs: Sender<Job>,
    outcomes: Receiver<JobOutcome>,
    ctx: egui::Context,
    service: ServiceState,
    generation: u64,
    folder: String,
    algorithm: Algorithm,
    suggestions: Vec<Suggestion>,
    suggestion_roots: Vec<String>,
    scan: Option<Scan>,
    estimate: Option<Estimate>,
    last: Option<Summary>,
    stage: Stage,
    preview: Option<PreviewKind>,
    elevated: bool,
    dry_run_only: bool,
    log: Option<ActionLog>,
    message: Option<String>,
    error: Option<String>,
}

impl Compression {
    pub fn new(
        ctx: egui::Context,
        jobs: Sender<Job>,
        outcomes: Receiver<JobOutcome>,
        folder: String,
        dry_run_only: bool,
    ) -> Self {
        let env = |name: &str| std::env::var(name).ok();
        let mut roots = Vec::new();
        let pf = env("ProgramFiles");
        let pf86 = env("ProgramFiles(x86)");
        roots.extend(pf.clone());
        roots.extend(pf86.clone());
        roots.extend(env("LOCALAPPDATA").map(|l| tree::join(&l, "Programs")));
        roots.extend(
            pf86.as_deref()
                .map(|p| tree::join(p, r"Steam\steamapps\common")),
        );
        roots.extend(pf.as_deref().map(|p| tree::join(p, "Epic Games")));
        let mut tool = Self {
            jobs,
            outcomes,
            ctx,
            service: ServiceState::Connected,
            generation: 0,
            folder,
            algorithm: Algorithm::Xpress8k,
            suggestions: Vec::new(),
            suggestion_roots: roots,
            scan: None,
            estimate: None,
            last: None,
            stage: Stage::Idle,
            preview: None,
            elevated: elevation::is_elevated(),
            dry_run_only,
            log: ActionLog::default_file().map(ActionLog::new),
            message: None,
            error: None,
        };
        tool.refresh_suggestions();
        tool
    }

    fn busy(&self) -> bool {
        matches!(self.stage, Stage::Running { .. })
    }

    /// Asks the index for the subfolders of the program folders.
    fn refresh_suggestions(&mut self) {
        self.generation += 1;
        self.suggestions.clear();
        for (i, root) in self.suggestion_roots.iter().enumerate() {
            let mut request = SizeTreeRequest::new(root.clone(), 1);
            request.max_children = 500;
            let job = Job {
                generation: self.generation,
                kind: JobKind::Location(i),
                request: Request::SizeTree(request),
            };
            let _ = self.jobs.send(job);
        }
    }

    fn receive(&mut self) {
        while let Ok(outcome) = self.outcomes.try_recv() {
            if outcome.generation != self.generation {
                continue;
            }
            self.service = outcome.service.clone();
            if let Ok(Response::SizeTree(found)) = outcome.result {
                for child in found.root.children {
                    if child.size < MIN_SUGGESTION
                        || child.flags & ferro_core::index::flags::DIR == 0
                    {
                        continue;
                    }
                    self.suggestions.push(Suggestion {
                        path: tree::join(&found.path, &child.name),
                        size: child.size,
                        alloc_size: child.alloc_size,
                        mtime: child.mtime,
                    });
                }
                self.suggestions.sort_by_key(|s| std::cmp::Reverse(s.size));
            }
        }
    }

    /// Runs `task` on a background thread with a fresh progress.
    fn spawn(
        &mut self,
        what: &'static str,
        task: impl FnOnce(&Progress) -> TaskResult + Send + 'static,
    ) {
        let progress = Arc::new(Progress::default());
        let (tx, rx) = channel();
        let (thread_progress, ctx) = (Arc::clone(&progress), self.ctx.clone());
        let spawned = std::thread::Builder::new()
            .name("compression".into())
            .spawn(move || {
                let _ = tx.send(task(&thread_progress));
                ctx.request_repaint();
            });
        match spawned {
            Ok(_) => {
                self.stage = Stage::Running {
                    what,
                    progress,
                    result: rx,
                };
            }
            Err(err) => self.error = Some(format!("cannot start: {err}")),
        }
    }

    fn windir() -> Option<String> {
        std::env::var("WINDIR").ok()
    }

    /// Scans the folder and estimates the current algorithm.
    pub fn analyze(&mut self) {
        let folder = self
            .folder
            .trim()
            .trim_matches('"')
            .trim_end_matches('\\')
            .to_owned();
        if let Err(reason) = compressor::check_folder(&folder, Self::windir().as_deref()) {
            self.error = Some(reason);
            return;
        }
        self.folder.clone_from(&folder);
        self.error = None;
        self.message = None;
        self.last = None;
        self.scan = None;
        self.estimate = None;
        let algorithm = self.algorithm;
        self.spawn("Analyzing", move |progress| {
            let scan = compressor::scan(&folder, progress);
            let estimate = compressor::estimate(&scan.candidates, algorithm, progress);
            TaskResult::Scanned(scan, estimate)
        });
    }

    fn re_estimate(&mut self) {
        let Some(scan) = &self.scan else {
            return;
        };
        let candidates = scan.candidates.clone();
        let algorithm = self.algorithm;
        self.estimate = None;
        self.spawn("Estimating", move |progress| {
            TaskResult::Estimated(compressor::estimate(&candidates, algorithm, progress))
        });
    }

    fn run(&mut self, kind: PreviewKind, dry_run: bool) {
        let Some(log) = self.log.clone() else {
            self.error = Some("LOCALAPPDATA is not set: cannot write the action log".into());
            return;
        };
        let dry_run = dry_run || self.dry_run_only;
        let folder = self.folder.clone();
        if let Err(reason) = compressor::check_folder(&folder, Self::windir().as_deref()) {
            self.error = Some(reason);
            return;
        }
        match kind {
            PreviewKind::Compress => {
                let Some(scan) = &self.scan else {
                    return;
                };
                let candidates = scan.candidates.clone();
                let algorithm = self.algorithm;
                self.spawn("Compressing", move |progress| {
                    TaskResult::Compressed(compressor::compress(
                        &folder,
                        &candidates,
                        algorithm,
                        dry_run,
                        &log,
                        progress,
                    ))
                });
            }
            PreviewKind::Decompress => {
                self.spawn("Decompressing", move |progress| {
                    TaskResult::Decompressed(compressor::decompress(
                        &folder, dry_run, &log, progress,
                    ))
                });
            }
        }
    }

    fn poll(&mut self) {
        let Stage::Running { result, .. } = &self.stage else {
            return;
        };
        let Ok(done) = result.try_recv() else {
            self.ctx.request_repaint_after(Duration::from_millis(100));
            return;
        };
        self.stage = Stage::Idle;
        match done {
            TaskResult::Scanned(scan, estimate) => {
                if scan.cancelled || estimate.cancelled {
                    self.message = Some("Analysis cancelled".into());
                } else {
                    self.scan = Some(scan);
                    self.estimate = Some(estimate);
                }
            }
            TaskResult::Estimated(estimate) => {
                if !estimate.cancelled {
                    self.estimate = Some(estimate);
                }
            }
            TaskResult::Compressed(summary) | TaskResult::Decompressed(summary) => {
                let verb = if summary.dry_run {
                    "Dry run logged; nothing changed."
                } else if summary.cancelled {
                    "Stopped."
                } else {
                    "Done."
                };
                let mut text = format!(
                    "{verb} {} changed: {} on disk before, {} after",
                    format::counted(summary.done.files, "file", None),
                    format::size(summary.disk_before),
                    format::size(summary.disk_after)
                );
                if summary.unchanged > 0 {
                    text.push_str(&format!(
                        ", {} left as they were",
                        format::count(summary.unchanged)
                    ));
                }
                text.push('.');
                self.message = Some(text);
                self.error = (summary.failed > 0).then(|| {
                    let mut e = format!("{} failed", format::counted(summary.failed, "file", None));
                    if let Some((path, reason)) = summary.failures.first() {
                        e.push_str(&format!(" (first: {path}: {reason})"));
                    }
                    if summary.access_denied && !self.elevated {
                        e.push_str(". Access denied: restart as administrator for this folder.");
                    }
                    e
                });
                let rescan = !summary.dry_run;
                self.last = Some(summary);
                if rescan {
                    // Show the new state of the folder.
                    let folder = self.folder.clone();
                    let algorithm = self.algorithm;
                    self.spawn("Measuring again", move |progress| {
                        let scan = compressor::scan(&folder, progress);
                        let estimate = compressor::estimate(&scan.candidates, algorithm, progress);
                        TaskResult::Scanned(scan, estimate)
                    });
                }
            }
        }
    }

    pub fn update(&mut self, ctx: &egui::Context) {
        self.receive();
        self.poll();
        egui::TopBottomPanel::top("compression-toolbar").show(ctx, |ui| self.toolbar(ui));
        egui::TopBottomPanel::bottom("compression-status").show(ctx, |ui| self.status(ui));
        egui::CentralPanel::default().show(ctx, |ui| {
            ScrollArea::vertical()
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    self.analysis(ui);
                    ui.add_space(10.0);
                    self.suggestion_list(ui);
                });
        });
        self.preview_dialog(ctx);
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let folder = ui.add(
                TextEdit::singleline(&mut self.folder)
                    .hint_text(r"Folder to compress, e.g. C:\Games\Something")
                    .desired_width(380.0),
            );
            let enter = folder.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            if ui
                .add_enabled(!self.busy(), egui::Button::new("Analyze"))
                .clicked()
                || (enter && !self.busy())
            {
                self.analyze();
            }
            ui.separator();
            let before = self.algorithm;
            egui::ComboBox::from_id_salt("algorithm")
                .selected_text(self.algorithm.name())
                .width(110.0)
                .show_ui(ui, |ui| {
                    for algorithm in Algorithm::ALL {
                        ui.selectable_value(&mut self.algorithm, algorithm, algorithm.describe());
                    }
                });
            if self.algorithm != before && !self.busy() {
                self.re_estimate();
            }
            let can_compress =
                !self.busy() && self.scan.as_ref().is_some_and(|s| !s.candidates.is_empty());
            if ui
                .add_enabled(can_compress, egui::Button::new("Compress…"))
                .clicked()
            {
                self.preview = Some(PreviewKind::Compress);
            }
            if ui
                .add_enabled(
                    !self.busy() && !self.folder.trim().is_empty(),
                    egui::Button::new("Decompress…"),
                )
                .on_hover_text("Undo: restores every compressed file in the folder")
                .clicked()
            {
                self.preview = Some(PreviewKind::Decompress);
            }
            if !self.elevated
                && self.may_need_admin()
                && ui
                    .button("Restart as administrator")
                    .on_hover_text("Needed for folders under Program Files")
                    .clicked()
            {
                let mut args = format!("--compression \"{}\"", self.folder.trim());
                if self.dry_run_only {
                    args.push_str(" --dry-run");
                }
                match elevation::relaunch_elevated(&args) {
                    Ok(()) => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
                    Err(err) => self.error = Some(format!("{err:#}")),
                }
            }
            if self.dry_run_only {
                ui.label(
                    RichText::new("--dry-run: actions are only logged")
                        .color(ui.visuals().warn_fg_color),
                );
            }
        });
        ui.add_space(2.0);
    }

    /// True if the folder is under Program Files or a run hit "access denied".
    fn may_need_admin(&self) -> bool {
        let folder = self.folder.to_lowercase();
        let protected = ["ProgramFiles", "ProgramFiles(x86)", "ProgramData"]
            .iter()
            .filter_map(|name| std::env::var(name).ok())
            .any(|root| folder.starts_with(&root.to_lowercase()));
        protected || self.last.as_ref().is_some_and(|s| s.access_denied)
    }

    fn status(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Stage::Running { what, progress, .. } = &self.stage {
                let (done, total) = (
                    progress.bytes_done.load(Ordering::Relaxed),
                    progress.bytes_total.load(Ordering::Relaxed),
                );
                let files = progress.files_done.load(Ordering::Relaxed);
                let fraction = if total == 0 {
                    0.0
                } else {
                    format::share(done, total)
                };
                let text = if total == 0 {
                    format!("{what}: {} files", format::count(files))
                } else {
                    format!("{what}: {} / {}", format::size(done), format::size(total))
                };
                ui.add(
                    egui::ProgressBar::new(fraction)
                        .desired_width(300.0)
                        .text(text),
                );
                if ui.button("Cancel").clicked() {
                    progress.cancel.store(true, Ordering::Relaxed);
                }
            }
            if let Some(error) = &self.error {
                ui.label(RichText::new(error).color(ui.visuals().error_fg_color));
            }
            if let Some(message) = &self.message {
                ui.label(message);
            }
        });
    }

    fn analysis(&mut self, ui: &mut egui::Ui) {
        let Some(scan) = &self.scan else {
            ui.label(
                RichText::new(
                    "Compression keeps files readable by every program (they are decompressed while read) and saves \
                     space on folders that rarely change: installed programs and games. Pick one below or type a folder.",
                )
                .weak(),
            );
            return;
        };
        ui.heading(&scan.folder);
        let line = |ui: &mut egui::Ui, label: &str, count: compressor::Count, note: &str| {
            ui.label(label);
            ui.label(format!(
                "{} · {}",
                format::counted(count.files, "file", None),
                format::size(count.bytes)
            ));
            ui.label(RichText::new(note).weak());
            ui.end_row();
        };
        egui::Grid::new("compression-scan")
            .num_columns(3)
            .spacing([24.0, 4.0])
            .show(ui, |ui| {
                line(
                    ui,
                    "All files",
                    scan.total,
                    &format!("{} on disk now", format::size(scan.on_disk)),
                );
                line(
                    ui,
                    "Will be compressed",
                    compressor::Count {
                        files: scan.candidates.len() as u64,
                        bytes: scan.candidate_bytes(),
                    },
                    "",
                );
                line(ui, "Compressed already", scan.already, "left as they are");
                line(
                    ui,
                    "Compressed formats",
                    scan.formats,
                    "jpg, mp4, zip, …: no gain, skipped",
                );
                line(ui, "Too small", scan.small, "under 8 KB");
                if scan.unreadable.files > 0 {
                    line(ui, "Unreadable", scan.unreadable, "no access");
                }
            });
        if scan.newest > 0 {
            let days = days_since(scan.newest);
            let cold = days >= COLD_DAYS;
            ui.label(
                RichText::new(format!(
                    "Newest change: {} ({} days ago){}",
                    format::date(scan.newest),
                    days,
                    if cold {
                        " — rarely updated, a good candidate"
                    } else {
                        " — updated recently; updates store changed files uncompressed again"
                    }
                ))
                .color(if cold {
                    egui::Color32::from_rgb(80, 170, 100)
                } else {
                    ui.visuals().warn_fg_color
                }),
            );
        }
        ui.add_space(6.0);
        match &self.estimate {
            Some(estimate) if estimate.before > 0 => {
                let percent = format::share(estimate.saved(), estimate.before) * 100.0;
                ui.label(
                    RichText::new(format!(
                        "{}: about {} becomes {}, saving {} ({percent:.0}%)",
                        estimate.algorithm.name(),
                        format::size(estimate.before),
                        format::size(estimate.after),
                        format::size(estimate.saved()),
                    ))
                    .size(16.0)
                    .strong(),
                );
                let mut basis = format!(
                    "Estimated by compressing a {} sample in memory.",
                    format::size(estimate.sampled)
                );
                if estimate.algorithm == Algorithm::Lzx {
                    basis.push_str(" LZX is approximated with LZMS; the real result may differ by a few percent.");
                }
                ui.label(RichText::new(basis).weak());
            }
            Some(_) => {
                ui.label("Nothing here would benefit from compression.");
            }
            None => {}
        }
    }

    fn suggestion_list(&mut self, ui: &mut egui::Ui) {
        if self.suggestions.is_empty() {
            return;
        }
        ui.label(RichText::new("Large program folders").strong());
        ui.label(
            RichText::new("From the index. Click one to analyze it. Folders under Program Files need administrator rights.")
                .weak(),
        );
        let mut picked = None;
        for suggestion in &self.suggestions {
            let (rect, response) =
                ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::click());
            row_background(ui, rect, false, response.hovered());
            let visuals = ui.visuals();
            let font = FontId::proportional(13.5);
            let right = 420.0;
            let path_rect = egui::Rect::from_x_y_ranges(
                rect.left() + 8.0..=rect.right() - right,
                rect.y_range(),
            );
            ui.painter().with_clip_rect(path_rect).text(
                path_rect.left_center(),
                Align2::LEFT_CENTER,
                fit_text(ui, &suggestion.path, &font, path_rect.width(), true),
                font.clone(),
                visuals.strong_text_color(),
            );
            let x = rect.right() - right;
            ui.painter().text(
                pos2(x + 90.0, rect.center().y),
                Align2::RIGHT_CENTER,
                format::size(suggestion.size),
                font.clone(),
                visuals.strong_text_color(),
            );
            let compressed = format::share(
                suggestion.size.saturating_sub(suggestion.alloc_size),
                suggestion.size,
            );
            ui.painter().text(
                pos2(x + 105.0, rect.center().y),
                Align2::LEFT_CENTER,
                if compressed > 0.05 {
                    format!("{:.0}% compressed already", compressed * 100.0)
                } else {
                    "not compressed".to_owned()
                },
                FontId::proportional(12.5),
                visuals.weak_text_color(),
            );
            let days = days_since(suggestion.mtime);
            ui.painter().text(
                pos2(rect.right() - 8.0, rect.center().y),
                Align2::RIGHT_CENTER,
                if suggestion.mtime == 0 {
                    String::new()
                } else if days >= COLD_DAYS {
                    format!("unchanged for {days} days")
                } else {
                    format!("changed {days} days ago")
                },
                FontId::proportional(12.5),
                if days >= COLD_DAYS {
                    egui::Color32::from_rgb(80, 170, 100)
                } else {
                    visuals.weak_text_color()
                },
            );
            if response.clicked() {
                picked = Some(suggestion.path.clone());
            }
        }
        if let Some(path) = picked
            && !self.busy()
        {
            self.folder = path;
            self.analyze();
        }
    }

    fn preview_dialog(&mut self, ctx: &egui::Context) {
        let Some(kind) = self.preview else {
            return;
        };
        let mut choice = None;
        let dry_run_only = self.dry_run_only;
        egui::Modal::new(egui::Id::new("compression-preview")).show(ctx, |ui| {
            ui.set_width(680.0);
            match kind {
                PreviewKind::Compress => {
                    let files = self.scan.as_ref().map_or(0, |s| s.candidates.len());
                    ui.heading(format!(
                        "Compress {} with {}",
                        format::counted(files as u64, "file", None),
                        self.algorithm.name()
                    ));
                    ui.label(RichText::new(&self.folder).weak());
                    if let Some(estimate) = &self.estimate {
                        ui.label(format!("Expected saving: about {}.", format::size(estimate.saved())));
                    }
                    ui.label(
                        "Files stay readable by every program; reading them costs a little CPU. Files that are \
                         in use for writing, compressed already or of compressed formats are skipped. When a program \
                         updates a file, that file is stored uncompressed again.",
                    );
                    ui.label(
                        RichText::new("Undo at any time with Decompress (or `compact /u /exe /s` in the folder).").weak(),
                    );
                }
                PreviewKind::Decompress => {
                    ui.heading("Decompress the folder");
                    ui.label(RichText::new(&self.folder).weak());
                    ui.label(
                        "Every WOF-compressed file in this folder is restored to normal. They take their full size \
                         on disk again; make sure there is enough free space.",
                    );
                }
            }
            if dry_run_only {
                ui.label(RichText::new("Started with --dry-run: actions are only logged.").color(ui.visuals().warn_fg_color));
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(!dry_run_only, egui::Button::new("Apply")).clicked() {
                    choice = Some(Some(false));
                }
                if ui.button("Dry run (log only)").clicked() {
                    choice = Some(Some(true));
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(None);
                }
            });
        });
        match choice {
            Some(Some(dry_run)) => {
                self.preview = None;
                self.run(kind, dry_run);
            }
            Some(None) => self.preview = None,
            None => {}
        }
    }
}

/// Whole days since a FILETIME.
fn days_since(filetime: u64) -> u64 {
    const UNIX_TO_FILETIME: u64 = 116_444_736_000_000_000;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let then = filetime.saturating_sub(UNIX_TO_FILETIME) / 10_000_000;
    now.saturating_sub(then) / 86_400
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_counts() {
        const UNIX_TO_FILETIME: u64 = 116_444_736_000_000_000;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let ten_days_ago = UNIX_TO_FILETIME + (now - 10 * 86_400) * 10_000_000;
        assert_eq!(days_since(ten_days_ago), 10);
    }
}
