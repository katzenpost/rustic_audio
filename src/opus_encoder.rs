use ogg::{PacketWriter, writing::PacketWriteEndInfo};
use opus_rs::{Application, OpusEncoder as CodecEncoder};
use std::fs::File;
use std::io::BufWriter;
use zeroize::Zeroize;

use crate::secure::SecureVec;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OpusEncodingMode {
    Cbr,
    Vbr,
}

#[derive(Clone)]
pub struct OpusEncoder {
    bitrate: i32,
    mode: OpusEncodingMode,
    vbr_quality: i32,
}

impl OpusEncoder {
    pub fn new() -> Self {
        Self {
            bitrate: 12000, // Default 12kbps
            mode: OpusEncodingMode::Cbr,
            vbr_quality: 5,
        }
    }

    // Add setter for bitrate
    pub fn set_bitrate(&mut self, bitrate: i32) {
        self.bitrate = bitrate;
    }

    // Get current bitrate
    pub fn get_bitrate(&self) -> i32 {
        self.bitrate
    }

    pub fn set_mode(&mut self, mode: OpusEncodingMode) {
        self.mode = mode;
    }

    pub fn get_mode(&self) -> OpusEncodingMode {
        self.mode
    }

    pub fn set_vbr_quality(&mut self, quality: i32) {
        self.vbr_quality = quality.clamp(0, 10);
    }

    pub fn get_vbr_quality(&self) -> i32 {
        self.vbr_quality
    }

    pub fn encode_wav_to_opus(&self, input_path: &str, output_path: &str) -> Result<(), Box<dyn std::error::Error>> {
        // Read the WAV file
        let mut reader = hound::WavReader::open(input_path)?;
        let spec = reader.spec();
        
        // Convert to 48kHz mono if needed
        let samples: Vec<f32> = if spec.sample_format == hound::SampleFormat::Float {
            reader.samples::<f32>().map(|s| s.unwrap()).collect()
        } else {
            reader.samples::<i16>().map(|s| s.unwrap() as f32 / 32768.0).collect()
        };
        
        // Convert to mono if stereo
        let mono_samples: Vec<f32> = if spec.channels == 2 {
            samples.chunks(2)
                .map(|chunk| chunk[0]) // Take left channel
                .collect()
        } else {
            samples
        };
        
        // Resample to 48kHz if needed
        let resampled_samples = if spec.sample_rate != 48000 {
            let input_duration = mono_samples.len() as f32 / spec.sample_rate as f32;
            let output_len = (input_duration * 48000.0) as usize;
            let scale = (mono_samples.len() - 1) as f32 / (output_len - 1) as f32;
            
            let mut output = Vec::with_capacity(output_len);
            for i in 0..output_len {
                let pos = i as f32 * scale;
                let index = pos as usize;
                let frac = pos - index as f32;
                
                let sample = if index + 1 < mono_samples.len() {
                    mono_samples[index] * (1.0 - frac) + mono_samples[index + 1] * frac
                } else {
                    mono_samples[index]
                };
                
                output.push(sample);
            }
            output
        } else {
            mono_samples
        };
        
        let mut encoder = CodecEncoder::new(48_000, 1, Application::Audio)
            .map_err(std::io::Error::other)?;
        encoder.bitrate_bps = self.bitrate;
        encoder.use_cbr = matches!(self.mode, OpusEncodingMode::Cbr);
        encoder.complexity = self.vbr_quality;
        
        println!("Converting to Opus:");
        println!("  Mode: {:?}", self.mode_name());
        println!("  Bitrate target: {} bps", self.bitrate);
        println!("  VBR quality: {}", self.vbr_quality);
        println!("  Frame size: 960 samples (20ms at 48kHz)");
        println!("  Total frames: {}", resampled_samples.len() / 960);

        let file = BufWriter::new(File::create(output_path)?);
        let serial = content_serial(&resampled_samples);
        let mut packet_writer = PacketWriter::new(file);

        // Opus header
        let mut id_header = Vec::new();
        id_header.extend_from_slice(b"OpusHead");
        id_header.push(1);  // Version
        id_header.push(1);  // Channel count
        id_header.extend_from_slice(&(0u16).to_le_bytes());  // Pre-skip
        id_header.extend_from_slice(&(48000u32).to_le_bytes());  // Input sample rate
        id_header.extend_from_slice(&[0, 0]);  // Output gain
        id_header.push(0);  // Channel mapping family

        packet_writer.write_packet(
            id_header,
            serial,
            PacketWriteEndInfo::EndPage,
            0
        )?;

        // Comment header
        let mut comment_header = Vec::new();
        comment_header.extend_from_slice(b"OpusTags");
        let vendor = b"";
        comment_header.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        comment_header.extend_from_slice(vendor);
        comment_header.extend_from_slice(&[0, 0, 0, 0]);

        packet_writer.write_packet(
            comment_header,
            serial,
            PacketWriteEndInfo::EndPage,
            0
        )?;

        let frame_size = 960;  // 20ms at 48kHz
        let mut input_buffer = vec![0.0f32; frame_size];
        let mut encoded_data = vec![0u8; 1275];
        let mut granulepos = 0i64;
        let frames: Vec<&[f32]> = resampled_samples.chunks(frame_size).collect();

        for (index, chunk) in frames.iter().enumerate() {
            input_buffer.clear();
            input_buffer.extend(*chunk);
            if input_buffer.len() < frame_size {
                input_buffer.resize(frame_size, 0.0);
            }

            let encoded_len = encoder
                .encode(&input_buffer, frame_size, &mut encoded_data)
                .map_err(std::io::Error::other)?;
            let encoded_packet = &encoded_data[..encoded_len];

            granulepos += frame_size as i64;

            let end_info = if index + 1 == frames.len() {
                PacketWriteEndInfo::EndStream
            } else {
                PacketWriteEndInfo::NormalPacket
            };

            packet_writer.write_packet(
                encoded_packet.to_vec(),
                serial,
                end_info,
                granulepos as u64
            )?;
        }

        let final_duration = granulepos as f32 / 48000.0;
        println!("Final Opus duration: {} seconds", final_duration);

        Ok(())
    }

