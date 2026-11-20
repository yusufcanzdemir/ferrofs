//! Embeds the application icon (drawn in `assets/icons.rs`).

include!("../../assets/icons.rs");
include!("../../assets/build_icon.rs");

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        embed_icon(IconKind::Search);
    }
    println!("cargo:rerun-if-changed=build.rs");
}
