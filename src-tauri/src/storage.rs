//! Separate streaming WAV sinks follow MeetRec's storage.rs (MIT).

use crate::capture::{CaptureEvents, Device};
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<TrackDiagnostics>,
}

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackDiagnostics {
    #[serde(flatten)]
    pub events: CaptureEvents,
    pub silence_frames: u64,
    pub overlap_frames: u64,
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
    pub sessions: Vec<LibraryEntry>,
    pub warnings: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryEntry {
    #[serde(flatten)]
    pub session: Session,
    pub bytes: u64,
    pub available_tracks: [bool; 2],
}

fn library_entry(folder: &Path, session: Session) -> LibraryEntry {
    let sizes: [Option<u64>; 2] = std::array::from_fn(|track| {
        track_path(folder, track)
            .ok()
            .and_then(|path| fs::metadata(path).ok())
            .map(|data| data.len())
    });
    LibraryEntry {
        session,
        bytes: sizes.iter().flatten().sum(),
        available_tracks: std::array::from_fn(|index| {
            track_path(folder, index)
                .ok()
                .is_some_and(|path| hound::WavReader::open(path).is_ok())
        }),
    }
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
                        recover(&folder, latest).map(|session| library_entry(&folder, session))
                    } else {
                        Ok(library_entry(&folder, latest))
                    };
                }
                Ok(library_entry(&folder, session))
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
        .sort_by_key(|item| std::cmp::Reverse(item.session.started_at));
    result
}

pub fn rename(folder: &Path, name: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 100 || name.chars().any(char::is_control) {
        return Err("Enter a recording name between 1 and 100 characters.".into());
    }
    let manifest = owned_file(folder, MANIFEST)?;
    // Change only the requested name, including when older metadata has fields
    // this build does not use.
    let mut data: serde_json::Value = serde_json::from_reader(File::open(&manifest)?)?;
    data["name"] = serde_json::Value::String(name.to_owned());
    atomic_json(&manifest, &data)
}

pub fn track_path(folder: &Path, track: usize) -> Result<PathBuf> {
    let name = TRACK_FILES.get(track).ok_or("Unknown recording track.")?;
    owned_file(folder, name)
}

pub fn suggested_name(session: &Session, track: Option<usize>) -> String {
    let stem = session
        .name
        .chars()
        .take(60)
        .map(|character| {
            if character.is_control() || "<>:\"/\\|?*".contains(character) {
                '_'
            } else {
                character
            }
        })
        .collect::<String>();
    let stem = stem.trim_matches(['.', ' ']);
    match track {
        Some(index) => {
            let label = TRACK_FILES[index].trim_end_matches(".wav");
            format!("NexQ - {stem} - {label}.wav")
        }
        None => format!("NexQ - {stem} - {}", &session.id[..8]),
    }
}

fn check_space(destination: &Path, bytes: u64) -> Result<()> {
    if fs2::available_space(destination).is_ok_and(|available| available < bytes) {
        return Err("Not enough free space for this export. Choose another drive.".into());
    }
    Ok(())
}

fn export_error(error: io::Error) -> String {
    match error.kind() {
        io::ErrorKind::AlreadyExists => {
            "That export already exists. Choose another name or folder.".into()
        }
        io::ErrorKind::PermissionDenied => format!(
            "Windows denied access to this export destination. Choose a writable location. ({error})"
        ),
        io::ErrorKind::StorageFull => {
            format!("The destination ran out of space. Choose another drive. ({error})")
        }
        _ => format!("Could not complete the export: {error}"),
    }
}

pub fn export_track(folder: &Path, track: usize, destination: &Path) -> Result<PathBuf> {
    let source = track_path(folder, track)?;
    let parent = destination
        .parent()
        .ok_or("Choose an export folder.")?
        .canonicalize()?;
    if parent.starts_with(folder) {
        return Err("Choose an export location outside this recording.".into());
    }
    let name = destination
        .file_name()
        .ok_or("Choose a filename for the exported WAV.")?;
    let destination = parent.join(name);
    if destination.exists() {
        return Err(export_error(io::Error::from(io::ErrorKind::AlreadyExists)).into());
    }
    let mut input = File::open(source)?;
    check_space(&parent, input.metadata()?.len())?;
    let mut staging = NamedTempFile::new_in(&parent).map_err(export_error)?;
    io::copy(&mut input, &mut staging).map_err(export_error)?;
    staging.as_file().sync_all().map_err(export_error)?;
    staging
        .persist_noclobber(&destination)
        .map_err(|error| export_error(error.error))?;
    Ok(destination)
}

