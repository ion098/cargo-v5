use core::panic;
use std::{env, time::Duration};

use cargo_metadata::camino::{Utf8Path, Utf8PathBuf};
#[cfg(feature = "field-control")]
use cargo_v5::commands::field_control::run_field_control_tui;
#[cfg(feature = "field-control")]
use vex_v5_serial::connection::serial::SerialDevice;
use cargo_v5::{
    commands::{
        build::{build, objcopy, CargoOpts},
        new::new,
        simulator::launch_simulator,
        upload::{upload_program, AfterUpload, UploadOpts},
    },
    errors::CliError,
    metadata::Metadata,
};
use chrono::Utc;
use clap::{Parser, Subcommand};
use flexi_logger::{
    AdaptiveFormat, Duplicate, FileSpec, LogSpecification, LogfileSelector, LoggerHandle,
};
use inquire::{
    validator::{ErrorMessage, Validation},
    CustomType,
};
use log::info;
use tokio::{
    io::{stdin, AsyncReadExt},
    runtime::Handle,
    select,
    task::{block_in_place, spawn_blocking},
    time::sleep,
};
use vex_v5_serial::{
    connection::{
        serial::{self, SerialConnection},
        Connection,
    },
    packets::{
        file::{
            FileLoadAction, FileVendor, LoadFileActionPacket, LoadFileActionPayload,
            LoadFileActionReplyPacket,
        },
        radio::{
            RadioChannel, SelectRadioChannelPacket, SelectRadioChannelPayload,
            SelectRadioChannelReplyPacket,
        },
        system::{
            GetSystemFlagsPacket, GetSystemFlagsReplyPacket, GetSystemVersionPacket,
            GetSystemVersionReplyPacket, ProductFlags,
        },
    },
    string::FixedLengthString,
};

cargo_subcommand_metadata::description!("Manage vexide projects");

/// Cargo's CLI arguments
#[derive(Parser, Debug)]
#[clap(name = "cargo", bin_name = "cargo")]
enum Cargo {
    /// Manage vexide projects.
    #[clap(version)]
    V5 {
        #[command(subcommand)]
        command: Command,

        #[arg(long, default_value = ".", global = true)]
        path: Utf8PathBuf,
    },
}

/// A possible `cargo v5` subcommand.
#[derive(Subcommand, Debug)]
enum Command {
    /// Build a project for the V5 brain.
    #[clap(visible_alias = "b")]
    Build {
        /// Build a binary for the WASM simulator instead of the native V5 target.
        #[arg(long, short)]
        simulator: bool,

        /// Arguments forwarded to `cargo`.
        #[clap(flatten)]
        cargo_opts: CargoOpts,
    },
    /// Build a project and upload it to the V5 brain.
    #[clap(visible_alias = "u")]
    Upload {
        #[arg(long, default_value = "none")]
        after: AfterUpload,

        #[clap(flatten)]
        upload_opts: UploadOpts,
    },
    /// Build, upload, and run a program on the V5 brain, showing its output in the terminal.
    #[clap(visible_alias = "r")]
    Run(UploadOpts),
    /// Access the brain's remote terminal I/O.
    #[clap(visible_alias = "t")]
    Terminal,
    /// Build a project and run it in the simulator.
    Sim {
        #[arg(long)]
        ui: Option<String>,

        /// Arguments forwarded to `cargo`.
        #[clap(flatten)]
        cargo_opts: CargoOpts,
    },
    /// Run a field control TUI.
    #[cfg(feature = "field-control")]
    #[clap(visible_aliases = ["fc", "comp-control"])]
    FieldControl,
    /// Create a new vexide project with a given name.
    #[clap(visible_alias = "n")]
    New {
        /// The name of the project.
        name: String,
    },
    /// Creates a new vexide project in the current directory
    Init,
}

#[tokio::main]
async fn main() -> miette::Result<()> {
    // Parse CLI arguments
    let Cargo::V5 { command, path } = Cargo::parse();

    let mut logger = flexi_logger::Logger::try_with_env_or_str("info")
        .unwrap()
        .log_to_file(
            FileSpec::default()
                .directory(env::temp_dir())
                .use_timestamp(false)
                .basename(format!(
                    "cargo-v5-{}",
                    Utc::now().format("%Y-%m-%d_%H-%M-%S")
                )),
        )
        .log_to_stdout()
        .duplicate_to_stderr(Duplicate::Warn)
        .adaptive_format_for_stderr(AdaptiveFormat::Default)
        .start()
        .unwrap();

    if let Err(err) = app(command, path, &mut logger).await {
        log::debug!("cargo-v5 is exiting due to an error: {}", err);
        if let Ok(files) = logger.existing_log_files(&LogfileSelector::default()) {
            for file in files {
                eprintln!("A log file is available at {}.", file.display());
            }
        }
        return Err(err);
    }
    Ok(())
}

