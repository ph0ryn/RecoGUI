use std::path::PathBuf;

fn main() {
    let mut arguments = std::env::args().skip(1);
    let mode = arguments.next().unwrap_or_else(|| "--write".into());
    let path = arguments
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("../src/generated/bindings.ts"));
    if arguments.next().is_some() {
        fail("usage: generate-bindings [--write|--check] [path]");
    }
    let result = match mode.as_str() {
        "--write" => reco_gui_lib::bindings::write_typescript(&path),
        "--check" => reco_gui_lib::bindings::check_typescript(&path),
        _ => Err("mode must be --write or --check".into()),
    };
    if let Err(error) = result {
        fail(&error);
    }
}

fn fail(message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(1);
}
