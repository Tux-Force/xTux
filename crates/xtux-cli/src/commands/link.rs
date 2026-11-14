use std::path::Path;
use std::process::ExitCode;

pub fn run(game_dir: String, dest: Option<String>) -> ExitCode {
    match xtux_games::link::create_links(Path::new(&game_dir), dest.as_deref().map(Path::new)) {
        Ok(links) => {
            println!("{}", links.display());
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}
