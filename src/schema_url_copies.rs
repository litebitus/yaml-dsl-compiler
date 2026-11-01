//! Schemas read from URLs, and the copies kept of them, by the editor's rules.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const EDITOR_PARAMETER_PREFIX: &str = "yaml_dsl_";
const MAX_AGE_PARAMETER: &str = "yaml_dsl_max_age_days";
const DAY_SECONDS: u64 = 24 * 60 * 60;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const STATUS_OK: u16 = 200;
const STATUS_NOT_MODIFIED: u16 = 304;

/// Whether a schema location is a URL rather than a path.
pub fn is_schema_url(location: &str) -> bool {
    location.starts_with("http://") || location.starts_with("https://")
}

/// Downloads schema URLs and keeps a copy of each in a folder the caller names.
pub struct SchemaUrlCopies {
    directory: PathBuf,
    agent: ureq::Agent,
}

/// A URL as fetched, without the editor's parameters, and the max age it gives.
#[derive(Debug, PartialEq, Eq)]
struct EditorParameters {
    fetch_url: String,
    max_age_days: u64,
}

#[derive(Serialize, Deserialize)]
struct KeptCopy {
    url: String,
    etag: Option<String>,
    last_modified: Option<String>,
    /// When the server last sent or confirmed the copy, in seconds since the epoch.
    confirmed_at: u64,
    body: String,
}

impl SchemaUrlCopies {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(REQUEST_TIMEOUT))
            .build()
            .into();
        Self {
            directory: directory.into(),
            agent,
        }
    }

    /// The schema text at `url`: the kept copy while it is younger than its max
    /// age, else what the server sends or confirms, else the kept copy when the
    /// server does not answer.
    pub fn read(&self, url: &str) -> Result<String, String> {
        let parameters = editor_parameters(url)?;
        let copy_path = self.copy_path(&parameters.fetch_url);
        let kept: Option<KeptCopy> = fs::read_to_string(&copy_path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok());
        let now = seconds_now();
        if let Some(kept) = &kept
            && parameters.max_age_days > 0
            && now.saturating_sub(kept.confirmed_at) < parameters.max_age_days * DAY_SECONDS
        {
            return Ok(kept.body.clone());
        }
        let mut request = self.agent.get(&parameters.fetch_url);
        if let Some(kept) = &kept {
            if let Some(etag) = &kept.etag {
                request = request.header("If-None-Match", etag);
            } else if let Some(last_modified) = &kept.last_modified {
                request = request.header("If-Modified-Since", last_modified);
            }
        }
        let answer = request.call();
        let copy = match (answer, kept) {
            (Ok(mut response), _) if response.status().as_u16() == STATUS_OK => {
                let header = |name: &str| {
                    response
                        .headers()
                        .get(name)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned)
                };
                let (etag, last_modified) = (header("etag"), header("last-modified"));
                let body = response
                    .body_mut()
                    .read_to_string()
                    .map_err(|error| format!("schema URL `{url}`: {error}"))?;
                KeptCopy {
                    url: parameters.fetch_url.clone(),
                    etag,
                    last_modified,
                    confirmed_at: now,
                    body,
                }
            }
            (Ok(response), Some(kept)) if response.status().as_u16() == STATUS_NOT_MODIFIED => {
                KeptCopy {
                    confirmed_at: now,
                    ..kept
                }
            }
            (Ok(response), None) => {
                return Err(format!(
                    "schema URL `{url}` answered {}",
                    response.status().as_u16()
                ));
            }
            (Ok(_), Some(kept)) => return Ok(kept.body),
            (Err(_), Some(kept)) => return Ok(kept.body),
            (Err(error), None) => return Err(format!("schema URL `{url}`: {error}")),
        };
        self.keep(&copy_path, &copy);
        Ok(copy.body)
    }

    fn copy_path(&self, fetch_url: &str) -> PathBuf {
        let digest = Sha256::digest(fetch_url.as_bytes());
        let name: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        self.directory.join(format!("{name}.json"))
    }

    /// Keeps `copy`, written beside its place and moved there, so a run reading the
    /// same URL never reads half a copy. A copy that cannot be kept is fetched again.
    fn keep(&self, copy_path: &PathBuf, copy: &KeptCopy) {
        let Ok(text) = serde_json::to_string(copy) else {
            return;
        };
        let partial = copy_path.with_extension(format!("{}.partial", std::process::id()));
        let kept = fs::create_dir_all(&self.directory)
            .and_then(|()| fs::write(&partial, text))
            .and_then(|()| fs::rename(&partial, copy_path));
        if kept.is_err() {
            let _ = fs::remove_file(&partial);
        }
    }
}

/// The URL the server is asked for, with the editor's parameters taken out and
/// every other parameter kept as written, in its order.
fn editor_parameters(url: &str) -> Result<EditorParameters, String> {
    let Some((base, query)) = url.split_once('?') else {
        return Ok(EditorParameters {
            fetch_url: url.to_owned(),
            max_age_days: 0,
        });
    };
    let mut passed = Vec::new();
    let mut max_age_days = 0;
    for parameter in query.split('&') {
        let (name, value) = parameter.split_once('=').unwrap_or((parameter, ""));
        if !name.starts_with(EDITOR_PARAMETER_PREFIX) {
            passed.push(parameter);
        } else if name != MAX_AGE_PARAMETER {
            return Err(format!("`{name}` is not a parameter the editor reads"));
        } else {
            max_age_days = value
                .parse()
                .map_err(|_| format!("`{MAX_AGE_PARAMETER}` must be a whole number of days"))?;
        }
    }
    let fetch_url = if passed.is_empty() {
        base.to_owned()
    } else {
        format!("{base}?{}", passed.join("&"))
    };
    Ok(EditorParameters {
        fetch_url,
        max_age_days,
    })
}

