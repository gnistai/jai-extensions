//! `jai token`: prints the server URL and token, for clients of the HTTP API
//! (OpenAI-style clients, `node bin/jai.mjs --local`).

use std::process::ExitCode;

use jai_core::token;
use jai_server::discovery;

use crate::settings;
use crate::TokenArgs;

pub async fn run(args: TokenArgs) -> anyhow::Result<ExitCode> {
    let paths = super::paths()?;
    let running = discovery::find(&paths).await;
    let token = if args.regenerate {
        if let Some((info, _)) = &running {
            // The running server keeps the token in memory; changing only the
            // file would lock every client out until it restarts.
            anyhow::bail!(
                "a JAI server is running (pid {}, port {}): regenerate the token in JAI Studio's \
                 Server tab, or stop the server first",
                info.pid,
                info.port
            );
        }
        token::regenerate_token(&paths)?
    } else {
        match &running {
            Some((_, t)) => t.clone(),
            None => token::load_or_create_token(&paths)?,
        }
    };
    let url = match &running {
        Some((info, _)) => settings::client_url(&info.host, info.port),
        None => {
            let cfg = settings::load_config(&paths);
            errln!("No server is running: start JAI Studio or run `jai serve`.");
            settings::client_url(&cfg.server.host, cfg.server.port)
        }
    };
    outln!("URL:   {url}");
    outln!("Token: {token}");
    if args.regenerate {
        errln!("Clients that use the server need the new token.");
    }
    Ok(ExitCode::SUCCESS)
}
