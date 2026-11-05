use std::{
    cmp::Reverse,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    fs::{self, OpenOptions},
    io::AsyncWriteExt,
};
use uuid::Uuid;

use crate::{
    config::{AppPaths, AppStateFile, BackendKind},
    domain::{ChatMessage, Role},
};

pub const SESSION_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionRecord {
    Session {
        schema_version: u32,
        id: Uuid,
        created_at: DateTime<Utc>,
        backend: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        recovered_from: Option<Uuid>,
    },
    Message {
        schema_version: u32,
        #[serde(flatten)]
        message: ChatMessage,
    },
}

#[derive(Clone, Debug)]
pub struct LoadedSession {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub backend: String,
    pub messages: Vec<ChatMessage>,
    pub path: PathBuf,
    pub recovered_from: Option<Uuid>,
    pub had_interruption: bool,
}

impl LoadedSession {
    pub fn title(&self) -> String {
        session_title(&self.messages)
    }
}

#[derive(Clone, Debug)]
pub struct SessionSummary {
    pub id: Uuid,
    pub title: String,
    pub modified_at: DateTime<Utc>,
    pub message_count: usize,
    pub recovered: bool,
}

#[derive(Clone, Debug)]
pub struct Storage {
    paths: AppPaths,
}

impl Storage {
    pub fn new(paths: AppPaths) -> Self {
        Self { paths }
    }

    pub const fn paths(&self) -> &AppPaths {
        &self.paths
    }

    pub async fn initialize(&self) -> Result<(), StorageError> {
        create_private_dir(&self.paths.root).await?;
        create_private_dir(&self.paths.chats).await?;
        create_private_dir(&self.paths.logs).await?;
        Ok(())
    }

    pub async fn create_session(&self) -> Result<LoadedSession, StorageError> {
        self.create_session_with_backend(BackendKind::Ollama).await
    }

    pub async fn create_session_with_backend(
        &self,
        backend: BackendKind,
    ) -> Result<LoadedSession, StorageError> {
        self.create_session_record(None, backend.storage_name().to_owned())
            .await
    }

    async fn create_session_recovered_from(
        &self,
        recovered_from: Option<Uuid>,
        backend: String,
    ) -> Result<LoadedSession, StorageError> {
        self.create_session_record(recovered_from, backend).await
    }

    async fn create_session_record(
        &self,
        recovered_from: Option<Uuid>,
        backend: String,
    ) -> Result<LoadedSession, StorageError> {
        let id = Uuid::now_v7();
        let created_at = Utc::now();
        let path = self.session_path(id);
        let record = SessionRecord::Session {
            schema_version: SESSION_SCHEMA_VERSION,
            id,
            created_at,
            backend: backend.clone(),
            recovered_from,
        };
        create_record_file(&path, &record).await?;
        Ok(LoadedSession {
            id,
            created_at,
            backend,
            messages: Vec::new(),
            path,
            recovered_from,
            had_interruption: false,
        })
    }

    pub async fn append_message(
        &self,
        session_id: Uuid,
        message: &ChatMessage,
    ) -> Result<(), StorageError> {
        let path = self.session_path(session_id);
        let record = SessionRecord::Message {
            schema_version: SESSION_SCHEMA_VERSION,
            message: message.clone(),
        };
        append_record(&path, &record).await
    }

    pub async fn load_session(&self, id: Uuid) -> Result<LoadedSession, StorageError> {
        let path = self.session_path(id);
        let parsed = parse_session_file(&path).await?;
        if parsed.corrupt {
            return self.recover_session(parsed.session).await;
        }
        Ok(parsed.session)
    }

