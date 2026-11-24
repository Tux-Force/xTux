use std::process::ExitCode;

use xodus::api::live::ExchangeTokenResponseOutcome;
use xodus::api::live::rst::RSTError;
use xodus::models::live::DAProperty;
use xodus::models::{secrets, soap};
use xodus::tokens::TokenManager;

use crate::webview;

const CLIENT_ID: &str = "000000004424da1f";
const LOGIN_MARKET: &str = "en-US";
const USER_AUTH_SCOPE: &str = "scope=service::user.auth.xboxlive.com::MBI_SSL&api-version=2.0";

pub async fn run(client: &reqwest::Client, tokens: &TokenManager) -> ExitCode {
    let token = tokens.get_device_sts_token().unwrap();
    let secrets::Token::Legacy(token) = token else {
        eprintln!("Invalid STS token");
        return ExitCode::FAILURE;
    };
    let handler = LoginHandler::new(client.clone(), token, tokens.clone());
    let output = webview::run_sessions(handler)
        .expect("failed to login")
        .flatten();
    match output {
        Some(resp) => resp.refresh_user(tokens).expect("user tokens to be saved"),
        None => {
            eprintln!("Didn't log in");
            return ExitCode::FAILURE;
        }
    };

    ExitCode::SUCCESS
}

struct LoginHandler {
    client: reqwest::Client,
    device: xodus::models::secrets::LegacyToken,
    client_id: String,
    finish: bool,
    tokens: TokenManager,
}

impl LoginHandler {
    fn new(
        client: reqwest::Client,
        device: xodus::models::secrets::LegacyToken,
        tokens: TokenManager,
    ) -> Self {
        Self {
            client,
            device,
            client_id: CLIENT_ID.to_string(),
            finish: false,
            tokens,
        }
    }

    fn exchange_user_token(
        &self,
        prop: DAProperty,
    ) -> Result<soap::RequestSecurityTokenResponseCollection, RSTError> {
        let client = self.client.clone();
        let device_token = self.device.clone();
        let client_id = self.client_id.clone();
        let username = prop.username;
        let inline_ft = prop.sts_inline_flow_token;
        let user_token = xodus::models::secrets::LegacyToken {
            key_name: None,
            token: prop.da_token,
            binary_secret: None,
            tpm_key: None,
            lifetime: soap::Timestamp {
                id: None,
                created: prop.da_start_time,
                expires: prop.da_expires,
            },
        };

        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let mut scopes = vec![(
                    USER_AUTH_SCOPE.to_string(),
                    Some(soap::PolicyReference::token_broker()),
                )];

                if self.finish {
                    scopes.push(("http://Passport.NET/tb".to_string(), None));
                }

                xodus::api::live::exchange_user_token(
                    &client,
                    user_token,
                    username,
                    device_token,
                    Some(inline_ft),
                    None,
                    client_id,
                    &scopes,
                )
                .await
            })
        })
    }
}

impl webview::SessionHandler for LoginHandler {
    type Output = Option<soap::RequestSecurityTokenResponseCollection>;

    fn bootstrap(
        &mut self,
        runtime: &mut webview::RuntimeCommands,
    ) -> Result<(), Box<dyn std::error::Error>> {
        runtime.open_session(webview::login_request(
            self.client_id.clone(),
            LOGIN_MARKET.to_string(),
        ));
        Ok(())
    }

    fn on_token(
        &mut self,
        session_id: webview::SessionId,
        data: DAProperty,
        runtime: &mut webview::RuntimeCommands,
    ) -> Result<webview::HandlerControl<Self::Output>, Box<dyn std::error::Error>> {
        match self.exchange_user_token(data.clone()) {
            Err(RSTError::InteractionRequired { auth_url, .. }) => {
                runtime.close_session(session_id);
                self.finish = true;
                runtime.open_session(webview::finalize_request(auth_url));
                Ok(webview::HandlerControl::Continue)
            }
            Err(RSTError::TokenFault(pp)) => {
                if let Some(pp) = pp
                    && let Some(auth_url) = pp.inline_auth_url
                {
                    runtime.close_session(session_id);
                    self.finish = true;
                    runtime.open_session(webview::finalize_request(auth_url));
                    return Ok(webview::HandlerControl::Continue);
                }

                println!("User token exchange returned a fault without inline auth");
                runtime.close_session(session_id);
                Ok(webview::HandlerControl::Complete(None))
            }
            Ok(exchanged) => {
                runtime.close_session(session_id);
                self.tokens
                    .save_user(&xodus::models::secrets::User {
                        puid: data.puid,
                        username: data.username,
                    })
                    .unwrap();
                Ok(webview::HandlerControl::Complete(Some(exchanged)))
            }
            Err(e) => Err(Box::new(e)),
        }
    }

    fn on_closed(
        &mut self,
        _session_id: webview::SessionId,
        _runtime: &mut webview::RuntimeCommands,
    ) -> Result<webview::HandlerControl<Self::Output>, Box<dyn std::error::Error>> {
        Ok(webview::HandlerControl::Complete(None))
    }
}