async fn app(command: Command, path: Utf8PathBuf, logger: &mut LoggerHandle) -> miette::Result<()> {
    match command {
        Command::Build {
            simulator,
            cargo_opts,
        } => {
            build(&path, cargo_opts, simulator, |path| {
                if !simulator {
                    block_in_place(|| {
                        Handle::current().block_on(async move {
                            objcopy(&path).await.unwrap();
                        });
                    });
                }
            })
            .await;
        }
        Command::Upload { upload_opts, after } => {
            upload(&path, upload_opts, after, &mut open_connection().await?).await?;
        }
        Command::Run(opts) => {
            let mut connection = open_connection().await?;

            upload(&path, opts, AfterUpload::Run, &mut connection).await?;

            select! {
                () = terminal(&mut connection, logger) => {}
                _ = tokio::signal::ctrl_c() => {
                    // Quit program
                    _ = connection.packet_handshake::<LoadFileActionReplyPacket>(
                        Duration::from_secs(2),
                        1,
                        LoadFileActionPacket::new(LoadFileActionPayload {
                            vendor: FileVendor::User,
                            action: FileLoadAction::Stop,
                            file_name: FixedLengthString::new(Default::default()).unwrap(),
                        })
                    ).await;

                    // Switch back to pit channel
                    _ = connection
                        .packet_handshake::<SelectRadioChannelReplyPacket>(
                            Duration::from_secs(2),
                            1,
                            SelectRadioChannelPacket::new(SelectRadioChannelPayload {
                                channel: RadioChannel::Pit,
                            }),
                        )
                        .await;

                    std::process::exit(0);
                }
            }
        }
        Command::Terminal => {
            let mut connection = open_connection().await?;
            switch_radio_channel(&mut connection, RadioChannel::Download).await?;
            terminal(&mut connection, logger).await;
        }
        Command::Sim { ui, cargo_opts } => {
            let mut artifact = None;
            build(&path, cargo_opts, true, |new_artifact| {
                artifact = Some(new_artifact);
            })
            .await;
            launch_simulator(
                ui.clone(),
                path.as_ref(),
                artifact
                    .expect("Binary target not found (is this a library?)")
                    .as_ref(),
            )
            .await;
        }
        #[cfg(feature = "field-control")]
        Command::FieldControl => {
            // Not using open_connection since we need to filter for controllers only here.
            let mut connection = {
                let devices = serial::find_devices().map_err(CliError::SerialError)?;

                spawn_blocking(move || {
                    Ok(devices
                        .into_iter()
                        .find(|device| matches!(device, SerialDevice::Controller { system_port: _ }))
                        .ok_or(CliError::NoController)?
                        .connect(Duration::from_secs(5))
                        .map_err(CliError::SerialError)?)
                })
                .await
                .unwrap()
            };

            run_field_control_tui(&mut connection).await?;
        }
        Command::New { name } => {
            new(path, Some(name)).await?;
        }
        Command::Init => {
            new(path, None).await?;
        }
    }

    Ok(())
}

async fn open_connection() -> miette::Result<SerialConnection> {
    // Find all vex devices on serial ports.
    let devices = serial::find_devices().map_err(CliError::SerialError)?;

    // Open a connection to the device.
    spawn_blocking(move || {
        Ok(devices
            .first()
            .ok_or(CliError::NoDevice)?
            .connect(Duration::from_secs(5))
            .map_err(CliError::SerialError)?)
    })
    .await
    .unwrap()
}

async fn is_connection_wireless(connection: &mut SerialConnection) -> Result<bool, CliError> {
    let version = connection
        .packet_handshake::<GetSystemVersionReplyPacket>(
            Duration::from_millis(500),
            1,
            GetSystemVersionPacket::new(()),
        )
        .await?;
    let system_flags = connection
        .packet_handshake::<GetSystemFlagsReplyPacket>(
            Duration::from_millis(500),
            1,
            GetSystemFlagsPacket::new(()),
        )
        .await?;
    let controller = version
        .payload
        .flags
        .contains(ProductFlags::CONNECTED_WIRELESS);

    let tethered = system_flags.payload.flags & (1 << 8) != 0;
    Ok(!tethered && controller)
}

pub async fn switch_radio_channel(
    connection: &mut SerialConnection,
    channel: RadioChannel,
) -> Result<(), CliError> {
    if is_connection_wireless(connection).await? {
        let channel_str = match channel {
            RadioChannel::Download => "download",
            RadioChannel::Pit => "pit",
        };

        info!("Switching radio to {channel_str} channel...");

        // Tell the controller to switch to the download channel.
        connection
            .packet_handshake::<SelectRadioChannelReplyPacket>(
                Duration::from_secs(2),
                3,
                SelectRadioChannelPacket::new(SelectRadioChannelPayload { channel }),
            )
            .await?;

        // Wait for the radio to switch channels before polling the connection
        sleep(Duration::from_millis(250)).await;

        // Poll the connection of the controller to ensure the radio has switched channels.
        let timeout = Duration::from_secs(5);
        select! {
            _ = sleep(timeout) => {
                return Err(CliError::RadioChannelTimeout)
            }
            _ = async {
                while !is_connection_wireless(connection).await.unwrap_or(false) {
                    sleep(Duration::from_millis(250)).await;
                }
            } => {
                info!("Radio successfully switched to {channel_str} channel.");
            }
        }
    }

    Ok(())
}

