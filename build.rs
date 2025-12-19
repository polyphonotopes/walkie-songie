use std::fs;
use std::path::Path;

fn main() {
    // Copy palette.css from polyphonotopic-colors as the source of truth
    let src = Path::new("../polyphonotopic-colors/palette.css");
    let dst = Path::new("assets/colors.css");

    if src.exists() {
        fs::copy(src, dst).expect("Failed to copy palette.css");
    }

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../polyphonotopic-colors/palette.css");
}
