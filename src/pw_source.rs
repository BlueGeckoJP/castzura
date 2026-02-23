use std::{
    io::{Cursor, Read},
    os::fd::OwnedFd,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use ashpd::desktop::{
    PersistMode,
    screencast::{CursorMode, Screencast, SourceType},
};
use eyre::OptionExt;
use pipewire::{
    context::ContextBox,
    main_loop::MainLoopBox,
    properties::properties,
    spa::{
        param::{
            ParamType,
            format::{MediaSubtype, MediaType},
            video::{VideoFormat, VideoInfoRaw},
        },
        pod::{Pod, serialize::PodSerializer},
    },
    stream::{StreamBox, StreamFlags},
};
use tracing::{error, info, warn};

use crate::ffmpeg_encoder::FfmpegEncoder;

struct UserData {
    format: VideoInfoRaw,
    encoder: Option<Arc<Mutex<FfmpegEncoder>>>,
    tx: tokio::sync::broadcast::Sender<Vec<u8>>,
    ffmpeg_running: Arc<AtomicBool>,
}

#[derive(Clone)]
pub struct PwSource;

impl PwSource {
    pub async fn get_pw_node_id() -> eyre::Result<(u32, OwnedFd)> {
        let proxy = Screencast::new().await?;
        let session = proxy.create_session().await?;
        proxy
            .select_sources(
                &session,
                CursorMode::Embedded,
                SourceType::Monitor.into(),
                false,
                None,
                PersistMode::DoNot,
            )
            .await?;

        let response = proxy.start(&session, None).await?.response()?;
        let stream = response
            .streams()
            .first()
            .ok_or_eyre("No streams available")?
            .to_owned();
        let node_id = stream.pipe_wire_node_id();

        let fd = proxy.open_pipe_wire_remote(&session).await?;

        Ok((node_id, fd))
    }

    pub async fn start_pw_stream(
        &mut self,
        fd: OwnedFd,
        node_id: u32,
        tx: tokio::sync::broadcast::Sender<Vec<u8>>,
        ffmpeg_running: Arc<AtomicBool>,
        pw_running: Arc<AtomicBool>,
    ) -> eyre::Result<()> {
        pipewire::init();

        let mainloop = MainLoopBox::new(None)?;
        let context = ContextBox::new(mainloop.loop_(), None)?;
        let core = context.connect_fd(fd, None)?;

        let data = UserData {
            format: Default::default(),
            encoder: None,
            tx,
            ffmpeg_running: ffmpeg_running.clone(),
        };

        let stream = StreamBox::new(
            &core,
            "castzura",
            properties! {
                *pipewire::keys::MEDIA_TYPE => "Video",
                *pipewire::keys::MEDIA_CATEGORY => "Capture",
                *pipewire::keys::MEDIA_ROLE => "Camera",
            },
        )?;

        let _listener = stream
            .add_local_listener_with_user_data(data)
            .state_changed(|_, _, old, new| {
                info!("State changed: {:?} -> {:?}", old, new);
            })
            .param_changed(|_, user_data, id, param| {
                let Some(param) = param else {
                    return;
                };

                if id != ParamType::Format.as_raw() {
                    return;
                };

                let (media_type, media_subtype) =
                    match pipewire::spa::param::format_utils::parse_format(param) {
                        Ok(v) => v,
                        Err(e) => {
                            error!("Failed to parse format: {:?}", e);
                            return;
                        }
                    };

                if media_type != MediaType::Video || media_subtype != MediaSubtype::Raw {
                    return;
                }

                user_data
                    .format
                    .parse(param)
                    .expect("Failed to parse param changed to VideoInfoRaw");

                info!("Got video format:");
                info!(
                    "- format: {} ({:?})",
                    user_data.format.format().as_raw(),
                    user_data.format.format()
                );
                info!(
                    "- size: {}x{}",
                    user_data.format.size().width,
                    user_data.format.size().height
                );
                info!(
                    "- framerate: {}/{}",
                    user_data.format.framerate().num,
                    user_data.format.framerate().denom,
                );

                if user_data.encoder.is_none() {
                    let format = user_data.format.format();
                    let size = user_data.format.size();
                    let framerate = user_data.format.framerate();

                    PwSource::setup_encoder(
                        size.width,
                        size.height,
                        framerate.num,
                        framerate.denom,
                        format,
                        user_data,
                    );
                }
            })
            .process(|stream, user_data| match stream.dequeue_buffer() {
                None => warn!("Out of buffers"),
                Some(mut buffer) => {
                    let datas = buffer.datas_mut();
                    if datas.is_empty() {
                        return;
                    }

                    let data = &mut datas[0];

                    let chunk = data.chunk();
                    let size = chunk.size() as usize;
                    let offset = chunk.offset() as usize;

                    if let Some(slice) = data.data() {
                        let frame_data = &slice[offset..(offset + size)];

                        if let Some(encoder) = &mut user_data.encoder
                            && let Ok(mut encoder) = encoder.try_lock()
                        {
                            encoder.write_frame(frame_data).unwrap_or_else(|e| {
                                error!("Failed to write frame to FFmpeg encoder: {:?}", e);
                            });
                        }
                    }
                }
            })
            .register()?;

        info!("Created stream: {:#?}", stream);

        let obj = pipewire::spa::pod::object!(
            pipewire::spa::utils::SpaTypes::ObjectParamFormat,
            pipewire::spa::param::ParamType::EnumFormat,
            pipewire::spa::pod::property!(
                pipewire::spa::param::format::FormatProperties::MediaType,
                Id,
                pipewire::spa::param::format::MediaType::Video
            ),
            pipewire::spa::pod::property!(
                pipewire::spa::param::format::FormatProperties::MediaSubtype,
                Id,
                pipewire::spa::param::format::MediaSubtype::Raw
            ),
            pipewire::spa::pod::property!(
                pipewire::spa::param::format::FormatProperties::VideoFormat,
                Choice,
                Enum,
                Id,
                pipewire::spa::param::video::VideoFormat::RGB,
                pipewire::spa::param::video::VideoFormat::RGB,
                pipewire::spa::param::video::VideoFormat::RGBA,
                pipewire::spa::param::video::VideoFormat::RGBx,
                pipewire::spa::param::video::VideoFormat::BGRx,
                pipewire::spa::param::video::VideoFormat::YUY2,
                pipewire::spa::param::video::VideoFormat::I420,
            ),
            pipewire::spa::pod::property!(
                pipewire::spa::param::format::FormatProperties::VideoSize,
                Choice,
                Range,
                Rectangle,
                pipewire::spa::utils::Rectangle {
                    width: 320,
                    height: 240
                },
                pipewire::spa::utils::Rectangle {
                    width: 1,
                    height: 1
                },
                pipewire::spa::utils::Rectangle {
                    width: 4096,
                    height: 4096
                }
            ),
            pipewire::spa::pod::property!(
                pipewire::spa::param::format::FormatProperties::VideoFramerate,
                Choice,
                Range,
                Fraction,
                pipewire::spa::utils::Fraction { num: 25, denom: 1 },
                pipewire::spa::utils::Fraction { num: 0, denom: 1 },
                pipewire::spa::utils::Fraction {
                    num: 1000,
                    denom: 1
                }
            ),
        );

        let values: Vec<u8> = PodSerializer::serialize(
            Cursor::new(Vec::new()),
            &pipewire::spa::pod::Value::Object(obj),
        )?
        .0
        .into_inner();

        let mut params = [Pod::from_bytes(&values).ok_or_eyre("Failed to create pod from bytes")?];

        stream.connect(
            pipewire::spa::utils::Direction::Input,
            Some(node_id),
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut params,
        )?;

        info!("Connected stream to node_id: {}", node_id);

        // pw_running was already set to true in ws_handler via compare_exchange

        mainloop.run();

        // mainloop exited: reset flags so the session can be restarted
        pw_running.store(false, Ordering::SeqCst);
        ffmpeg_running.store(false, Ordering::SeqCst);

        Ok(())
    }

    pub fn convert_format_to_ffmpeg(format: VideoFormat) -> &'static str {
        match format {
            VideoFormat::RGB => "rgb24",
            VideoFormat::RGBA => "rgba",
            VideoFormat::RGBx => "rgb24",
            VideoFormat::BGRx => "bgr0",
            VideoFormat::YUY2 => "yuyv422",
            VideoFormat::I420 => "yuv420p",
            _ => "yuv420p", // Default to a common format
        }
    }

    fn setup_encoder(
        width: u32,
        height: u32,
        framerate_num: u32,
        framerate_denom: u32,
        format: VideoFormat,
        user_data: &mut UserData,
    ) {
        match FfmpegEncoder::new(width, height, framerate_num, framerate_denom, format) {
            Ok((encoder, mut stdout)) => {
                info!("FFmpeg encoder started successfully");
                user_data.encoder = Some(Arc::new(Mutex::new(encoder)));
                user_data.ffmpeg_running.store(true, Ordering::Relaxed);

                let tx = user_data.tx.clone();
                std::thread::spawn(move || {
                    let mut read_buf = [0u8; 65536];
                    let mut accumulator: Vec<u8> = Vec::new();

                    loop {
                        match stdout.read(&mut read_buf) {
                            Ok(0) => {
                                if !accumulator.is_empty() {
                                    let _ = tx.send(accumulator);
                                }
                                break;
                            }
                            Ok(n) => {
                                accumulator.extend_from_slice(&read_buf[..n]);

                                // Extract complete NAL units delimited by
                                // Annex B start codes (00 00 00 01).
                                // Only send data once a *second* start code is
                                // found, guaranteeing the previous NAL unit is
                                // complete.
                                let mut search_pos = 0;
                                while let Some(start) =
                                    Self::find_start_code(&accumulator, search_pos)
                                {
                                    match Self::find_start_code(&accumulator, start + 4) {
                                        Some(next) => {
                                            let nal = accumulator[start..next].to_vec();
                                            if let Err(e) = tx.send(nal) {
                                                // No active receivers - keep running, clients may reconnect
                                                let _ = e;
                                            }
                                            search_pos = next;
                                        }
                                        None => break, // wait for more data
                                    }
                                }

                                if search_pos > 0 {
                                    accumulator = accumulator[search_pos..].to_vec();
                                }
                            }
                            Err(e) => {
                                error!("Failed to read from FFmpeg encoder: {:?}", e);
                                break;
                            }
                        }
                    }
                });
            }
            Err(e) => error!("Failed to start FFmpeg encoder: {:?}", e),
        }
    }

    fn find_start_code(data: &[u8], start: usize) -> Option<usize> {
        if data.len() < start + 4 {
            return None;
        }
        (start..data.len() - 3)
            .find(|&i| data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && data[i + 3] == 1)
    }
}
