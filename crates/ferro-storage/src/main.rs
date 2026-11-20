//! Storage tools on top of the ferrofs index:
//! - analyzer (SPEC §5.1): folder sizes as a tree table and a treemap, with the largest
//!   files and a per-extension breakdown;
//! - duplicate finder (SPEC §5.2): equal-size files from the index, compared by content,
//!   removed or replaced with hard links after a preview.
//!
//! ```text
//! ferro-storage [folder]                 analyzer at `folder` (default: all volumes)
//! ferro-storage --duplicates [folder]    duplicate finder; searches `folder` right away
//! ferro-storage --dry-run ...            destructive actions are only previewed and logged
//! ```

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
mod analyzer;
mod app;
mod duplicates;
mod format;
mod hasher;
mod logging;
mod paint;
mod shell;
mod tree;
mod treemap;
mod worker;

use anyhow::{Result, anyhow};
use eframe::egui;
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};

use crate::app::Tool;

const TITLE: &str = "ferrofs storage";

/// Parsed command line.
#[derive(Debug, Default, PartialEq, Eq)]
struct Args {
    duplicates: bool,
    dry_run: bool,
    folder: Option<String>,
}

fn parse_args(args: impl Iterator<Item = String>) -> Args {
    let mut parsed = Args::default();
    for arg in args {
        match arg.as_str() {
            "--duplicates" => parsed.duplicates = true,
            "--dry-run" => parsed.dry_run = true,
            // `"C:\"` arrives as `C:"` because `\"` escapes the quote: trim quotes.
            _ => parsed.folder = Some(arg.trim().trim_matches('"').to_owned()),
        }
    }
    parsed
}

fn main() -> Result<()> {
    let _log = logging::init()?;
    let args = parse_args(std::env::args().skip(1));
    tracing::info!(?args, "starting");

    // Shell calls (reveal in Explorer) need an STA on the UI thread. S_FALSE and
    // RPC_E_CHANGED_MODE are fine: some apartment already exists.
    // SAFETY: called once on the main thread before any window exists.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_inner_size([1360.0, 820.0])
            .with_min_inner_size([800.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        TITLE,
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            let (jobs, outcomes) = worker::spawn(ctx.clone())?;
            let analyzer_start = if args.duplicates {
                String::new()
            } else {
                args.folder.clone().unwrap_or_default()
            };
            let analyzer = analyzer::Analyzer::new(jobs, outcomes, analyzer_start);
            let (jobs, outcomes) = worker::spawn(ctx.clone())?;
            let dup_folder = if args.duplicates {
                args.folder.clone().unwrap_or_default()
            } else {
                String::new()
            };
            let mut duplicates =
                duplicates::Duplicates::new(ctx, jobs, outcomes, dup_folder, args.dry_run);
            let tool = if args.duplicates {
                if args.folder.is_some() {
                    duplicates.start();
                }
                Tool::Duplicates
            } else {
                Tool::Analyzer
            };
            Ok(Box::new(app::App::new(
                tool,
                analyzer,
                duplicates,
                args.dry_run,
            )))
        }),
    )
    .map_err(|err| anyhow!("{err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Args {
        parse_args(args.iter().map(|a| (*a).to_owned()))
    }

    #[test]
    fn command_line() {
        assert_eq!(parse(&[]), Args::default());
        assert_eq!(
            // `"C:\"` as Windows hands it over (`\"` escapes the quote).
            parse(&["--duplicates", "C:\"", "--dry-run"]),
            Args {
                duplicates: true,
                dry_run: true,
                folder: Some("C:".to_owned()),
            }
        );
        assert_eq!(parse(&[r"D:\Photos"]).folder.as_deref(), Some(r"D:\Photos"));
    }
}
