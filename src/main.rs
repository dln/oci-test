use anyhow::{Context, Result};
use flate2::read::GzDecoder;
use libcontainer::container::builder::ContainerBuilder;
use libcontainer::oci_spec::runtime::{
    LinuxBuilder, LinuxIdMappingBuilder, LinuxNamespace, LinuxNamespaceBuilder, LinuxNamespaceType,
    Mount, Spec,
};
use libcontainer::syscall::syscall::SyscallType;
use libcontainer::workload::{Executor, ExecutorError, ExecutorValidationError};
use nix::{
    sys::{
        signal::{self, kill},
        signalfd::SigSet,
        wait::{waitpid, WaitPidFlag, WaitStatus},
    },
    unistd::Pid,
};
use oci_distribution::client::*;
use oci_distribution::manifest;
use oci_distribution::secrets::RegistryAuth;
use oci_distribution::Reference;
use serde_json::to_writer_pretty;
use std::fs::File;
use std::io::{BufWriter, Cursor, Write};
use std::path::Path;
use std::path::PathBuf;
use tar::Archive;
use tracing_subscriber::prelude::*;

#[derive(Clone)]
pub struct MyExecutor {}

impl Executor for MyExecutor {
    fn exec(&self, spec: &Spec) -> Result<(), ExecutorError> {
        libcontainer::workload::default::get_executor().exec(spec)
    }

    fn validate(&self, spec: &Spec) -> Result<(), ExecutorValidationError> {
        libcontainer::workload::default::get_executor().validate(spec)
    }
}

pub fn get_rootless() -> Result<Spec> {
    // Remove network and user namespace from the default spec
    let mut namespaces: Vec<LinuxNamespace> =
        libcontainer::oci_spec::runtime::get_default_namespaces()
            .into_iter()
            .filter(|ns| {
                ns.typ() != LinuxNamespaceType::Network && ns.typ() != LinuxNamespaceType::User
            })
            .collect();

    // Add user namespace
    namespaces.push(
        LinuxNamespaceBuilder::default()
            .typ(LinuxNamespaceType::User)
            .build()?,
    );

    let uid = nix::unistd::geteuid().as_raw();
    let gid = nix::unistd::getegid().as_raw();

    let linux = LinuxBuilder::default()
        .namespaces(namespaces)
        .uid_mappings(vec![LinuxIdMappingBuilder::default()
            .host_id(uid)
            .container_id(0_u32)
            .size(1_u32)
            .build()?])
        .gid_mappings(vec![LinuxIdMappingBuilder::default()
            .host_id(gid)
            .container_id(0_u32)
            .size(1_u32)
            .build()?])
        .build()?;

    // Prepare the mounts

    let mut mounts: Vec<Mount> = libcontainer::oci_spec::runtime::get_default_mounts();
    for mount in &mut mounts {
        if mount.destination().eq(Path::new("/sys")) {
            mount
                .set_source(Some(PathBuf::from("/sys")))
                .set_typ(Some(String::from("none")))
                .set_options(Some(vec![
                    "rbind".to_string(),
                    "nosuid".to_string(),
                    "noexec".to_string(),
                    "nodev".to_string(),
                    "ro".to_string(),
                ]));
        } else {
            let options: Vec<String> = mount
                .options()
                .as_ref()
                .unwrap_or(&vec![])
                .iter()
                .filter(|&o| !o.starts_with("gid=") && !o.starts_with("uid="))
                .map(|o| o.to_string())
                .collect();
            mount.set_options(Some(options));
        }
    }

    let mut spec = Spec::default();
    spec.set_linux(Some(linux)).set_mounts(Some(mounts));
    Ok(spec)
}

pub fn spec() -> Result<()> {
    tracing::info!("Creating container spec");
    let spec = get_rootless()?;

    // write data to config.json
    let file = File::create("test/config.json")?;
    let mut writer = BufWriter::new(file);
    to_writer_pretty(&mut writer, &spec)?;
    writer.flush()?;
    Ok(())
}

