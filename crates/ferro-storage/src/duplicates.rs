//! The duplicate finder tool (SPEC §5.2): asks the service for files of equal size,
//! compares their contents here (see `hasher`), lets the user mark copies, and removes
//! them or replaces them with hard links after a preview (see `actions`).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use eframe::egui::{
    self, Align2, Color32, FontId, Key, Rect, RichText, ScrollArea, Sense, Stroke, TextEdit, pos2,
    vec2,
};
use ferro_ipc::{DuplicateCandidatesRequest, DuplicateCandidatesResponse, Request, Response};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
use windows::core::w;

use crate::actions::{self, ActionLog, Disposal, Kind, Outcome, Planned};
use crate::format;
use crate::hasher::{self, Candidate, DupGroup, Progress, Report};
use crate::paint::{fit_text, row_background};
use crate::shell;
use crate::worker::{Job, JobKind, Outcome as JobOutcome, ServiceState};

const ROW_HEIGHT: f32 = 22.0;
/// Most files the service lists per search.
const MAX_FILES: u32 = 200_000;
const DEFAULT_MIN_SIZE: &str = "64 KB";

/// Which copy "Mark duplicates" keeps in each group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepRule {
    Oldest,
    Newest,
    ShortestPath,
}

impl KeepRule {
    const ALL: [Self; 3] = [Self::Oldest, Self::Newest, Self::ShortestPath];

    fn label(self) -> &'static str {
        match self {
            Self::Oldest => "Keep the oldest copy",
            Self::Newest => "Keep the newest copy",
            Self::ShortestPath => "Keep the shortest path",
        }
    }

    /// Index of the file to keep.
    fn pick(self, group: &DupGroup) -> usize {
        let files = group.files.iter().enumerate();
        let best = match self {
            Self::Oldest => files.min_by_key(|(_, f)| (f.state.mtime, f.path.len())),
            Self::Newest => {
                files.max_by_key(|(_, f)| (f.state.mtime, std::cmp::Reverse(f.path.len())))
            }
            Self::ShortestPath => files.min_by(|(_, a), (_, b)| {
                a.path
                    .len()
                    .cmp(&b.path.len())
                    .then_with(|| a.path.cmp(&b.path))
            }),
        };
        best.map_or(0, |(i, _)| i)
    }
}

struct GroupView {
    group: DupGroup,
    /// Files marked for removal (or replacement).
    marked: Vec<bool>,
}

impl GroupView {
    fn marked_count(&self) -> usize {
        self.marked.iter().filter(|m| **m).count()
    }
}

/// Totals reported by the service for the last search.
struct Candidates {
    total_groups: u64,
    total_files: u64,
    listed_files: u64,
    took_us: u64,
    complete: bool,
}

/// Progress of a running batch of actions.
#[derive(Default)]
struct ActProgress {
    done: AtomicU64,
}

enum Stage {
    Idle,
    /// Waiting for the service.
    Fetching,
    Hashing {
        progress: Arc<Progress>,
        result: Receiver<Report>,
    },
    Acting {
        progress: Arc<ActProgress>,
        total: usize,
        plan: Vec<Planned>,
        dry_run: bool,
        result: Receiver<Vec<Outcome>>,
    },
}

/// The confirmation dialog for a planned batch.
struct Preview {
    kind: Kind,
    plan: Vec<Planned>,
    /// Marked files that cannot be handled, with the reason.
    unplannable: Vec<(String, String)>,
    permanent: bool,
}

/// Something the user did this frame; applied after drawing.
enum UiAction {
    Toggle(usize, usize),
    KeepOnly(usize, usize),
    Reveal(String),
    Open(String),
    CopyPath(String),
}

pub struct Duplicates {
    jobs: Sender<Job>,
    outcomes: Receiver<JobOutcome>,
    ctx: egui::Context,
    service: ServiceState,
    generation: u64,
    folder: String,
    min_size: String,
    filter: String,
    stage: Stage,
    candidates: Option<Candidates>,
    groups: Vec<GroupView>,
    skipped: Vec<(String, String)>,
    keep_rule: KeepRule,
    preview: Option<Preview>,
    /// Started with `--dry-run`: actions are only logged.
    dry_run_only: bool,
    log: Option<ActionLog>,
    message: Option<String>,
    error: Option<String>,
    actions: Vec<UiAction>,
}