pub fn export(folder: &Path, destination: &Path, session: &Session) -> Result<PathBuf> {
    let destination = destination.canonicalize()?;
    if destination.starts_with(folder) {
        return Err("Choose an export folder outside this recording.".into());
    }
    let output = destination.join(suggested_name(session, None));
    if output.exists() {
        return Err(export_error(io::Error::from(io::ErrorKind::AlreadyExists)).into());
    }
    let sources = [
        track_path(folder, 0)?,
        track_path(folder, 1)?,
        owned_file(folder, MANIFEST)?,
    ];
    let bytes = sources.iter().try_fold(0u64, |sum, path| {
        Ok::<_, io::Error>(sum.saturating_add(fs::metadata(path)?.len()))
    })?;
    check_space(&destination, bytes)?;
    let staging = tempdir_in(&destination).map_err(export_error)?;
    for (source, name) in sources
        .iter()
        .zip([TRACK_FILES[0], TRACK_FILES[1], MANIFEST])
    {
        let copy = staging.path().join(name);
        fs::copy(source, &copy).map_err(export_error)?;
        File::options()
            .write(true)
            .open(&copy)
            .and_then(|file| file.sync_all())
            .map_err(export_error)?;
    }
    // Windows does not replace an existing directory when renaming a directory.
    fs::rename(staging.path(), &output).map_err(export_error)?;
    Ok(output)
}

pub fn delete(folder: &Path, session: &Session) -> Result<()> {
    for entry in fs::read_dir(folder)? {
        let entry = entry?;
        let name = entry.file_name();
        if !name.to_str().is_some_and(|name| {
            [TRACK_FILES[0], TRACK_FILES[1], MANIFEST, RECORDING_LOCK].contains(&name)
        }) || !entry.file_type()?.is_file()
        {
            return Err("This folder contains files outside the recording. Open the folder to review them before deleting.".into());
        }
    }
    let holding = folder.with_file_name(format!(
        ".deleted-{}-{}",
        session.id,
        Uuid::new_v4().simple()
    ));
    fs::rename(folder, &holding)?;
    for name in [TRACK_FILES[0], TRACK_FILES[1], RECORDING_LOCK, MANIFEST] {
        if let Err(error) = fs::remove_file(holding.join(name))
            && error.kind() != io::ErrorKind::NotFound
        {
            return Err(format!(
                "Could not finish deleting {} ({error}).",
                display_path(&holding)
            )
            .into());
        }
    }
    fs::remove_dir(&holding)?;
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
    let mut files: [Option<(File, u64)>; 2] = std::array::from_fn(|index| {
        let open = || -> Result<_> {
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(track_path(folder, index)?)?;
            let frames = recoverable_frames(&mut file)?;
            repair_header(&mut file, frames)?;
            Ok((file, frames))
        };
        match open() {
            Ok(file) => Some(file),
            Err(error) => {
                session
                    .issues
                    .push(format!("Could not recover {}: {error}", TRACK_FILES[index]));
                None
            }
        }
    });
    let frames = files
        .iter()
        .flatten()
        .map(|(_, frames)| *frames)
        .max()
        .ok_or("Neither WAV could be repaired. Original files were kept.")?;
    for (index, recovered) in files.iter_mut().enumerate() {
        let Some((file, original)) = recovered else {
            continue;
        };
        let track = &mut session.tracks[index];
        track.captured_frames = track.captured_frames.min(*original);
        track.last_capture_frame = track.last_capture_frame.min(*original);
        if *original < frames {
            match pad_recovered(file, *original, frames) {
                Ok(()) => {
                    let added = frames - *original;
                    track
                        .diagnostics
                        .get_or_insert_with(TrackDiagnostics::default)
                        .silence_frames += added;
                    session.issues.push(format!(
                        "{} was padded with {added} silent frames during recovery.",
                        TRACK_FILES[index]
                    ));
                }
                Err(error) => session
                    .issues
                    .push(format!("Could not align {}: {error}", TRACK_FILES[index])),
            }
        }
    }
    session.bookmarks.retain(|&frame| frame <= frames);
    session.pauses.retain(|pause| pause.at_frame <= frames);
    session.duration_frames = frames;
    session.save(folder)?;
    Ok(session)
}