    fn mode_name(&self) -> &'static str {
        match self.mode {
            OpusEncodingMode::Cbr => "CBR",
            OpusEncodingMode::Vbr => "VBR",
        }
    }
}

/// Encode mono samples at `rate` Hz as Ogg Opus at 48 kHz, as [`OpusEncoder::encode_wav_to_opus`]
/// does, but in memory: the samples, the frames and the stream never leave guarded memory or the
/// stack, which is wiped. Returns the stream and its length in seconds.
pub(crate) fn encode_in_memory(
    encoder: &OpusEncoder,
    samples: &[f32],
    rate: u32,
) -> Result<(SecureVec<u8>, f64), String> {
    if samples.len() < 2 {
        return Err("nothing was recorded".to_string());
    }
    let mut codec = CodecEncoder::new(48_000, 1, Application::Audio).map_err(|err| err.to_string())?;
    codec.bitrate_bps = encoder.bitrate;
    codec.use_cbr = matches!(encoder.mode, OpusEncodingMode::Cbr);
    codec.complexity = encoder.vbr_quality;

    let mut out = SecureVec::new();
    let mut pages = OggPages::new(content_serial(samples));
    let mut head = [0u8; 19];
    head[..8].copy_from_slice(b"OpusHead");
    head[8] = 1; // version
    head[9] = 1; // channels
    head[12..16].copy_from_slice(&48_000u32.to_le_bytes());
    pages.page(&mut out, &[&head], 0, BOS);
    pages.page(&mut out, &[b"OpusTags\0\0\0\0\0\0\0\0"], 0, 0);

    // Linear resampling to 48 kHz, frame by frame.
    let out_len = if rate == 48_000 {
        samples.len()
    } else {
        (samples.len() as f64 / rate as f64 * 48_000.0) as usize
    };
    let scale = (samples.len() - 1) as f64 / (out_len.max(2) - 1) as f64;
    let at = |i: usize| -> f32 {
        if rate == 48_000 {
            return samples[i];
        }
        let pos = i as f64 * scale;
        let index = pos as usize;
        let frac = (pos - index as f64) as f32;
        match samples.get(index + 1) {
            Some(&next) => samples[index] * (1.0 - frac) + next * frac,
            None => samples[index],
        }
    };

    const FRAME: usize = 960;
    let mut frame = [0f32; FRAME];
    let mut packet = [0u8; 1275];
    let mut pending = SecureVec::<u8>::new();
    let mut lengths: Vec<usize> = Vec::new();
    let frames = out_len.div_ceil(FRAME);
    let mut granule = 0u64;
    let result: Result<(), String> = (|| {
        for f in 0..frames {
            for (k, x) in frame.iter_mut().enumerate() {
                let i = f * FRAME + k;
                *x = if i < out_len { at(i) } else { 0.0 };
            }
            let n = codec
                .encode(&frame, FRAME, &mut packet)
                .map_err(|err| err.to_string())?;
            pending.extend_from_slice(&packet[..n]);
            lengths.push(n);
            granule += FRAME as u64;
            let last = f + 1 == frames;
            // A page of a second or so, and the last one marked as the end.
            if lengths.len() == 50 || last {
                let mut parts = Vec::with_capacity(lengths.len());
                let mut start = 0;
                for &n in &lengths {
                    parts.push(&pending[start..start + n]);
                    start += n;
                }
                pages.page(&mut out, &parts, granule, if last { EOS } else { 0 });
                drop(parts);
                pending = SecureVec::new();
                lengths.clear();
            }
        }
        Ok(())
    })();
    frame.zeroize();
    packet.zeroize();
    result?;
    Ok((out, granule as f64 / 48_000.0))
}

