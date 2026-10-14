fn main() {
    nixe_core::app::init(nixe_core::app::App { slug: "nixe-glimpse", name: "Glimpse" });
    let p = nixe_glimpse::config::Paths::discover();
    let args: Vec<_> = std::env::args().skip(1).collect();
    if let Err(e) = nixe_glimpse::settings::dispatch(&p, &args) {
        eprintln!("{e:#}");
        std::process::exit(1);
    }
}
