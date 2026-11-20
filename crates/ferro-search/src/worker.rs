//! Background thread that talks to the service so the UI never blocks on the pipe.
//! Only the newest pending job is executed; older ones are dropped (SPEC §4: cancel
//! in-flight queries on new input).

use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use eframe::egui;
use ferro_ipc::{
    Client, ErrorCode, PIPE_NAME, Request, Response, SearchRequest, SearchResponse, ServerTrust,
};

/// Set to `1` to accept a service that is not running as a Windows service
/// (e.g. `ferro-service run --console` or ferro-service's `fake_service` example) while developing.
const DEV_TRUST_ENV: &str = "FERROFS_DEV_ANY_SERVER";

#[derive(Debug)]
pub struct Job {
    pub generation: u64,
    pub request: SearchRequest,
    /// Next page of the same search rather than a new search.
    pub append: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceState {
    Connected,
    NotRunning,
    Unavailable(String),
}

#[derive(Debug)]
pub struct Outcome {
    pub generation: u64,
    pub append: bool,
    pub service: ServiceState,
    /// `Err` holds a message for the status line (bad query, transport error).
    pub result: Result<SearchResponse, String>,
}

pub fn spawn(ctx: egui::Context) -> std::io::Result<(Sender<Job>, Receiver<Outcome>)> {
    let (job_tx, job_rx) = channel::<Job>();
    let (out_tx, out_rx) = channel::<Outcome>();
    std::thread::Builder::new()
        .name("search-worker".into())
        .spawn(move || {
            let trust = if std::env::var(DEV_TRUST_ENV).as_deref() == Ok("1") {
                ServerTrust::Any
            } else {
                ServerTrust::ServiceSession
            };
            let mut client: Option<Client> = None;
            while let Ok(mut job) = job_rx.recv() {
                while let Ok(newer) = job_rx.try_recv() {
                    job = newer;
                }
                let outcome = run(&mut client, trust, job);
                if out_tx.send(outcome).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        })?;
    Ok((job_tx, out_rx))
}

fn run(client: &mut Option<Client>, trust: ServerTrust, job: Job) -> Outcome {
    let outcome = |service, result| Outcome {
        generation: job.generation,
        append: job.append,
        service,
        result,
    };
    // One retry: the service may have restarted since the last request.
    for _ in 0..2 {
        if client.is_none() {
            match Client::connect(PIPE_NAME, "ferro-search", Duration::from_secs(2), trust) {
                Ok(connected) => *client = Some(connected),
                Err(ferro_ipc::Error::NotRunning) => {
                    return outcome(ServiceState::NotRunning, Err("service not running".into()));
                }
                Err(err) => {
                    tracing::warn!("cannot connect to the service: {err}");
                    return outcome(
                        ServiceState::Unavailable(err.to_string()),
                        Err(err.to_string()),
                    );
                }
            }
        }
        let Some(connected) = client.as_mut() else {
            continue;
        };
        match connected.request(&Request::Search(job.request.clone())) {
            Ok(Response::Search(result)) => return outcome(ServiceState::Connected, Ok(result)),
            Ok(Response::Error { code, message }) => {
                let message = match code {
                    ErrorCode::BadQuery => message,
                    other => format!("{other:?}: {message}"),
                };
                return outcome(ServiceState::Connected, Err(message));
            }
            Ok(other) => {
                return outcome(
                    ServiceState::Connected,
                    Err(format!("unexpected response {other:?}")),
                );
            }
            Err(err) => {
                tracing::info!("request failed, reconnecting: {err}");
                *client = None;
            }
        }
    }
    outcome(
        ServiceState::NotRunning,
        Err("lost the connection to the service".into()),
    )
}
