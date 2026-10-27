fn main() {
    nyx_core::app::init(nyx_core::app::App { slug: "nyx-glimpse", name: "Glimpse" });
    let p = nyx_glimpse::config::Paths::discover();
    let args: Vec<_> = std::env::args().skip(1).collect();
    if let Err(e) = nyx_glimpse::settings::dispatch(&p, &args) {
        eprintln!("{e:#}");
        std::process::exit(1);
    }
}