impl Duplicates {
    pub fn new(
        ctx: egui::Context,
        jobs: Sender<Job>,
        outcomes: Receiver<JobOutcome>,
        folder: String,
        dry_run_only: bool,
    ) -> Self {
        Self {
            jobs,
            outcomes,
            ctx,
            service: ServiceState::Connected,
            generation: 0,
            folder,
            min_size: DEFAULT_MIN_SIZE.to_owned(),
            filter: String::new(),
            stage: Stage::Idle,
            candidates: None,
            groups: Vec::new(),
            skipped: Vec::new(),
            keep_rule: KeepRule::Oldest,
            preview: None,
            dry_run_only,
            log: ActionLog::default_file().map(ActionLog::new),
            message: None,
            error: None,
            actions: Vec::new(),
        }
    }

    fn busy(&self) -> bool {
        !matches!(self.stage, Stage::Idle)
    }

    /// Asks the service for equal-size files and compares them afterwards.
    pub fn start(&mut self) {
        let Some(min_size) = format::parse_size(&self.min_size) else {
            self.error = Some(format!("invalid minimum size `{}`", self.min_size));
            return;
        };
        self.cancel();
        self.generation += 1;
        self.groups.clear();
        self.skipped.clear();
        self.candidates = None;
        self.error = None;
        self.message = None;
        let folder = self.folder.trim().trim_matches('"').to_owned();
        let request = DuplicateCandidatesRequest {
            roots: if folder.is_empty() {
                Vec::new()
            } else {
                vec![folder]
            },
            min_size,
            filter: self.filter.trim().to_owned(),
            max_files: MAX_FILES,
        };
        let job = Job {
            generation: self.generation,
            kind: JobKind::Duplicates,
            request: Request::DuplicateCandidates(request),
        };
        if self.jobs.send(job).is_err() {
            self.error = Some("the background worker stopped".into());
            return;
        }
        self.stage = Stage::Fetching;
    }

    fn cancel(&mut self) {
        match &self.stage {
            Stage::Hashing { progress, .. } => {
                progress.cancel.store(true, Ordering::Relaxed);
                self.stage = Stage::Idle;
                self.message = Some("Comparison cancelled".into());
            }
            Stage::Fetching => {
                self.generation += 1; // ignore the pending answer
                self.stage = Stage::Idle;
            }
            // Actions are not interrupted halfway.
            Stage::Acting { .. } | Stage::Idle => {}
        }
    }

    fn receive(&mut self) {
        while let Ok(outcome) = self.outcomes.try_recv() {
            if outcome.generation != self.generation {
                continue;
            }
            self.service = outcome.service;
            match outcome.result {
                Ok(Response::DuplicateCandidates(found)) => self.compare(found),
                Ok(other) => {
                    self.stage = Stage::Idle;
                    self.error = Some(format!("unexpected response {other:?}"));
                }
                Err(message) => {
                    self.stage = Stage::Idle;
                    self.error = Some(message);
                }
            }
        }
    }

    /// Starts hashing the candidates on a background thread.
    fn compare(&mut self, found: DuplicateCandidatesResponse) {
        let listed_files = found.groups.iter().map(|g| g.files.len() as u64).sum();
        self.candidates = Some(Candidates {
            total_groups: found.total_groups,
            total_files: found.total_files,
            listed_files,
            took_us: found.took_us,
            complete: found.complete,
        });
        if found.groups.is_empty() {
            self.stage = Stage::Idle;
            self.message = Some("No files of equal size: no duplicates.".into());
            return;
        }
        let groups: Vec<Vec<Candidate>> = found
            .groups
            .into_iter()
            .map(|g| {
                g.files
                    .into_iter()
                    .map(|f| Candidate {
                        path: f.path,
                        size: g.size,
                    })
                    .collect()
            })
            .collect();
        let progress = Arc::new(Progress::default());
        let (tx, rx) = channel();
        let (thread_progress, ctx) = (Arc::clone(&progress), self.ctx.clone());
        let spawned = std::thread::Builder::new()
            .name("duplicate-hasher".into())
            .spawn(move || {
                let report = hasher::confirm(groups, &thread_progress);
                let _ = tx.send(report);
                ctx.request_repaint();
            });
        match spawned {
            Ok(_) => {
                self.stage = Stage::Hashing {
                    progress,
                    result: rx,
                };
            }
            Err(err) => {
                self.stage = Stage::Idle;
                self.error = Some(format!("cannot start the comparison: {err}"));
            }
        }
    }

