//! Well-known cleanup locations (SPEC §5.4): Windows temporary data and package manager
//! caches, resolved from the current user's environment. Their sizes come from the index.

/// Group shown in the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Category {
    Windows,
    Caches,
    Developer,
    VirtualDisks,
}

impl Category {
    pub const ALL: [Self; 4] = [
        Self::Windows,
        Self::Caches,
        Self::Developer,
        Self::VirtualDisks,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Windows => "Windows temporary files",
            Self::Caches => "Package caches",
            Self::Developer => "Developer artifacts",
            Self::VirtualDisks => "Virtual disks (WSL, Docker)",
        }
    }
}

/// How much is lost if the item is removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Risk {
    /// Regenerated automatically or never needed again.
    Low,
    /// Downloaded or rebuilt again when needed; may be slow.
    Medium,
    /// Hard or impossible to get back.
    High,
}

impl Risk {
    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "low risk",
            Self::Medium => "medium risk",
            Self::High => "high risk",
        }
    }
}

/// What a cleanup removes at a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Everything inside the folder; the folder itself stays.
    Contents,
    /// The folder and everything in it.
    Folder,
    File,
}

impl Scope {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Contents => "contents",
            Self::Folder => "whole folder",
            Self::File => "file",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub category: Category,
    pub label: &'static str,
    pub path: String,
    pub scope: Scope,
    pub risk: Risk,
    /// Only an elevated process may delete it.
    pub needs_admin: bool,
    /// Listed for its size only; removed with other tools.
    pub report_only: bool,
    pub selected_by_default: bool,
    pub note: &'static str,
}

