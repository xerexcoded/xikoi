//! Native host audio I/O and small, deterministic PCM utilities.
//!
//! CPAL callbacks only touch bounded lock-free queues and atomics. Inference and
//! persistence stay off the realtime audio threads.

use std::{
    io::Cursor,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};

use cpal::{
    FromSample, Sample, SampleFormat, SizedSample, Stream,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use crossbeam_queue::ArrayQueue;
use thiserror::Error;

const CAPTURE_QUEUE_SECONDS: usize = 10;
const MAX_CAPTURE_SECONDS: usize = 120;
const PLAYBACK_QUEUE_SECONDS: usize = 120;
pub const ASR_SAMPLE_RATE: u32 = 16_000;
pub const QWEN_TTS_SAMPLE_RATE: u32 = 24_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioDirection {
    Input,
    Output,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
    pub direction: AudioDirection,
    pub is_default: bool,
    pub channels: u16,
    pub sample_rate: u32,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AudioInventory {
    pub inputs: Vec<AudioDevice>,
    pub outputs: Vec<AudioDevice>,
}

impl AudioInventory {
    pub fn discover() -> Result<Self, AudioFailure> {
        let host = cpal::default_host();
        let default_input = host
            .default_input_device()
            .and_then(|device| device.id().ok())
            .map(|id| id.to_string());
        let default_output = host
            .default_output_device()
            .and_then(|device| device.id().ok())
            .map(|id| id.to_string());

        let inputs = host
            .input_devices()
            .map_err(AudioFailure::Cpal)?
            .filter_map(|device| {
                describe_device(&device, AudioDirection::Input, default_input.as_deref()).ok()
            })
            .collect();
        let outputs = host
            .output_devices()
            .map_err(AudioFailure::Cpal)?
            .filter_map(|device| {
                describe_device(&device, AudioDirection::Output, default_output.as_deref()).ok()
            })
            .collect();
        Ok(Self { inputs, outputs })
    }
}

pub struct CaptureSession {
    stream: Stream,
    samples: Arc<ArrayQueue<f32>>,
    captured: Vec<f32>,
    sample_rate: u32,
    maximum_samples: usize,
    level: Arc<AtomicU32>,
    overflowed: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    pub device_id: String,
    pub device_name: String,
}

impl CaptureSession {
    pub fn start(requested_device_id: Option<&str>) -> Result<Self, AudioFailure> {
        let host = cpal::default_host();
        let device = select_device(&host, AudioDirection::Input, requested_device_id)?;
        let device_id = device.id().map_err(AudioFailure::Cpal)?.to_string();
        let device_name = device
            .description()
            .map_err(AudioFailure::Cpal)?
            .name()
            .to_owned();
        let supported = device.default_input_config().map_err(AudioFailure::Cpal)?;
        let sample_rate = supported.sample_rate();
        let channels = usize::from(supported.channels());
        let samples = Arc::new(ArrayQueue::new(
            usize::try_from(sample_rate).unwrap_or(48_000) * CAPTURE_QUEUE_SECONDS,
        ));
        let level = Arc::new(AtomicU32::new(0));
        let overflowed = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));
        let config = supported.config();

        let stream = match supported.sample_format() {
            SampleFormat::F32 => build_input_stream::<f32>(
                &device,
                config,
                channels,
                Arc::clone(&samples),
                Arc::clone(&level),
                Arc::clone(&overflowed),
                Arc::clone(&failed),
            )?,
            SampleFormat::I16 => build_input_stream::<i16>(
                &device,
                config,
                channels,
                Arc::clone(&samples),
                Arc::clone(&level),
                Arc::clone(&overflowed),
                Arc::clone(&failed),
            )?,
            SampleFormat::U16 => build_input_stream::<u16>(
                &device,
                config,
                channels,
                Arc::clone(&samples),
                Arc::clone(&level),
                Arc::clone(&overflowed),
                Arc::clone(&failed),
            )?,
            format => return Err(AudioFailure::UnsupportedSampleFormat(format)),
        };
        stream.play().map_err(AudioFailure::Cpal)?;

        Ok(Self {
            stream,
            samples,
            captured: Vec::new(),
            sample_rate,
            maximum_samples: usize::try_from(sample_rate).unwrap_or(48_000) * MAX_CAPTURE_SECONDS,
            level,
            overflowed,
            failed,
            device_id,
            device_name,
        })
    }

    pub fn drain(&mut self) {
        while self.captured.len() < self.maximum_samples {
            let Some(sample) = self.samples.pop() else {
                break;
            };
            self.captured.push(sample);
        }
        if self.captured.len() == self.maximum_samples && !self.samples.is_empty() {
            self.overflowed.store(true, Ordering::Relaxed);
            while self.samples.pop().is_some() {}
        }
    }

    pub fn level(&self) -> u64 {
        u64::from(self.level.load(Ordering::Relaxed))
    }

    pub fn has_problem(&self) -> bool {
        self.overflowed.load(Ordering::Relaxed) || self.failed.load(Ordering::Relaxed)
    }

    pub fn finish(mut self) -> RecordedAudio {
        let _ = self.stream.pause();
        self.drain();
        RecordedAudio {
            samples: self.captured,
            sample_rate: self.sample_rate,
        }
    }
}