    /// Picks up finished background work.
    fn poll(&mut self) {
        let mut finished_hash = None;
        let mut finished_acts = None;
        match &self.stage {
            Stage::Hashing { result, .. } => match result.try_recv() {
                Ok(report) => finished_hash = Some(report),
                Err(_) => self.ctx.request_repaint_after(Duration::from_millis(100)),
            },
            Stage::Acting { result, .. } => match result.try_recv() {
                Ok(outcomes) => finished_acts = Some(outcomes),
                Err(_) => self.ctx.request_repaint_after(Duration::from_millis(100)),
            },
            Stage::Idle | Stage::Fetching => {}
        }
        if let Some(report) = finished_hash {
            self.stage = Stage::Idle;
            self.finish_compare(report);
        }
        if let Some(outcomes) = finished_acts {
            if let Stage::Acting { plan, dry_run, .. } =
                std::mem::replace(&mut self.stage, Stage::Idle)
            {
                self.finish_actions(&plan, &outcomes, dry_run);
            }
        }
    }

    fn finish_compare(&mut self, report: Report) {
        if report.cancelled {
            self.message = Some("Comparison cancelled".into());
            return;
        }
        self.skipped = report.skipped;
        let linked = report.already_linked;
        self.groups = report
            .groups
            .into_iter()
            .map(|group| GroupView {
                marked: vec![false; group.files.len()],
                group,
            })
            .collect();
        let wasted: u64 = self.groups.iter().map(|g| g.group.wasted()).sum();
        let copies: usize = self.groups.iter().map(|g| g.group.files.len() - 1).sum();
        let mut message = if self.groups.is_empty() {
            "No duplicates.".to_owned()
        } else {
            format!(
                "{} of identical files; {} use {}.",
                format::counted(self.groups.len() as u64, "group", None),
                format::counted(copies as u64, "extra copy", Some("extra copies")),
                format::size(wasted)
            )
        };
        if linked > 0 {
            message.push_str(&format!(
                " {} already hard links (one file, no space wasted).",
                format::counted(linked as u64, "name is", Some("names are"))
            ));
        }
        self.message = Some(message);
    }

    fn finish_actions(&mut self, plan: &[Planned], outcomes: &[Outcome], dry_run: bool) {
        let (mut done, mut bytes, mut skipped, mut failed) = (0usize, 0u64, 0usize, Vec::new());
        for (item, outcome) in plan.iter().zip(outcomes) {
            match outcome {
                Outcome::Done => {
                    done += 1;
                    bytes += item.state.size;
                    self.forget(&item.path);
                }
                Outcome::Logged => {
                    done += 1;
                    bytes += item.state.size;
                }
                Outcome::Skipped(reason) => {
                    skipped += 1;
                    failed.push(format!("{}: {reason}", item.path));
                }
                Outcome::Failed(reason) => failed.push(format!("{}: {reason}", item.path)),
            }
        }
        self.groups.retain(|g| g.group.files.len() >= 2);
        let verb = match (dry_run, plan.first().map(|p| p.kind)) {
            (true, _) => "Dry run logged",
            (false, Some(Kind::HardLink)) => "Replaced with hard links:",
            (false, _) => "Removed",
        };
        let mut message = format!(
            "{verb} {} ({}).",
            format::counted(done as u64, "file", None),
            format::size(bytes)
        );
        if let Some(log) = &self.log {
            message.push_str(&format!(" Log: {}", log.path().display()));
        }
        self.message = Some(message);
        self.error = (!failed.is_empty()).then(|| {
            let failures = failed.len() - skipped;
            format!(
                "{skipped} skipped, {failures} failed. First: {}",
                failed.first().map_or("", String::as_str)
            )
        });
    }

    /// Drops a file that no longer needs attention from its group.
    fn forget(&mut self, path: &str) {
        for view in &mut self.groups {
            if let Some(i) = view.group.files.iter().position(|f| f.path == path) {
                view.group.files.remove(i);
                view.marked.remove(i);
                return;
            }
        }
    }

    fn mark_by_rule(&mut self) {
        for view in &mut self.groups {
            let keep = self.keep_rule.pick(&view.group);
            for (i, marked) in view.marked.iter_mut().enumerate() {
                *marked = i != keep;
            }
        }
    }

