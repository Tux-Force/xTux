//! Anti-cheat reports of AreWeAntiCheatYet (areweanticheatyet.com), a community list of games with anti-cheat and
//! whether they run on Linux.
//!
//! The list names games as Steam and Epic do and knows no Store IDs, so a game is found by its name. Store names
//! differ in small ways (`Forza Horizon 5 Standard Edition`, `Tom Clancy's Rainbow Six® Siege`, `Grand Theft Auto V`
//! against `Grand Theft Auto 5`), so every report says how likely it is that it belongs to the game: 100 for the same
//! name, less the more the names differ. Games whose numbers differ (sequels) are never matched.
//!
//! The list is public; only the whole file is downloaded, no game names are sent. It is cached for a week in
//! `$XDG_CACHE_HOME/xtux/areweanticheatyet.json`, and an older copy is used when it cannot be refreshed.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

pub const SOURCE: &str = "https://raw.githubusercontent.com/AreWeAntiCheatYet/AreWeAntiCheatYet/refs/heads/master/games.json";
const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Store names that mean another game than the same name in the list, with the list's name for it: on Xbox,
/// "Call of Duty" is the Call of Duty HQ launcher (Black Ops 6, Warzone …), not the game of 2003.
const ALIASES: &[(&str, &str, u8)] = &[("Call of Duty", "Call of Duty: Warzone", 80)];
/// When several games of the list match equally well and say different things, the report is at most this
/// likely.
const AMBIGUOUS_LIKELIHOOD: u8 = 75;

/// Reports below this likelihood are not given.
pub const MIN_LIKELIHOOD: u8 = 60;
/// A blocking report marks a store title as likely Windows only from this likelihood on.
pub const WINDOWS_ONLY_LIKELIHOOD: u8 = 80;

/// What AreWeAntiCheatYet says about a game.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// The game's name in the list, which may differ from the Store's.
    pub name: String,
    /// `Supported`, `Running`, `Planned`, `Broken` or `Denied`.
    pub status: String,
    /// For example `Easy Anti-Cheat`.
    pub anticheats: Vec<String>,
    /// The game's page on areweanticheatyet.com.
    pub url: String,
    /// How likely the report belongs to the game asked for, 60 to 100.
    pub likelihood: u8,
}

impl Report {
    /// The game will not start or play online: the anti-cheat is not enabled for Linux, or the developer refuses it.
    pub fn blocks(&self) -> bool {
        matches!(self.status.as_str(), "Broken" | "Denied")
    }

    /// How sure the names match: `same name` (95 and more), `likely` (80 and more) or `possible`. Only the
    /// names are compared; a Store game with the same name as an older game of the list is still another game.
    pub fn confidence(&self) -> &'static str {
        match self.likelihood {
            95.. => "same name",
            80.. => "likely",
            _ => "possible",
        }
    }

    /// One line for users, for example `Supported (BattlEye), same name match "DayZ" 100%`.
    pub fn summary(&self) -> String {
        let anticheats = if self.anticheats.is_empty() {
            String::new()
        } else {
            format!(" ({})", self.anticheats.join(", "))
        };
        format!(
            "{}{anticheats}, {} match \"{}\" {}%",
            self.status,
            self.confidence(),
            self.name,
            self.likelihood
        )
    }
}

#[derive(Debug, Deserialize)]
struct Game {
    name: String,
    status: String,
    #[serde(default)]
    anticheats: Vec<String>,
    #[serde(default)]
    slug: String,
}

/// A name taken apart for comparing.
#[derive(Debug, Clone, PartialEq)]
struct Key {
    /// Lower case, as written.
    lower: String,
    /// All words, roman numerals as digits.
    words: Vec<String>,
    /// The words without edition and platform words.
    core: Vec<String>,
}

/// Words that only name an edition, a platform or a release state.
const EDITION_WORDS: &[&str] = &[
    "edition",
    "standard",
    "deluxe",
    "ultimate",
    "gold",
    "premium",
    "definitive",
    "complete",
    "goty",
    "year",
    "pc",
    "windows",
    "win10",
    "preview",
    "bundle",
    "cross",
    "gen",
    "xbox",
    "series",
    "launch",
    "early",
    "access",
    "enhanced",
    "directors",
    "cut",
    "digital",
    "elite",
    "special",
    "limited",
    "collectors",
    "anniversary",
    "the",
    "of",
    "for",
    "game",
    "and",
];

