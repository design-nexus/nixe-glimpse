mod player;
mod pw;
mod ui;
use gtk::{gio, glib, prelude::*};
fn main() -> glib::ExitCode {
    unsafe {
        libc::umask(0o077);
    }
    nixe_core::app::init(nixe_core::app::App { slug: "nixe-glimpse", name: "Glimpse" });
    let args: Vec<String> = std::env::args().skip(1).collect();
    match nixe_glimpse::cli::dispatch(&args) {
        Ok(true) => return glib::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e:#}");
            return glib::ExitCode::FAILURE;
        }
        Ok(false) => {}
    }
    if std::env::var_os("GSK_RENDERER").is_none() {
        unsafe {
            std::env::set_var("GSK_RENDERER", "ngl");
        }
    }
    let id = std::env::var("GLIMPSE_APP_ID").unwrap_or_else(|_| "io.github.design_nexus.nixe.Glimpse".into());
    let app = gtk::Application::builder().application_id(id).flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE).build();
    app.connect_startup(|_| ui::theme());
    app.connect_command_line(|app, cmd| {
        let args = cmd.arguments();
        let has = |s: &str| args.iter().any(|v| v == s);
        if has("--hide") {
            ui::dismiss();
        } else if has("--add-folder") {
            ui::add_folder(app);
        } else if has("--plugins") {
            ui::plugin_manager(app);
        } else if has("--add-plugin") {
            ui::add_plugin(app);
        } else {
            let query =
                args.iter().position(|s| s == "--query").and_then(|i| args.get(i + 1)).map(|s| s.to_string_lossy().into_owned());
            ui::present(app, has("--toggle"), query.as_deref());
        }
        glib::ExitCode::SUCCESS
    });
    app.run()
}
