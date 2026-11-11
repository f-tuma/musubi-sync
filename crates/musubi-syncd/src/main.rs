//! `musubi-syncd`: the server binary.
//!
//! ```text
//! musubi-syncd serve   --config <file>
//! musubi-syncd backup  --config <file> --to <dir>     (node running)
//! musubi-syncd restore --config <file> --from <dir>   (node stopped)
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

use musubi_syncd::App;
use musubi_syncd::config::Config;

const USAGE: &str = "usage: musubi-syncd serve --config <file>
       musubi-syncd backup --config <file> --to <dir>
       musubi-syncd restore --config <file> --from <dir>";

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("musubi-syncd: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().ok_or(USAGE)?;
    let (mut config, mut dir) = (None, None);
    while let Some(flag) = args.next() {
        let v = args.next().ok_or(USAGE)?;
        match (cmd.as_str(), flag.as_str()) {
            (_, "--config") => config = Some(PathBuf::from(v)),
            ("backup", "--to") | ("restore", "--from") => dir = Some(PathBuf::from(v)),
            _ => return Err(USAGE.into()),
        }
    }
    let config = Config::load(&config.ok_or(USAGE)?)?;
    let rt = || {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())
    };
    match (cmd.as_str(), dir) {
        ("serve", None) => rt()?.block_on(serve(config)),
        ("backup", Some(to)) => rt()?.block_on(musubi_syncd::backup(&config, &to)),
        ("restore", Some(from)) => musubi_syncd::restore(&config, &from),
        _ => Err(USAGE.into()),
    }
}

async fn serve(c: Config) -> Result<(), String> {
    let app = App::open(&c)?;
    // The scheduler runs until this handle drops at shutdown.
    let _engine = app.engine().start();
    let listener = tokio::net::TcpListener::bind(c.listen)
        .await
        .map_err(|e| format!("{}: {e}", c.listen))?;
    eprintln!("musubi-syncd: listening on {}", c.listen);
    axum::serve(listener, app.router())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|e| e.to_string())
}
