//! `musubi-syncd`: the server binary.
//!
//! ```text
//! musubi-syncd serve   --config <file>
//! musubi-syncd backup  --config <file> --to <dir>     (node running)
//! musubi-syncd restore --config <file> --from <dir>   (node stopped)
//! ```

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use musubi_syncd::App;
use musubi_syncd::config::Config;

const USAGE: &str = "usage: musubi-syncd serve --config <file>
       musubi-syncd backup --config <file> --to <dir>
       musubi-syncd restore --config <file> --from <dir>";

/// How long a stop signal waits for open requests and running work.
const GRACE: Duration = Duration::from_secs(5);

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
        ("serve", None) => {
            let rt = rt()?;
            let r = rt.block_on(serve(config));
            // Work still running (a blocking store call) does not hold the exit.
            rt.shutdown_timeout(GRACE);
            r
        }
        ("backup", Some(to)) => rt()?.block_on(musubi_syncd::backup(&config, &to)),
        ("restore", Some(from)) => musubi_syncd::restore(&config, &from),
        _ => Err(USAGE.into()),
    }
}

async fn serve(c: Config) -> Result<(), String> {
    let app = App::open(&c)?;
    app.engine().catch_up().await.map_err(|e| e.to_string())?;
    // The scheduler runs until this handle drops at shutdown.
    let _engine = app.engine().start();
    // Stored items decoded by an older codec or schema; runs once, in the background.
    let _redecode = app.engine().start_redecode(app.tenants()?);
    let listener = tokio::net::TcpListener::bind(c.listen)
        .await
        .map_err(|e| format!("{}: {e}", c.listen))?;
    let stop = stop_signal()?;
    eprintln!("musubi-syncd: listening on {}", c.listen);
    let (stopping, mut stopped) = tokio::sync::watch::channel(false);
    let server = axum::serve(listener, app.router()).with_graceful_shutdown(async move {
        stop.await;
        eprintln!("musubi-syncd: stopping");
        let _ = stopping.send(true);
    });
    // Open streams (`/v1/stream`) never end by themselves: after the grace
    // period they are cut.
    let cut = async {
        let _ = stopped.wait_for(|s| *s).await;
        tokio::time::sleep(GRACE).await;
    };
    tokio::select! {
        r = server => r.map_err(|e| e.to_string()),
        () = cut => {
            eprintln!("musubi-syncd: open connections cut after {GRACE:?}");
            Ok(())
        }
    }
}

/// SIGINT or SIGTERM. The handlers are installed here, so a signal that
/// arrives while the server starts is not missed.
#[cfg(unix)]
fn stop_signal() -> Result<impl Future<Output = ()>, String> {
    use tokio::signal::unix::{SignalKind, signal};
    let listen = |k| signal(k).map_err(|e| format!("signal handler: {e}"));
    let mut int = listen(SignalKind::interrupt())?;
    let mut term = listen(SignalKind::terminate())?;
    Ok(async move {
        tokio::select! {
            _ = int.recv() => {}
            _ = term.recv() => {}
        }
    })
}

/// Ctrl-C.
#[cfg(not(unix))]
fn stop_signal() -> Result<impl Future<Output = ()>, String> {
    Ok(async {
        let _ = tokio::signal::ctrl_c().await;
    })
}
