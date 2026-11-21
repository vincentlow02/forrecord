use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tauri::http::{Method, Request, Response, StatusCode, header};
use uuid::Uuid;

const CHUNK: u64 = 1024 * 1024;
const APP_ORIGIN: &str = "http://tauri.localhost";

#[derive(Clone, Default)]
pub struct Player(Arc<Mutex<Option<Track>>>);

struct Track {
    ticket: String,
    audio: Arc<Mutex<File>>,
}

impl Player {
    pub fn prepare(&self, path: &Path) -> crate::storage::Result<String> {
        let audio = File::open(path)?;
        // Validate the format without reading the audio samples.
        hound::WavReader::new(std::io::BufReader::new(&audio))?;
        let ticket = Uuid::new_v4().simple().to_string();
        *self.0.lock().unwrap() = Some(Track {
            ticket: ticket.clone(),
            audio: Arc::new(Mutex::new(audio)),
        });
        Ok(ticket)
    }

    pub fn release(&self, ticket: Option<&str>) {
        let mut track = self.0.lock().unwrap();
        if ticket.is_none()
            || track
                .as_ref()
                .is_some_and(|track| Some(track.ticket.as_str()) == ticket)
        {
            *track = None;
        }
    }

    pub fn serve(&self, label: &str, request: Request<Vec<u8>>) -> Response<Vec<u8>> {
        if label != "main"
            || request
                .headers()
                .get(header::ORIGIN)
                .is_some_and(|value| value != APP_ORIGIN)
        {
            return empty(StatusCode::FORBIDDEN);
        }
        let method = request.method();
        if !matches!(*method, Method::GET | Method::HEAD) {
            return empty(StatusCode::METHOD_NOT_ALLOWED);
        }
        let path = request.uri().path().strip_prefix('/').unwrap_or_default();
        let audio = {
            let track = self.0.lock().unwrap();
            track
                .as_ref()
                .filter(|track| track.ticket == path)
                .map(|track| track.audio.clone())
        };
        let Some(audio) = audio else {
            return empty(StatusCode::NOT_FOUND);
        };
        let mut audio = audio.lock().unwrap();
        let Ok(size) = audio.metadata().map(|metadata| metadata.len()) else {
            return empty(StatusCode::INTERNAL_SERVER_ERROR);
        };
        let range = request.headers().get(header::RANGE);
        let mut builder = Response::builder()
            .header(header::CONTENT_TYPE, "audio/wav")
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, APP_ORIGIN)
            .header(header::CACHE_CONTROL, "no-store")
            .header("X-Content-Type-Options", "nosniff");

        if *method == Method::HEAD {
            return builder
                .header(header::CONTENT_LENGTH, size)
                .body(vec![])
                .unwrap();
        }

        let Some((start, end)) = requested_slice(range, size) else {
            return builder
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{size}"))
                .body(vec![])
                .unwrap();
        };
        let amount = end - start + 1;
        if range.is_some() || amount != size {
            builder = builder
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"));
        }
        let mut bytes = vec![0; amount as usize];
        if audio
            .seek(SeekFrom::Start(start))
            .and_then(|_| audio.read_exact(&mut bytes))
            .is_err()
        {
            return empty(StatusCode::INTERNAL_SERVER_ERROR);
        }
        builder
            .header(header::CONTENT_LENGTH, amount)
            .body(bytes)
            .unwrap()
    }
}

fn requested_slice(range: Option<&header::HeaderValue>, size: u64) -> Option<(u64, u64)> {
    let last = size.checked_sub(1)?;
    let Some(range) = range else {
        return Some((0, last.min(CHUNK - 1)));
    };
    let (first, end) = range
        .to_str()
        .ok()?
        .strip_prefix("bytes=")?
        .split_once('-')?;
    if first.is_empty() {
        let count = end.parse::<u64>().ok()?.min(CHUNK).min(size);
        return (count > 0).then_some((size - count, last));
    }
    let first = first.parse::<u64>().ok()?;
    let wanted = if end.is_empty() {
        last
    } else {
        end.parse::<u64>().ok()?.min(last)
    };
    (first <= wanted).then_some((first, wanted.min(first.saturating_add(CHUNK - 1))))
}

fn empty(status: StatusCode) -> Response<Vec<u8>> {
    Response::builder().status(status).body(vec![]).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_seek_requests_with_a_fixed_memory_ceiling() {
        let range =
            |value, size| requested_slice(Some(&header::HeaderValue::from_static(value)), size);
        assert_eq!(range("bytes=10-30", 25), Some((10, 24)));
        assert_eq!(range("bytes=10-", CHUNK * 5), Some((10, CHUNK + 9)));
        assert_eq!(range("bytes=-2", 50), Some((48, 49)));
        assert_eq!(range("bytes=50-", 50), None);
        assert_eq!(range("bytes=0-2,4-8", 50), None);
        assert_eq!(range("bytes=5-2", 50), None);
    }

    #[test]
    fn playback_serves_only_the_active_ticket_to_the_app_window() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("test.wav");
        let mut wav = crate::storage::WavSink::create(&path).unwrap();
        wav.write(&[100, 200]).unwrap();
        wav.finish().unwrap();
        let player = Player::default();
        let ticket = player.prepare(&path).unwrap();
        let request = |token: &str, origin| {
            Request::builder()
                .uri(format!("http://nexq-audio.localhost/{token}"))
                .header(header::ORIGIN, origin)
                .header(header::RANGE, "bytes=44-47")
                .body(vec![])
                .unwrap()
        };
        assert_eq!(
            player
                .serve("main", request(&ticket, "https://example.test"))
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            player.serve("other", request(&ticket, APP_ORIGIN)).status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            player.serve("main", request("other", APP_ORIGIN)).status(),
            StatusCode::NOT_FOUND
        );
        let response = player.serve("main", request(&ticket, APP_ORIGIN));
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.body(), &[100, 0, 200, 0]);
        player.release(Some(&ticket));
        assert_eq!(
            player.serve("main", request(&ticket, APP_ORIGIN)).status(),
            StatusCode::NOT_FOUND
        );
    }
}