    fn clear_marks(&mut self) {
        for view in &mut self.groups {
            view.marked.fill(false);
        }
    }

    fn toggle(&mut self, group: usize, file: usize) {
        let Some(view) = self.groups.get_mut(group) else {
            return;
        };
        if !view.marked[file] && view.marked_count() + 1 == view.marked.len() {
            self.message = Some("At least one copy in each group is kept.".into());
            return;
        }
        view.marked[file] = !view.marked[file];
    }

    fn keep_only(&mut self, group: usize, file: usize) {
        if let Some(view) = self.groups.get_mut(group) {
            for (i, marked) in view.marked.iter_mut().enumerate() {
                *marked = i != file;
            }
        }
    }

    fn marked_summary(&self) -> (usize, u64) {
        self.groups.iter().fold((0, 0), |(n, bytes), view| {
            let marked = view.marked_count();
            (n + marked, bytes + view.group.size * marked as u64)
        })
    }

    /// Builds the plan for the marked files and opens the confirmation dialog.
    fn open_preview(&mut self, kind: Kind) {
        let mut plan = Vec::new();
        let mut unplannable = Vec::new();
        for view in &self.groups {
            let kept: Vec<_> = view
                .group
                .files
                .iter()
                .zip(&view.marked)
                .filter(|(_, marked)| !**marked)
                .map(|(f, _)| f)
                .collect();
            for (file, _) in view
                .group
                .files
                .iter()
                .zip(&view.marked)
                .filter(|(_, m)| **m)
            {
                let keep = match kind {
                    Kind::Remove => kept.first(),
                    Kind::HardLink => kept
                        .iter()
                        .find(|k| k.state.volume_serial == file.state.volume_serial),
                };
                match keep {
                    Some(keep) => plan.push(Planned {
                        kind,
                        path: file.path.clone(),
                        state: file.state,
                        keep: keep.path.clone(),
                        keep_state: keep.state,
                        hash: view.group.hash_hex(),
                    }),
                    None => unplannable.push((
                        file.path.clone(),
                        "no kept copy on the same volume".to_owned(),
                    )),
                }
            }
        }
        if plan.is_empty() && unplannable.is_empty() {
            self.message = Some("Mark the copies to remove first.".into());
            return;
        }
        self.preview = Some(Preview {
            kind,
            plan,
            unplannable,
            permanent: false,
        });
    }

    /// Runs the previewed plan on a background thread with its own STA apartment.
    fn run_plan(&mut self, dry_run: bool) {
        let Some(preview) = self.preview.take() else {
            return;
        };
        let Some(log) = self.log.clone() else {
            self.error = Some("LOCALAPPDATA is not set: cannot write the action log".into());
            return;
        };
        if preview.plan.is_empty() {
            return;
        }
        let disposal = if preview.permanent {
            Disposal::Permanent
        } else {
            Disposal::RecycleBin
        };
        let dry_run = dry_run || self.dry_run_only;
        let progress = Arc::new(ActProgress::default());
        let (tx, rx) = channel();
        let plan = preview.plan.clone();
        let (thread_progress, ctx) = (Arc::clone(&progress), self.ctx.clone());
        let spawned = std::thread::Builder::new()
            .name("duplicate-actions".into())
            .spawn(move || {
                // SAFETY: first COM call on this new thread; balanced by CoUninitialize below.
                let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
                let owner = own_window();
                let outcomes = actions::execute(&plan, disposal, dry_run, &log, owner, |i, _| {
                    thread_progress.done.store(i as u64 + 1, Ordering::Relaxed);
                });
                if com.is_ok() {
                    // SAFETY: matches the successful CoInitializeEx on this thread.
                    unsafe { CoUninitialize() };
                }
                let _ = tx.send(outcomes);
                ctx.request_repaint();
            });
        match spawned {
            Ok(_) => {
                self.stage = Stage::Acting {
                    progress,
                    total: preview.plan.len(),
                    plan: preview.plan,
                    dry_run,
                    result: rx,
                };
            }
            Err(err) => self.error = Some(format!("cannot start: {err}")),
        }
    }

