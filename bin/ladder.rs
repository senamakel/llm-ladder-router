//! The `ladder` proxy binary.
//!
//! Deliberately a shell: every decision it makes lives in
//! [`llm_ladder_router::cli`], where it is unit-tested. See that module for why
//! this file sits outside `src/`.

#[tokio::main]
async fn main() -> std::process::ExitCode {
    // A local `.env` is a convenience for development; real deployments set the
    // variables themselves, so a missing file is not an error.
    let _ = dotenvy::dotenv();
    llm_ladder_router::cli::init_tracing();

    match llm_ladder_router::cli::run(std::env::args().skip(1)).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(error = %error, "router stopped");
            std::process::ExitCode::FAILURE
        }
    }
}
