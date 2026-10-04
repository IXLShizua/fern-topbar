use std::env;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"));
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set by Cargo"));

    println!("cargo:rerun-if-changed=assets");

    let status = Command::new("sass")
        .args([
            "--load-path",
            "assets",
            "--style",
            "expanded",
            "--no-source-map",
            "--stop-on-error",
            manifest_dir
                .join("assets")
                .join("styles.scss")
                .to_string_lossy()
                .as_ref(),
            out_dir.join("styles.css").to_string_lossy().as_ref(),
        ])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?
        .wait()?;

    if !status.success() {
        return Err("Failed to compile SCSS files".into());
    }

    relm4_icons_build::bundle_icons(
        "icon_names.rs",
        Some("com.example.FernTopbar"),
        None::<&str>,
        None::<&str>,
        [
            "volume-off-fill",
            "volume-mute-fill",
            "volume-down-fill",
            "volume-up-fill",
            "mic-fill",
            "mic-off-fill",
            "brightness-high-fill",
            "wifi-fill",
            "lan-fill",
            "wifi-off-fill",
            "battery-full-fill",
            "battery-low-fill",
            "battery-charging-full-fill",
            "notifications-fill",
            "chevron-left-fill",
            "chevron-right-fill",
            "close-fill",
            "check-fill",
            "radio-button-checked-fill",
            "radio-button-unchecked-fill",
        ],
    );

    Ok(())
}
