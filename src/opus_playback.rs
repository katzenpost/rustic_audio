use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ogg::reading::PacketReader;
use opus_rs::OpusDecoder;
use std::fs::File;
use std::io::{BufReader, Cursor, Read, Seek};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const SAMPLE_RATE: u32 = 48_000;
// The longest packet Opus allows: 120 ms.
const MAX_PACKET_SAMPLES: usize = 5760;
const FRAME_QUEUE_CAPACITY: usize = 8;
const STARTUP_FRAME_WAIT: Duration = Duration::from_secs(1);

const PRODUCER_RUNNING: u8 = 0;
const PRODUCER_COMPLETED: u8 = 1;
const PRODUCER_FAILED: u8 = 2;

type PcmFrame = Vec<f32>;

pub fn get_opus_info(file_path: &str) -> Result<(u64, f64), Box<dyn std::error::Error>> {
    let file = File::open(file_path)?;
    let file_size = file.metadata()?.len();

    // Count the number of audio packets to estimate duration
    let reader = BufReader::new(file);
    let mut packet_reader = PacketReader::new(reader);

    // Skip headers
    packet_reader.read_packet()?; // OpusHead
    packet_reader.read_packet()?; // OpusTags

    let mut samples = 0;
    while let Ok(Some(packet)) = packet_reader.read_packet() {
        samples += packet_samples(&packet.data).unwrap_or(0);
    }
    let duration = samples as f64 / SAMPLE_RATE as f64;

    Ok((file_size, duration))
}

/// Samples at 48 kHz in an Opus packet, from its TOC byte (RFC 6716 3.1), or `None` if it is
/// not a legal packet.
fn packet_samples(packet: &[u8]) -> Option<usize> {
    let toc = *packet.first()?;
    let config = (toc >> 3) as usize;
    let per_frame = match config {
        0..=11 => [480, 960, 1920, 2880][config & 3], // SILK
        12..=15 => [480, 960][config & 1],            // Hybrid
        _ => [120, 240, 480, 960][config & 3],        // CELT
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => (*packet.get(1)? & 0x3f) as usize,
    };
    let samples = per_frame * frames;
    (frames > 0 && samples <= MAX_PACKET_SAMPLES).then_some(samples)
}

/// Where a clip to play is: a file, or bytes already in memory, such as a clip decrypted for
/// playing that should never be written out.
pub(crate) enum ClipSource {
    File(String),
    Bytes(Vec<u8>),
}

trait ClipReader: Read + Seek + Send {}

impl<T: Read + Seek + Send> ClipReader for T {}

impl ClipSource {
    fn reader(self) -> Result<Box<dyn ClipReader>, String> {
        match self {
            ClipSource::File(path) => Ok(Box::new(BufReader::new(
                File::open(path).map_err(|err| err.to_string())?,
            ))),
            ClipSource::Bytes(bytes) => Ok(Box::new(Cursor::new(bytes))),
        }
    }
}

pub fn playback_opus(file_path: &str, is_playing_flag: Arc<AtomicBool>) -> Result<(), String> {
    playback(ClipSource::File(file_path.to_string()), is_playing_flag)
}