fn roman(word: &str) -> Option<&'static str> {
    // "i" is left alone: it is a word as often as a numeral.
    Some(match word {
        "ii" => "2",
        "iii" => "3",
        "iv" => "4",
        "v" => "5",
        "vi" => "6",
        "vii" => "7",
        "viii" => "8",
        "ix" => "9",
        "x" => "10",
        "xi" => "11",
        "xii" => "12",
        "xiii" => "13",
        "xiv" => "14",
        "xv" => "15",
        "xvi" => "16",
        _ => return None,
    })
}

impl Key {
    fn new(name: &str) -> Self {
        let lower = name.trim().to_lowercase();
        // Parentheses hold notes such as "(PC)" or "(Game Preview)".
        let mut plain = String::new();
        let mut depth = 0usize;
        for c in lower.chars() {
            match c {
                '(' | '[' => depth += 1,
                ')' | ']' => depth = depth.saturating_sub(1),
                _ if depth > 0 => {}
                '&' => plain.push_str(" and "),
                '\'' | '’' | '™' | '®' | '©' => {}
                c if c.is_alphanumeric() => plain.push(c),
                _ => plain.push(' '),
            }
        }
        let raw: Vec<&str> = plain.split_whitespace().collect();
        // "Xbox Series X|S" names the console, not a sequel.
        let mut words = Vec::with_capacity(raw.len());
        let mut core = Vec::with_capacity(raw.len());
        for (i, word) in raw.iter().enumerate() {
            let console = (matches!(*word, "x" | "s")
                && (raw[..i].ends_with(&["series"]) || raw[..i].ends_with(&["series", "x"])))
                || (*word == "one" && raw[..i].ends_with(&["xbox"]));
            let word = if console {
                word.to_string()
            } else {
                roman(word)
                    .map(str::to_string)
                    .unwrap_or_else(|| word.to_string())
            };
            // A year or number in front of "Edition" names the edition ("2025 Edition").
            let edition_number =
                raw.get(i + 1) == Some(&"edition") && word.chars().all(|c| c.is_ascii_digit());
            if !console && !edition_number && !EDITION_WORDS.contains(&word.as_str()) {
                core.push(word.clone());
            }
            words.push(word);
        }
        Key { lower, words, core }
    }

    fn numbers(&self) -> Vec<&str> {
        self.core
            .iter()
            .filter(|w| w.chars().all(|c| c.is_ascii_digit()))
            .map(String::as_str)
            .collect()
    }
}

/// How likely `query` (a Store name) and `entry` (a name of the list) name the same game, 0 to 100.
fn likelihood(query: &Key, entry: &Key) -> u8 {
    if query.lower == entry.lower {
        return 100;
    }
    if query.words == entry.words {
        return 95;
    }
    if query.core.is_empty() || entry.core.is_empty() {
        return 0;
    }
    // Different numbers are different games: "Battlefield 1" is not "Battlefield 4", nor "Planet Coaster 2"
    // "Planet Coaster".
    if query.numbers() != entry.numbers() {
        return 0;
    }
    if query.core == entry.core {
        return 90;
    }
    // The Store name adds words the list lacks: an expansion or remaster of the listed game ("Assassin's Creed III
    // Remastered") or another game of a series ("Far Cry New Dawn"), so the more words are added the less likely.
    // A shorter Store name ("Quake" against "Quake Live") is the start of a series, not the listed game.
    if query.core.starts_with(&entry.core) {
        return (40 + 30 * entry.core.len() / query.core.len()) as u8;
    }
    let common = entry.core.iter().filter(|w| query.core.contains(w)).count();
    (2 * common * 70 / (query.core.len() + entry.core.len())) as u8
}

struct Entry {
    game: Game,
    key: Key,
}

/// The list, ready for lookups.
pub struct Database {
    entries: Vec<Entry>,
}

