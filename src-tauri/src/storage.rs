//! Separate streaming WAV sinks follow MeetRec's storage.rs (MIT).

use crate::capture::Device;
use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use tempfile::{NamedTempFile, tempdir_in};
use uuid::Uuid;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub const SAMPLE_RATE: u32 = 16_000;
pub const TRACK_FILES: [&str; 2] = ["interviewer.wav", "me.wav"];
const MANIFEST: &str = "session.json";
const RECORDING_LOCK: &str = ".recording.lock";
const WAV_HEADER_SIZE: u64 = 44;
const MAX_WAV_FRAMES: u64 = (u32::MAX as u64 - 36) / 2;
const SPEC: hound::WavSpec = hound::WavSpec {
    channels: 1,
    sample_rate: SAMPLE_RATE,
    bits_per_sample: 16,
    sample_format: hound::SampleFormat::Int,
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub microphone: Option<String>,
    pub output: Option<String>,
    pub folder: PathBuf,
    #[serde(default, skip_serializing)]
    pub(crate) previous_folders: Vec<PathBuf>,
}

impl Settings {
    pub fn load(path: &Path, default_folder: PathBuf) -> Result<Self> {
        let mut settings = match File::open(path) {
            Ok(file) => serde_json::from_reader(file)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Self {
                microphone: None,
                output: None,
                folder: default_folder,
                previous_folders: vec![],
            },
            Err(error) => return Err(error.into()),
        };
        if fs::create_dir_all(&settings.folder).is_ok()
            && let Ok(folder) = settings.folder.canonicalize()
        {
            settings.folder = folder;
        }
        Ok(settings)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        // Keep library locations on disk while exposing only active settings to the UI.
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct StoredSettings<'a> {
            #[serde(flatten)]
            settings: &'a Settings,
            previous_folders: &'a [PathBuf],
        }
        atomic_json(
            path,
            &StoredSettings {
                settings: self,
                previous_folders: &self.previous_folders,
            },
        )
    }

    fn roots(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.folder.as_path())
            .chain(self.previous_folders.iter().map(PathBuf::as_path))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SaveState {
    InProgress,
    Saved,
    Interrupted,
    Recovered,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackInfo {
    pub path: String,
    pub device: Device,
    pub source_rate: u32,
    pub source_channels: u16,
    pub source_format: String,
    pub captured_frames: u64,
    pub last_capture_frame: u64,
    pub discontinuities: u32,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: String,
    pub name: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub state: SaveState,
    pub duration_frames: u64,
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub synchronization: String,
    pub tracks: [TrackInfo; 2],
    pub pauses: Vec<Pause>,
    pub bookmarks: Vec<u64>,
    pub issues: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pause {
    pub at_frame: u64,
    pub paused_at: DateTime<Utc>,
    pub resumed_at: Option<DateTime<Utc>>,
}

impl Session {
    fn new(tracks: [TrackInfo; 2]) -> Self {
        Self {
            id: Uuid::new_v4().simple().to_string(),
            name: format!("Interview · {}", Local::now().format("%d %b %Y, %H:%M")),
            started_at: Utc::now(),
            ended_at: None,
            state: SaveState::InProgress,
            duration_frames: 0,
            sample_rate: SAMPLE_RATE,
            channels: SPEC.channels,
            bits_per_sample: SPEC.bits_per_sample,
            synchronization: "Shared WASAPI QPC timeline; pauses removed; gaps padded with silence"
                .into(),
            tracks,
            pauses: vec![],
            bookmarks: vec![],
            issues: vec![],
        }
    }

    pub fn save(&self, folder: &Path) -> Result<()> {
        atomic_json(&folder.join(MANIFEST), self)
    }
}

pub struct WavSink {
    writer: hound::WavWriter<BufWriter<File>>,
    file: File,
}

impl WavSink {
    pub(crate) fn create(path: &Path) -> Result<Self> {
        let file = File::create_new(path)?;
        let writer = hound::WavWriter::new(BufWriter::new(file.try_clone()?), SPEC)?;
        Ok(Self { writer, file })
    }

    pub fn write(&mut self, samples: &[i16]) -> Result<()> {
        let mut writer = self.writer.get_i16_writer(samples.len() as u32);
        for &sample in samples {
            writer.write_sample(sample);
        }
        writer.flush()?;
        Ok(())
    }

    pub fn checkpoint(&mut self) -> Result<()> {
        self.writer.flush()?;
        self.file.sync_data()?;
        Ok(())
    }

    pub fn finish(self) -> Result<()> {
        self.writer.finalize()?;
        self.file.sync_all()?;
        Ok(())
    }
}

pub struct SessionFiles {
    pub folder: PathBuf,
    pub session: Session,
    pub lock: File,
    pub writers: [WavSink; 2],
}

impl SessionFiles {
    pub fn create(root: &Path, tracks: [TrackInfo; 2]) -> Result<Self> {
        let session = Session::new(tracks);
        let folder = root.join(&session.id);
        fs::create_dir(&folder)?;
        let result = (|| {
            let lock = open_lock(&folder.join(RECORDING_LOCK))?;
            let mut writers = [
                WavSink::create(&folder.join(TRACK_FILES[0]))?,
                WavSink::create(&folder.join(TRACK_FILES[1]))?,
            ];
            for writer in &mut writers {
                writer.checkpoint()?;
            }
            session.save(&folder)?;
            Ok(Self {
                folder: folder.clone(),
                session,
                lock,
                writers,
            })
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&folder);
        }
        result
    }
}

pub fn open_lock(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    fs2::FileExt::try_lock_exclusive(&file)?;
    Ok(file)
}

pub fn atomic_json(path: &Path, data: &impl Serialize) -> Result<()> {
    let parent = path.parent().ok_or("Invalid file path.")?;
    let mut file = NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut file, data)?;
    file.write_all(b"\n")?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}

fn valid_id(id: &str) -> bool {
    Uuid::try_parse(id).is_ok_and(|uuid| uuid.simple().to_string() == id)
}

fn session_dir(root: &Path, id: &str) -> Result<PathBuf> {
    if !valid_id(id) {
        return Err("Invalid recording identifier.".into());
    }
    let root = root.canonicalize()?;
    let folder = root.join(id).canonicalize()?;
    if folder.parent() != Some(root.as_path()) || folder.file_name() != Some(id.as_ref()) {
        return Err("Recording folder points outside its original location.".into());
    }
    Ok(folder)
}

fn owned_file(folder: &Path, name: &str) -> Result<PathBuf> {
    let path = folder.join(name);
    if !fs::symlink_metadata(&path)?.is_file() || path.canonicalize()? != path {
        return Err(format!("{name} is not a regular recording file.").into());
    }
    Ok(path)
}

fn read_session(folder: &Path, id: &str) -> Result<Session> {
    let session: Session = serde_json::from_reader(File::open(owned_file(folder, MANIFEST)?)?)?;
    if session.id != id {
        return Err("Recording identifier does not match its folder.".into());
    }
    Ok(session)
}

pub fn find(settings: &Settings, id: &str) -> Result<(PathBuf, Session)> {
    for root in settings.roots() {
        if let Ok(folder) = session_dir(root, id) {
            let session = read_session(&folder, id)?;
            if session.state != SaveState::InProgress {
                return Ok((folder, session));
            }
        }
    }
    Err("The saved recording could not be found.".into())
}

#[derive(Serialize)]
pub struct Library {
    pub sessions: Vec<Session>,
    pub warnings: Vec<String>,
}

pub fn library(settings: &Settings) -> Library {
    let mut result = Library {
        sessions: vec![],
        warnings: vec![],
    };
    for path in settings.roots() {
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) => {
                result
                    .warnings
                    .push(format!("Could not read {} ({error}).", display_path(path)));
                continue;
            }
        };
        for entry in entries.flatten() {
            let Some(id) = entry
                .file_name()
                .into_string()
                .ok()
                .filter(|id| valid_id(id))
            else {
                continue;
            };
            let session = session_dir(path, &id).and_then(|folder| {
                let session = read_session(&folder, &id)?;
                if session.state == SaveState::InProgress {
                    let _lock = open_lock(&owned_file(&folder, RECORDING_LOCK)?)?;
                    // The recording may have finished between reading the manifest and locking it.
                    let latest = read_session(&folder, &id)?;
                    return if latest.state == SaveState::InProgress {
                        recover(&folder, latest)
                    } else {
                        Ok(latest)
                    };
                }
                Ok(session)
            });
            match session {
                Ok(session) => result.sessions.push(session),
                Err(error)
                    if error.downcast_ref::<io::Error>().is_some_and(|e| {
                        e.kind() == io::ErrorKind::NotFound
                            || e.kind() == io::ErrorKind::WouldBlock
                            || e.raw_os_error() == Some(33)
                    }) => {}
                Err(error) => result
                    .warnings
                    .push(format!("Could not read recording {id}: {error}")),
            }
        }
    }
    result
        .sessions
        .sort_by_key(|item| std::cmp::Reverse(item.started_at));
    result
}