    async fn recover_session(&self, source: LoadedSession) -> Result<LoadedSession, StorageError> {
        let mut recovered = self
            .create_session_recovered_from(Some(source.id), source.backend.clone())
            .await?;
        for message in &source.messages {
            self.append_message(recovered.id, message).await?;
        }
        recovered.messages = source.messages;
        recovered.had_interruption = true;
        Ok(recovered)
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>, StorageError> {
        let mut entries = match fs::read_dir(&self.paths.chats).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(StorageError::Io(error)),
        };

        let mut summaries = Vec::new();
        loop {
            let entry = match entries.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(error) => {
                    tracing::warn!(%error, "skipping an unreadable chat entry");
                    continue;
                }
            };
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                continue;
            }
            let parsed = match parse_session_file(&path).await {
                Ok(parsed) => parsed,
                Err(error) => {
                    // A single broken or unreadable session must not hide every
                    // other valid chat log or block startup.
                    tracing::warn!(%error, path = %path.display(), "skipping an invalid chat entry");
                    continue;
                }
            };
            let modified_at = match entry.metadata().await {
                Ok(metadata) => metadata
                    .modified()
                    .map_or(parsed.session.created_at, DateTime::<Utc>::from),
                Err(error) => {
                    tracing::warn!(%error, path = %path.display(), "using session time for an unreadable chat entry");
                    parsed.session.created_at
                }
            };
            summaries.push(SessionSummary {
                id: parsed.session.id,
                title: parsed.session.title(),
                modified_at,
                message_count: parsed.session.messages.len(),
                recovered: parsed.corrupt || parsed.session.recovered_from.is_some(),
            });
        }
        summaries.sort_by_key(|summary| Reverse(summary.modified_at));
        Ok(summaries)
    }

    pub async fn load_state(&self) -> Result<AppStateFile, StorageError> {
        let contents = match fs::read_to_string(&self.paths.state).await {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(AppStateFile::default());
            }
            Err(error) => return Err(StorageError::Io(error)),
        };
        match serde_json::from_str::<AppStateFile>(&contents) {
            Ok(state) => Ok(state),
            Err(error) => {
                self.preserve_corrupt_state(&contents).await?;
                tracing::warn!(
                    %error,
                    state = %self.paths.state.display(),
                    "discarding corrupt state file and using defaults"
                );
                Ok(AppStateFile::default())
            }
        }
    }

    /// Move a corrupt `state.json` aside under a unique `.corrupt` name so the
    /// on-disk evidence is preserved without blocking subsequent startups.
    async fn preserve_corrupt_state(&self, contents: &str) -> Result<(), StorageError> {
        if contents.trim().is_empty() {
            return Ok(());
        }
        // A timestamp plus UUID suffix keeps the backup unique even when two
        // corruptions land in the same second.
        let timestamp = Utc::now().format("%Y%m%d%H%M%S");
        let suffix = Uuid::now_v7().simple().to_string();
        let candidate = self
            .paths
            .state
            .with_file_name(format!("state.json.{timestamp}.{suffix}.corrupt"));
        if let Err(error) = fs::rename(&self.paths.state, &candidate).await {
            tracing::warn!(
                %error,
                from = %self.paths.state.display(),
                to = %candidate.display(),
                "could not preserve corrupt state file"
            );
            return Err(StorageError::Io(error));
        }
        Ok(())
    }

    pub async fn save_state(&self, state: &AppStateFile) -> Result<(), StorageError> {
        let temporary = self.paths.state.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(state)?;
        write_private_file(&temporary, &bytes).await?;
        fs::rename(&temporary, &self.paths.state).await?;
        Ok(())
    }

    fn session_path(&self, id: Uuid) -> PathBuf {
        self.paths.chats.join(format!("{id}.jsonl"))
    }
}

struct ParsedSession {
    session: LoadedSession,
    corrupt: bool,
}

