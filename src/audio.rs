use std::time::Duration;

use anyhow::Result;

use crate::config::{AudioConfig, SpeechConfig, WakeConfig};
use crate::wake_sidecar::WakeDetection;

#[cfg(feature = "audio-cpal")]
use crate::wake_sidecar::WakeSidecar;
#[cfg(feature = "audio-cpal")]
use anyhow::{Context, bail};
#[cfg(feature = "audio-cpal")]
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
#[cfg(feature = "audio-cpal")]
use cpal::{Device, SampleFormat, SampleRate, StreamConfig};
#[cfg(feature = "audio-cpal")]
use std::collections::VecDeque;
#[cfg(feature = "audio-cpal")]
use std::sync::mpsc::{self, Receiver, SyncSender};
#[cfg(feature = "audio-cpal")]
use std::thread;

#[cfg(feature = "audio-cpal")]
const WAKE_SAMPLE_RATE_HZ: u32 = 16_000;
#[cfg(feature = "audio-cpal")]
const WAKE_FRAME_MS: usize = 80;

#[derive(Debug, Clone)]
pub struct AudioInput {
    audio: AudioConfig,
    speech: SpeechConfig,
}

#[derive(Debug, Clone)]
pub struct CapturedAudio {
    pub wav_bytes: Vec<u8>,
    pub sample_rate_hz: u32,
    pub duration: Duration,
}

impl AudioInput {
    pub fn new(audio: AudioConfig, speech: SpeechConfig) -> Self {
        Self { audio, speech }
    }

    pub fn describe_configuration(&self) -> Result<()> {
        tracing::info!(
            "audio input configured: device='{}', sample_rate_hz={}, channel_count={}, threshold={:.4}",
            self.audio.input_device,
            self.audio.sample_rate_hz,
            self.audio.channel_count,
            self.speech.level_threshold
        );
        Ok(())
    }

    pub fn list_input_devices(&self) -> Result<Vec<String>> {
        #[cfg(feature = "audio-cpal")]
        {
            let host = cpal::default_host();
            let devices = host
                .input_devices()
                .context("failed to query input devices")?;

            let mut names = Vec::new();
            for device in devices {
                let name = device
                    .name()
                    .unwrap_or_else(|_| "<unavailable-name>".to_string());
                names.push(name);
            }

            Ok(names)
        }

        #[cfg(not(feature = "audio-cpal"))]
        {
            tracing::warn!("audio-cpal feature is disabled; device enumeration is stubbed");
            Ok(Vec::new())
        }
    }

    pub fn capture_utterance(&self) -> Result<CapturedAudio> {
        #[cfg(feature = "audio-cpal")]
        {
            self.capture_with_cpal()
        }

        #[cfg(not(feature = "audio-cpal"))]
        {
            anyhow::bail!("microphone capture requires the `audio-cpal` feature")
        }
    }

    pub fn capture_after_wake(&self, wake: &WakeConfig) -> Result<(WakeDetection, CapturedAudio)> {
        #[cfg(feature = "audio-cpal")]
        {
            self.capture_with_cpal_after_wake(wake)
        }

        #[cfg(not(feature = "audio-cpal"))]
        {
            let _ = wake;
            anyhow::bail!("microphone capture requires the `audio-cpal` feature")
        }
    }

    #[cfg(feature = "audio-cpal")]
    fn capture_with_cpal(&self) -> Result<CapturedAudio> {
        let device = self.select_input_device()?;
        let stream_config = self.select_stream_config(&device)?;
        let device_name = device
            .name()
            .unwrap_or_else(|_| "<unavailable-name>".to_string());

        let (tx, rx) = mpsc::sync_channel::<Vec<f32>>(64);
        let stream = build_input_stream(
            &device,
            &stream_config,
            tx,
            usize::from(stream_config.config.channels),
        )?;
        stream.play().context("failed to start microphone stream")?;

        tracing::info!(
            "listening on '{}' at {} Hz / {} channel(s)",
            device_name,
            stream_config.config.sample_rate.0,
            stream_config.config.channels
        );

        let mono = capture_until_pause(
            rx,
            stream_config.config.sample_rate.0,
            &self.speech,
            Duration::from_millis(self.speech.cooldown_ms),
        )?;
        drop(stream);

        let resampled = resample_mono(
            &mono,
            stream_config.config.sample_rate.0,
            self.audio.sample_rate_hz,
        );
        if resampled.is_empty() {
            bail!("captured audio buffer was empty after resampling");
        }

        let wav_bytes = encode_wav_mono_i16(&resampled, self.audio.sample_rate_hz)?;
        let duration =
            Duration::from_secs_f64(resampled.len() as f64 / self.audio.sample_rate_hz as f64);

        Ok(CapturedAudio {
            wav_bytes,
            sample_rate_hz: self.audio.sample_rate_hz,
            duration,
        })
    }