fn pad_recovered(file: &mut File, original: u64, target: u64) -> Result<()> {
    let original_end = WAV_HEADER_SIZE + original * 2;
    file.seek(SeekFrom::Start(original_end))?;
    let mut remaining = (target - original) * 2;
    let zeroes = [0; 8192];
    while remaining > 0 {
        let count = remaining.min(zeroes.len() as u64) as usize;
        if let Err(error) = file.write_all(&zeroes[..count]) {
            repair_header(file, original)?;
            return Err(error.into());
        }
        remaining -= count as u64;
    }
    repair_header(file, target)
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
            diagnostics: None,
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
        rename(&folder, "Operations interview").unwrap();
        let (_, renamed) = find(&settings, &session.id).unwrap();
        assert_eq!(renamed.name, "Operations interview");
        assert_eq!(renamed.tracks[0].source_rate, 48_000);
        assert_eq!(renamed.duration_frames, 3);
        assert!(rename(&folder, " ").is_err());
        assert!(rename(&folder, "Bad\nname").is_err());
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
        let recovered = &result.sessions[0].session;
        assert!(recovered.state == SaveState::Recovered);
        assert_eq!(recovered.id, session.id);
        assert_eq!(recovered.duration_frames, 4);
        for track in 0..2 {
            let mut reader = hound::WavReader::open(track_path(&folder, track).unwrap()).unwrap();
            let data: Vec<_> = reader
                .samples::<i16>()
                .map(|sample| sample.unwrap())
                .collect();
            assert_eq!(
                data,
                if track == 0 {
                    vec![100, 200, 300, 513]
                } else {
                    vec![100, 200, 0, 0]
                }
            );
        }
        assert_eq!(library(&settings).sessions[0].session.issues.len(), 2);
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

    #[test]
    fn single_track_export_is_byte_identical_and_never_replaces_a_file() {
        let home = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let settings = settings(home.path());
        let (folder, session) = save_example(&settings.folder);
        let output = destination.path().join(suggested_name(&session, Some(0)));
        let saved = export_track(&folder, 0, &output).unwrap();
        assert_eq!(
            fs::read(&saved).unwrap(),
            fs::read(track_path(&folder, 0).unwrap()).unwrap()
        );
        fs::write(&saved, "existing file").unwrap();
        assert!(export_track(&folder, 1, &output).is_err());
        assert_eq!(fs::read(&saved).unwrap(), b"existing file");
        assert!(export_track(&folder, 0, &folder.join("copy.wav")).is_err());
        assert!(export_track(&folder, 8, &output).is_err());
        assert_eq!(fs::read_dir(destination.path()).unwrap().count(), 1);
    }

    #[test]
    fn legacy_session_is_readable_without_metadata_changes_and_export_retains_original_json() {
        let home = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let settings = settings(home.path());
        let (folder, session) = save_example(&settings.folder);
        let path = folder.join(MANIFEST);
        let mut legacy: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        legacy["legacyExtension"] = serde_json::Value::Bool(true);
        let original = serde_json::to_vec(&legacy).unwrap();
        fs::write(&path, &original).unwrap();
        let result = library(&settings);
        assert!(result.sessions[0].session.tracks[0].diagnostics.is_none());
        assert_eq!(result.sessions[0].available_tracks, [true, true]);
        let exported = export(&folder, destination.path(), &session).unwrap();
        assert_eq!(fs::read(exported.join(MANIFEST)).unwrap(), original);
        assert_eq!(fs::read(path).unwrap(), original);
        rename(&folder, "Renamed").unwrap();
        let renamed: serde_json::Value =
            serde_json::from_reader(File::open(folder.join(MANIFEST)).unwrap()).unwrap();
        assert_eq!(renamed["legacyExtension"], true);
        assert_eq!(renamed["name"], "Renamed");
    }

    #[test]
    fn deletion_declines_unmanaged_files_and_reports_permission_errors() {
        let home = tempfile::tempdir().unwrap();
        let settings = settings(home.path());
        let (folder, session) = save_example(&settings.folder);
        let custom = folder.join("personal.txt");
        fs::write(&custom, "keep").unwrap();
        assert!(delete(&folder, &session).is_err());
        assert_eq!(fs::read(custom).unwrap(), b"keep");
        assert!(track_path(&folder, 0).is_ok());
        assert!(export_error(io::Error::from(io::ErrorKind::PermissionDenied)).contains("denied"));
        assert!(export_error(io::Error::from(io::ErrorKind::StorageFull)).contains("space"));
    }

    #[test]
    fn interrupted_session_with_a_missing_source_keeps_its_other_wav() {
        let home = tempfile::tempdir().unwrap();
        let settings = settings(home.path());
        let (folder, mut session) = save_example(&settings.folder);
        let original = fs::read(folder.join(TRACK_FILES[0])).unwrap();
        fs::remove_file(folder.join(TRACK_FILES[1])).unwrap();
        session.state = SaveState::InProgress;
        session.save(&folder).unwrap();
        let library = library(&settings);
        let recovered = &library.sessions[0];
        assert!(recovered.session.state == SaveState::Recovered);
        assert_eq!(recovered.available_tracks, [true, false]);
        assert_eq!(fs::read(folder.join(TRACK_FILES[0])).unwrap(), original);
    }
}
