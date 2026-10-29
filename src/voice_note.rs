use crate::dsp::AudioProcessor;
use crate::opus_encoder::OpusEncoder;
use crate::opus_playback::{get_opus_info, playback, ClipSource};
use crate::record::record_audio;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug)]
pub struct VoiceNoteClip {
    path: PathBuf,
    duration_seconds: f64,
    file_size_bytes: u64,
}

impl VoiceNoteClip {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn duration_seconds(&self) -> f64 {
        self.duration_seconds
    }

    pub fn file_size_bytes(&self) -> u64 {
        self.file_size_bytes
    }
}

struct RecordingJob {
    is_recording: Arc<AtomicBool>,
    cancel_requested: Arc<AtomicBool>,
    handle: thread::JoinHandle<Result<VoiceNoteClip, String>>,
}

struct PlaybackJob {
    is_playing: Arc<AtomicBool>,
    last_error: Arc<Mutex<Option<String>>>,
    handle: thread::JoinHandle<()>,
}

pub struct VoiceNoteEngine {
    cache_dir: PathBuf,
    processor: AudioProcessor,
    opus_encoder: OpusEncoder,
    recording: Option<RecordingJob>,
    playback: Option<PlaybackJob>,
}

impl VoiceNoteEngine {
    pub fn new(cache_dir: impl Into<PathBuf>) -> Result<Self, String> {
        let cache_dir = cache_dir.into();
        ensure_cache_dir(&cache_dir)?;

        let mut processor = AudioProcessor::new(48_000.0);
        processor.filters_enabled = true;
        processor.highpass_freq = 75.0;
        processor.lowpass_freq = 20_000.0;
        processor.spectral_gate_enabled = false;
        processor.amplitude_gate_enabled = false;
        processor.gain_boost_enabled = false;
        processor.rms_enabled = false;
        processor.limiter_enabled = true;

        Ok(Self {
            cache_dir,
            processor,
            opus_encoder: OpusEncoder::new(),
            recording: None,
            playback: None,
        })
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub fn set_cache_dir(&mut self, cache_dir: impl Into<PathBuf>) -> Result<(), String> {
        let cache_dir = cache_dir.into();
        ensure_cache_dir(&cache_dir)?;
        self.cache_dir = cache_dir;
        Ok(())
    }

    pub fn start_capture(&mut self, stem: Option<&str>) -> Result<PathBuf, String> {
        if self.is_recording() {
            return Err("capture is already active".to_string());
        }

        self.stop_playback()?;

        let opus_path = next_clip_path(&self.cache_dir, stem)?;
        let wav_path = opus_path.with_extension("wav");
        let processor = self.processor.clone();
        let opus_encoder = self.opus_encoder.clone();
        let is_recording = Arc::new(AtomicBool::new(true));
        let cancel_requested = Arc::new(AtomicBool::new(false));

        let recording_flag = Arc::clone(&is_recording);
        let cancel_flag = Arc::clone(&cancel_requested);
        let wav_path_for_thread = wav_path.clone();
        let opus_path_for_thread = opus_path.clone();

        let handle = thread::spawn(move || {
            let wav_path_str = wav_path_for_thread.to_string_lossy().into_owned();
            let opus_path_str = opus_path_for_thread.to_string_lossy().into_owned();

            let recorded = record_audio(&wav_path_str, recording_flag, processor).map_err(|err| err.to_string());

            if cancel_flag.load(Ordering::Relaxed) {
                let _ = fs::remove_file(&wav_path_for_thread);
                let _ = fs::remove_file(&opus_path_for_thread);
                return Err("recording cancelled".to_string());
            }
            recorded?;

            let encode_result = opus_encoder
                .encode_wav_to_opus(&wav_path_str, &opus_path_str)
                .map_err(|err| err.to_string());
            let _ = fs::remove_file(&wav_path_for_thread);
            encode_result?;

            let (file_size_bytes, duration_seconds) =
                get_opus_info(&opus_path_str).map_err(|err| err.to_string())?;

            Ok(VoiceNoteClip {
                path: opus_path_for_thread,
                duration_seconds,
                file_size_bytes,
            })
        });

        self.recording = Some(RecordingJob {
            is_recording,
            cancel_requested,
            handle,
        });

        Ok(opus_path)
    }

    pub fn stop_capture(&mut self) -> Result<VoiceNoteClip, String> {
        let recording = self
            .recording
            .take()
            .ok_or_else(|| "capture is not active".to_string())?;

        recording.is_recording.store(false, Ordering::Relaxed);

        match recording.handle.join() {
            Ok(result) => result,
            Err(_) => Err("failed to join capture thread".to_string()),
        }
    }

    pub fn cancel_capture(&mut self) -> Result<bool, String> {
        let recording = match self.recording.take() {
            Some(recording) => recording,
            None => return Ok(false),
        };

        recording.cancel_requested.store(true, Ordering::Relaxed);
        recording.is_recording.store(false, Ordering::Relaxed);

        match recording.handle.join() {
            Ok(Ok(clip)) => {
                let _ = fs::remove_file(clip.path());
                Ok(true)
            }
            Ok(Err(err)) if err == "recording cancelled" => Ok(true),
            Ok(Err(err)) => Err(err),
            Err(_) => Err("failed to join capture thread".to_string()),
        }
    }

    pub fn play_preview(&mut self, file_path: impl AsRef<Path>) -> Result<(), String> {
        self.play_clip(file_path)
    }

    pub fn play_received(&mut self, file_path: impl AsRef<Path>) -> Result<(), String> {
        self.play_clip(file_path)
    }

    /// Play a received clip held in memory, such as one decrypted for playing that should never
    /// be written out.
    pub fn play_received_bytes(&mut self, bytes: Vec<u8>) -> Result<(), String> {
        self.start_playback(ClipSource::Bytes(bytes))
    }

    pub fn stop_playback(&mut self) -> Result<(), String> {
        let playback = match self.playback.take() {
            Some(playback) => playback,
            None => return Ok(()),
        };

        playback.is_playing.store(false, Ordering::Relaxed);

        match playback.handle.join() {
            Ok(()) => Ok(()),
            Err(_) => Err("failed to join playback thread".to_string()),
        }
    }

    pub fn take_playback_error(&self) -> Option<String> {
        let playback = self.playback.as_ref()?;
        playback.last_error.lock().ok()?.take()
    }

    pub fn is_recording(&self) -> bool {
        self.recording
            .as_ref()
            .is_some_and(|recording| recording.is_recording.load(Ordering::Relaxed))
    }

    pub fn is_playing(&self) -> bool {
        self.playback
            .as_ref()
            .is_some_and(|playback| playback.is_playing.load(Ordering::Relaxed))
    }

    fn play_clip(&mut self, file_path: impl AsRef<Path>) -> Result<(), String> {
        if self.is_recording() {
            return Err("capture is active".to_string());
        }

        self.stop_playback()?;

        let file_path = file_path.as_ref().to_path_buf();
        if !file_path.is_file() {
            return Err(format!(
                "clip does not exist: {}",
                file_path.to_string_lossy()
            ));
        }

        self.start_playback(ClipSource::File(file_path.to_string_lossy().into_owned()))
    }

    fn start_playback(&mut self, clip: ClipSource) -> Result<(), String> {
        if self.is_recording() {
            return Err("capture is active".to_string());
        }

        self.stop_playback()?;

        let is_playing = Arc::new(AtomicBool::new(true));
        let playback_flag = Arc::clone(&is_playing);
        let last_error = Arc::new(Mutex::new(None));
        let playback_error = Arc::clone(&last_error);
        let handle = thread::spawn(move || {
            if let Err(err) = playback(clip, playback_flag) {
                store_playback_error(playback_error.as_ref(), err);
            }
        });

        self.playback = Some(PlaybackJob {
            is_playing,
            last_error,
            handle,
        });
        Ok(())
    }
}

impl Drop for VoiceNoteEngine {
    fn drop(&mut self) {
        let _ = self.cancel_capture();
        let _ = self.stop_playback();
    }
}

fn ensure_cache_dir(cache_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(cache_dir).map_err(|err| {
        format!(
            "failed to create cache directory {}: {err}",
            cache_dir.to_string_lossy()
        )
    })
}

fn next_clip_path(cache_dir: &Path, stem: Option<&str>) -> Result<PathBuf, String> {
    ensure_cache_dir(cache_dir)?;
    let stem = sanitize_stem(stem.unwrap_or("voice-note"));
    let unique_suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| err.to_string())?
        .as_millis();
    Ok(cache_dir.join(format!("{stem}-{unique_suffix}.opus")))
}