    fn apply_actions(&mut self, ctx: &egui::Context) {
        for action in std::mem::take(&mut self.actions) {
            match action {
                UiAction::Toggle(g, f) => self.toggle(g, f),
                UiAction::KeepOnly(g, f) => self.keep_only(g, f),
                UiAction::Reveal(path) => {
                    if let Err(err) = shell::reveal(&path) {
                        self.error = Some(format!("{err:#}"));
                    }
                }
                UiAction::Open(path) => {
                    if let Err(err) = shell::open(&path) {
                        self.error = Some(format!("{err:#}"));
                    }
                }
                UiAction::CopyPath(path) => {
                    ctx.copy_text(path.clone());
                    self.message = Some(format!("Copied {path}"));
                }
            }
        }
    }

    pub fn update(&mut self, ctx: &egui::Context) {
        self.receive();
        self.poll();
        egui::TopBottomPanel::top("dup-toolbar").show(ctx, |ui| self.toolbar(ui));
        egui::TopBottomPanel::bottom("dup-status").show(ctx, |ui| self.status(ui));
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.service == ServiceState::Connected {
                self.list(ui);
            } else {
                self.service_missing(ui);
            }
        });
        self.preview_dialog(ctx);
        self.apply_actions(ctx);
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let mut submit = false;
            let folder = ui.add(
                TextEdit::singleline(&mut self.folder)
                    .hint_text("Folder (empty = all volumes)")
                    .desired_width(340.0),
            );
            submit |= folder.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            ui.label("Min size");
            let min = ui.add(TextEdit::singleline(&mut self.min_size).desired_width(70.0));
            submit |= min.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            let filter = ui.add(
                TextEdit::singleline(&mut self.filter)
                    .hint_text("Filter: ext:jpg;png  dm:year")
                    .desired_width(220.0),
            );
            submit |= filter.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            let searching = matches!(self.stage, Stage::Fetching | Stage::Hashing { .. });
            if searching {
                if ui.button("Cancel").clicked() {
                    self.cancel();
                }
            } else if ui
                .add_enabled(!self.busy(), egui::Button::new("Find duplicates"))
                .clicked()
                || (submit && !self.busy())
            {
                self.start();
            }
        });
        if !self.groups.is_empty() {
            ui.horizontal(|ui| {
                let idle = !self.busy();
                egui::ComboBox::from_id_salt("keep-rule")
                    .selected_text(self.keep_rule.label())
                    .show_ui(ui, |ui| {
                        for rule in KeepRule::ALL {
                            ui.selectable_value(&mut self.keep_rule, rule, rule.label());
                        }
                    });
                if ui
                    .add_enabled(idle, egui::Button::new("Mark duplicates"))
                    .on_hover_text("Marks every copy except the one the rule keeps")
                    .clicked()
                {
                    self.mark_by_rule();
                }
                if ui
                    .add_enabled(idle, egui::Button::new("Clear marks"))
                    .clicked()
                {
                    self.clear_marks();
                }
                ui.separator();
                let (marked, bytes) = self.marked_summary();
                let any = idle && marked > 0;
                if ui
                    .add_enabled(any, egui::Button::new("Remove marked…"))
                    .on_hover_text("Moves the marked copies to the Recycle Bin (preview first)")
                    .clicked()
                {
                    self.open_preview(Kind::Remove);
                }
                if ui
                    .add_enabled(any, egui::Button::new("Replace marked with hard links…"))
                    .on_hover_text("Keeps the paths but makes them share one copy (preview first)")
                    .clicked()
                {
                    self.open_preview(Kind::HardLink);
                }
                if marked > 0 {
                    ui.label(format!(
                        "{} marked ({})",
                        format::count(marked as u64),
                        format::size(bytes)
                    ));
                }
                if self.dry_run_only {
                    ui.label(
                        RichText::new("--dry-run: actions are only logged")
                            .color(ui.visuals().warn_fg_color),
                    );
                }
            });
        }
        ui.add_space(2.0);
    }

    fn status(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            match &self.stage {
                Stage::Fetching => {
                    ui.add(egui::Spinner::new().size(12.0));
                    ui.label("Looking for files of equal size…");
                }
                Stage::Hashing { progress, .. } => {
                    let phase = progress.phase.load(Ordering::Relaxed);
                    let (done, total) = (
                        progress.bytes_done.load(Ordering::Relaxed),
                        progress.bytes_total.load(Ordering::Relaxed),
                    );
                    let files = (
                        progress.files_done.load(Ordering::Relaxed),
                        progress.files_total.load(Ordering::Relaxed),
                    );
                    let step = if phase == 0 {
                        "Comparing file starts and ends"
                    } else {
                        "Comparing full contents"
                    };
                    ui.add(
                        egui::ProgressBar::new(format::share(done, total))
                            .desired_width(260.0)
                            .text(format!(
                                "{step}: {} / {} files, {} / {}",
                                format::count(files.0),
                                format::count(files.1),
                                format::size(done),
                                format::size(total)
                            )),
                    );
                }
                Stage::Acting {
                    progress, total, ..
                } => {
                    let done = progress.done.load(Ordering::Relaxed);
                    ui.add(
                        egui::ProgressBar::new(format::share(done, *total as u64))
                            .desired_width(260.0)
                            .text(format!("{done} / {total}")),
                    );
                }
                Stage::Idle => {}
            }
            if let Some(found) = &self.candidates {
                let mut text = format!(
                    "{} files in {} equal-size groups ({:.0} ms)",
                    format::count(found.total_files),
                    format::count(found.total_groups),
                    found.took_us as f64 / 1000.0
                );
                if found.listed_files < found.total_files {
                    text.push_str(&format!(
                        "; compared the {} files with the most potential savings",
                        format::count(found.listed_files)
                    ));
                }
                ui.label(RichText::new(text).weak());
                if !found.complete {
                    ui.label(
                        RichText::new("indexing not finished").color(ui.visuals().warn_fg_color),
                    );
                }
            }
            if !self.skipped.is_empty() {
                let list: String = self
                    .skipped
                    .iter()
                    .take(30)
                    .map(|(path, reason)| format!("{path}: {reason}\n"))
                    .collect();
                ui.label(
                    RichText::new(format!("{} not compared", self.skipped.len()))
                        .color(ui.visuals().warn_fg_color),
                )
                .on_hover_text(list);
            }
            if let Some(error) = &self.error {
                ui.label(RichText::new(error).color(ui.visuals().error_fg_color));
            } else if let Some(message) = &self.message {
                ui.label(message);
            }
        });
    }

    fn service_missing(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(80.0);
            let (title, detail) = match &self.service {
                ServiceState::NotRunning => (
                    "The ferrofs service is not running.",
                    "Install it from an elevated shell: ferro-service install".to_owned(),
                ),
                ServiceState::Unavailable(reason) => {
                    ("Cannot use the ferrofs service.", reason.clone())
                }
                ServiceState::Connected => return,
            };
            ui.label(RichText::new(title).size(16.0).strong());
            ui.label(RichText::new(detail).color(ui.visuals().weak_text_color()));
            ui.add_space(8.0);
            if ui.button("Retry").clicked() {
                self.service = ServiceState::Connected;
                self.start();
            }
        });
    }

    fn list(&mut self, ui: &mut egui::Ui) {
        if self.groups.is_empty() {
            if matches!(self.stage, Stage::Idle) && self.candidates.is_none() {
                ui.vertical_centered(|ui| {
                    ui.add_space(80.0);
                    ui.label(
                        RichText::new("Pick a folder (or leave it empty for all volumes) and press Find duplicates.")
                            .weak(),
                    );
                    ui.label(
                        RichText::new("Files are compared by content; nothing is changed until you confirm a preview.")
                            .weak(),
                    );
                });
            }
            return;
        }
        // One line per group header and per file.
        let lines: Vec<(usize, Option<usize>)> = self
            .groups
            .iter()
            .enumerate()
            .flat_map(|(g, view)| {
                std::iter::once((g, None))
                    .chain((0..view.group.files.len()).map(move |f| (g, Some(f))))
            })
            .collect();
        let Self {
            groups, actions, ..
        } = self;
        let font = FontId::proportional(13.5);
        ScrollArea::vertical().auto_shrink([false; 2]).show_rows(
            ui,
            ROW_HEIGHT,
            lines.len(),
            |ui, range| {
                for &(g, file) in &lines[range] {
                    let view = &groups[g];
                    let (rect, response) = ui.allocate_exact_size(
                        vec2(ui.available_width(), ROW_HEIGHT),
                        if file.is_some() {
                            Sense::click()
                        } else {
                            Sense::hover()
                        },
                    );
                    let visuals = ui.visuals();
                    let Some(f) = file else {
                        paint_header(ui, rect, &view.group);
                        continue;
                    };
                    let item = &view.group.files[f];
                    let marked = view.marked[f];
                    let any_marked = view.marked_count() > 0;
                    row_background(ui, rect, false, response.hovered());
                    let check = Rect::from_center_size(
                        pos2(rect.left() + 22.0, rect.center().y),
                        vec2(13.0, 13.0),
                    );
                    paint_checkbox(ui, check, marked);
                    let badge_width = 64.0;
                    let date_width = 130.0;
                    let text_rect = Rect::from_x_y_ranges(
                        check.right() + 8.0..=rect.right() - badge_width - date_width - 12.0,
                        rect.y_range(),
                    );
                    let color = if marked {
                        visuals.weak_text_color()
                    } else {
                        visuals.strong_text_color()
                    };
                    let drawn = ui.painter().with_clip_rect(text_rect).text(
                        text_rect.left_center(),
                        Align2::LEFT_CENTER,
                        fit_text(ui, &item.path, &font, text_rect.width(), true),
                        font.clone(),
                        color,
                    );
                    if marked {
                        ui.painter().hline(
                            drawn.x_range(),
                            drawn.center().y,
                            Stroke::new(1.0_f32, color),
                        );
                    }
                    ui.painter().text(
                        pos2(rect.right() - badge_width - 12.0, rect.center().y),
                        Align2::RIGHT_CENTER,
                        format::date_time(item.state.mtime),
                        FontId::proportional(12.5),
                        visuals.weak_text_color(),
                    );
                    let badge = if marked {
                        Some(("remove", visuals.error_fg_color))
                    } else if any_marked {
                        Some(("keep", Color32::from_rgb(80, 170, 100)))
                    } else {
                        None
                    };
                    if let Some((text, color)) = badge {
                        ui.painter().text(
                            pos2(rect.right() - 8.0, rect.center().y),
                            Align2::RIGHT_CENTER,
                            text,
                            FontId::proportional(12.5),
                            color,
                        );
                    }
                    let response = response.on_hover_text(&item.path);
                    if response.double_clicked() {
                        actions.push(UiAction::Reveal(item.path.clone()));
                    } else if response.clicked() {
                        actions.push(UiAction::Toggle(g, f));
                    }
                    response.context_menu(|ui| {
                        if ui.button("Keep only this copy").clicked() {
                            actions.push(UiAction::KeepOnly(g, f));
                        }
                        if ui.button("Show in Explorer").clicked() {
                            actions.push(UiAction::Reveal(item.path.clone()));
                        }
                        if ui.button("Open").clicked() {
                            actions.push(UiAction::Open(item.path.clone()));
                        }
                        if ui.button("Copy path").clicked() {
                            actions.push(UiAction::CopyPath(item.path.clone()));
                        }
                    });
                }
            },
        );
    }

    fn preview_dialog(&mut self, ctx: &egui::Context) {
        let Some(preview) = &mut self.preview else {
            return;
        };
        let mut choice = None;
        let dry_run_only = self.dry_run_only;
        egui::Modal::new(egui::Id::new("duplicate-preview")).show(ctx, |ui| {
            ui.set_width(720.0);
            let bytes: u64 = preview.plan.iter().map(|p| p.state.size).sum();
            let files = format::counted(preview.plan.len() as u64, "file", None);
            let title = match (preview.kind, preview.permanent) {
                (Kind::Remove, false) => format!("Move {files} to the Recycle Bin"),
                (Kind::Remove, true) => format!("Permanently delete {files}"),
                (Kind::HardLink, _) => format!("Replace {files} with hard links"),
            };
            ui.heading(title);
            ui.label(format!(
                "Frees {}. Each file is checked again right before it is touched: if it or the copy that is kept changed, it is skipped.",
                format::size(bytes)
            ));
            if preview.kind == Kind::HardLink {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "Hard links are one file with several names: editing it through any name changes all of them. \
                         Programs that save by writing a new file and renaming it (many editors, Office) silently break the link.",
                    )
                    .color(ui.visuals().warn_fg_color),
                );
                ui.label(
                    "The original duplicates go to the Recycle Bin (or are deleted, see below) before the links take their place.",
                );
            }
            ui.add_space(6.0);
            ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                for item in preview.plan.iter().take(500) {
                    ui.label(RichText::new(&item.path).strong());
                    ui.label(RichText::new(format!("    keeps {}", item.keep)).weak());
                }
                if preview.plan.len() > 500 {
                    ui.label(format!("… and {} more", preview.plan.len() - 500));
                }
                for (path, reason) in &preview.unplannable {
                    ui.label(
                        RichText::new(format!("{path}: {reason} (left alone)"))
                            .color(ui.visuals().warn_fg_color),
                    );
                }
            });
            ui.add_space(6.0);
            ui.checkbox(
                &mut preview.permanent,
                "Delete permanently instead of using the Recycle Bin (cannot be undone)",
            );
            if dry_run_only {
                ui.label(
                    RichText::new("Started with --dry-run: actions are only logged.")
                        .color(ui.visuals().warn_fg_color),
                );
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let can_apply = !dry_run_only && !preview.plan.is_empty();
                if ui.add_enabled(can_apply, egui::Button::new("Apply")).clicked() {
                    choice = Some(Some(false));
                }
                if ui
                    .add_enabled(!preview.plan.is_empty(), egui::Button::new("Dry run (log only)"))
                    .clicked()
                {
                    choice = Some(Some(true));
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(None);
                }
            });
        });
        match choice {
            Some(Some(dry_run)) => self.run_plan(dry_run),
            Some(None) => self.preview = None,
            None => {}
        }
    }
}