pub struct PlaybackSession {
    stream: Stream,
    samples: Arc<ArrayQueue<f32>>,
    sample_rate: u32,
    level: Arc<AtomicU32>,
    overflowed: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    pending_byte: Option<u8>,
    pub device_id: String,
    pub device_name: String,
}

impl PlaybackSession {
    pub fn start(requested_device_id: Option<&str>) -> Result<Self, AudioFailure> {
        let host = cpal::default_host();
        let device = select_device(&host, AudioDirection::Output, requested_device_id)?;
        let device_id = device.id().map_err(AudioFailure::Cpal)?.to_string();
        let device_name = device
            .description()
            .map_err(AudioFailure::Cpal)?
            .name()
            .to_owned();
        let supported = device.default_output_config().map_err(AudioFailure::Cpal)?;
        let sample_rate = supported.sample_rate();
        let channels = usize::from(supported.channels());
        let samples = Arc::new(ArrayQueue::new(
            usize::try_from(sample_rate).unwrap_or(48_000) * PLAYBACK_QUEUE_SECONDS,
        ));
        let level = Arc::new(AtomicU32::new(0));
        let overflowed = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));
        let config = supported.config();

        let stream = match supported.sample_format() {
            SampleFormat::F32 => build_output_stream::<f32>(
                &device,
                config,
                channels,
                Arc::clone(&samples),
                Arc::clone(&level),
                Arc::clone(&failed),
            )?,
            SampleFormat::I16 => build_output_stream::<i16>(
                &device,
                config,
                channels,
                Arc::clone(&samples),
                Arc::clone(&level),
                Arc::clone(&failed),
            )?,
            SampleFormat::U16 => build_output_stream::<u16>(
                &device,
                config,
                channels,
                Arc::clone(&samples),
                Arc::clone(&level),
                Arc::clone(&failed),
            )?,
            format => return Err(AudioFailure::UnsupportedSampleFormat(format)),
        };
        stream.play().map_err(AudioFailure::Cpal)?;

        Ok(Self {
            stream,
            samples,
            sample_rate,
            level,
            overflowed,
            failed,
            pending_byte: None,
            device_id,
            device_name,
        })
    }

    pub fn enqueue_pcm16(&mut self, bytes: &[u8], source_rate: u32) {
        let mut pcm = Vec::with_capacity(bytes.len() / 2 + 1);
        let mut chunks = bytes.chunks_exact(2);
        if let Some(low) = self.pending_byte.take() {
            if let Some((&high, rest)) = bytes.split_first() {
                pcm.push(f32::from(i16::from_le_bytes([low, high])) / 32_768.0);
                chunks = rest.chunks_exact(2);
            } else {
                self.pending_byte = Some(low);
                return;
            }
        }
        pcm.extend(
            chunks
                .by_ref()
                .map(|chunk| f32::from(i16::from_le_bytes([chunk[0], chunk[1]])) / 32_768.0),
        );
        self.pending_byte = chunks.remainder().first().copied();

        for sample in resample_linear(&pcm, source_rate, self.sample_rate) {
            if self.samples.push(sample).is_err() {
                self.overflowed.store(true, Ordering::Relaxed);
                break;
            }
        }
    }

    pub fn level(&self) -> u64 {
        u64::from(self.level.load(Ordering::Relaxed))
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn has_problem(&self) -> bool {
        self.overflowed.load(Ordering::Relaxed) || self.failed.load(Ordering::Relaxed)
    }

    pub fn stop(self) {
        let _ = self.stream.pause();
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RecordedAudio {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

impl RecordedAudio {
    #[allow(clippy::cast_precision_loss)]
    pub fn duration_seconds(&self) -> f32 {
        if self.sample_rate == 0 {
            return 0.0;
        }
        self.samples.len() as f32 / self.sample_rate as f32
    }

    #[allow(clippy::cast_possible_truncation)]
    pub fn to_asr_wav(&self) -> Result<Vec<u8>, AudioFailure> {
        let resampled = resample_linear(&self.samples, self.sample_rate, ASR_SAMPLE_RATE);
        let mut cursor = Cursor::new(Vec::new());
        {
            let spec = hound::WavSpec {
                channels: 1,
                sample_rate: ASR_SAMPLE_RATE,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            };
            let mut writer = hound::WavWriter::new(&mut cursor, spec).map_err(AudioFailure::Wav)?;
            for sample in resampled {
                let value = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16;
                writer.write_sample(value).map_err(AudioFailure::Wav)?;
            }
            writer.finalize().map_err(AudioFailure::Wav)?;
        }
        Ok(cursor.into_inner())
    }
}

fn describe_device(
    device: &cpal::Device,
    direction: AudioDirection,
    default_id: Option<&str>,
) -> Result<AudioDevice, AudioFailure> {
    let id = device.id().map_err(AudioFailure::Cpal)?.to_string();
    let description = device.description().map_err(AudioFailure::Cpal)?;
    let config = match direction {
        AudioDirection::Input => device.default_input_config(),
        AudioDirection::Output => device.default_output_config(),
    }
    .map_err(AudioFailure::Cpal)?;
    Ok(AudioDevice {
        is_default: default_id == Some(id.as_str()),
        id,
        name: description.name().to_owned(),
        direction,
        channels: config.channels(),
        sample_rate: config.sample_rate(),
    })
}

fn select_device(
    host: &cpal::Host,
    direction: AudioDirection,
    requested_id: Option<&str>,
) -> Result<cpal::Device, AudioFailure> {
    if let Some(id) = requested_id {
        let mut devices = match direction {
            AudioDirection::Input => host
                .input_devices()
                .map_err(AudioFailure::Cpal)?
                .collect::<Vec<_>>(),
            AudioDirection::Output => host
                .output_devices()
                .map_err(AudioFailure::Cpal)?
                .collect::<Vec<_>>(),
        };
        if let Some(position) = devices.iter().position(|device| {
            device
                .id()
                .is_ok_and(|candidate| candidate.to_string() == id)
        }) {
            return Ok(devices.swap_remove(position));
        }
    }
    match direction {
        AudioDirection::Input => host.default_input_device(),
        AudioDirection::Output => host.default_output_device(),
    }
    .ok_or(AudioFailure::NoDevice(direction))
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn build_input_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    channels: usize,
    samples: Arc<ArrayQueue<f32>>,
    level: Arc<AtomicU32>,
    overflowed: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
) -> Result<Stream, AudioFailure>
where
    T: SizedSample + Copy,
    f32: FromSample<T>,
{
    device
        .build_input_stream::<T, _, _>(
            config,
            move |data, _| {
                let mut energy = 0.0_f32;
                let mut count = 0_u32;
                for frame in data.chunks(channels.max(1)) {
                    let mono = frame.iter().copied().map(f32::from_sample).sum::<f32>()
                        / frame.len().max(1) as f32;
                    energy += mono * mono;
                    count = count.saturating_add(1);
                    if samples.push(mono).is_err() {
                        overflowed.store(true, Ordering::Relaxed);
                    }
                }
                let rms = if count == 0 {
                    0.0
                } else {
                    (energy / count as f32).sqrt()
                };
                level.store((rms.clamp(0.0, 1.0) * 1000.0) as u32, Ordering::Relaxed);
            },
            move |_| failed.store(true, Ordering::Relaxed),
            None,
        )
        .map_err(AudioFailure::Cpal)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn build_output_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    channels: usize,
    samples: Arc<ArrayQueue<f32>>,
    level: Arc<AtomicU32>,
    failed: Arc<AtomicBool>,
) -> Result<Stream, AudioFailure>
where
    T: SizedSample + FromSample<f32>,
{
    device
        .build_output_stream::<T, _, _>(
            config,
            move |output, _| {
                let mut energy = 0.0_f32;
                let mut count = 0_u32;
                for frame in output.chunks_mut(channels.max(1)) {
                    let sample = samples.pop().unwrap_or(0.0);
                    energy += sample * sample;
                    count = count.saturating_add(1);
                    for channel in frame {
                        *channel = T::from_sample(sample);
                    }
                }
                let rms = if count == 0 {
                    0.0
                } else {
                    (energy / count as f32).sqrt()
                };
                level.store((rms.clamp(0.0, 1.0) * 1000.0) as u32, Ordering::Relaxed);
            },
            move |_| failed.store(true, Ordering::Relaxed),
            None,
        )
        .map_err(AudioFailure::Cpal)
}

#[allow(clippy::cast_precision_loss)]
pub fn resample_linear(input: &[f32], source_rate: u32, target_rate: u32) -> Vec<f32> {
    if input.is_empty() || source_rate == 0 || target_rate == 0 {
        return Vec::new();
    }
    if source_rate == target_rate {
        return input.to_vec();
    }
    let source_rate = usize::try_from(source_rate).unwrap_or(usize::MAX);
    let target_rate = usize::try_from(target_rate).unwrap_or(usize::MAX);
    let output_len = input
        .len()
        .saturating_mul(target_rate)
        .checked_div(source_rate)
        .unwrap_or_default()
        .max(1);
    (0..output_len)
        .map(|index| {
            let position = index.saturating_mul(source_rate);
            let left = position / target_rate;
            let right = (left + 1).min(input.len() - 1);
            let fraction = (position % target_rate) as f32 / target_rate as f32;
            input[left.min(input.len() - 1)] * (1.0 - fraction) + input[right] * fraction
        })
        .collect()
}

#[derive(Debug, Error)]
pub enum AudioFailure {
    #[error("audio host error: {0}")]
    Cpal(#[source] cpal::Error),
    #[error("no {0:?} audio device is available")]
    NoDevice(AudioDirection),
    #[error("audio sample format {0} is not supported yet")]
    UnsupportedSampleFormat(SampleFormat),
    #[error("could not encode captured audio: {0}")]
    Wav(#[source] hound::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_resampling_preserves_duration_and_endpoints() {
        let input = vec![0.0, 0.5, 1.0, 0.5];
        let output = resample_linear(&input, 4, 8);
        assert_eq!(output.len(), 8);
        assert!(output[0].abs() < f32::EPSILON);
        assert!((output[2] - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn captured_audio_is_encoded_as_mono_16khz_wav() {
        let audio = RecordedAudio {
            samples: vec![0.0; 48_000],
            sample_rate: 48_000,
        };
        let wav = audio.to_asr_wav().unwrap();
        let reader = hound::WavReader::new(Cursor::new(wav)).unwrap();
        assert_eq!(reader.spec().channels, 1);
        assert_eq!(reader.spec().sample_rate, ASR_SAMPLE_RATE);
        assert_eq!(reader.duration(), ASR_SAMPLE_RATE);
    }
}