/// `xtux-cli login --game <game>`: some games need the sign-in confirmed once in a browser (Microsoft asks whether
/// the game may use the account) before they get Xbox Live tokens. Windows shows that page when the game starts;
/// here it opens in the sign-in window.
pub async fn run_game(client: &reqwest::Client, tokens: &TokenManager, source: &str) -> ExitCode {
    let game = match find_game(source) {
        Ok(game) => game,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    let (Ok(secrets::Token::Legacy(device)), Ok(secrets::Token::Legacy(user))) =
        (tokens.get_device_sts_token(), tokens.get_user_sts_token())
    else {
        eprintln!("Not signed in: run `xtux-cli login` first");
        return ExitCode::FAILURE;
    };
    let scopes = game_scopes(&game.msa_app_id);
    let silent = xodus::api::live::exchange_user_token(
        client,
        user,
        "USERNAME".to_string(),
        device.clone(),
        None,
        Some("Silent".to_string()),
        game.msa_app_id.clone(),
        &scopes,
    )
    .await;
    let auth_url = match silent {
        Ok(_) => {
            println!("{}: the sign-in is already confirmed.", game.name);
            return ExitCode::SUCCESS;
        }
        Err(RSTError::InteractionRequired { auth_url, .. }) => auth_url,
        Err(err) => {
            eprintln!("Xbox Live sign-in for {} failed: {err}", game.name);
            return ExitCode::FAILURE;
        }
    };
    println!(
        "{}: Microsoft asks you to confirm the sign-in for this game once.",
        game.name
    );
    let handler = GameConsentHandler {
        client: client.clone(),
        device,
        client_id: game.msa_app_id.clone(),
        auth_url: Some(auth_url),
        attempts: 0,
    };
    let output = match webview::run_sessions(handler) {
        Ok(output) => output.flatten(),
        Err(err) => {
            eprintln!("The sign-in window failed: {err}");
            return ExitCode::FAILURE;
        }
    };
    let Some(resp) = output else {
        eprintln!("Not confirmed: the window was closed first.");
        return ExitCode::FAILURE;
    };
    if let Err(err) = resp.refresh_user(tokens) {
        eprintln!("Could not save the new sign-in: {err}");
        return ExitCode::FAILURE;
    }
    // The confirmation is kept by Microsoft; check that the game's own sign-in works now.
    match xodus::auth::TitleSession::establish(
        client,
        tokens,
        &game.msa_app_id,
        i64::from(game.title_id),
    )
    .await
    {
        Ok(_) => {
            println!(
                "{}: confirmed, Xbox Live works for the game now.",
                game.name
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!(
                "Confirmed, but the sign-in for {} still fails: {err}",
                game.name
            );
            ExitCode::FAILURE
        }
    }
}

/// The game by name or folder, or by its title id as the bridge reports it (8 hex digits).
pub(crate) fn find_game(source: &str) -> Result<xtux_games::cloudsave::Game, String> {
    let title = source.strip_prefix("title ").unwrap_or(source);
    if title.len() == 8
        && let Ok(title_id) = u32::from_str_radix(title, 16)
        && let Some(game) = super::list::games()
            .iter()
            .filter_map(|g| xtux_games::cloudsave::Game::for_folder(&g.path))
            .find(|g| g.title_id == title_id)
    {
        return Ok(game);
    }
    let dir = super::list::resolve(source)?;
    xtux_games::cloudsave::Game::for_folder(&dir).ok_or_else(|| {
        format!("{source}: the game has no Xbox Live identity (MSA app id and title id)")
    })
}

fn game_scopes(client_id: &str) -> Vec<(String, Option<soap::PolicyReference>)> {
    vec![
        (
            format!("scope=xboxlive.signin&api-version=2.0&clientid={client_id}"),
            Some(soap::PolicyReference::token_broker()),
        ),
        ("http://Passport.NET/tb".to_string(), None),
    ]
}

struct GameConsentHandler {
    client: reqwest::Client,
    device: xodus::models::secrets::LegacyToken,
    client_id: String,
    /// The one time confirmation page Microsoft named.
    auth_url: Option<String>,
    attempts: u32,
}

impl webview::SessionHandler for GameConsentHandler {
    type Output = Option<soap::RequestSecurityTokenResponseCollection>;

    fn bootstrap(
        &mut self,
        runtime: &mut webview::RuntimeCommands,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(url) = self.auth_url.take() {
            runtime.open_session(webview::finalize_request(url));
        }
        Ok(())
    }

    fn on_token(
        &mut self,
        session_id: webview::SessionId,
        data: DAProperty,
        runtime: &mut webview::RuntimeCommands,
    ) -> Result<webview::HandlerControl<Self::Output>, Box<dyn std::error::Error>> {
        let user_token = xodus::models::secrets::LegacyToken {
            key_name: None,
            token: data.da_token,
            binary_secret: None,
            tpm_key: None,
            lifetime: soap::Timestamp {
                id: None,
                created: data.da_start_time,
                expires: data.da_expires,
            },
        };
        let (client, device, client_id) = (
            self.client.clone(),
            self.device.clone(),
            self.client_id.clone(),
        );
        let username = data.username;
        let inline_ft = data.sts_inline_flow_token;
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let scopes = game_scopes(&client_id);
                xodus::api::live::exchange_user_token(
                    &client,
                    user_token,
                    username,
                    device,
                    Some(inline_ft),
                    None,
                    client_id,
                    &scopes,
                )
                .await
            })
        });
        runtime.close_session(session_id);
        match result {
            Ok(exchanged) => Ok(webview::HandlerControl::Complete(Some(exchanged))),
            // Microsoft can ask for one more page (for example the account's password); at most a few.
            Err(RSTError::InteractionRequired { auth_url, .. }) if self.attempts < 3 => {
                self.attempts += 1;
                runtime.open_session(webview::finalize_request(auth_url));
                Ok(webview::HandlerControl::Continue)
            }
            Err(err) => Err(Box::new(err)),
        }
    }

    fn on_closed(
        &mut self,
        _session_id: webview::SessionId,
        _runtime: &mut webview::RuntimeCommands,
    ) -> Result<webview::HandlerControl<Self::Output>, Box<dyn std::error::Error>> {
        Ok(webview::HandlerControl::Complete(None))
    }
}
