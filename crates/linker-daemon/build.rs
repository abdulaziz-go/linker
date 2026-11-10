//! macOS only: embed an Info.plist into the `linkerd` binary
//! (`__TEXT,__info_plist`). TCC needs the usage strings to show the
//! microphone / camera permission prompts for a bare (non-.app) executable,
//! and the bundle identifier matches the codesign identifier used by
//! `runbook/macos/install.sh` (`io.linker.daemon`). Does nothing elsewhere.

use std::path::PathBuf;

const MIC_USAGE: &str = "Linker shares this Mac's microphone with your paired Ubuntu machine while an app there uses \"Linker Microphone\".";
const CAMERA_USAGE: &str = "Linker shares this Mac's camera with your paired Ubuntu machine while an app there uses \"Linker Camera\".";

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let entries = [
        ("CFBundleIdentifier", "io.linker.daemon".to_string()),
        ("CFBundleName", "linkerd".to_string()),
        ("CFBundleInfoDictionaryVersion", "6.0".to_string()),
        ("CFBundleShortVersionString", version.clone()),
        ("CFBundleVersion", version),
        ("NSMicrophoneUsageDescription", MIC_USAGE.to_string()),
        ("NSCameraUsageDescription", CAMERA_USAGE.to_string()),
    ];
    let mut plist = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n",
    );
    for (k, v) in entries {
        plist.push_str(&format!(
            "\t<key>{}</key>\n\t<string>{}</string>\n",
            xml_escape(k),
            xml_escape(&v)
        ));
    }
    plist.push_str("</dict>\n</plist>\n");

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR is set by cargo"));
    let path = out.join("Info.plist");
    std::fs::write(&path, plist).expect("write Info.plist");
    println!(
        "cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
        path.display()
    );
}