pub fn rename(folder: &Path, mut session: Session, name: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 100 || name.chars().any(char::is_control) {
        return Err("Enter a recording name between 1 and 100 characters.".into());
    }
    session.name = name.to_owned();
    session.save(folder)
}

pub fn track_path(folder: &Path, track: usize) -> Result<PathBuf> {
    let name = TRACK_FILES.get(track).ok_or("Unknown recording track.")?;
    owned_file(folder, name)
}

pub fn export(folder: &Path, destination: &Path, session: &Session) -> Result<PathBuf> {
    let destination = destination.canonicalize()?;
    if destination.starts_with(folder) {
        return Err("Choose an export folder outside this recording.".into());
    }
    let staging = tempdir_in(&destination)?;
    for (track, name) in TRACK_FILES.iter().enumerate() {
        fs::copy(track_path(folder, track)?, staging.path().join(name))?;
    }
    session.save(staging.path())?;
    let output = destination.join(format!(
        "NexQ-{}-{}",
        session.started_at.format("%Y%m%d-%H%M%S"),
        &session.id[..8],
    ));
    fs::rename(staging.path(), &output).map_err(|error| {
        format!(
            "Could not export to {} ({error}). Choose another folder if this copy already exists.",
            display_path(&output),
        )
    })?;
    Ok(output)
}

