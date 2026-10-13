mod app;
fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|s| s == "--help" || s == "-h") {
        println!(
            "Hiturbo-rs 0.1.0\n\nUsage:\n  hiturbo-rs                       Open desktop app\n  hiturbo-rs --replay FILE         Replay MJPEG or camera packet JSONL offline\n  hiturbo-rs --verify-log FILE [--capture-dir DIR] [--every N]\n                                 Decode and verify a packet recording offline\n  hiturbo-rs --smoke-test --replay FILE\n                                 Render, screenshot and close (developer check)"
        );
        return Ok(());
    }
    if args.iter().any(|s| s == "--version") {
        println!("hiturbo-rs 0.1.0");
        return Ok(());
    }
    if args.get(1).is_some_and(|s| s == "--verify-log") {
        if let Err(e) = hiturbo_rs::session::verify_log(&args[2..]) {
            eprintln!("{e}");
            std::process::exit(1);
        }
        return Ok(());
    }
    let replay = args
        .windows(2)
        .find(|a| a[0] == "--replay")
        .map(|a| std::path::PathBuf::from(&a[1]));
    let smoke = args.iter().any(|s| s == "--smoke-test");
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("hiturbo-rs: JPEG decoder")
            .with_inner_size([1460., 940.])
            .with_min_inner_size([1120., 760.]),
        ..Default::default()
    };
    eframe::run_native(
        "Hiturbo · Camera & Flight",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, replay, smoke)))),
    )
}