fn seconds_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread::{self, JoinHandle};

    use super::*;

    const SCHEMA: &str = r#"{"type": "object"}"#;
    const ETAG: &str = "\"mock-etag\"";

    const SILENT_SERVER_POLL: Duration = Duration::from_millis(5);

    /// An HTTP server on a port of its own, answering every request with the schema,
    /// or with `304` to a request carrying its ETag, and counting the full answers it
    /// sends. Dropping it, as a failing test does too, stops and joins its thread.
    struct MockSchemaServer {
        url: String,
        full_answers: Arc<AtomicUsize>,
        server: Arc<tiny_http::Server>,
        serving: Option<JoinHandle<()>>,
    }

    impl MockSchemaServer {
        fn start() -> Self {
            let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
            let url = format!("http://{}/mock.schema.json", server.server_addr());
            let full_answers = Arc::new(AtomicUsize::new(0));
            let (serving, counting) = (Arc::clone(&server), Arc::clone(&full_answers));
            let serving = thread::spawn(move || {
                for request in serving.incoming_requests() {
                    let confirms = request.headers().iter().any(|header| {
                        header.field.equiv("If-None-Match") && header.value.as_str() == ETAG
                    });
                    let etag = tiny_http::Header::from_bytes("ETag", ETAG).unwrap();
                    let response = if confirms {
                        tiny_http::Response::from_string("").with_status_code(304)
                    } else {
                        counting.fetch_add(1, Ordering::SeqCst);
                        tiny_http::Response::from_string(SCHEMA)
                    };
                    let _ = request.respond(response.with_header(etag));
                }
            });
            Self {
                url,
                full_answers,
                server,
                serving: Some(serving),
            }
        }
    }

    impl Drop for MockSchemaServer {
        fn drop(&mut self) {
            self.server.unblock();
            if let Some(serving) = self.serving.take() {
                let _ = serving.join();
            }
        }
    }

    /// A port of its own that takes every connection and closes it unanswered.
    /// Dropping it, as a failing test does too, stops and joins its thread.
    struct SilentServer {
        url: String,
        stopped: Arc<AtomicBool>,
        serving: Option<JoinHandle<()>>,
    }

    impl SilentServer {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}/mock.schema.json", listener.local_addr().unwrap());
            let stopped = Arc::new(AtomicBool::new(false));
            let stopping = Arc::clone(&stopped);
            let serving = thread::spawn(move || {
                while !stopping.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((connection, _)) => drop(connection),
                        Err(error) if error.kind() == ErrorKind::WouldBlock => {
                            thread::sleep(SILENT_SERVER_POLL);
                        }
                        Err(_) => return,
                    }
                }
            });
            Self {
                url,
                stopped,
                serving: Some(serving),
            }
        }
    }

    impl Drop for SilentServer {
        fn drop(&mut self) {
            self.stopped.store(true, Ordering::SeqCst);
            if let Some(serving) = self.serving.take() {
                let _ = serving.join();
            }
        }
    }

    struct ScratchDirectory(PathBuf);

    impl ScratchDirectory {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("schema-urls-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            Self(path)
        }
    }

    impl Drop for ScratchDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_editor_parameters_are_taken_out_and_others_kept_in_order() {
        assert_eq!(
            editor_parameters("https://mock.example/s.json?b=2&yaml_dsl_max_age_days=7&a=1"),
            Ok(EditorParameters {
                fetch_url: "https://mock.example/s.json?b=2&a=1".into(),
                max_age_days: 7,
            })
        );
        assert!(editor_parameters("https://mock.example/s.json?yaml_dsl_other=1").is_err());
        assert!(editor_parameters("https://mock.example/s.json?yaml_dsl_max_age_days=x").is_err());
    }

    #[test]
    fn a_copy_younger_than_its_max_age_is_read_without_asking_the_server() {
        let server = MockSchemaServer::start();
        let scratch = ScratchDirectory::new("max-age");
        let copies = SchemaUrlCopies::new(&scratch.0);
        let aged = format!("{}?yaml_dsl_max_age_days=1", server.url);
        assert_eq!(copies.read(&aged).unwrap(), SCHEMA);
        assert_eq!(copies.read(&aged).unwrap(), SCHEMA);
        assert_eq!(server.full_answers.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn with_no_max_age_the_copy_is_checked_and_a_304_keeps_it() {
        let server = MockSchemaServer::start();
        let scratch = ScratchDirectory::new("checked");
        let copies = SchemaUrlCopies::new(&scratch.0);
        assert_eq!(copies.read(&server.url).unwrap(), SCHEMA);
        assert_eq!(copies.read(&server.url).unwrap(), SCHEMA);
        assert_eq!(server.full_answers.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_kept_copy_is_read_when_the_server_does_not_answer() {
        let server = SilentServer::start();
        let scratch = ScratchDirectory::new("offline");
        let copies = SchemaUrlCopies::new(&scratch.0);
        let unanswered = server.url.as_str();
        assert!(copies.read(unanswered).is_err());
        copies.keep(
            &copies.copy_path(unanswered),
            &KeptCopy {
                url: unanswered.to_owned(),
                etag: Some(ETAG.to_owned()),
                last_modified: None,
                confirmed_at: 0,
                body: SCHEMA.to_owned(),
            },
        );
        assert_eq!(copies.read(unanswered).unwrap(), SCHEMA);
    }
}
