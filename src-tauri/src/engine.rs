use crate::capture::{self, Clock, Source};
use crate::recording::{AudioTest, Input, Phase, Recording, Snapshot};
use crate::storage::{self, Result, Session, Settings};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TICK: Duration = Duration::from_millis(25);

type Job = Box<dyn FnOnce(&mut Worker) + Send>;

#[derive(Clone)]
pub struct Engine {
    commands: SyncSender<Job>,
    snapshot: Arc<Mutex<Snapshot>>,
    pub closing: Arc<AtomicBool>,
}

impl Engine {
    pub fn start(data: PathBuf, recordings: PathBuf) -> Result<Self> {
        fs::create_dir_all(&data)?;
        let lock = storage::open_lock(&data.join("nexq.lock")).map_err(|error| {
            format!("Could not open NexQ Recorder. Check for another NexQ window. ({error})")
        })?;
        let config = data.join("settings.json");
        let settings = Settings::load(&config, recordings)?;
        let clock = Clock::new()?;
        let (commands, receiver) = mpsc::sync_channel::<Job>(16);
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        let status = Arc::clone(&snapshot);
        std::thread::Builder::new()
            .name("nexq-recorder".into())
            .spawn(move || {
                let _lock = lock;
                let mut worker = Worker {
                    clock,
                    config,
                    settings,
                    recording: None,
                    test: None,
                    last: Snapshot::default(),
                };
                loop {
                    match receiver.recv_timeout(TICK) {
                        Ok(job) => job(&mut worker),
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            let _ = worker.stop(None, false);
                            break;
                        }
                    }
                    worker.tick();
                    *status.lock().unwrap() = worker.snapshot();
                }
            })?;
        Ok(Self {
            commands,
            snapshot,
            closing: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().unwrap().clone()
    }

    pub async fn request<T: Send + 'static>(
        &self,
        job: impl FnOnce(&mut Worker) -> Result<T> + Send + 'static,
    ) -> std::result::Result<T, String> {
        let (reply, receive) = mpsc::sync_channel(1);
        self.commands
            .try_send(Box::new(move |worker| {
                let _ = reply.send(job(worker).map_err(|error| error.to_string()));
            }))
            .map_err(|error| {
                match error {
                    TrySendError::Full(_) => "NexQ Recorder is busy. Try again shortly.",
                    TrySendError::Disconnected(_) => "The recording engine is unavailable.",
                }
                .to_owned()
            })?;
        tauri::async_runtime::spawn_blocking(move || {
            receive
                .recv()
                .map_err(|_| "The recording engine did not respond.".to_owned())?
        })
        .await
        .map_err(|error| error.to_string())?
    }
}