pub(crate) fn playback(clip: ClipSource, is_playing_flag: Arc<AtomicBool>) -> Result<(), String> {
    let host = cpal::default_host();
    let device = host.default_output_device()
        .ok_or_else(|| "Failed to get default output device".to_string())?;
    let config = device.default_output_config().map_err(|err| err.to_string())?;

    let output_config = cpal::StreamConfig {
        channels: config.channels(),
        sample_rate: SAMPLE_RATE,
        buffer_size: cpal::BufferSize::Default,
    };
    let output_channels = output_config.channels as usize;

    let (frame_tx, frame_rx) = sync_channel(FRAME_QUEUE_CAPACITY);
    let producer_state = Arc::new(AtomicU8::new(PRODUCER_RUNNING));
    let producer_error = Arc::new(Mutex::new(None));
    let producer_handle = spawn_frame_producer(
        clip,
        frame_tx,
        Arc::clone(&is_playing_flag),
        Arc::clone(&producer_state),
        Arc::clone(&producer_error),
    );

    let mut current_frame = wait_for_startup_frame(
        &frame_rx,
        &producer_state,
        &producer_error,
        &is_playing_flag,
    )?;
    if current_frame.is_none() {
        join_frame_producer(producer_handle)?;
        return Ok(());
    }
    let mut frame_position = 0usize;

    let callback_error = Arc::new(Mutex::new(None));

    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => {
            let is_playing = Arc::clone(&is_playing_flag);
            let is_playing_errors = Arc::clone(&is_playing_flag);
            let producer_state = Arc::clone(&producer_state);
            let callback_error = Arc::clone(&callback_error);
            let callback_error_errors = Arc::clone(&callback_error);

            device.build_output_stream(
                &output_config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    if !is_playing.load(Ordering::Relaxed) {
                        data.fill(0.0);
                        return;
                    }

                    let mut pos = 0;
                    while pos < data.len() {
                        let needs_frame = current_frame
                            .as_ref()
                            .map(|frame| frame_position >= frame.len())
                            .unwrap_or(true);
                        if needs_frame {
                            match frame_rx.try_recv() {
                                Ok(frame) => {
                                    current_frame = Some(frame);
                                    frame_position = 0;
                                    continue;
                                }
                                Err(TryRecvError::Empty) => {
                                    let state = producer_state.load(Ordering::Relaxed);
                                    if state == PRODUCER_COMPLETED || state == PRODUCER_FAILED {
                                        is_playing.store(false, Ordering::Relaxed);
                                    }
                                    break;
                                }
                                Err(TryRecvError::Disconnected) => {
                                    if producer_state.load(Ordering::Relaxed) == PRODUCER_RUNNING {
                                        store_error_once(
                                            callback_error.as_ref(),
                                            "playback producer disconnected unexpectedly"
                                                .to_string(),
                                        );
                                    }
                                    is_playing.store(false, Ordering::Relaxed);
                                    break;
                                }
                            }
                        }

                        let Some(frame) = current_frame.as_ref() else {
                            break;
                        };
                        let copied = copy_frame_to_output(
                            &mut data[pos..],
                            output_channels,
                            frame,
                            &mut frame_position,
                        );
                        if copied == 0 {
                            current_frame = None;
                            continue;
                        }
                        pos += copied;
                    }

                    data[pos..].fill(0.0);
                },
                move |err| {
                    store_error_once(
                        callback_error_errors.as_ref(),
                        format!("stream callback error: {err:?}"),
                    );
                    is_playing_errors.store(false, Ordering::Relaxed);
                },
                None,
            )
            .map_err(|err| err.to_string())?
        },
        _ => return Err("Unsupported output format".to_string()),
    };

    stream.play().map_err(|err| err.to_string())?;

    while is_playing_flag.load(Ordering::Relaxed) {
        if callback_error.lock().is_ok_and(|error| error.is_some()) {
            is_playing_flag.store(false, Ordering::Relaxed);
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }

    drop(stream);
    join_frame_producer(producer_handle)?;

    if let Some(error) = take_error(callback_error.as_ref()) {
        return Err(error);
    }

    if producer_state.load(Ordering::Relaxed) == PRODUCER_FAILED {
        return Err(
            take_error(producer_error.as_ref())
                .unwrap_or_else(|| "playback producer failed".to_string()),
        );
    }

    Ok(())
} 

fn spawn_frame_producer(
    clip: ClipSource,
    frame_tx: SyncSender<PcmFrame>,
    is_playing: Arc<AtomicBool>,
    producer_state: Arc<AtomicU8>,
    producer_error: Arc<Mutex<Option<String>>>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        match clip
            .reader()
            .and_then(|reader| produce_frames(reader, frame_tx, is_playing))
        {
            Ok(()) => producer_state.store(PRODUCER_COMPLETED, Ordering::Relaxed),
            Err(err) => {
                store_error_once(producer_error.as_ref(), err);
                producer_state.store(PRODUCER_FAILED, Ordering::Relaxed);
            }
        }
    })
}

fn produce_frames(
    reader: impl Read + Seek,
    frame_tx: SyncSender<PcmFrame>,
    is_playing: Arc<AtomicBool>,
) -> Result<(), String> {
    let mut decoder = OpusDecoder::new(SAMPLE_RATE as i32, 1).map_err(|err| err.to_string())?;
    let mut packet_reader = PacketReader::new(reader);
    let mut decode_buffer = vec![0f32; MAX_PACKET_SAMPLES];

    packet_reader
        .read_packet()
        .map_err(|err| err.to_string())?;
    packet_reader
        .read_packet()
        .map_err(|err| err.to_string())?;

    while is_playing.load(Ordering::Relaxed) {
        let Some(packet) = packet_reader.read_packet().map_err(|err| err.to_string())? else {
            return Ok(());
        };
        if packet.data.is_empty() {
            continue;
        }
        let frame_size = packet_samples(&packet.data).ok_or("invalid Opus packet")?;
        let n_samples = match decoder.decode(&packet.data, frame_size, &mut decode_buffer) {
            Ok(n_samples) => n_samples,
            Err("Input packet empty") => continue,
            Err(err) => return Err(err.to_string()),
        };
        if n_samples == 0 {
            continue;
        }
        if frame_tx.send(decode_buffer[..n_samples].to_vec()).is_err() {
            return Ok(());
        }
    }

    Ok(())
}