/// Locations for the environment `env` (normally `std::env::var`). Entries whose base
/// variable is missing are left out.
pub fn known(env: impl Fn(&str) -> Option<String>) -> Vec<Location> {
    let join = |base: &str, rest: &str| format!("{}\\{rest}", base.trim_end_matches('\\'));
    let windir = env("WINDIR").or_else(|| env("SystemRoot"));
    let local = env("LOCALAPPDATA");
    let profile = env("USERPROFILE");
    let mut out = Vec::new();
    let mut add =
        |category, label, path: Option<String>, scope, risk, needs_admin, note: &'static str| {
            if let Some(path) = path {
                out.push(Location {
                    category,
                    label,
                    path,
                    scope,
                    risk,
                    needs_admin,
                    report_only: false,
                    selected_by_default: false,
                    note,
                });
            }
        };
    use Category::{Caches, Windows};
    use Risk::{Low, Medium};
    use Scope::{Contents, File};

    add(
        Windows,
        "Your temporary files",
        env("TEMP")
            .or_else(|| env("TMP"))
            .map(|t| t.trim_end_matches('\\').to_owned()),
        Contents,
        Low,
        false,
        "Files in use are skipped.",
    );
    add(
        Windows,
        "Windows temporary files",
        windir.as_deref().map(|w| join(w, "Temp")),
        Contents,
        Low,
        true,
        "Files in use are skipped.",
    );
    add(
        Windows,
        "Windows Update downloads",
        windir
            .as_deref()
            .map(|w| join(w, "SoftwareDistribution\\Download")),
        Contents,
        Medium,
        true,
        "Windows downloads pending updates again if needed.",
    );
    add(Windows, "Delivery Optimization cache",
        windir.as_deref().map(|w| join(w, "ServiceProfiles\\NetworkService\\AppData\\Local\\Microsoft\\Windows\\DeliveryOptimization\\Cache")),
        Contents, Low, true, "Update pieces shared with other PCs.");
    add(
        Windows,
        "App crash dumps",
        local.as_deref().map(|l| join(l, "CrashDumps")),
        Contents,
        Low,
        false,
        "Only needed to report a crash to a developer.",
    );
    add(
        Windows,
        "System crash dumps",
        windir.as_deref().map(|w| join(w, "Minidump")),
        Contents,
        Low,
        true,
        "Only needed to diagnose a blue screen.",
    );
    add(
        Windows,
        "Kernel reports",
        windir.as_deref().map(|w| join(w, "LiveKernelReports")),
        Contents,
        Low,
        true,
        "Diagnostic dumps written after hardware or driver hangs.",
    );
    add(
        Windows,
        "Memory dump",
        windir.as_deref().map(|w| join(w, "MEMORY.DMP")),
        File,
        Low,
        true,
        "Full memory dump of the last blue screen.",
    );

    let cargo = env("CARGO_HOME").or_else(|| profile.as_deref().map(|p| join(p, ".cargo")));
    let gradle = env("GRADLE_USER_HOME").or_else(|| profile.as_deref().map(|p| join(p, ".gradle")));
    let again = "Downloaded again on the next install or build.";
    add(
        Caches,
        "npm cache",
        local.as_deref().map(|l| join(l, "npm-cache")),
        Contents,
        Medium,
        false,
        again,
    );
    add(
        Caches,
        "pnpm store",
        local.as_deref().map(|l| join(l, "pnpm\\store")),
        Contents,
        Medium,
        false,
        "Projects keep working (pnpm hard-links from the store); new installs download again.",
    );
    add(
        Caches,
        "Yarn cache",
        local.as_deref().map(|l| join(l, "Yarn\\Cache")),
        Contents,
        Medium,
        false,
        again,
    );
    add(
        Caches,
        "pip cache",
        local.as_deref().map(|l| join(l, "pip\\cache")),
        Contents,
        Medium,
        false,
        again,
    );
    add(
        Caches,
        "Cargo registry",
        cargo.map(|c| join(&c, "registry")),
        Contents,
        Medium,
        false,
        again,
    );
    add(
        Caches,
        "NuGet packages",
        profile.as_deref().map(|p| join(p, ".nuget\\packages")),
        Contents,
        Medium,
        false,
        again,
    );
    add(
        Caches,
        "Gradle caches",
        gradle.map(|g| join(&g, "caches")),
        Contents,
        Medium,
        false,
        again,
    );
    add(
        Caches,
        "Maven repository",
        profile.as_deref().map(|p| join(p, ".m2\\repository")),
        Contents,
        Medium,
        false,
        again,
    );

    if let Some(first) = out
        .first_mut()
        .filter(|l| l.label == "Your temporary files")
    {
        first.selected_by_default = true;
    }
    if let Some(drive) = env("SystemDrive") {
        out.push(Location {
            category: Windows,
            label: "Previous Windows installation",
            path: join(&drive, "Windows.old"),
            scope: Scope::Folder,
            risk: Risk::High,
            needs_admin: true,
            report_only: true,
            selected_by_default: false,
            note: "Needed to go back to the previous Windows version. Remove it with Settings > \
                   System > Storage > Temporary files, or Disk Cleanup > Clean up system files.",
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_follow_the_environment() {
        let env = |name: &str| match name {
            "WINDIR" => Some(r"C:\Windows".to_owned()),
            "LOCALAPPDATA" => Some(r"C:\Users\me\AppData\Local".to_owned()),
            "USERPROFILE" => Some(r"C:\Users\me".to_owned()),
            "TEMP" => Some(r"C:\Users\me\AppData\Local\Temp\".to_owned()),
            "CARGO_HOME" => Some(r"D:\cargo".to_owned()),
            "SystemDrive" => Some("C:".to_owned()),
            _ => None,
        };
        let all = known(env);
        let find = |label: &str| all.iter().find(|l| l.label == label).unwrap();
        let temp = find("Your temporary files");
        assert_eq!(temp.path, r"C:\Users\me\AppData\Local\Temp");
        assert!(temp.selected_by_default && !temp.needs_admin);
        assert_eq!(all.iter().filter(|l| l.selected_by_default).count(), 1);
        assert_eq!(find("Cargo registry").path, r"D:\cargo\registry");
        assert_eq!(find("Gradle caches").path, r"C:\Users\me\.gradle\caches");
        assert_eq!(find("Memory dump").scope, Scope::File);
        assert!(find("Windows Update downloads").needs_admin);
        let old = find("Previous Windows installation");
        assert!(old.report_only && old.path == r"C:\Windows.old");

        // Missing variables drop their entries instead of producing relative paths.
        let bare = known(|_| None);
        assert!(bare.is_empty());
    }
}
