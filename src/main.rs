mod core;
mod ui;

fn main() -> gtk::glib::ExitCode {
    // The same executable serves double duty: normally it launches the GUI,
    // but when systemd fires a scheduled backup it invokes us with
    // `--run-backup <repo-uuid> <schedule-uuid>`. In that case we skip GTK
    // entirely, run the backup synchronously and exit.
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 4 && args[1] == "--run-backup" {
        core::runner::run(&args[2], &args[3]);
    }
    // Payload for a waybar custom module (the desktop-widget stand-in).
    if args.len() >= 2 && args[1] == "--status-json" {
        println!("{}", core::snapshot::waybar_json());
        return gtk::glib::ExitCode::SUCCESS;
    }
    if args.len() >= 2 && (args[1] == "--version" || args[1] == "-V") {
        println!("Luna {}", env!("CARGO_PKG_VERSION"));
        return gtk::glib::ExitCode::SUCCESS;
    }
    // `--background`: start in the tray without opening the main window.
    let start_hidden = args.iter().skip(1).any(|a| a == "--background" || a == "--hidden");
    ui::app::run(start_hidden)
}