pub struct Worker {
    clock: Clock,
    config: PathBuf,
    pub settings: Settings,
    recording: Option<Recording>,
    test: Option<AudioTest>,
    last: Snapshot,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Start,
    Pause,
    Resume,
    Stop,
    Bookmark,
    EndTest,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preferences {
    pub microphone: Option<String>,
    pub output: Option<String>,
    pub folder: String,
    pub available_bytes: Option<u64>,
    pub outputs: Vec<capture::Device>,
    pub microphones: Vec<capture::Device>,
    pub device_warnings: Vec<String>,
}

impl Worker {
    fn tick(&mut self) {
        let now = self.clock.now();
        if let Some(recording) = &mut self.recording {
            match recording.tick(now) {
                Ok(Some(reason)) => {
                    let _ = self.stop(Some(reason), false);
                }
                Err(error) => {
                    let _ = self.stop(Some(format!("Recording write failed: {error}")), true);
                }
                Ok(None) => {}
            }
        }
        if let Some(test) = &mut self.test {
            if now >= test.expires {
                self.test = None;
            } else {
                for input in test.inputs.iter_mut().flatten() {
                    input.poll(now, |_| true);
                }
            }
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        if let Some(recording) = &self.recording {
            recording.snapshot(self.clock.now())
        } else if let Some(test) = &self.test {
            test.snapshot()
        } else {
            self.last.clone()
        }
    }

    pub fn require_idle(&self) -> Result<()> {
        if self.recording.is_some() {
            return Err("Stop the current recording first.".into());
        }
        Ok(())
    }

    fn input(&self, source: Source) -> std::result::Result<Input, String> {
        let id = match source {
            Source::Interviewer => self.settings.output.as_deref(),
            Source::Me => self.settings.microphone.as_deref(),
        };
        Input::open(source, id)
    }

    pub fn preferences(&self) -> Preferences {
        let mut device_warnings = Vec::new();
        let [outputs, microphones] = Source::ALL.map(|source| {
            capture::devices(source).unwrap_or_else(|error| {
                device_warnings.push(error);
                Vec::new()
            })
        });
        Preferences {
            microphone: self.settings.microphone.clone(),
            output: self.settings.output.clone(),
            folder: storage::display_path(&self.settings.folder),
            available_bytes: fs2::available_space(&self.settings.folder).ok(),
            outputs,
            microphones,
            device_warnings,
        }
    }

    pub fn set_preferences(
        &mut self,
        microphone: Option<String>,
        output: Option<String>,
        folder: PathBuf,
    ) -> Result<Preferences> {
        self.require_idle()?;
        self.test = None;
        if !folder.is_absolute() || !folder.is_dir() {
            return Err("Choose an existing recording folder.".into());
        }
        let folder = folder.canonicalize()?;
        let writable = tempfile::NamedTempFile::new_in(&folder);
        writable.map_err(|error| format!("Cannot write to the selected folder: {error}"))?;
        let mut next = self.settings.clone();
        next.microphone = microphone;
        next.output = output;
        if next.folder != folder {
            next.previous_folders.retain(|location| location != &folder);
            next.previous_folders
                .push(std::mem::replace(&mut next.folder, folder));
        }
        next.save(&self.config)?;
        self.settings = next;
        Ok(self.preferences())
    }

    pub fn action(&mut self, action: Action) -> Result<Snapshot> {
        match action {
            Action::Start => {
                self.require_idle()?;
                self.test = None;
                let interviewer = self.input(Source::Interviewer)?;
                let me = self.input(Source::Me)?;
                fs::create_dir_all(&self.settings.folder)?;
                self.recording = Some(Recording::start(
                    [interviewer, me],
                    &self.settings.folder,
                    &self.clock,
                )?);
            }
            Action::Pause | Action::Resume | Action::Bookmark => {
                let recording = self
                    .recording
                    .as_mut()
                    .ok_or("There is no active recording.")?;
                let result = match action {
                    Action::Pause => recording.pause(&self.clock),
                    Action::Resume => recording.resume(&self.clock),
                    Action::Bookmark => recording.bookmark(self.clock.now()),
                    _ => unreachable!(),
                };
                if let Err(error) = result {
                    self.stop(Some(format!("Recording write failed: {error}")), true)?;
                }
            }
            Action::Stop => {
                self.stop(None, false)?;
            }
            Action::EndTest => self.test = None,
        }
        Ok(self.snapshot())
    }

    pub fn test_devices(
        &mut self,
        microphone: Option<String>,
        output: Option<String>,
    ) -> Result<Snapshot> {
        self.require_idle()?;
        self.test = None;
        let now = self.clock.now();
        let start = |source, id: Option<&str>| {
            test_input(Input::open(source, id).and_then(|mut input| {
                input.play(now)?;
                Ok(input)
            }))
        };
        let (interviewer, interviewer_error) = start(Source::Interviewer, output.as_deref());
        let (me, me_error) = start(Source::Me, microphone.as_deref());
        self.test = Some(AudioTest {
            inputs: [interviewer, me],
            errors: [interviewer_error, me_error],
            expires: self.clock.now() + Duration::from_secs(30),
        });
        Ok(self.snapshot())
    }

    pub fn stop(&mut self, reason: Option<String>, write_failed: bool) -> Result<Option<Session>> {
        self.test = None;
        let Some(recording) = self.recording.take() else {
            return Ok(None);
        };
        let (snapshot, result) = recording.finish(&self.clock, reason, write_failed);
        self.last = snapshot;
        result.map(Some)
    }
}

fn test_input(result: std::result::Result<Input, String>) -> (Option<Input>, Option<String>) {
    match result {
        Ok(input) => (Some(input), None),
        Err(error) => (None, Some(error)),
    }
}

pub struct AppState {
    pub engine: std::result::Result<Engine, String>,
}

impl AppState {
    pub fn engine(&self) -> std::result::Result<Engine, String> {
        self.engine.clone()
    }

    pub fn snapshot(&self) -> Snapshot {
        self.engine.as_ref().map_or_else(
            |message| Snapshot {
                phase: Phase::Interrupted,
                message: Some(message.clone()),
                ..Snapshot::default()
            },
            Engine::snapshot,
        )
    }
}
