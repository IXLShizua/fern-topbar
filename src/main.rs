mod alerts;
mod app;
mod backend;
mod config;
mod features;
mod logging;
mod runtime;
mod ui;

fn main() -> std::process::ExitCode {
    logging::load();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting topbar");

    match app::run() {
        Ok(()) => {
            tracing::info!("topbar stopped");

            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            tracing::error!(%error, "topbar failed");

            std::process::ExitCode::FAILURE
        }
    }
}
