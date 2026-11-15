#[cfg(unix)]
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

#[cfg(not(unix))]
fn main() {
    eprintln!(
        "The first Watchmpv client uses Unix-domain mpv IPC. Windows named-pipe support is planned."
    );
    std::process::exit(1);
}
