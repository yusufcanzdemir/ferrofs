//! Windows service integration (SPEC §3.1): install/uninstall and the service main.

use std::ffi::OsString;
use std::time::Duration;

use anyhow::{Context, Result};
use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

use ferro_ipc::{PIPE_NAME, PipeServer};

pub const SERVICE_NAME: &str = "ferrofs";
const DISPLAY_NAME: &str = "ferrofs indexing service";
const DESCRIPTION: &str = "Indexes NTFS volumes from the MFT and USN journal and answers \
                           file search requests from ferrofs apps.";

define_windows_service!(ffi_service_main, service_main);

/// Hands the process to the service control manager. Only works when started by the SCM.
pub fn run_dispatcher() -> Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .context("not started by the service control manager (use `run --console` to debug)")
}

fn service_main(_arguments: Vec<OsString>) {
    if let Err(err) = run_service() {
        tracing::error!("service failed: {err:#}");
    }
}

fn run_service() -> Result<()> {
    let server = PipeServer::new(PIPE_NAME)?;
    let stop = server.stop_signal();
    let status = service_control_handler::register(SERVICE_NAME, move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            tracing::info!(?control, "stop requested");
            stop.stop();
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;

    let report = |state, accept, wait_hint| {
        status.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accept,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint,
            process_id: None,
        })
    };
    report(
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        Duration::default(),
    )?;
    tracing::info!("service running");

    let result = crate::run(&server);

    report(
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        Duration::default(),
    )?;
    tracing::info!("service stopped");
    result
}

/// Registers the service (LocalSystem, automatic delayed start) and starts it.
pub fn install() -> Result<()> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .context("cannot open the service manager (run from an elevated shell)")?;
    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: DISPLAY_NAME.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: std::env::current_exe()?,
        launch_arguments: Vec::new(),
        dependencies: Vec::new(),
        account_name: None, // LocalSystem
        account_password: None,
    };
    let service = manager
        .create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)
        .context("cannot create the service")?;
    service.set_description(DESCRIPTION)?;
    service.set_delayed_auto_start(true)?;
    service
        .start::<&str>(&[])
        .context("service created but did not start")?;
    println!("Installed and started the `{SERVICE_NAME}` service.");
    Ok(())
}

/// Stops (if running) and removes the service.
pub fn uninstall() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("cannot open the service manager (run from an elevated shell)")?;
    let service = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
        )
        .context("the service is not installed")?;
    if service.query_status()?.current_state != ServiceState::Stopped {
        service.stop()?;
        for _ in 0..100 {
            if service.query_status()?.current_state == ServiceState::Stopped {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    service.delete()?;
    println!("Removed the `{SERVICE_NAME}` service.");
    Ok(())
}
