#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(any(unix, windows))]
fn main() -> eframe::Result<()> {
    eframe::run_native(
        "Watchmpv",
        eframe::NativeOptions {
            viewport: eframe::egui::ViewportBuilder::default()
                .with_inner_size([760.0, 640.0])
                .with_min_inner_size([600.0, 520.0]),
            ..Default::default()
        },
        Box::new(|_context| Ok(Box::new(watchmpv::client::WatchApp::default()))),
    )
}

#[cfg(not(any(unix, windows)))]
fn main() {
    eprintln!("Watchmpv requires Windows or a Unix desktop environment.");
    std::process::exit(1);
}
