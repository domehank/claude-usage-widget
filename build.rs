// Embeds the application manifest: Windows 10/11 compatibility (needed for a layered child
// window inside the taskbar), per-monitor DPI awareness, and Common Controls v6 (tooltip style).
use embed_manifest::manifest::DpiAwareness;
use embed_manifest::{embed_manifest, new_manifest};

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        embed_manifest(new_manifest("Claude.UsageWidget").dpi_awareness(DpiAwareness::PerMonitorV2))
            .expect("unable to embed manifest file");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