pub fn delete(folder: &Path, session: &Session) -> Result<()> {
    let holding = folder.with_file_name(format!(
        ".deleted-{}-{}",
        session.id,
        Uuid::new_v4().simple()
    ));
    fs::rename(folder, &holding)?;
    if let Err(error) = fs::remove_dir_all(&holding) {
        return Err(format!(
            "Could not finish deleting {} ({error}).",
            display_path(&holding)
        )
        .into());
    }
    Ok(())
}

pub fn display_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    if let Some(unc) = value.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{unc}");
    }
    value.strip_prefix(r"\\?\").unwrap_or(&value).to_owned()
}

fn recover(folder: &Path, mut session: Session) -> Result<Session> {
    session.state = SaveState::Recovered;
    session.issues.push(
        "Recovered after an unexpected shutdown. Track statistics reflect the last checkpoint."
            .into(),
    );
    salvage(folder, session)
}

pub fn salvage(folder: &Path, mut session: Session) -> Result<Session> {
    let mut files = [
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(track_path(folder, 0)?)?,
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(track_path(folder, 1)?)?,
    ];
    let frames = recoverable_frames(&mut files[0])?.min(recoverable_frames(&mut files[1])?);
    for file in &mut files {
        repair_header(file, frames)?;
    }
    for track in &mut session.tracks {
        track.captured_frames = track.captured_frames.min(frames);
        track.last_capture_frame = track.last_capture_frame.min(frames);
    }
    session.bookmarks.retain(|&frame| frame <= frames);
    session.pauses.retain(|pause| pause.at_frame <= frames);
    session.duration_frames = frames;
    session.save(folder)?;
    Ok(session)
}

fn recoverable_frames(file: &mut File) -> Result<u64> {
    let mut header = [0u8; WAV_HEADER_SIZE as usize];
    file.read_exact(&mut header)?;
    let signature = (&header[..4], &header[8..16], &header[36..40]);
    if signature != (&b"RIFF"[..], &b"WAVEfmt "[..], &b"data"[..])
        || header[16..36]
            != [
                16, 0, 0, 0, 1, 0, 1, 0, 128, 62, 0, 0, 0, 125, 0, 0, 2, 0, 16, 0,
            ]
    {
        return Err("Unrecognized WAV format during recovery.".into());
    }
    Ok(((file.metadata()?.len() - WAV_HEADER_SIZE) / 2).min(MAX_WAV_FRAMES))
}

