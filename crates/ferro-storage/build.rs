//! Embeds the application manifest (long path awareness, per-monitor v2 DPI, UTF-8 code
//! page, Common Controls v6: the defaults of `embed_manifest::new_manifest`) and the
//! application icon (drawn in `assets/icons.rs`).

include!("../../assets/icons.rs");
include!("../../assets/build_icon.rs");

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        if let Err(err) =
            embed_manifest::embed_manifest(embed_manifest::new_manifest("Ferrofs.Storage"))
        {
            panic!("cannot embed the application manifest: {err}");
        }
        embed_icon(IconKind::Storage);
    }
    println!("cargo:rerun-if-changed=build.rs");
}
