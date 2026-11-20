// Build-script helper, included by the apps' build.rs: writes the app's .ico into OUT_DIR,
// compiles a one-line .rc with the Windows SDK's rc.exe and links the resource into the
// executables. Without rc.exe the build continues with a warning (no exe icon).

#[allow(dead_code)]
fn embed_icon(kind: IconKind) {
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default());
    let ico = out.join("app.ico");
    let rc = out.join("app.rc");
    let res = out.join("app.res");
    if std::fs::write(&ico, ico_file(kind)).is_err() {
        println!("cargo:warning=cannot write {}", ico.display());
        return;
    }
    let ico_text = ico.display().to_string().replace('\\', "\\\\");
    if std::fs::write(&rc, format!("1 ICON \"{ico_text}\"\n")).is_err() {
        println!("cargo:warning=cannot write {}", rc.display());
        return;
    }
    let Some(compiler) = find_rc() else {
        println!("cargo:warning=rc.exe (Windows SDK) not found: the executable gets no icon");
        return;
    };
    let status = std::process::Command::new(&compiler)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res)
        .arg(&rc)
        .status();
    match status {
        Ok(status) if status.success() => {
            println!("cargo:rustc-link-arg-bins={}", res.display());
        }
        _ => println!("cargo:warning=rc.exe failed: the executable gets no icon"),
    }
    println!("cargo:rerun-if-changed=../../assets/icons.rs");
    println!("cargo:rerun-if-changed=../../assets/build_icon.rs");
}

/// rc.exe from the newest Windows 10/11 SDK (or the `RC` environment variable).
fn find_rc() -> Option<std::path::PathBuf> {
    if let Some(rc) = std::env::var_os("RC") {
        return Some(rc.into());
    }
    let base = std::path::PathBuf::from(
        std::env::var_os("ProgramFiles(x86)").unwrap_or_else(|| "C:\\Program Files (x86)".into()),
    )
    .join("Windows Kits")
    .join("10")
    .join("bin");
    let mut versions: Vec<std::path::PathBuf> = std::fs::read_dir(&base)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("x64").join("rc.exe").exists())
        .collect();
    versions.sort();
    versions.pop().map(|p| p.join("x64").join("rc.exe"))
}
