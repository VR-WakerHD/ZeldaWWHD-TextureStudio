#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod cli;
mod images;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(|a| a == "--headless").unwrap_or(false) {
        attach_console();
        std::process::exit(cli::main(args[1..].to_vec()));
    }
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("ZeldaWWHD Texture Studio")
            .with_inner_size([1500.0, 920.0])
            .with_min_inner_size([960.0, 600.0]),
        ..Default::default()
    };
    if let Err(e) = eframe::run_native("ZeldaWWHD Texture Studio", options, Box::new(|cc| Ok(Box::new(app::App::new(cc))))) {
        eprintln!("cannot start the GUI: {e}");
        std::process::exit(1);
    }
}

/// The release binary is a GUI program on Windows; give headless runs the
/// parent console back so output is visible.
fn attach_console() {
    #[cfg(windows)]
    unsafe {
        unsafe extern "system" {
            fn AttachConsole(pid: u32) -> i32;
        }
        AttachConsole(u32::MAX);
    }
}