fn wait_for_startup_frame(
    frame_rx: &Receiver<PcmFrame>,
    producer_state: &Arc<AtomicU8>,
    producer_error: &Arc<Mutex<Option<String>>>,
    is_playing: &Arc<AtomicBool>,
) -> Result<Option<PcmFrame>, String> {
    loop {
        if !is_playing.load(Ordering::Relaxed) {
            return Ok(None);
        }

        match frame_rx.recv_timeout(STARTUP_FRAME_WAIT) {
            Ok(frame) => return Ok(Some(frame)),
            Err(RecvTimeoutError::Timeout) => match producer_state.load(Ordering::Relaxed) {
                PRODUCER_COMPLETED => return Ok(None),
                PRODUCER_FAILED => {
                    return Err(
                        take_error(producer_error.as_ref())
                            .unwrap_or_else(|| "playback producer failed".to_string()),
                    );
                }
                _ => continue,
            },
            Err(RecvTimeoutError::Disconnected) => match producer_state.load(Ordering::Relaxed) {
                PRODUCER_COMPLETED => return Ok(None),
                PRODUCER_FAILED => {
                    return Err(
                        take_error(producer_error.as_ref())
                            .unwrap_or_else(|| "playback producer failed".to_string()),
                    );
                }
                _ => {
                    return Err("playback producer disconnected unexpectedly".to_string())
                }
            },
        }
    }
}

fn join_frame_producer(handle: thread::JoinHandle<()>) -> Result<(), String> {
    handle
        .join()
        .map_err(|_| "failed to join playback producer thread".to_string())
}

fn copy_frame_to_output(
    data: &mut [f32],
    channels: usize,
    frame: &[f32],
    frame_position: &mut usize,
) -> usize {
    let remaining_samples = frame.len().saturating_sub(*frame_position);
    if remaining_samples == 0 {
        return 0;
    }

    let samples_to_copy = (data.len() / channels).min(remaining_samples);
    for i in 0..samples_to_copy {
        let sample = frame[*frame_position + i];
        for c in 0..channels {
            data[i * channels + c] = sample;
        }
    }

    *frame_position += samples_to_copy;
    samples_to_copy * channels
}

fn store_error_once(slot: &Mutex<Option<String>>, message: String) {
    if let Ok(mut error) = slot.lock() {
        if error.is_none() {
            *error = Some(message);
        }
    }
}

fn take_error(slot: &Mutex<Option<String>>) -> Option<String> {
    slot.lock().ok()?.take()
}

#[cfg(test)]
mod tests {
    use super::{copy_frame_to_output, produce_frames, ClipSource};
    use crate::opus_encoder::OpusEncoder;
    use hound::{SampleFormat, WavSpec, WavWriter};
    use ogg::writing::PacketWriteEndInfo;
    use std::fs;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc::sync_channel;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn produce_frames_skips_empty_end_of_stream_packet() {
        let unique_suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!(
            "rustic-audio-eos-test-{unique_suffix}"
        ));
        fs::create_dir_all(&temp_dir).unwrap();

        let wav_path = temp_dir.join("test.wav");
        let opus_path = temp_dir.join("test.opus");

        let spec = WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut writer = WavWriter::create(&wav_path, spec).unwrap();
        for _ in 0..(960 * 2) {
            writer.write_sample(0i16).unwrap();
        }
        writer.finalize().unwrap();

        OpusEncoder::new()
            .encode_wav_to_opus(
                wav_path.to_str().unwrap(),
                opus_path.to_str().unwrap(),
            )
            .unwrap();

        let (frame_tx, frame_rx) = sync_channel(8);
        let is_playing = Arc::new(AtomicBool::new(true));
        produce_frames(
            ClipSource::File(opus_path.to_str().unwrap().to_string())
                .reader()
                .unwrap(),
            frame_tx,
            Arc::clone(&is_playing),
        )
        .expect("playback should not fail on empty EOS packet");

        let frame_count = frame_rx.iter().count();
        assert!(
            frame_count > 0,
            "expected at least one decoded audio frame, got {frame_count}"
        );

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn clip_in_memory_decodes_as_from_file() {
        let unique_suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!(
            "rustic-audio-bytes-test-{unique_suffix}"
        ));
        fs::create_dir_all(&temp_dir).unwrap();

        let wav_path = temp_dir.join("test.wav");
        let opus_path = temp_dir.join("test.opus");