async fn upload(
    path: &Utf8Path,
    UploadOpts {
        file,
        slot,
        name,
        description,
        icon,
        uncompressed,
        cargo_opts,
    }: UploadOpts,
    after: AfterUpload,
    connection: &mut SerialConnection,
) -> miette::Result<()> {
    // We'll use `cargo-metadata` to parse the output of `cargo metadata` and find valid `Cargo.toml`
    // files in the workspace directory.
    let cargo_metadata =
        block_in_place(|| cargo_metadata::MetadataCommand::new().no_deps().exec()).ok();

    // Locate packages with valid v5 metadata fields.
    let package = cargo_metadata.and_then(|metadata| {
        metadata
            .packages
            .iter()
            .find(|p| {
                if let Some(v5_metadata) = p.metadata.get("v5") {
                    v5_metadata.is_object()
                } else {
                    false
                }
            })
            .cloned()
            .or(metadata.packages.first().cloned())
    });

    // Uploading has the option to use the `package.metadata.v5` table for default configuration options.
    // Attempt to serialize `package.metadata.v5` into a [`Metadata`] struct. This will just Default::default to
    // all `None`s if it can't find a specific field, or error if the field is malformed.
    let metadata = if let Some(ref package) = package {
        Some(Metadata::new(package)?)
    } else {
        None
    };

    // Get the build artifact we'll be uploading with.
    //
    // The user either directly passed an file through the `--file` argument, or they didn't and we need to run
    // `cargo build`.
    let mut artifact = None;
    if let Some(file) = file {
        if file.extension() == Some("bin") {
            artifact = Some(file);
        } else {
            // If a BIN file wasn't provided, we'll attempt to objcopy it as if it were an ELF.
            artifact = Some(objcopy(&file).await?);
        }
    } else {
        // Run cargo build, then objcopy.
        build(path, cargo_opts, false, |new_artifact| {
            let mut bin_path = new_artifact.clone();
            bin_path.set_extension("bin");
            block_in_place(|| {
                Handle::current().block_on(async move {
                    objcopy(&new_artifact).await.unwrap();
                });
            });
            artifact = Some(bin_path);
        })
        .await;
    }

    // The program's slot number is absolutely required for uploading. If the slot argument isn't directly provided:
    //
    // - Check for the `package.metadata.v5.slot` field in Cargo.toml.
    // - If that doesn't exist, directly prompt the user asking what slot to upload to.
    let slot = slot
        .or(metadata.and_then(|m| m.slot))
        .or_else(|| {
            CustomType::<u8>::new("Choose a program slot to upload to:")
                .with_validator(|slot: &u8| {
                    Ok(if (1..=8).contains(slot) {
                        Validation::Valid
                    } else {
                        Validation::Invalid(ErrorMessage::Custom("Slot out of range".to_string()))
                    })
                })
                .with_help_message("Type a slot number from 1 to 8, inclusive")
                .prompt()
                .ok()
        })
        .ok_or(CliError::NoSlot)?;

    // Ensure [1, 8] range bounds for slot number
    if !(1..8).contains(&slot) {
        Err(CliError::SlotOutOfRange)?;
    }

    // Switch the radio to the download channel if the controller is wireless.
    switch_radio_channel(connection, RadioChannel::Download).await?;

    // Pass information to the upload routine.
    upload_program(
        connection,
        &artifact.ok_or(CliError::NoArtifact)?,
        after,
        slot,
        name.or(package.as_ref().map(|pkg| pkg.name.clone()))
            .unwrap_or("cargo-v5".to_string()),
        description
            .or(package.as_ref().and_then(|pkg| pkg.description.clone()))
            .unwrap_or("Uploaded with cargo-v5.".to_string()),
        icon.or(metadata.and_then(|metadata| metadata.icon))
            .unwrap_or_default(),
        "Rust".to_string(), // `program_type` hardcoded for now, maybe configurable in the future.
        match uncompressed {
            Some(val) => !val,
            None => metadata
                .and_then(|metadata| metadata.compress)
                .unwrap_or(true),
        },
    )
    .await?;

    Ok(())
}

async fn terminal(connection: &mut SerialConnection, logger: &mut LoggerHandle) -> ! {
    info!("Started terminal.");

    logger.push_temp_spec(LogSpecification::off());

    let mut stdin = stdin();

    loop {
        let mut program_output = [0; 1024];
        let mut program_input = [0; 1024];
        select! {
            read = connection.read_user(&mut program_output) => {
                if let Ok(size) = read {
                    print!("{}", std::str::from_utf8(&program_output[..size]).unwrap());
                }
            },
            read = stdin.read(&mut program_input) => {
                if let Ok(size) = read {
                    connection.write_user(&program_input[..size]).await.unwrap();
                }
            }
        }

        sleep(Duration::from_millis(10)).await;
    }
}
