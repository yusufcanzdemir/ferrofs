//! Background thread that talks to the service so the UI never blocks on the pipe. Each
//! tool has its own worker (and connection). Jobs from an older generation (the user
//! navigated or started a new search since) are dropped unexecuted.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use eframe::egui;
use ferro_ipc::{Client, ErrorCode, Request, Response, ServerTrust};

/// Set to `1` to accept a service that is not running as a Windows service
/// (e.g. `ferro-service run --console` or ferro-service's `fake_service` example).
const DEV_TRUST_ENV: &str = "FERROFS_DEV_ANY_SERVER";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobKind {
    /// Load the current location (tree root, treemap, reports).
    Navigate,
    /// Load the children of the folder at this path.
    Expand(String),
    /// Find equal-size duplicate candidates.
    Duplicates,
    /// Find developer artifacts.
    Artifacts,
    /// Size of the well-known cleanup location with this index.
    Location(usize),
    /// Find virtual disk files.
    VirtualDisks,
}

#[derive(Debug)]
pub struct Job {
    pub generation: u64,
    pub kind: JobKind,
    pub request: Request,
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
    pub kind: JobKind,
    pub service: ServiceState,
    /// `Err` holds a message for the status line.
    /// Never `Response::Error`: errors arrive as `Err`.
    pub result: Result<Response, String>,
}

pub fn spawn(ctx: egui::Context) -> std::io::Result<(Sender<Job>, Receiver<Outcome>)> {
    let (job_tx, job_rx) = channel::<Job>();
    let (out_tx, out_rx) = channel::<Outcome>();
    std::thread::Builder::new()
        .name("storage-worker".into())
        .spawn(move || {
            let trust = if std::env::var(DEV_TRUST_ENV).as_deref() == Ok("1") {
                ServerTrust::Any
            } else {
                ServerTrust::ServiceSession
            };
            let mut client: Option<Client> = None;
            while let Ok(first) = job_rx.recv() {
                let mut jobs = vec![first];
                jobs.extend(job_rx.try_iter());
                let newest = jobs.iter().map(|j| j.generation).max().unwrap_or(0);
                for job in jobs.into_iter().filter(|j| j.generation == newest) {
                    let outcome = run(&mut client, trust, job);
                    if out_tx.send(outcome).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
            }
        })?;
    Ok((job_tx, out_rx))
}

fn run(client: &mut Option<Client>, trust: ServerTrust, job: Job) -> Outcome {
    let outcome = |service, result| Outcome {
        generation: job.generation,
        kind: job.kind.clone(),
        service,
        result,
    };
    // One retry: the service may have restarted since the last request.
    for _ in 0..2 {
        if client.is_none() {
            match Client::connect(
                &ferro_ipc::pipe_name(),
                "ferro-storage",
                Duration::from_secs(2),
                trust,
            ) {
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
        match connected.request(&job.request) {
            Ok(Response::Error { code, message }) => {
                let message = match code {
                    ErrorCode::BadQuery | ErrorCode::NotFound | ErrorCode::NotReady => message,
                    other => format!("{other:?}: {message}"),
                };
                return outcome(ServiceState::Connected, Err(message));
            }
            Ok(response) => return outcome(ServiceState::Connected, Ok(response)),
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