async fn parse_session_file(path: &Path) -> Result<ParsedSession, StorageError> {
    let contents = fs::read_to_string(path).await?;
    let ends_in_newline = contents.ends_with('\n');
    let lines: Vec<_> = contents.lines().collect();
    let mut id = None;
    let mut created_at = None;
    let mut recovered_from = None;
    let mut backend = None;
    let mut messages = Vec::new();
    let mut corrupt = false;

    for (index, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record: SessionRecord = match serde_json::from_str(line) {
            Ok(record) => record,
            Err(_error) if index + 1 == lines.len() && !ends_in_newline => {
                corrupt = true;
                break;
            }
            Err(_) => {
                corrupt = true;
                break;
            }
        };
        match record {
            SessionRecord::Session {
                schema_version,
                id: session_id,
                created_at: session_created_at,
                backend: session_backend,
                recovered_from: source,
            } => {
                validate_schema(schema_version)?;
                if id.is_some() || !messages.is_empty() {
                    corrupt = true;
                    break;
                }
                id = Some(session_id);
                created_at = Some(session_created_at);
                backend = Some(session_backend);
                recovered_from = source;
            }
            SessionRecord::Message {
                schema_version,
                message,
            } => {
                validate_schema(schema_version)?;
                if id.is_none() {
                    corrupt = true;
                    break;
                }
                messages.push(message);
            }
        }
    }

    let id = id.ok_or_else(|| StorageError::InvalidSession(path.to_path_buf()))?;
    let created_at = created_at.ok_or_else(|| StorageError::InvalidSession(path.to_path_buf()))?;
    let had_interruption = messages
        .last()
        .is_some_and(|message| message.role == Role::User);
    Ok(ParsedSession {
        session: LoadedSession {
            id,
            created_at,
            backend: backend.ok_or_else(|| StorageError::InvalidSession(path.to_path_buf()))?,
            messages,
            path: path.to_path_buf(),
            recovered_from,
            had_interruption,
        },
        corrupt,
    })
}

fn validate_schema(schema_version: u32) -> Result<(), StorageError> {
    if schema_version != SESSION_SCHEMA_VERSION {
        return Err(StorageError::UnsupportedSchema {
            found: schema_version,
            supported: SESSION_SCHEMA_VERSION,
        });
    }
    Ok(())
}

async fn create_record_file(path: &Path, record: &SessionRecord) -> Result<(), StorageError> {
    let mut bytes = serde_json::to_vec(record)?;
    bytes.push(b'\n');
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    set_private_file_mode(&mut options);
    let mut file = options.open(path).await?;
    file.write_all(&bytes).await?;
    file.flush().await?;
    Ok(())
}

async fn append_record(path: &Path, record: &SessionRecord) -> Result<(), StorageError> {
    let mut bytes = serde_json::to_vec(record)?;
    bytes.push(b'\n');
    let mut options = OpenOptions::new();
    options.append(true);
    set_private_file_mode(&mut options);
    let mut file = options.open(path).await?;
    file.write_all(&bytes).await?;
    file.flush().await?;
    Ok(())
}

async fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), StorageError> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    set_private_file_mode(&mut options);
    let mut file = options.open(path).await?;
    file.write_all(bytes).await?;
    file.flush().await?;
    Ok(())
}

async fn create_private_dir(path: &Path) -> Result<(), StorageError> {
    fs::create_dir_all(path).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).await?;
    }
    Ok(())
}

fn set_private_file_mode(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        options.mode(0o600);
    }
}

