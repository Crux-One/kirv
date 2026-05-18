use kirv::service;

fn main() {
    if let Err(err) = service::run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}
