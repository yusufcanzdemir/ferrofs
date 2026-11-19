//! Embeds the application manifest: long path awareness, per-monitor v2 DPI,
//! UTF-8 code page and Common Controls v6 (defaults of `embed_manifest::new_manifest`).

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some()
        && let Err(err) =
            embed_manifest::embed_manifest(embed_manifest::new_manifest("Ferrofs.Link"))
    {
        panic!("cannot embed the application manifest: {err}");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