        let spec = WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut writer = WavWriter::create(&wav_path, spec).unwrap();
        for n in 0..(960 * 10) {
            let t = n as f32 / 48_000.0;
            writer
                .write_sample(((t * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16)
                .unwrap();
        }
        writer.finalize().unwrap();

        OpusEncoder::new()
            .encode_wav_to_opus(
                wav_path.to_str().unwrap(),
                opus_path.to_str().unwrap(),
            )
            .unwrap();

        let frames = |clip: ClipSource| {
            let (frame_tx, frame_rx) = sync_channel(64);
            produce_frames(
                clip.reader().unwrap(),
                frame_tx,
                Arc::new(AtomicBool::new(true)),
            )
            .unwrap();
            frame_rx.iter().collect::<Vec<_>>()
        };
        let from_file = frames(ClipSource::File(opus_path.to_str().unwrap().to_string()));
        let from_bytes = frames(ClipSource::Bytes(fs::read(&opus_path).unwrap()));
        assert!(from_file.len() >= 5, "got {} frames", from_file.len());
        assert_eq!(from_bytes, from_file);

        let _ = fs::remove_dir_all(temp_dir);
    }

    /// One second of a 440 Hz tone encoded by libopus at 8 kbit/s (tests/data/make_opus_fixtures.py),
    /// wrapped in Ogg as a clip is, decoded to its samples.
    fn decode_libopus_tone(fixture: &[u8]) -> (usize, Vec<f32>) {
        let mut ogg = Vec::new();
        let mut writer = ogg::PacketWriter::new(std::io::Cursor::new(&mut ogg));
        let head = b"OpusHead\x01\x01\0\0\x80\xbb\0\0\0\0\0".to_vec();
        writer.write_packet(head, 1, PacketWriteEndInfo::EndPage, 0).unwrap();
        writer
            .write_packet(b"OpusTags\0\0\0\0\0\0\0\0".to_vec(), 1, PacketWriteEndInfo::EndPage, 0)
            .unwrap();
        let mut packets = Vec::new();
        let mut rest = fixture;
        while !rest.is_empty() {
            let len = u16::from_le_bytes([rest[0], rest[1]]) as usize;
            packets.push(rest[2..2 + len].to_vec());
            rest = &rest[2 + len..];
        }
        let count = packets.len();
        for (i, packet) in packets.into_iter().enumerate() {
            let end = if i + 1 == count {
                PacketWriteEndInfo::EndStream
            } else {
                PacketWriteEndInfo::NormalPacket
            };
            writer.write_packet(packet, 1, end, 0).unwrap();
        }
        drop(writer);

        let (frame_tx, frame_rx) = sync_channel(256);
        produce_frames(
            ClipSource::Bytes(ogg).reader().unwrap(),
            frame_tx,
            Arc::new(AtomicBool::new(true)),
        )
        .unwrap();
        (count, frame_rx.iter().flatten().collect())
    }

    fn assert_plays_tone(fixture: &[u8], samples_per_packet: usize) {
        let (packets, pcm) = decode_libopus_tone(fixture);
        assert_eq!(pcm.len(), packets * samples_per_packet);
        // After the codec settles, the tone is there: most energy at 440 Hz (Goertzel).
        let tail = &pcm[pcm.len() / 4..];
        let k = 2.0 * (std::f32::consts::TAU * 440.0 / 48_000.0).cos();
        let (mut s1, mut s2) = (0f32, 0f32);
        for &x in tail {
            (s1, s2) = (x + k * s1 - s2, s1);
        }
        let tone = (s1 * s1 + s2 * s2 - k * s1 * s2) * 2.0 / tail.len() as f32;
        let energy = tail.iter().map(|x| x * x).sum::<f32>();
        assert!(tone > 0.5 * energy, "tone {tone} of energy {energy}");
    }

    #[test]
    fn plays_libopus_20ms_frames() {
        assert_plays_tone(include_bytes!("../tests/data/tone_20ms.bin"), 960);
    }

    #[test]
    fn plays_libopus_40ms_frames() {
        assert_plays_tone(include_bytes!("../tests/data/tone_40ms.bin"), 1920);
    }

    #[test]
    fn plays_libopus_60ms_frames() {
        assert_plays_tone(include_bytes!("../tests/data/tone_60ms.bin"), 2880);
    }

    #[test]
    fn plays_libopus_120ms_frames() {
        assert_plays_tone(include_bytes!("../tests/data/tone_120ms.bin"), 5760);
    }

    #[test]
    fn copy_frame_to_output_expands_mono_samples_across_channels() {
        let mut output = [0.0; 6];
        let mut frame_position = 0;

        let copied = copy_frame_to_output(&mut output, 2, &[0.25, -0.5, 1.0], &mut frame_position);

        assert_eq!(copied, 6);
        assert_eq!(frame_position, 3);
        assert_eq!(output, [0.25, 0.25, -0.5, -0.5, 1.0, 1.0]);
    }

    #[test]
    fn copy_frame_to_output_respects_existing_frame_offset() {
        let mut output = [0.0; 4];
        let mut frame_position = 1;

        let copied = copy_frame_to_output(&mut output, 2, &[0.1, 0.2, 0.3], &mut frame_position);

        assert_eq!(copied, 4);
        assert_eq!(frame_position, 3);
        assert_eq!(output, [0.2, 0.2, 0.3, 0.3]);
    }
}