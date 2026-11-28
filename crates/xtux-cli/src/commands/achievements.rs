//! `xtux-cli achievements <game>`: the game's Xbox achievements and the user's progress.

use std::process::ExitCode;

use xodus::tokens::TokenManager;
use xtux_games::achievements::Achievements;

pub async fn run(client: &reqwest::Client, tokens: &TokenManager, source: &str) -> ExitCode {
    let result = async {
        let game = super::login::find_game(source)?;
        let list = Achievements::connect(client, tokens, &game)
            .await?
            .list()
            .await?;
        Ok::<_, String>((game, list))
    }
    .await;
    let (game, list) = match result {
        Ok(found) => found,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    if list.achievements.is_empty() {
        println!("{} has no achievements.", game.name);
        return ExitCode::SUCCESS;
    }
    println!(
        "{}: {} of {} achievements, {} of {} G",
        game.name,
        list.unlocked(),
        list.achievements.len(),
        list.gamerscore(),
        list.total_gamerscore()
    );
    for a in &list.achievements {
        let state = match (a.unlocked, a.percent) {
            (true, _) => "[x]".to_string(),
            (false, Some(percent)) => format!("[{percent}%]"),
            (false, None) => "[ ]".to_string(),
        };
        println!("{state:<6} {:>4} G  {}", a.gamerscore, a.name);
        if !a.description.is_empty() {
            println!("               {}", a.description);
        }
    }
    ExitCode::SUCCESS
}