const BOS: u8 = 0x02;
const EOS: u8 = 0x04;

/// Ogg pages of one logical stream, written straight into guarded memory.
struct OggPages {
    serial: u32,
    sequence: u32,
}

impl OggPages {
    fn new(serial: u32) -> Self {
        Self { serial, sequence: 0 }
    }

    /// One page holding `packets` whole, each of them ending on it.
    fn page(&mut self, out: &mut SecureVec<u8>, packets: &[&[u8]], granule: u64, flags: u8) {
        let mut lacing: Vec<u8> = Vec::new();
        for p in packets {
            lacing.extend(std::iter::repeat_n(255u8, p.len() / 255));
            lacing.push((p.len() % 255) as u8);
        }
        assert!(lacing.len() <= 255, "too many packets for one Ogg page");
        let start = out.len();
        out.extend_from_slice(b"OggS");
        out.push(0);
        out.push(flags);
        out.extend_from_slice(&granule.to_le_bytes());
        out.extend_from_slice(&self.serial.to_le_bytes());
        out.extend_from_slice(&self.sequence.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.push(lacing.len() as u8);
        out.extend_from_slice(&lacing);
        for p in packets {
            out.extend_from_slice(p);
        }
        let crc = ogg_crc(&out[start..]);
        out[start + 22..start + 26].copy_from_slice(&crc.to_le_bytes());
        self.sequence += 1;
    }
}

/// Ogg's CRC-32: polynomial 0x04c11db7, not reflected, starting from zero.
fn ogg_crc(bytes: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &b in bytes {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ 0x04c1_1db7 } else { crc << 1 };
        }
    }
    crc
}