impl Database {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let games: Vec<Game> = serde_json::from_slice(bytes)
            .map_err(|e| Error::msg(format!("unexpected AreWeAntiCheatYet list: {e}")))?;
        Ok(Database {
            entries: games
                .into_iter()
                .map(|game| Entry {
                    key: Key::new(&game.name),
                    game,
                })
                .collect(),
        })
    }

    /// The cached list, without network; `None` before the first download.
    pub fn cached() -> Option<Self> {
        Self::parse(&std::fs::read(cache_file()?).ok()?).ok()
    }

    /// The list: from the cache when it is younger than a week and `refresh` is false, else downloaded. When the
    /// download fails an older cached copy is used.
    pub async fn load(client: &reqwest::Client, refresh: bool) -> Result<Self> {
        let file = cache_file();
        let fresh = file
            .as_ref()
            .and_then(|f| f.metadata().ok()?.modified().ok())
            .and_then(|m| SystemTime::now().duration_since(m).ok())
            .is_some_and(|age| age < MAX_AGE);
        if fresh
            && !refresh
            && let Some(db) = Self::cached()
        {
            return Ok(db);
        }
        match download(client).await {
            Ok(bytes) => {
                let db = Self::parse(&bytes)?;
                if let Some(file) = file {
                    write_cache(&file, &bytes);
                }
                Ok(db)
            }
            Err(err) => {
                tracing::warn!("AreWeAntiCheatYet list not refreshed: {err}");
                Self::cached().ok_or(err)
            }
        }
    }

    /// The report of the game named `name`, when one matches with at least `MIN_LIKELIHOOD`.
    pub fn find(&self, name: &str) -> Option<Report> {
        let key = Key::new(name);
        if let Some((_, listed, likelihood)) = ALIASES
            .iter()
            .find(|(store, _, _)| Key::new(store).words == key.words)
        {
            let entry = self.entries.iter().find(|e| e.game.name == *listed)?;
            return Some(report(entry, *likelihood));
        }
        let scored: Vec<(&Entry, u8)> = self
            .entries
            .iter()
            .map(|e| (e, likelihood(&key, &e.key)))
            .filter(|(_, l)| *l >= MIN_LIKELIHOOD)
            .collect();
        let best = scored.iter().map(|(_, l)| *l).max()?;
        let tied: Vec<&Entry> = scored
            .iter()
            .filter(|(_, l)| *l == best)
            .map(|(e, _)| *e)
            .collect();
        // A note in parentheses other than a year marks a variant of the game on another platform ("Valorant
        // (FACEIT)", "SMITE 2 (ChallengerMode)"), not what the Store sells.
        let plain: Vec<&Entry> = tied.iter().copied().filter(|e| !variant(&e.key)).collect();
        let tied = if plain.is_empty() { tied } else { plain };
        // Games of the same name are told apart by a year in the list ("Modern Warfare II (2022)"); the Store sells
        // the newer ones. Of entries without a year the first in the list.
        let entry = tied
            .iter()
            .copied()
            .fold(None, |pick: Option<&Entry>, e| match pick {
                Some(p) if year(&p.key) >= year(&e.key) => Some(p),
                _ => Some(e),
            })?;
        let differ = tied.iter().any(|e| {
            e.game.status != entry.game.status || e.game.anticheats != entry.game.anticheats
        });
        Some(report(
            entry,
            if differ {
                best.min(AMBIGUOUS_LIKELIHOOD)
            } else {
                best
            },
        ))
    }
}

/// The list's name has a note in parentheses that is not a year.
fn variant(key: &Key) -> bool {
    key.lower
        .split('(')
        .skip(1)
        .filter_map(|part| part.split(')').next())
        .any(|note| note.trim().parse::<u16>().is_err())
}

/// A year in parentheses in the list's name, for example 2022 of "Call of Duty: Modern Warfare II (2022)".
fn year(key: &Key) -> u16 {
    key.lower
        .split(['(', ')'])
        .filter_map(|part| part.trim().parse::<u16>().ok())
        .filter(|y| (1970..=2100).contains(y))
        .max()
        .unwrap_or(0)
}

fn report(entry: &Entry, likelihood: u8) -> Report {
    Report {
        name: entry.game.name.clone(),
        status: entry.game.status.clone(),
        anticheats: entry.game.anticheats.clone(),
        url: page_url(&entry.game.slug),
        likelihood,
    }
}

/// The game's page; empty when the slug is not plain letters, digits and dashes (it is opened in a browser).
fn page_url(slug: &str) -> String {
    let plain = !slug.is_empty()
        && slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if plain {
        format!("https://areweanticheatyet.com/game/{slug}")
    } else {
        String::new()
    }
}

async fn download(client: &reqwest::Client) -> Result<Vec<u8>> {
    crate::catalog::body(client.get(SOURCE).send().await?).await
}

/// `$XDG_CACHE_HOME/xtux/areweanticheatyet.json`.
fn cache_file() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("xtux/areweanticheatyet.json"))
}