fn paint_header(ui: &egui::Ui, rect: Rect, group: &DupGroup) {
    let visuals = ui.visuals();
    ui.painter().rect_filled(
        rect.shrink2(vec2(0.0, 1.0)),
        2.0,
        visuals.widgets.noninteractive.weak_bg_fill,
    );
    let text = format!(
        "{} copies of {}  ·  {} extra",
        group.files.len(),
        format::size(group.size),
        format::size(group.wasted())
    );
    ui.painter().text(
        pos2(rect.left() + 8.0, rect.center().y),
        Align2::LEFT_CENTER,
        text,
        FontId::proportional(13.0),
        visuals.strong_text_color(),
    );
    ui.painter().text(
        pos2(rect.right() - 8.0, rect.center().y),
        Align2::RIGHT_CENTER,
        format!("blake3 {}", &group.hash_hex()[..12]),
        FontId::monospace(11.5),
        visuals.weak_text_color(),
    );
}

fn paint_checkbox(ui: &egui::Ui, rect: Rect, checked: bool) {
    let visuals = ui.visuals();
    let fill = if checked {
        visuals.selection.bg_fill
    } else {
        visuals.extreme_bg_color
    };
    ui.painter().rect_filled(rect, 2.0, fill);
    ui.painter().rect_stroke(
        rect,
        2.0,
        visuals.widgets.inactive.fg_stroke,
        egui::StrokeKind::Inside,
    );
    if checked {
        let stroke = Stroke::new(2.0_f32, visuals.selection.stroke.color);
        let (a, b, c) = (
            pos2(rect.left() + 3.0, rect.center().y),
            pos2(rect.center().x - 1.0, rect.bottom() - 3.0),
            pos2(rect.right() - 3.0, rect.top() + 3.0),
        );
        ui.painter().line_segment([a, b], stroke);
        ui.painter().line_segment([b, c], stroke);
    }
}

/// The storage window, as owner for shell dialogs (e.g. "delete permanently?").
fn own_window() -> Option<HWND> {
    // SAFETY: the title is a static NUL-terminated literal.
    unsafe { FindWindowW(None, w!("ferrofs storage")) }.ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hasher::{DupFile, FileState};

    fn file(path: &str, mtime: u64) -> DupFile {
        DupFile {
            path: path.into(),
            state: FileState {
                volume_serial: 1,
                file_id: mtime,
                size: 10,
                mtime,
            },
        }
    }

    #[test]
    fn keep_rules() {
        let group = DupGroup {
            size: 10,
            blake3: [0; 32],
            files: vec![
                file(r"C:\photos\2020\a.jpg", 30),
                file(r"C:\b.jpg", 20),
                file(r"C:\backup\old\a.jpg", 10),
            ],
        };
        assert_eq!(KeepRule::Oldest.pick(&group), 2);
        assert_eq!(KeepRule::Newest.pick(&group), 0);
        assert_eq!(KeepRule::ShortestPath.pick(&group), 1);
    }
}