fn repair_header(file: &mut File, frames: u64) -> Result<()> {
    let bytes = (frames * 2) as u32;
    file.set_len(WAV_HEADER_SIZE + u64::from(bytes))?;
    file.seek(SeekFrom::Start(4))?;
    file.write_all(&(36 + bytes).to_le_bytes())?;
    file.seek(SeekFrom::Start(40))?;
    file.write_all(&bytes.to_le_bytes())?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(root: &Path) -> Settings {
        Settings {
            microphone: None,
            output: None,
            folder: root.canonicalize().unwrap(),
            previous_folders: vec![],
        }
    }

    fn tracks() -> [TrackInfo; 2] {
        TRACK_FILES.map(|name| TrackInfo {
            path: name.into(),
            device: Device {
                id: name.into(),
                name: name.into(),
            },
            source_rate: 48_000,
            source_channels: 2,
            source_format: "f32".into(),
            captured_frames: 0,
            last_capture_frame: 0,
            discontinuities: 0,
        })
    }

    fn save_example(root: &Path) -> (PathBuf, Session) {
        let SessionFiles {
            folder,
            mut session,
            writers,
            ..
        } = SessionFiles::create(root, tracks()).unwrap();
        for mut writer in writers {
            writer.write(&[100, 200, 0]).unwrap();
            writer.finish().unwrap();
        }
        session.duration_frames = 3;
        session.ended_at = Some(Utc::now());
        session.state = SaveState::Saved;
        session.save(&folder).unwrap();
        (folder, session)
    }

    #[test]
    fn metadata_and_library_operations_round_trip() {
        let home = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let settings = settings(home.path());
        let (folder, session) = save_example(&settings.folder);
        assert_eq!(library(&settings).sessions.len(), 1);
        rename(&folder, session.clone(), "Operations interview").unwrap();
        let (_, renamed) = find(&settings, &session.id).unwrap();
        assert_eq!(renamed.name, "Operations interview");
        assert_eq!(renamed.tracks[0].source_rate, 48_000);
        assert_eq!(renamed.duration_frames, 3);
        assert!(rename(&folder, renamed.clone(), " ").is_err());
        assert!(rename(&folder, renamed.clone(), "Bad\nname").is_err());
        let copy = export(&folder, destination.path(), &renamed).unwrap();
        for track in 0..2 {
            assert_eq!(
                fs::read(track_path(&copy, track).unwrap()).unwrap(),
                fs::read(track_path(&folder, track).unwrap()).unwrap()
            );
        }
        assert!(export(&folder, destination.path(), &renamed).is_err());
        assert!(find(&settings, "../another-folder").is_err());
        delete(&folder, &renamed).unwrap();
        assert!(library(&settings).sessions.is_empty());
        assert!(copy.is_dir());
    }

    #[test]
    fn recovery_skips_live_sessions_and_repairs_both_headers_after_a_crash() {
        let home = tempfile::tempdir().unwrap();
        let settings = settings(home.path());
        let SessionFiles {
            folder,
            session,
            lock,
            writers,
        } = SessionFiles::create(&settings.folder, tracks()).unwrap();
        assert!(library(&settings).sessions.is_empty());
        for mut writer in writers {
            writer.write(&[100, 200, 300]).unwrap();
            writer.finish().unwrap();
        }
        let mut track = OpenOptions::new()
            .append(true)
            .open(folder.join(TRACK_FILES[0]))
            .unwrap();
        track.write_all(&[1, 2, 3]).unwrap();
        drop(track);
        File::options()
            .write(true)
            .open(folder.join(TRACK_FILES[1]))
            .unwrap()
            .set_len(48)
            .unwrap();
        drop(lock);
        let result = library(&settings);
        assert!(result.warnings.is_empty());
        let recovered = &result.sessions[0];
        assert!(recovered.state == SaveState::Recovered);
        assert_eq!(recovered.id, session.id);
        assert_eq!(recovered.duration_frames, 2);
        for track in 0..2 {
            let reader = hound::WavReader::open(track_path(&folder, track).unwrap()).unwrap();
            assert_eq!(reader.duration(), 2);
        }
        assert_eq!(library(&settings).sessions[0].issues.len(), 1);
    }

    #[test]
    fn settings_keep_the_history_of_previous_storage_folders() {
        let home = tempfile::tempdir().unwrap();
        let earlier = tempfile::tempdir().unwrap();
        let mut config = settings(home.path());
        config.microphone = Some("mic endpoint".into());
        config
            .previous_folders
            .push(earlier.path().canonicalize().unwrap());
        save_example(earlier.path());
        let path = home.path().join("settings.json");
        config.save(&path).unwrap();
        let restored = Settings::load(&path, home.path().into()).unwrap();
        assert_eq!(restored.microphone, config.microphone);
        assert_eq!(restored.previous_folders, config.previous_folders);
        assert_eq!(library(&restored).sessions.len(), 1);
    }
}