fn sanitize_stem(stem: &str) -> String {
    let sanitized = stem
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => ch,
            _ => '-',
        })
        .collect::<String>();
    let sanitized = sanitized.trim_matches('-');
    if sanitized.is_empty() {
        "voice-note".to_string()
    } else {
        sanitized.to_string()
    }
}

fn store_playback_error(slot: &Mutex<Option<String>>, message: String) {
    if let Ok(mut error) = slot.lock() {
        if error.is_none() {
            *error = Some(message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{next_clip_path, sanitize_stem, store_playback_error};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn sanitize_stem_normalizes_unsafe_characters() {
        assert_eq!(sanitize_stem("../voice note?.opus"), "voice-note--opus");
        assert_eq!(sanitize_stem(""), "voice-note");
    }

    #[test]
    fn next_clip_path_stays_inside_cache_dir() {
        let unique_suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let cache_dir = std::env::temp_dir().join(format!(
            "rustic-audio-tool-test-{unique_suffix}"
        ));
        fs::create_dir_all(&cache_dir).unwrap();

        let clip_path = next_clip_path(&cache_dir, Some("../draft preview")).unwrap();

        assert_eq!(clip_path.parent(), Some(cache_dir.as_path()));
        assert_eq!(clip_path.extension().and_then(|ext| ext.to_str()), Some("opus"));
        assert!(clip_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| stem.starts_with("draft-preview-")));

        let _ = fs::remove_dir_all(PathBuf::from(cache_dir));
    }

    #[test]
    fn store_playback_error_keeps_the_first_failure() {
        let slot = Mutex::new(None);

        store_playback_error(&slot, "first".to_string());
        store_playback_error(&slot, "second".to_string());

        assert_eq!(slot.into_inner().unwrap(), Some("first".to_string()));
    }
}