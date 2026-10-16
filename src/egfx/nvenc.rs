//! NVIDIA NVENC H.264 encoding through FFmpeg's `h264_nvenc`.
//!
//! Frames are converted to YUV420P with the same BT.709 full-range conversion as
//! the software encoder, so colours match whichever backend is active. NVENC runs
//! with low-latency settings (no B-frames, no lookahead, zero delay), and an IDR
//! is only produced on request, as with OpenH264.
use anyhow::{bail, Context, Result};
use ffmpeg_next as ffmpeg;
use ffmpeg::{codec, encoder, format::Pixel, frame, picture, Dictionary, Packet, Rational};

use super::h264::{convert_bgra_to_yuv420_planes, extract_sps_pps, validate_bgra_buffer};
use super::H264RateControl;

pub struct NvencEncoder {
    encoder: encoder::video::Encoder,
    frame: frame::Video,
    width: usize,
    height: usize,
    y_buf: Vec<u8>,
    u_buf: Vec<u8>,
    v_buf: Vec<u8>,
    /// SPS/PPS from the last IDR, prepended to P-frames for Windows MFT decoders.
    cached_sps_pps: Option<Vec<u8>>,
    force_idr: bool,
    pts: i64,
}

impl NvencEncoder {
    pub fn new(
        width: u32,
        height: u32,
        bitrate: u32,
        fps: u32,
        quality: u8,
        rate_control: H264RateControl,
    ) -> Result<Self> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            bail!("dimensions must be non-zero and even: {}x{}", width, height);
        }

        ffmpeg::init().context("FFmpeg initialization failed")?;
        let codec = encoder::find_by_name("h264_nvenc")
            .context("FFmpeg was built without the h264_nvenc encoder")?;
        let mut video = codec::context::Context::new_with_codec(codec)
            .encoder()
            .video()
            .context("h264_nvenc is not a video encoder")?;

        let fps = fps.max(1) as i32;
        video.set_width(width);
        video.set_height(height);
        video.set_format(Pixel::YUV420P);
        video.set_time_base(Rational::new(1, fps));
        video.set_frame_rate(Some(Rational::new(fps, 1)));
        video.set_max_b_frames(0);
        video.set_colorspace(ffmpeg::color::Space::BT709);
        video.set_color_range(ffmpeg::color::Range::JPEG);
        // SAFETY: the context is owned by `video` and not yet opened; these fields
        // have no setters in ffmpeg-next. A negative GOP means no periodic IDR.
        unsafe {
            let ctx = video.as_mut_ptr();
            (*ctx).color_primaries = ffmpeg::ffi::AVColorPrimaries::AVCOL_PRI_BT709;
            (*ctx).color_trc = ffmpeg::ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
            (*ctx).gop_size = -1;
        }

        let mut options = Dictionary::new();
        options.set("preset", "p1");
        options.set("tune", "ull");
        options.set("zerolatency", "1");
        options.set("delay", "0");
        options.set("rc-lookahead", "0");
        options.set("forced-idr", "1");
        match rate_control {
            H264RateControl::Vbr => {
                video.set_bit_rate(bitrate as usize);
                video.set_max_bit_rate(bitrate as usize * 2);
                options.set("rc", "vbr");
            }
            H264RateControl::Cqp => {
                options.set("rc", "constqp");
                options.set("qp", &quality.min(51).to_string());
            }
        }

        let encoder = video
            .open_with(options)
            .context("failed to open h264_nvenc (no usable NVIDIA encoder?)")?;
        tracing::info!(
            bitrate,
            fps,
            quality,
            rate_control = ?rate_control,
            "NVENC encoder settings"
        );

        let w = width as usize;
        let h = height as usize;
        Ok(Self {
            encoder,
            frame: frame::Video::new(Pixel::YUV420P, width, height),
            width: w,
            height: h,
            y_buf: vec![0u8; w * h],
            u_buf: vec![0u8; (w / 2) * (h / 2)],
            v_buf: vec![0u8; (w / 2) * (h / 2)],
            cached_sps_pps: None,
            // The first frame must be an IDR for the client's decoder to start.
            force_idr: true,
            pts: 0,
        })
    }

    /// Encode a BGRA frame to H.264 NAL units (Annex B format).
    pub fn encode(&mut self, bgra: &[u8], stride: usize) -> Result<Vec<u8>> {
        validate_bgra_buffer(self.width, self.height, stride, bgra.len())?;
        convert_bgra_to_yuv420_planes(
            self.width,
            self.height,
            bgra,
            stride,
            &mut self.y_buf,
            &mut self.u_buf,
            &mut self.v_buf,
        )?;
        copy_plane(&mut self.frame, 0, &self.y_buf, self.width, self.height);
        copy_plane(&mut self.frame, 1, &self.u_buf, self.width / 2, self.height / 2);
        copy_plane(&mut self.frame, 2, &self.v_buf, self.width / 2, self.height / 2);

        self.frame.set_pts(Some(self.pts));
        self.pts += 1;
        self.frame.set_kind(if self.force_idr {
            picture::Type::I
        } else {
            picture::Type::None
        });
        self.force_idr = false;

        self.encoder
            .send_frame(&self.frame)
            .context("NVENC rejected the frame")?;

        let mut data = Vec::new();
        let mut keyframe = false;
        let mut packet = Packet::empty();
        loop {
            match self.encoder.receive_packet(&mut packet) {
                Ok(()) => {
                    if let Some(bytes) = packet.data() {
                        data.extend_from_slice(bytes);
                    }
                    keyframe |= packet.is_key();
                }
                Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::error::EAGAIN => break,
                Err(error) => return Err(error).context("NVENC failed to deliver a packet"),
            }
        }
        anyhow::ensure!(!data.is_empty(), "NVENC produced no packet for the frame");

        if keyframe {
            if let Some(sps_pps) = extract_sps_pps(&data) {
                self.cached_sps_pps = Some(sps_pps);
            }
        } else if let Some(sps_pps) = &self.cached_sps_pps {
            let mut with_headers = Vec::with_capacity(sps_pps.len() + data.len());
            with_headers.extend_from_slice(sps_pps);
            with_headers.extend_from_slice(&data);
            data = with_headers;
        }
        Ok(data)
    }

    /// Force the next encoded frame to be an IDR frame.
    pub fn force_idr(&mut self) {
        self.force_idr = true;
    }
}

/// Copy a tightly packed plane into the frame, which may pad its rows.
fn copy_plane(frame: &mut frame::Video, plane: usize, src: &[u8], width: usize, height: usize) {
    let stride = frame.stride(plane);
    let dst = frame.data_mut(plane);
    for row in 0..height {
        dst[row * stride..row * stride + width].copy_from_slice(&src[row * width..(row + 1) * width]);
    }
}