    #[cfg(feature = "audio-cpal")]
    fn capture_with_cpal_after_wake(
        &self,
        wake: &WakeConfig,
    ) -> Result<(WakeDetection, CapturedAudio)> {
        let device = self.select_input_device()?;
        let stream_config = self.select_stream_config(&device)?;
        let device_name = device
            .name()
            .unwrap_or_else(|_| "<unavailable-name>".to_string());

        let (tx, rx) = mpsc::sync_channel::<Vec<f32>>(64);
        let stream = build_input_stream(
            &device,
            &stream_config,
            tx,
            usize::from(stream_config.config.channels),
        )?;
        stream.play().context("failed to start microphone stream")?;

        tracing::info!(
            "listening on '{}' at {} Hz / {} channel(s) for wake word and command capture",
            device_name,
            stream_config.config.sample_rate.0,
            stream_config.config.channels
        );

        let input_rate_hz = stream_config.config.sample_rate.0;
        let detection = wait_for_wake_on_stream(&rx, input_rate_hz, wake)?;
        tracing::info!("wake detected; waiting for command speech");

        let mono = capture_until_pause(
            rx,
            input_rate_hz,
            &self.speech,
            Duration::from_millis(self.speech.cooldown_ms),
        )?;
        drop(stream);

        let resampled = resample_mono(&mono, input_rate_hz, self.audio.sample_rate_hz);
        if resampled.is_empty() {
            bail!("captured audio buffer was empty after resampling");
        }

        let wav_bytes = encode_wav_mono_i16(&resampled, self.audio.sample_rate_hz)?;
        let duration =
            Duration::from_secs_f64(resampled.len() as f64 / self.audio.sample_rate_hz as f64);

        Ok((
            detection,
            CapturedAudio {
                wav_bytes,
                sample_rate_hz: self.audio.sample_rate_hz,
                duration,
            },
        ))
    }

    #[cfg(feature = "audio-cpal")]
    fn select_input_device(&self) -> Result<Device> {
        let host = cpal::default_host();

        if self.audio.input_device.trim().is_empty() {
            return host
                .default_input_device()
                .context("no default input device is available");
        }

        let wanted = self.audio.input_device.trim().to_ascii_lowercase();
        let devices = host
            .input_devices()
            .context("failed to query input devices")?;

        for device in devices {
            let Ok(name) = device.name() else {
                continue;
            };
            if name.to_ascii_lowercase() == wanted || name.to_ascii_lowercase().contains(&wanted) {
                return Ok(device);
            }
        }

        bail!("input device '{}' was not found", self.audio.input_device)
    }

    #[cfg(feature = "audio-cpal")]
    fn select_stream_config(&self, device: &Device) -> Result<ResolvedInputConfig> {
        let default = device
            .default_input_config()
            .context("failed to query default input config")?;
        let preferred_rate = self.audio.sample_rate_hz;
        let preferred_channels = self.audio.channel_count;

        let chosen = device
            .supported_input_configs()
            .ok()
            .and_then(|configs| {
                configs.into_iter().find_map(|range| {
                    if range.channels() != preferred_channels {
                        return None;
                    }
                    if range.min_sample_rate().0 <= preferred_rate
                        && preferred_rate <= range.max_sample_rate().0
                    {
                        Some(ResolvedInputConfig {
                            sample_format: range.sample_format(),
                            config: StreamConfig {
                                channels: preferred_channels,
                                sample_rate: SampleRate(preferred_rate),
                                buffer_size: cpal::BufferSize::Default,
                            },
                        })
                    } else {
                        None
                    }
                })
            })
            .unwrap_or_else(|| {
                if default.sample_rate().0 != preferred_rate || default.channels() != preferred_channels
                {
                    tracing::info!(
                        "preferred input config {} Hz / {} channel(s) is unavailable; using device default {} Hz / {} channel(s)",
                        preferred_rate,
                        preferred_channels,
                        default.sample_rate().0,
                        default.channels()
                    );
                }

                ResolvedInputConfig {
                    sample_format: default.sample_format(),
                    config: default.config(),
                }
            });

        Ok(chosen)
    }
}

#[cfg(feature = "audio-cpal")]
#[derive(Clone)]
struct ResolvedInputConfig {
    sample_format: SampleFormat,
    config: StreamConfig,
}

