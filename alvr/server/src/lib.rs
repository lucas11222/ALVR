mod bitrate;
mod body_tracking;
mod c_api;
mod connection;
mod face_tracking;
mod graphics;
mod hand_gestures;
mod haptics;
mod input_mapping;
mod logging_backend;
mod openvr;
mod sockets;
mod statistics;
mod tracking;
mod web_server;

#[allow(
    non_camel_case_types,
    non_upper_case_globals,
    dead_code,
    non_snake_case,
    clippy::unseparated_literal_suffix
)]
mod bindings {
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}
use bindings::*;

use alvr_common::{
    error,
    glam::Vec2,
    once_cell::sync::Lazy,
    parking_lot::{Mutex, RwLock},
    settings_schema::Switch,
    warn, ConnectionState, Fov, LifecycleState, OptLazy, Pose, RelaxedAtomic, DEVICE_ID_TO_PATH,
};
use alvr_events::{EventType, HapticsEvent};
use alvr_filesystem::{self as afs, Layout};
use alvr_packets::{
    BatteryInfo, ButtonEntry, ClientListAction, DecoderInitializationConfig, Haptics, Tracking,
    VideoPacketHeader,
};
use alvr_server_io::ServerDataManager;
use alvr_session::{CodecType, OpenvrProperty, Settings};
use bitrate::{BitrateManager, DynamicEncoderParams};
use statistics::StatisticsManager;
use std::{
    collections::VecDeque,
    env,
    ffi::CString,
    fs::File,
    io::Write,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::TrySendError,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use sysinfo::{ProcessRefreshKind, RefreshKind};
use tokio::{runtime::Runtime, sync::broadcast};

use crate::connection::{VideoPacket, VIDEO_CHANNEL_SENDER};

// todo: use this as the network packet
pub struct ViewsConfig {
    // transforms relative to the head
    pub local_view_transforms: [Pose; 2],
    pub fov: [Fov; 2],
}

pub enum ServerCoreEvent {
    SetOpenvrProperty {
        device_id: u64,
        prop: OpenvrProperty,
    },
    ClientConnected,
    ClientDisconnected,
    Battery(BatteryInfo),
    PlayspaceSync(Vec2),
    ViewsConfig(ViewsConfig),
    Tracking {
        tracking: Box<Tracking>,
        controllers_pose_time_offset: Duration,
    },
    Buttons(Vec<ButtonEntry>), // Note: this is after mapping
    RequestIDR,
    GameRenderLatencyFeedback(Duration), // only used for SteamVR
    ShutdownPending,
    RestartPending,
}

pub static EVENTS_QUEUE: Mutex<VecDeque<ServerCoreEvent>> = Mutex::new(VecDeque::new());

pub static LIFECYCLE_STATE: RwLock<LifecycleState> = RwLock::new(LifecycleState::StartingUp);
pub static IS_RESTARTING: RelaxedAtomic = RelaxedAtomic::new(false);
static CONNECTION_THREAD: RwLock<Option<JoinHandle<()>>> = RwLock::new(None);

static FILESYSTEM_LAYOUT: Lazy<Layout> = Lazy::new(|| {
    afs::filesystem_layout_from_openvr_driver_root_dir(
        &alvr_server_io::get_driver_dir_from_registered().unwrap(),
    )
});
static SERVER_DATA_MANAGER: Lazy<RwLock<ServerDataManager>> =
    Lazy::new(|| RwLock::new(ServerDataManager::new(&FILESYSTEM_LAYOUT.session())));
static WEBSERVER_RUNTIME: OptLazy<Runtime> = Lazy::new(|| Mutex::new(Runtime::new().ok()));

static STATISTICS_MANAGER: OptLazy<StatisticsManager> = alvr_common::lazy_mut_none();
static BITRATE_MANAGER: Lazy<Mutex<BitrateManager>> =
    Lazy::new(|| Mutex::new(BitrateManager::new(256, 60.0)));

static VIDEO_MIRROR_SENDER: OptLazy<broadcast::Sender<Vec<u8>>> = alvr_common::lazy_mut_none();
static VIDEO_RECORDING_FILE: OptLazy<File> = alvr_common::lazy_mut_none();

static DECODER_CONFIG: OptLazy<DecoderInitializationConfig> = alvr_common::lazy_mut_none();

pub fn create_recording_file(settings: &Settings) {
    let codec = settings.video.preferred_codec;
    let ext = match codec {
        CodecType::H264 => "h264",
        CodecType::Hevc => "h265",
        CodecType::AV1 => "av1",
    };

    let path = FILESYSTEM_LAYOUT.log_dir.join(format!(
        "recording.{}.{ext}",
        chrono::Local::now().format("%F.%H-%M-%S")
    ));

    match File::create(path) {
        Ok(mut file) => {
            if let Some(config) = &*DECODER_CONFIG.lock() {
                file.write_all(&config.config_buffer).ok();
            }

            *VIDEO_RECORDING_FILE.lock() = Some(file);

            unsafe { RequestIDR() };
        }
        Err(e) => {
            error!("Failed to record video on disk: {e}");
        }
    }
}

pub fn notify_restart_driver() {
    let mut system = sysinfo::System::new_with_specifics(
        RefreshKind::new().with_processes(ProcessRefreshKind::everything()),
    );
    system.refresh_processes();

    if system
        .processes_by_name(afs::dashboard_fname())
        .next()
        .is_some()
    {
        alvr_events::send_event(EventType::ServerRequestsSelfRestart);
    } else {
        error!("Cannot restart SteamVR. No dashboard process found on local device.");
    }
}

struct ServerCoreContext {}

impl ServerCoreContext {
    fn new() -> Self {
        if SERVER_DATA_MANAGER
            .read()
            .settings()
            .extra
            .logging
            .prefer_backtrace
        {
            env::set_var("RUST_BACKTRACE", "1");
        }

        SERVER_DATA_MANAGER.write().clean_client_list();

        if let Some(runtime) = WEBSERVER_RUNTIME.lock().as_mut() {
            runtime.spawn(async { alvr_common::show_err(web_server::web_server().await) });
        }

        unsafe {
            g_sessionPath = CString::new(FILESYSTEM_LAYOUT.session().to_string_lossy().to_string())
                .unwrap()
                .into_raw();
            g_driverRootDir = CString::new(
                FILESYSTEM_LAYOUT
                    .openvr_driver_root_dir
                    .to_string_lossy()
                    .to_string(),
            )
            .unwrap()
            .into_raw();
        };

        graphics::initialize_shaders();

        unsafe {
            LogError = Some(c_api::alvr_log_error);
            LogWarn = Some(c_api::alvr_log_warn);
            LogInfo = Some(c_api::alvr_log_info);
            LogDebug = Some(c_api::alvr_log_debug);
            LogPeriodically = Some(c_api::alvr_log_periodically);
            PathStringToHash = Some(c_api::alvr_path_to_id);

            CppInit();
        }

        Self {}
    }

    fn start_connection(&self) {
        // Note: Idle state is not used on the server side
        *LIFECYCLE_STATE.write() = LifecycleState::Resumed;

        thread::spawn(move || {
            connection::handshake_loop();
        });
    }

    fn poll_event(&self) -> Option<ServerCoreEvent> {
        EVENTS_QUEUE.lock().pop_front()
    }

    fn send_haptics(&self, haptics: Haptics) {
        let haptics_config = {
            let data_manager_lock = SERVER_DATA_MANAGER.read();

            if data_manager_lock.settings().extra.logging.log_haptics {
                alvr_events::send_event(EventType::Haptics(HapticsEvent {
                    path: DEVICE_ID_TO_PATH
                        .get(&haptics.device_id)
                        .map(|p| (*p).to_owned())
                        .unwrap_or_else(|| format!("Unknown (ID: {:#16x})", haptics.device_id)),
                    duration: haptics.duration,
                    frequency: haptics.frequency,
                    amplitude: haptics.amplitude,
                }))
            }

            data_manager_lock
                .settings()
                .headset
                .controllers
                .as_option()
                .and_then(|c| c.haptics.as_option().cloned())
        };

        if let (Some(config), Some(sender)) =
            (haptics_config, &mut *connection::HAPTICS_SENDER.lock())
        {
            sender
                .send_header(&haptics::map_haptics(&config, haptics))
                .ok();
        }
    }

    fn set_video_config_nals(&self, config_buffer: Vec<u8>, codec: CodecType) {
        if let Some(sender) = &*VIDEO_MIRROR_SENDER.lock() {
            sender.send(config_buffer.clone()).ok();
        }

        if let Some(file) = &mut *VIDEO_RECORDING_FILE.lock() {
            file.write_all(&config_buffer).ok();
        }

        *DECODER_CONFIG.lock() = Some(DecoderInitializationConfig {
            codec,
            config_buffer,
        });
    }

    fn send_video_nal(&self, target_timestamp: Duration, nal_buffer: Vec<u8>, is_idr: bool) {
        // start in the corrupts state, the client didn't receive the initial IDR yet.
        static STREAM_CORRUPTED: AtomicBool = AtomicBool::new(true);
        static LAST_IDR_INSTANT: Lazy<Mutex<Instant>> = Lazy::new(|| Mutex::new(Instant::now()));

        if let Some(sender) = &*VIDEO_CHANNEL_SENDER.lock() {
            let buffer_size = nal_buffer.len();

            if is_idr {
                STREAM_CORRUPTED.store(false, Ordering::SeqCst);
            }

            if let Switch::Enabled(config) = &SERVER_DATA_MANAGER
                .read()
                .settings()
                .extra
                .capture
                .rolling_video_files
            {
                if Instant::now()
                    > *LAST_IDR_INSTANT.lock() + Duration::from_secs(config.duration_s)
                {
                    EVENTS_QUEUE.lock().push_back(ServerCoreEvent::RequestIDR);

                    if is_idr {
                        crate::create_recording_file(SERVER_DATA_MANAGER.read().settings());
                        *LAST_IDR_INSTANT.lock() = Instant::now();
                    }
                }
            }

            if !STREAM_CORRUPTED.load(Ordering::SeqCst)
                || !SERVER_DATA_MANAGER
                    .read()
                    .settings()
                    .connection
                    .avoid_video_glitching
            {
                if let Some(sender) = &*VIDEO_MIRROR_SENDER.lock() {
                    sender.send(nal_buffer.clone()).ok();
                }

                if let Some(file) = &mut *VIDEO_RECORDING_FILE.lock() {
                    file.write_all(&nal_buffer).ok();
                }

                if matches!(
                    sender.try_send(VideoPacket {
                        header: VideoPacketHeader {
                            timestamp: target_timestamp,
                            is_idr
                        },
                        payload: nal_buffer,
                    }),
                    Err(TrySendError::Full(_))
                ) {
                    STREAM_CORRUPTED.store(true, Ordering::SeqCst);
                    EVENTS_QUEUE.lock().push_back(ServerCoreEvent::RequestIDR);
                    warn!("Dropping video packet. Reason: Can't push to network");
                }
            } else {
                warn!("Dropping video packet. Reason: Waiting for IDR frame");
            }

            if let Some(stats) = &mut *STATISTICS_MANAGER.lock() {
                let encoder_latency = stats.report_frame_encoded(target_timestamp, buffer_size);

                BITRATE_MANAGER.lock().report_frame_encoded(
                    target_timestamp,
                    encoder_latency,
                    buffer_size,
                );
            }
        }
    }

    fn get_dynamic_encoder_params(&self) -> Option<DynamicEncoderParams> {
        let pair = {
            let server_data_lock = SERVER_DATA_MANAGER.read();
            BITRATE_MANAGER
                .lock()
                .get_encoder_params(&server_data_lock.settings().video.bitrate)
        };

        if let Some((params, stats)) = pair {
            if let Some(stats_manager) = &mut *STATISTICS_MANAGER.lock() {
                stats_manager.report_nominal_bitrate_stats(stats);
            }

            Some(params)
        } else {
            None
        }
    }

    fn report_composed(&self, target_timestamp: Duration, offset: Duration) {
        if let Some(stats) = &mut *STATISTICS_MANAGER.lock() {
            stats.report_frame_composed(target_timestamp, offset);
        }
    }

    fn report_present(&self, target_timestamp: Duration, offset: Duration) {
        if let Some(stats) = &mut *STATISTICS_MANAGER.lock() {
            stats.report_frame_present(target_timestamp, offset);
        }

        let server_data_lock = SERVER_DATA_MANAGER.read();
        BITRATE_MANAGER
            .lock()
            .report_frame_present(&server_data_lock.settings().video.bitrate.adapt_to_framerate);
    }

    fn duration_until_next_vsync(&self) -> Option<Duration> {
        STATISTICS_MANAGER
            .lock()
            .as_mut()
            .map(|stats| stats.duration_until_next_vsync())
    }

    fn restart(self) {
        IS_RESTARTING.set(true);

        // drop is called here for self
    }
}

impl Drop for ServerCoreContext {
    fn drop(&mut self) {
        // Invoke connection runtimes shutdown
        *LIFECYCLE_STATE.write() = LifecycleState::ShuttingDown;

        {
            let mut data_manager_lock = SERVER_DATA_MANAGER.write();

            let hostnames = data_manager_lock
                .client_list()
                .iter()
                .filter(|&(_, info)| {
                    !matches!(
                        info.connection_state,
                        ConnectionState::Disconnected | ConnectionState::Disconnecting { .. }
                    )
                })
                .map(|(hostname, _)| hostname.clone())
                .collect::<Vec<_>>();

            for hostname in hostnames {
                data_manager_lock.update_client_list(
                    hostname,
                    ClientListAction::SetConnectionState(ConnectionState::Disconnecting),
                );
            }
        }

        if let Some(thread) = CONNECTION_THREAD.write().take() {
            thread.join().ok();
        }

        // apply openvr config for the next launch
        {
            let mut server_data_lock = SERVER_DATA_MANAGER.write();
            server_data_lock.session_mut().openvr_config =
                connection::contruct_openvr_config(server_data_lock.session());
        }

        if let Some(backup) = SERVER_DATA_MANAGER
            .write()
            .session_mut()
            .drivers_backup
            .take()
        {
            alvr_server_io::driver_registration(&backup.other_paths, true).ok();
            alvr_server_io::driver_registration(&[backup.alvr_path], false).ok();
        }

        while SERVER_DATA_MANAGER
            .read()
            .client_list()
            .iter()
            .any(|(_, info)| info.connection_state != ConnectionState::Disconnected)
        {
            thread::sleep(Duration::from_millis(100));
        }

        #[cfg(target_os = "windows")]
        WEBSERVER_RUNTIME.lock().take();
    }
}