fn write_cache(file: &std::path::Path, bytes: &[u8]) {
    let Some(dir) = file.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    if let Ok(mut temp) = tempfile::NamedTempFile::new_in(dir) {
        use std::io::Write;
        if temp.write_all(bytes).is_ok() {
            let _ = temp.persist(file);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Database {
        Database::parse(
            r#"[
            {"name":"DayZ","status":"Supported","anticheats":["BattlEye"],"slug":"dayz"},
            {"name":"Grand Theft Auto 5","status":"Denied","anticheats":["BattlEye"],"slug":"gta-v"},
            {"name":"Tom Clancy's Rainbow Six® Siege","status":"Denied","anticheats":["BattlEye"],"slug":"r6"},
            {"name":"Battlefield 4","status":"Broken","anticheats":["PunkBuster"],"slug":"bf4"},
            {"name":"Halo Infinite","status":"Running","anticheats":["Easy Anti-Cheat"],"slug":"halo-infinite"},
            {"name":"Forza Horizon 5","status":"Supported","anticheats":[],"slug":"fh5"},
            {"name":"Planet Coaster","status":"Running","anticheats":[],"slug":"pc"},
            {"name":"Call of Duty","status":"Supported","anticheats":["PunkBuster"],"slug":"cod"},
            {"name":"Call of Duty: Modern Warfare 2","status":"Supported","anticheats":["VAC"],"slug":"mw2"},
            {"name":"Call of Duty: Modern Warfare II (2022)","status":"Broken","anticheats":["RICOCHET"],"slug":"mw2-2022"},
            {"name":"Call of Duty: Warzone","status":"Broken","anticheats":["RICOCHET"],"slug":"wz"},
            {"name":"Valorant","status":"Denied","anticheats":["Vanguard"],"slug":"valorant"},
            {"name":"Valorant (FACEIT)","status":"Broken","anticheats":["FACEIT"],"slug":"valorant-faceit"}
        ]"#
            .as_bytes(),
        )
        .unwrap()
    }

    #[test]
    fn same_name_is_certain() {
        let report = db().find("DayZ").unwrap();
        assert_eq!(report.likelihood, 100);
        assert_eq!(report.url, "https://areweanticheatyet.com/game/dayz");
        assert_eq!(page_url("a b;rm"), "");
        assert!(!report.blocks());
    }

    #[test]
    fn roman_numerals_and_symbols() {
        let report = db().find("Grand Theft Auto V").unwrap();
        assert_eq!(report.name, "Grand Theft Auto 5");
        assert_eq!(report.likelihood, 95);
        assert!(report.blocks());
        assert_eq!(
            db().find("Tom Clancy’s Rainbow Six Siege")
                .unwrap()
                .likelihood,
            95
        );
    }

    #[test]
    fn edition_names_are_likely() {
        let report = db().find("Forza Horizon 5 Standard Edition").unwrap();
        assert_eq!(report.likelihood, 90);
        assert_eq!(report.confidence(), "likely");
        assert_eq!(db().find("Forza Horizon 5 (PC)").unwrap().likelihood, 95);
        assert_eq!(
            db().find("Halo Infinite: 2025 Edition").unwrap().likelihood,
            90
        );
    }

    #[test]
    fn subtitles_are_possible() {
        let report = db().find("Halo Infinite Campaign").unwrap();
        assert!(
            (60..80).contains(&report.likelihood),
            "{}",
            report.likelihood
        );
        assert_eq!(report.confidence(), "possible");
        // Another game of the series, or only the series' name.
        assert_eq!(
            db().find("Halo Infinite Campaign Multiplayer Bundle Forge"),
            None
        );
        assert_eq!(db().find("Halo"), None);
    }

    #[test]
    fn sequels_and_strangers_do_not_match() {
        assert_eq!(db().find("Battlefield 1"), None);
        assert_eq!(db().find("Planet Coaster 2"), None);
        assert_eq!(db().find("Microsoft Flight Simulator 2024"), None);
        assert_eq!(db().find("Forza Motorsport"), None);
    }

    #[test]
    fn same_names_prefer_the_newer_game_and_are_less_sure() {
        let report = db().find("Call of Duty®: Modern Warfare® II").unwrap();
        assert_eq!(report.name, "Call of Duty: Modern Warfare II (2022)");
        assert_eq!(report.likelihood, 75);
        assert!(report.blocks());
    }

    #[test]
    fn platform_variants_are_not_the_store_game() {
        let report = db().find("VALORANT PC").unwrap();
        assert_eq!(report.name, "Valorant");
        assert_eq!(report.likelihood, 90);
    }

    #[test]
    fn xbox_names_of_other_games() {
        let report = db().find("Call of Duty®").unwrap();
        assert_eq!(report.name, "Call of Duty: Warzone");
        assert_eq!(report.likelihood, 80);
    }

    #[test]
    fn console_names_are_not_numerals() {
        assert_eq!(
            db().find("Forza Horizon 5 Xbox Series X|S")
                .unwrap()
                .likelihood,
            90
        );
        assert_eq!(db().find("DayZ Xbox One").unwrap().likelihood, 90);
    }
}