fn session_title(messages: &[ChatMessage]) -> String {
    let Some(first) = messages.iter().find(|message| message.role == Role::User) else {
        return "Untitled link".to_owned();
    };
    let compact = first
        .content
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut chars = compact.chars();
    let title: String = chars.by_ref().take(64).collect();
    if chars.next().is_some() {
        format!("{title}…")
    } else if title.is_empty() {
        "Untitled link".to_owned()
    } else {
        title
    }
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("storage I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("session data is invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("session file has no valid header: {0}")]
    InvalidSession(PathBuf),
    #[error("unsupported chat schema {found}; this build supports {supported}")]
    UnsupportedSchema { found: u32, supported: u32 },
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    use super::*;
    use crate::{
        config::AppPaths,
        domain::{MessageStatus, Usage},
    };

    async fn test_storage() -> (TempDir, Storage) {
        let temporary = TempDir::new().unwrap();
        let storage = Storage::new(AppPaths::from_root(temporary.path().join("xikoi")));
        storage.initialize().await.unwrap();
        (temporary, storage)
    }

    #[tokio::test]
    async fn creates_appends_lists_and_resumes_a_session() {
        let (_temporary, storage) = test_storage().await;
        let mut session = storage.create_session().await.unwrap();
        let message = ChatMessage::user("Teach me ownership");
        storage.append_message(session.id, &message).await.unwrap();
        session.messages.push(message);

        let loaded = storage.load_session(session.id).await.unwrap();
        assert_eq!(loaded.messages, session.messages);
        assert_eq!(loaded.title(), "Teach me ownership");

        let summaries = storage.list_sessions().await.unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].title, "Teach me ownership");
    }

    #[tokio::test]
    async fn uses_private_unix_permissions() {
        let (_temporary, storage) = test_storage().await;
        let session = storage.create_session().await.unwrap();
        let directory_mode = std::fs::metadata(&storage.paths().chats)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let file_mode = std::fs::metadata(&session.path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }

    #[tokio::test]
    async fn recovers_a_truncated_final_record_into_a_new_file() {
        let (_temporary, storage) = test_storage().await;
        let session = storage.create_session().await.unwrap();
        let message = ChatMessage::user("Persist this");
        storage.append_message(session.id, &message).await.unwrap();

        let mut file = OpenOptions::new()
            .append(true)
            .open(&session.path)
            .await
            .unwrap();
        file.write_all(br#"{"type":"message""#).await.unwrap();
        file.flush().await.unwrap();

        let recovered = storage.load_session(session.id).await.unwrap();
        assert_ne!(recovered.id, session.id);
        assert_eq!(recovered.recovered_from, Some(session.id));
        assert_eq!(recovered.messages, vec![message]);
        assert!(recovered.had_interruption);
    }

    #[tokio::test]
    async fn recovers_only_the_valid_prefix_before_interior_corruption() {
        let (_temporary, storage) = test_storage().await;
        let session = storage.create_session().await.unwrap();
        let prefix = ChatMessage::user("Keep this prefix");
        storage.append_message(session.id, &prefix).await.unwrap();

        let mut file = OpenOptions::new()
            .append(true)
            .open(&session.path)
            .await
            .unwrap();
        file.write_all(b"{not valid json}\n").await.unwrap();
        file.flush().await.unwrap();
        let ignored_tail = ChatMessage::user("Do not recover this tail");
        storage
            .append_message(session.id, &ignored_tail)
            .await
            .unwrap();

        let recovered = storage.load_session(session.id).await.unwrap();
        assert_ne!(recovered.id, session.id);
        assert_eq!(recovered.recovered_from, Some(session.id));
        assert_eq!(recovered.messages, vec![prefix]);
        assert!(recovered.had_interruption);
    }

    #[tokio::test]
    async fn persists_cancelled_partial_assistant_output_and_usage() {
        let (_temporary, storage) = test_storage().await;
        let session = storage.create_session().await.unwrap();
        let user = ChatMessage::user("Begin");
        storage.append_message(session.id, &user).await.unwrap();
        let mut assistant = ChatMessage::streaming_assistant("local-model");
        assistant.content = "partial output".to_owned();
        assistant.status = MessageStatus::Cancelled;
        assistant.finish_reason = Some("cancelled".to_owned());
        assistant.usage = Some(Usage {
            eval_count: Some(2),
            ..Usage::default()
        });
        storage
            .append_message(session.id, &assistant)
            .await
            .unwrap();

        let loaded = storage.load_session(session.id).await.unwrap();
        assert_eq!(loaded.messages, vec![user, assistant]);
        assert!(!loaded.had_interruption);
    }

    #[tokio::test]
    async fn state_file_is_private_and_round_trips() {
        let (_temporary, storage) = test_storage().await;
        let state = AppStateFile {
            last_model: Some("qwen".to_owned()),
            recent_session: Some(Uuid::now_v7().to_string()),
            ..AppStateFile::default()
        };
        storage.save_state(&state).await.unwrap();

        let mode = std::fs::metadata(&storage.paths().state)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let loaded = storage.load_state().await.unwrap();
        assert_eq!(loaded.last_model, state.last_model);
        assert_eq!(loaded.recent_session, state.recent_session);
    }

    #[test]
    fn message_records_keep_schema_and_message_fields_at_the_top_level() {
        let message = ChatMessage::user("schema check");
        let record = SessionRecord::Message {
            schema_version: SESSION_SCHEMA_VERSION,
            message: message.clone(),
        };
        let value = serde_json::to_value(record).unwrap();

        assert_eq!(value["type"], "message");
        assert_eq!(value["schema_version"], SESSION_SCHEMA_VERSION);
        assert_eq!(value["id"], message.id.to_string());
        assert_eq!(value["role"], "user");
        assert!(value.get("message").is_none());
    }

    #[tokio::test]
    async fn corrupt_state_is_preserved_and_defaults_are_returned() {
        let (_temporary, storage) = test_storage().await;
        fs::write(&storage.paths().state, b"{not valid json\n")
            .await
            .unwrap();

        let state = storage.load_state().await.unwrap();
        assert!(state.last_model.is_none());
        assert!(state.recent_session.is_none());

        // The original file is moved aside, not silently deleted.
        assert!(!storage.paths().state.exists());
        let corrupt_entries: Vec<_> = std::fs::read_dir(&storage.paths().root)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("state.json.")
                    && entry.file_name().to_string_lossy().ends_with(".corrupt")
            })
            .collect();
        assert_eq!(
            corrupt_entries.len(),
            1,
            "one .corrupt backup should be kept"
        );
    }

    #[tokio::test]
    async fn truncated_state_is_preserved_and_defaults_are_returned() {
        let (_temporary, storage) = test_storage().await;
        fs::write(&storage.paths().state, b"{\"last_model\": \"qwen\"")
            .await
            .unwrap();

        let state = storage.load_state().await.unwrap();
        assert!(state.last_model.is_none());
        assert!(!storage.paths().state.exists());
    }

    #[tokio::test]
    async fn a_broken_chat_entry_does_not_hide_valid_sessions() {
        let (_temporary, storage) = test_storage().await;
        let session = storage.create_session().await.unwrap();
        storage
            .append_message(session.id, &ChatMessage::user("valid session"))
            .await
            .unwrap();

        // A non-JSON chat file that would previously abort the entire listing.
        fs::write(storage.paths().chats.join("broken.jsonl"), b"not-json\n")
            .await
            .unwrap();

        let summaries = storage.list_sessions().await.unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].title, "valid session");
    }

    #[tokio::test]
    async fn an_unreadable_chat_entry_does_not_hide_valid_sessions() {
        use std::os::unix::fs::PermissionsExt;
        let (_temporary, storage) = test_storage().await;
        let session = storage.create_session().await.unwrap();
        storage
            .append_message(session.id, &ChatMessage::user("still listed"))
            .await
            .unwrap();

        let broken = storage
            .paths()
            .chats
            .join("00000000-0000-0000-0000-000000000000.jsonl");
        fs::write(&broken, b"garbage").await.unwrap();
        std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o000)).unwrap();

        let summaries = storage.list_sessions().await.unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].title, "still listed");
    }

    #[tokio::test]
    async fn rejects_an_unknown_schema() {
        let (_temporary, storage) = test_storage().await;
        let id = Uuid::now_v7();
        let path = storage.session_path(id);
        let record = serde_json::json!({
            "type": "session",
            "schema_version": 99,
            "id": id,
            "created_at": Utc::now(),
            "backend": "ollama"
        });
        fs::write(&path, format!("{record}\n")).await.unwrap();

        let error = storage.load_session(id).await.unwrap_err();
        assert!(matches!(
            error,
            StorageError::UnsupportedSchema { found: 99, .. }
        ));
    }
}
