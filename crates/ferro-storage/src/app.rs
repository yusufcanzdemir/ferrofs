//! The storage window: a tab bar choosing the tool, then the tool itself.

use eframe::egui::{self, RichText};

use crate::analyzer::Analyzer;
use crate::cleanup::Cleanup;
use crate::duplicates::Duplicates;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Analyzer,
    Duplicates,
    Cleanup,
}

pub struct App {
    tool: Tool,
    analyzer: Analyzer,
    duplicates: Duplicates,
    cleanup: Cleanup,
    dry_run: bool,
}

impl App {
    pub fn new(
        tool: Tool,
        analyzer: Analyzer,
        duplicates: Duplicates,
        cleanup: Cleanup,
        dry_run: bool,
    ) -> Self {
        Self {
            tool,
            analyzer,
            duplicates,
            cleanup,
            dry_run,
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::TopBottomPanel::top("tools").show(ctx, |ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tool, Tool::Analyzer, "Analyzer");
                ui.selectable_value(&mut self.tool, Tool::Duplicates, "Duplicates");
                ui.selectable_value(&mut self.tool, Tool::Cleanup, "Cleanup");
                if self.dry_run {
                    ui.separator();
                    ui.label(
                        RichText::new("dry run: nothing is changed on disk")
                            .color(ui.visuals().warn_fg_color),
                    );
                }
            });
            ui.add_space(1.0);
        });
        match self.tool {
            Tool::Analyzer => self.analyzer.update(ctx),
            Tool::Duplicates => self.duplicates.update(ctx),
            Tool::Cleanup => self.cleanup.update(ctx),
        }
    }
}