#[tracing::instrument()]
async fn pull_image(image: &str) -> Result<ImageData, Box<dyn std::error::Error>> {
    let reference = Reference::try_from(image)?;
    let auth = RegistryAuth::Anonymous;
    let config = ClientConfig {
        protocol: ClientProtocol::Https,
        ..Default::default()
    };
    let mut client = Client::new(config);

    tracing::info!(image = image, "Pulling image");
    let types = vec![
        manifest::IMAGE_LAYER_MEDIA_TYPE, // OCI
        manifest::IMAGE_LAYER_GZIP_MEDIA_TYPE,
        manifest::IMAGE_DOCKER_LAYER_TAR_MEDIA_TYPE, // Docker
        manifest::IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE,
    ];
    Ok(client.pull(&reference, &auth, types).await?)
}

#[tracing::instrument(skip(image_data))]
async fn unpack_image(image_data: oci_distribution::client::ImageData) -> std::io::Result<()> {
    tracing::info!("Unpacking image");
    for layer in image_data.layers {
        let tar_gz = Cursor::new(layer.data);
        let tar = GzDecoder::new(tar_gz);
        let mut archive = Archive::new(tar);
        archive.unpack("test/rootfs")?;
    }
    Ok(())
}

fn run_container() -> Result<(), Box<dyn std::error::Error>> {
    let container_id = "my-container";
    tracing::info!(container_id, "Creating container");
    let mut container = ContainerBuilder::new(container_id.to_owned(), SyscallType::default())
        .with_executor(MyExecutor {})
        .with_pid_file(Some("test/container.pid"))
        .expect("invalid pid file")
        // .with_console_socket(Some("/tmp/container/console.sock"))
        .with_root_path("test")
        .expect("invalid root path")
        .validate_id()?
        .as_init("test")
        .with_systemd(false)
        .with_detach(false)
        .build()?;

    tracing::info!(container_id, "Starting container");

    container
        .start()
        .with_context(|| format!("failed to start container {}", container_id))?;

    let _foreground_result = handle_foreground(container.pid().unwrap());

    tracing::info!(container_id, "Deleting container");
    container.delete(true)?;

    Ok(())
}

#[tracing::instrument(level = "trace")]
fn handle_foreground(init_pid: Pid) -> Result<i32> {
    tracing::trace!("waiting for container init process to exit");
    // We mask all signals here and forward most of the signals to the container
    // init process.
    let signal_set = SigSet::all();
    signal_set
        .thread_block()
        .with_context(|| "failed to call pthread_sigmask")?;
    loop {
        match signal_set
            .wait()
            .with_context(|| "failed to call sigwait")?
        {
            signal::SIGCHLD => {
                // Reap all child until either container init process exits or
                // no more child to be reaped. Once the container init process
                // exits we can then return.
                tracing::trace!("reaping child processes");
                loop {
                    match waitpid(None, Some(WaitPidFlag::WNOHANG))? {
                        WaitStatus::Exited(pid, status) => {
                            if pid.eq(&init_pid) {
                                return Ok(status);
                            }

                            // Else, some random child process exited, ignoring...
                        }
                        WaitStatus::Signaled(pid, signal, _) => {
                            if pid.eq(&init_pid) {
                                return Ok(signal as i32);
                            }

                            // Else, some random child process exited, ignoring...
                        }
                        WaitStatus::StillAlive => {
                            // No more child to reap.
                            break;
                        }
                        _ => {}
                    }
                }
            }
            signal::SIGURG => {
                // In `runc`, SIGURG is used by go runtime and should not be forwarded to
                // the container process. Here, we just ignore the signal.
            }
            signal::SIGWINCH => {
                // TODO: resize the terminal
            }
            signal => {
                tracing::trace!(?signal, "forwarding signal");
                // There is nothing we can do if we fail to forward the signal.
                let _ = kill(init_pid, Some(signal)).map_err(|err| {
                    tracing::warn!(
                        ?err,
                        ?signal,
                        "failed to forward signal to container init process",
                    );
                });
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer())
        .init();

    let image_data = pull_image("docker.io/library/alpine:latest").await?;
    unpack_image(image_data).await?;
    spec()?;
    run_container()?;
    Ok(())
}