fn content_serial(samples: &[f32]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for sample in samples {
        for byte in sample.to_le_bytes() {
            hash ^= u32::from(byte);
            hash = hash.wrapping_mul(0x0100_0193);
        }
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::OpusEncoder;
    use hound::{SampleFormat, WavSpec, WavWriter};
    use ogg::reading::PacketReader;
    use std::fs;
    use std::io::BufReader;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn in_memory_matches_the_file_encoder() {
        // Two and a half seconds of a 440 Hz tone at 44.1 kHz: several pages, resampled.
        let rate = 44_100;
        let samples: Vec<f32> = (0..rate * 5 / 2)
            .map(|n| (n as f32 / rate as f32 * 440.0 * std::f32::consts::TAU).sin() * 0.25)
            .collect();
        let (bytes, seconds) = super::encode_in_memory(&OpusEncoder::new(), &samples, rate as u32).unwrap();
        assert!(bytes.is_guarded());
        assert!((seconds - 2.5).abs() < 0.03, "{seconds}");

        let unique_suffix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let temp_dir = std::env::temp_dir().join(format!("rustic-audio-memory-test-{unique_suffix}"));
        fs::create_dir_all(&temp_dir).unwrap();
        let wav_path = temp_dir.join("test.wav");
        let opus_path = temp_dir.join("test.opus");
        let spec = WavSpec {
            channels: 1,
            sample_rate: rate as u32,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        };
        let mut writer = WavWriter::create(&wav_path, spec).unwrap();
        for &x in &samples {
            writer.write_sample(x).unwrap();
        }
        writer.finalize().unwrap();
        OpusEncoder::new()
            .encode_wav_to_opus(wav_path.to_str().unwrap(), opus_path.to_str().unwrap())
            .unwrap();

        // Every page's checksum is checked as it is read.
        let packets = |reader: &mut PacketReader<_>| {
            let mut out = Vec::new();
            while let Some(p) = reader.read_packet().unwrap() {
                out.push((p.last_in_stream(), p.absgp_page(), p.data));
            }
            out
        };
        let mine = packets(&mut PacketReader::new(std::io::Cursor::new(bytes.to_vec())));
        let theirs = packets(&mut PacketReader::new(std::io::Cursor::new(fs::read(&opus_path).unwrap())));
        assert_eq!(mine.len(), theirs.len());
        assert_eq!(mine[0].2, theirs[0].2, "the same OpusHead");
        assert!(mine.last().unwrap().0, "the stream ends");
        assert_eq!(mine.last().unwrap().1, theirs.last().unwrap().1, "the same length");
        assert!(mine.len() > 100);
        // Mostly the same packets: the file encoder rounds to 16 bits in between.
        let same = mine.iter().zip(&theirs).skip(2).filter(|(a, b)| a.2.len() == b.2.len()).count();
        assert!(same * 10 >= (mine.len() - 2) * 9, "{same} of {}", mine.len() - 2);
        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn ogg_crc_of_a_known_page() {
        // "OggS" and nothing else is not a page, but its checksum is fixed.
        assert_eq!(super::ogg_crc(b"OggS"), 0x5fb0a94f);
        assert_eq!(super::ogg_crc(b""), 0);
    }

    #[test]
    fn encode_wav_to_opus_does_not_write_trailing_empty_packet() {
        let unique_suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!(
            "rustic-audio-encoder-test-{unique_suffix}"
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

        let file = BufReader::new(fs::File::open(&opus_path).unwrap());
        let mut packet_reader = PacketReader::new(file);
        packet_reader.read_packet().unwrap();
        packet_reader.read_packet().unwrap();

        let mut last_audio_packet = None;
        while let Ok(Some(packet)) = packet_reader.read_packet() {
            last_audio_packet = Some(packet);
        }

        let last_packet = last_audio_packet.expect("expected at least one audio packet");
        assert!(
            !last_packet.data.is_empty(),
            "last audio packet should not be empty"
        );

        let _ = fs::remove_dir_all(temp_dir);
    }

    fn encode(dir: &std::path::Path, value: i16) -> Vec<u8> {
        let spec = WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let wav = dir.join(format!("in-{value}.wav"));
        let opus = dir.join(format!("out-{value}.opus"));
        let mut writer = WavWriter::create(&wav, spec).unwrap();
        for _ in 0..(960 * 2) {
            writer.write_sample(value).unwrap();
        }
        writer.finalize().unwrap();
        OpusEncoder::new()
            .encode_wav_to_opus(wav.to_str().unwrap(), opus.to_str().unwrap())
            .unwrap();
        fs::read(&opus).unwrap()
    }

    fn scratch() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rustic-audio-{}",
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn serial(bytes: &[u8]) -> u32 {
        u32::from_le_bytes(bytes[14..18].try_into().unwrap())
    }

    #[test]
    fn the_serial_follows_the_content_and_nothing_else() {
        let dir = scratch();
        let once = encode(&dir, 1234);
        assert_eq!(once, encode(&dir, 1234), "a re-encode must match");
        assert_ne!(serial(&once), serial(&encode(&dir, 2000)));
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(now.abs_diff(u64::from(serial(&once))) > 86_400);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn the_tags_packet_names_nothing() {
        let dir = scratch();
        let bytes = encode(&dir, 55);
        let at = bytes.windows(8).position(|w| w == b"OpusTags").unwrap() + 8;
        let vendor = bytes[at..at + 4].try_into().unwrap();
        assert_eq!(u32::from_le_bytes(vendor), 0);
        assert_eq!(
            u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()),
            0
        );
        let _ = fs::remove_dir_all(dir);
    }
}
