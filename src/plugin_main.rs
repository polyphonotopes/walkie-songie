//! Plugin binary entry point.
//!
//! This file exists only to satisfy Cargo's binary target requirement.
//! The actual plugin exports are in src/plugin/mod.rs via nih_export_* macros.

fn main() {
    // This binary is not meant to be run directly.
    // Use `cargo xtask bundle walkie-songie-plugin --release` to build the plugin.
    eprintln!("This is a plugin binary. Build with: cargo xtask bundle walkie-songie --release");
}