#[cfg(feature = "audio-cpal")]
fn build_input_stream(
    device: &Device,
    stream_config: &ResolvedInputConfig,
    tx: SyncSender<Vec<f32>>,
    channels: usize,
) -> Result<cpal::Stream> {
    let config = stream_config.config.clone();
    let err_fn = move |error: cpal::StreamError| {
        tracing::warn!("microphone stream error: {error}");
    };

    match stream_config.sample_format {
        SampleFormat::F32 => device
            .build_input_stream(
                &config,
                move |data: &[f32], _| send_input_chunk_f32(data, channels, &tx),
                err_fn,
                None,
            )
            .context("failed to build f32 input stream"),
        SampleFormat::I16 => device
            .build_input_stream(
                &config,
                move |data: &[i16], _| send_input_chunk_i16(data, channels, &tx),
                err_fn,
                None,
            )
            .context("failed to build i16 input stream"),
        SampleFormat::U16 => device
            .build_input_stream(
                &config,
                move |data: &[u16], _| send_input_chunk_u16(data, channels, &tx),
                err_fn,
                None,
            )
            .context("failed to build u16 input stream"),
        SampleFormat::U8 => device
            .build_input_stream(
                &config,
                move |data: &[u8], _| send_input_chunk_u8(data, channels, &tx),
                err_fn,
                None,
            )
            .context("failed to build u8 input stream"),
        other => bail!("unsupported input sample format: {other:?}"),
    }
}

#[cfg(feature = "audio-cpal")]
fn send_input_chunk_f32(data: &[f32], channels: usize, tx: &SyncSender<Vec<f32>>) {
    let _ = tx.try_send(downmix_to_mono(data, channels, |sample| {
        sample.clamp(-1.0, 1.0)
    }));
}

#[cfg(feature = "audio-cpal")]
fn send_input_chunk_i16(data: &[i16], channels: usize, tx: &SyncSender<Vec<f32>>) {
    let scale = i16::MAX as f32;
    let _ = tx.try_send(downmix_to_mono(data, channels, |sample| {
        sample as f32 / scale
    }));
}

#[cfg(feature = "audio-cpal")]
fn send_input_chunk_u16(data: &[u16], channels: usize, tx: &SyncSender<Vec<f32>>) {
    let center = u16::MAX as f32 / 2.0;
    let _ = tx.try_send(downmix_to_mono(data, channels, |sample| {
        (sample as f32 - center) / center
    }));
}

#[cfg(feature = "audio-cpal")]
fn send_input_chunk_u8(data: &[u8], channels: usize, tx: &SyncSender<Vec<f32>>) {
    let center = u8::MAX as f32 / 2.0;
    let _ = tx.try_send(downmix_to_mono(data, channels, |sample| {
        (sample as f32 - center) / center
    }));
}

#[cfg(feature = "audio-cpal")]
fn downmix_to_mono<T, F>(data: &[T], channels: usize, convert: F) -> Vec<f32>
where
    T: Copy,
    F: Fn(T) -> f32,
{
    if channels <= 1 {
        return data.iter().copied().map(convert).collect();
    }

    let mut mono = Vec::with_capacity(data.len() / channels);
    for frame in data.chunks(channels) {
        let total = frame.iter().copied().map(&convert).sum::<f32>();
        mono.push(total / frame.len() as f32);
    }
    mono
}

#[cfg(feature = "audio-cpal")]
fn capture_until_pause(
    rx: Receiver<Vec<f32>>,
    input_rate_hz: u32,
    speech: &SpeechConfig,
    cooldown: Duration,
) -> Result<Vec<f32>> {
    let frame_samples = ((input_rate_hz as usize) / 50).max(1);
    let pre_roll_samples =
        ((speech.pre_roll_ms as usize * input_rate_hz as usize) / 1000).max(frame_samples);
    let start_frames = ((speech.start_window_ms as usize).div_ceil(20)).max(1);
    let silence_frames = ((speech.trailing_silence_ms as usize).div_ceil(20)).max(1);
    let min_samples = ((speech.min_utterance_ms as usize * input_rate_hz as usize) / 1000).max(1);
    let max_samples = ((speech.max_utterance_ms as usize * input_rate_hz as usize) / 1000).max(1);

    let mut pending = Vec::new();
    let mut pre_roll = VecDeque::with_capacity(pre_roll_samples + frame_samples);
    let mut voiced_streak = 0usize;
    let mut silence_streak = 0usize;
    let mut recording = false;
    let mut captured = Vec::new();

    loop {
        if !recording || pending.len() < frame_samples {
            let chunk = rx
                .recv()
                .context("microphone stream ended before an utterance was captured")?;
            pending.extend(chunk);
        }

        while pending.len() >= frame_samples {
            let frame: Vec<f32> = pending.drain(..frame_samples).collect();
            let level = frame_rms(&frame);
            let is_voiced = level >= speech.level_threshold;

            if !recording {
                extend_ring(&mut pre_roll, &frame, pre_roll_samples);
                voiced_streak = if is_voiced { voiced_streak + 1 } else { 0 };

                if voiced_streak >= start_frames {
                    tracing::info!("speech detected; capturing utterance");
                    recording = true;
                    captured.extend(pre_roll.iter().copied());
                    captured.extend(frame.iter().copied());
                    silence_streak = 0;
                }
                continue;
            }

            captured.extend(frame.iter().copied());
            silence_streak = if is_voiced { 0 } else { silence_streak + 1 };

            if captured.len() >= max_samples {
                tracing::info!("ending capture at maximum utterance length");
                thread::sleep(cooldown);
                return Ok(captured);
            }

            if captured.len() >= min_samples && silence_streak >= silence_frames {
                tracing::info!("ending capture after trailing silence");
                thread::sleep(cooldown);
                return Ok(captured);
            }
        }
    }
}

