use std::{
    io::Write,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

use pipewire::spa::param::video::VideoFormat;

use crate::pw_source::PwSource;

pub struct FfmpegEncoder {
    stdin: ChildStdin,
    _child: Child,
}

impl FfmpegEncoder {
    pub fn new(
        width: u32,
        height: u32,
        framerate_num: u32,
        framerate_denom: u32,
        format: VideoFormat,
    ) -> eyre::Result<(Self, ChildStdout)> {
        let pixel_format = PwSource::convert_format_to_ffmpeg(format);

        let framerate = if framerate_num == 0 {
            "60/1".to_string()
        } else {
            format!("{}/{}", framerate_num, framerate_denom)
        };
        let video_size = format!("{}x{}", width, height);

        let mut child = Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "rawvideo",
                "-vcodec",
                "rawvideo",
                "-s",
                &video_size,
                "-pix_fmt",
                pixel_format,
                "-r",
                &framerate,
                "-i",
                "-",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-tune",
                "zerolatency",
                "-profile:v",
                "baseline",
                "-level",
                "3.1",
                "-pix_fmt",
                "yuv420p",
                "-x264-params",
                "keyint=30:min-keyint=30:scenecut=0",
                "-bsf:v",
                "h264_mp4toannexb",
                "-f",
                "h264",
                "-",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;

        let stdin = child.stdin.take().expect("Failed to open ffmpeg stdin");
        let stdout = child.stdout.take().expect("Failed to open ffmpeg stdout");

        Ok((
            Self {
                stdin,
                _child: child,
            },
            stdout,
        ))
    }

    pub fn write_frame(&mut self, data: &[u8]) -> eyre::Result<()> {
        self.stdin.write_all(data)?;
        Ok(())
    }
}