#[cfg(feature = "audio-cpal")]
fn wait_for_wake_on_stream(
    rx: &Receiver<Vec<f32>>,
    input_rate_hz: u32,
    wake: &WakeConfig,
) -> Result<WakeDetection> {
    let mut sidecar = WakeSidecar::start(wake)?;
    let wake_frame_samples = ((input_rate_hz as usize * WAKE_FRAME_MS) / 1000).max(1);
    let mut pending = Vec::new();

    loop {
        if let Some(detection) = sidecar.try_detection() {
            sidecar.stop();
            return Ok(detection);
        }

        let chunk = rx
            .recv()
            .context("microphone stream ended before wake word was detected")?;
        pending.extend(chunk);

        while pending.len() >= wake_frame_samples {
            let frame: Vec<f32> = pending.drain(..wake_frame_samples).collect();
            let resampled = resample_mono(&frame, input_rate_hz, WAKE_SAMPLE_RATE_HZ);
            let pcm = f32_samples_to_i16(&resampled);
            sidecar.write_pcm_i16(&pcm)?;

            if let Some(detection) = sidecar.try_detection() {
                sidecar.stop();
                return Ok(detection);
            }
        }
    }
}

#[cfg(feature = "audio-cpal")]
fn f32_samples_to_i16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|sample| (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
        .collect()
}

#[cfg(feature = "audio-cpal")]
fn extend_ring(ring: &mut VecDeque<f32>, samples: &[f32], max_len: usize) {
    for sample in samples {
        ring.push_back(*sample);
    }

    while ring.len() > max_len {
        ring.pop_front();
    }
}

#[cfg(feature = "audio-cpal")]
fn frame_rms(frame: &[f32]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }

    let sum = frame.iter().map(|sample| sample * sample).sum::<f32>();
    (sum / frame.len() as f32).sqrt()
}

#[cfg(feature = "audio-cpal")]
fn resample_mono(samples: &[f32], input_rate_hz: u32, output_rate_hz: u32) -> Vec<f32> {
    if samples.is_empty() {
        return Vec::new();
    }

    if input_rate_hz == output_rate_hz {
        return samples.to_vec();
    }

    let output_len =
        ((samples.len() as u64 * output_rate_hz as u64) / input_rate_hz as u64).max(1) as usize;
    let mut output = Vec::with_capacity(output_len);

    for idx in 0..output_len {
        let source = idx as f64 * input_rate_hz as f64 / output_rate_hz as f64;
        let left = source.floor() as usize;
        let right = (left + 1).min(samples.len() - 1);
        let fraction = (source - left as f64) as f32;
        let a = samples[left];
        let b = samples[right];
        output.push(a + (b - a) * fraction);
    }

    output
}

#[cfg(feature = "audio-cpal")]
fn encode_wav_mono_i16(samples: &[f32], sample_rate_hz: u32) -> Result<Vec<u8>> {
    let mut cursor = std::io::Cursor::new(Vec::new());
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: sample_rate_hz,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    {
        let mut writer =
            hound::WavWriter::new(&mut cursor, spec).context("failed to create WAV writer")?;
        for sample in samples {
            let value = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            writer
                .write_sample(value)
                .context("failed to write WAV sample")?;
        }
        writer.finalize().context("failed to finalize WAV buffer")?;
    }

    Ok(cursor.into_inner())
}
