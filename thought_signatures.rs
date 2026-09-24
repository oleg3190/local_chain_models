use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// How long a captured `thought_signature` stays usable. Conversations
/// that idle longer than this simply fall back to the reserve chain.
const TTL: Duration = Duration::from_secs(6 * 60 * 60);

/// Hard cap so a long-running proxy process cannot grow unbounded.
const MAX_ENTRIES: usize = 20_000;

/// Rewrite the append-only log once this many records have been added
/// since the last rewrite, so the file cannot grow forever. Kept well
/// above `MAX_ENTRIES` so a normal conversation never pays for it.
const COMPACT_AFTER_APPENDS: usize = 50_000;

/// One line of the on-disk log. `ts` is Unix seconds (not an `Instant`)
/// because it has to survive a process restart.
#[derive(Serialize, Deserialize)]
struct Record {
    id: String,
    sig: String,
    ts: u64,
}

struct Entry {
    signature: String,
    stored_unix: u64,
}

struct Inner {
    entries: HashMap<String, Entry>,
    /// Append handle for the on-disk log. `None` means "memory only"
    /// (persistence disabled, or the file turned out to be unwritable).
    writer: Option<File>,
    appends_since_compact: usize,
}

/// Cache of Gemini thought signatures keyed by the OpenAI-style
/// `tool_call_id` they were produced for.
///
/// Gemini's OpenAI-compatible endpoint requires every `functionCall`
/// part in the *input* history to carry back the signature the model
/// originally emitted (see the Gemini "thought signatures" docs).
/// Standard OpenAI clients (pi/Qwen Code) don't preserve
/// `extra_content.google.thought_signature`, so the proxy remembers it
/// from the response and re-attaches it on the next turn. Without this,
/// Gemini answers `400 INVALID_ARGUMENT: Function call is missing a
/// thought_signature`.
///
/// The cache is append-only persisted (see `THOUGHT_SIG_CACHE_PATH`) so
/// that restarting the proxy does not throw away signatures for a
/// conversation that is still in flight.
pub struct ThoughtSignatures {
    inner: Mutex<Inner>,
    path: Option<PathBuf>,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn is_expired(stored_unix: u64, now: u64) -> bool {
    now.saturating_sub(stored_unix) >= TTL.as_secs()
}

/// Expands a leading `~/` using `$HOME` (dotenv does not do this).
/// Any other path is used verbatim.
fn expand_tilde(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(raw)
}

impl ThoughtSignatures {
    /// Memory-only cache: nothing is read or written on disk.
    pub fn in_memory() -> Self {
        Self {
            inner: Mutex::new(Inner {
                entries: HashMap::new(),
                writer: None,
                appends_since_compact: 0,
            }),
            path: None,
        }
    }

    /// Builds the cache from a `THOUGHT_SIG_CACHE_PATH`-style setting:
    /// `None`/empty/`off` means memory-only, anything else is treated as
    /// a file path (with `~/` expansion).
    pub fn from_setting(setting: Option<&str>) -> Self {
        match setting.map(str::trim) {
            Some(v) if !v.is_empty() && !v.eq_ignore_ascii_case("off") => {
                Self::load(expand_tilde(v))
            }
            _ => Self::in_memory(),
        }
    }

    /// Loads the cache from `path`, replaying every still-valid record.
    /// A missing file is fine (first run). Any I/O problem is logged and
    /// degrades to a memory-only cache rather than failing startup: a cold
    /// cache only costs us fallbacks, never correctness.
    pub fn load(path: PathBuf) -> Self {
        let mut entries: HashMap<String, Entry> = HashMap::new();
        let mut writer = None;

        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                if let Err(e) = fs::create_dir_all(dir) {
                    warn!(
                        path = %path.display(),
                        error = %e,
                        "cannot create thought-signature cache directory; continuing in memory"
                    );
                }
            }
        }

        if path.exists() {
            match File::open(&path) {
                Ok(f) => {
                    let now = unix_now();
                    let (mut records, mut malformed) = (0usize, 0usize);
                    for line in BufReader::new(f).lines() {
                        let Ok(line) = line else {
                            malformed += 1;
                            continue;
                        };
                        let line = line.trim();
                        if line.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<Record>(line) {
                            Ok(rec) => {
                                records += 1;
                                // Later lines win: the log is append-only,
                                // so the newest signature for an id is last.
                                if !is_expired(rec.ts, now) {
                                    entries.insert(
                                        rec.id,
                                        Entry {
                                            signature: rec.sig,
                                            stored_unix: rec.ts,
                                        },
                                    );
                                }
                            }
                            Err(_) => malformed += 1,
                        }
                    }
                    info!(
                        path = %path.display(),
                        live = entries.len(),
                        records,
                        "loaded Gemini thought-signature cache"
                    );
                    if malformed > 0 {
                        warn!(
                            path = %path.display(),
                            skipped = malformed,
                            "skipped malformed lines in thought-signature cache"
                        );
                    }
                }
                Err(e) => warn!(
                    path = %path.display(),
                    error = %e,
                    "cannot read thought-signature cache; continuing in memory"
                ),
            }
        }

        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => writer = Some(f),
            Err(e) => warn!(
                path = %path.display(),
                error = %e,
                "cannot open thought-signature cache for appending; continuing in memory"
            ),
        }

        Self {
            inner: Mutex::new(Inner {
                entries,
                writer,
                appends_since_compact: 0,
            }),
            path: Some(path),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Remembers `signature` for the given tool-call id, in memory and
    /// (when persistence is on) appended to the on-disk log.
    pub fn put(&self, tool_call_id: &str, signature: &str) {
        if tool_call_id.is_empty() || signature.is_empty() {
            return;
        }

        let now = unix_now();
        let mut inner = self.lock();

        if inner.entries.len() >= MAX_ENTRIES {
            inner.entries.retain(|_, e| !is_expired(e.stored_unix, now));
            if inner.entries.len() >= MAX_ENTRIES {
                // Everything is still fresh: the cap has to win, so drop
                // the whole map and rewrite the log from the empty state.
                inner.entries.clear();
                inner.appends_since_compact = COMPACT_AFTER_APPENDS;
            }
        }

        inner.entries.insert(
            tool_call_id.to_string(),
            Entry {
                signature: signature.to_string(),
                stored_unix: now,
            },
        );

        self.append(&mut inner, tool_call_id, signature, now);

        if inner.appends_since_compact >= COMPACT_AFTER_APPENDS {
            self.compact(&mut inner);
        }
    }

    /// Returns the still-valid signature for `tool_call_id`, if any.
    pub fn get(&self, tool_call_id: &str) -> Option<String> {
        let now = unix_now();
        let inner = self.lock();
        let entry = inner.entries.get(tool_call_id)?;
        if is_expired(entry.stored_unix, now) {
            return None;
        }
        Some(entry.signature.clone())
    }

    /// Number of live (non-expired) entries.
    pub fn len(&self) -> usize {
        let now = unix_now();
        self.lock()
            .entries
            .values()
            .filter(|e| !is_expired(e.stored_unix, now))
            .count()
    }

    /// Appends one record to the log. On failure persistence is disabled
    /// for the rest of the process instead of failing every `put`.
    fn append(&self, inner: &mut Inner, id: &str, sig: &str, ts: u64) {
        let record = Record {
            id: id.to_string(),
            sig: sig.to_string(),
            ts,
        };
        let Ok(mut line) = serde_json::to_string(&record) else {
            return;
        };
        line.push('\n');

        let result = match inner.writer.as_mut() {
            Some(w) => w.write_all(line.as_bytes()),
            None => return,
        };

        match result {
            Ok(()) => inner.appends_since_compact += 1,
            Err(e) => {
                warn!(
                    error = %e,
                    "failed to persist thought signature; continuing in memory"
                );
                inner.writer = None;
            }
        }
    }

    /// Atomically rewrites the log with only the live entries, dropping
    /// expired/overwritten records and any garbage that accumulated.
    fn compact(&self, inner: &mut Inner) {
        inner.appends_since_compact = 0;

        let Some(path) = self.path.clone() else {
            return;
        };

        let now = unix_now();
        inner.entries.retain(|_, e| !is_expired(e.stored_unix, now));

        let tmp = PathBuf::from(format!("{}.tmp", path.display()));
        match write_snapshot(&tmp, &inner.entries) {
            Ok(()) => {
                if let Err(e) = fs::rename(&tmp, &path) {
                    warn!(
                        path = %path.display(),
                        error = %e,
                        "failed to replace thought-signature cache atomically"
                    );
                    let _ = fs::remove_file(&tmp);
                    return;
                }
                match OpenOptions::new().create(true).append(true).open(&path) {
                    Ok(f) => inner.writer = Some(f),
                    Err(e) => {
                        warn!(
                            path = %path.display(),
                            error = %e,
                            "cannot reopen thought-signature cache; continuing in memory"
                        );
                        inner.writer = None;
                    }
                }
                info!(
                    path = %path.display(),
                    live = inner.entries.len(),
                    "compacted thought-signature cache"
                );
            }
            Err(e) => {
                warn!(
                    path = %tmp.display(),
                    error = %e,
                    "failed to write thought-signature snapshot"
                );
                let _ = fs::remove_file(&tmp);
            }
        }
    }
}

fn write_snapshot(path: &Path, entries: &HashMap<String, Entry>) -> std::io::Result<()> {
    let mut file = File::create(path)?;
    for (id, entry) in entries {
        let record = Record {
            id: id.clone(),
            sig: entry.signature.clone(),
            ts: entry.stored_unix,
        };
        let Ok(line) = serde_json::to_string(&record) else {
            continue;
        };
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
    }
    file.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("llm-router-ts-{tag}-{}-{nanos}.jsonl", std::process::id()))
    }

    #[test]
    fn survives_a_reload() {
        let path = temp_path("roundtrip");

        {
            let cache = ThoughtSignatures::load(path.clone());
            cache.put("call_1", "sig-aaa");
            cache.put("call_2", "sig-bbb");
        } // dropped: simulates a proxy restart

        let reloaded = ThoughtSignatures::load(path.clone());
        assert_eq!(reloaded.get("call_1").as_deref(), Some("sig-aaa"));
        assert_eq!(reloaded.get("call_2").as_deref(), Some("sig-bbb"));
        assert_eq!(reloaded.len(), 2);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn newest_signature_wins() {
        let path = temp_path("newest");
        let cache = ThoughtSignatures::load(path.clone());
        cache.put("call_1", "old");
        cache.put("call_1", "new");
        assert_eq!(cache.get("call_1").as_deref(), Some("new"));

        let reloaded = ThoughtSignatures::load(path.clone());
        assert_eq!(reloaded.get("call_1").as_deref(), Some("new"));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn skips_expired_and_malformed_records() {
        let path = temp_path("expired");
        let stale = unix_now().saturating_sub(TTL.as_secs() + 60);
        let contents = format!(
            "{{\"id\":\"fresh\",\"sig\":\"ok\",\"ts\":{}}}\nnot json\n{{\"id\":\"stale\",\"sig\":\"gone\",\"ts\":{stale}}}\n",
            unix_now()
        );
        fs::write(&path, contents).unwrap();

        let cache = ThoughtSignatures::load(path.clone());
        assert_eq!(cache.get("fresh").as_deref(), Some("ok"));
        assert_eq!(cache.get("stale"), None);
        assert_eq!(cache.len(), 1);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn off_setting_keeps_everything_in_memory() {
        let cache = ThoughtSignatures::from_setting(Some("off"));
        cache.put("call_1", "sig");
        assert_eq!(cache.get("call_1").as_deref(), Some("sig"));
        assert!(cache.path.is_none());
    }

    #[test]
    fn compaction_rewrites_only_live_entries() {
        let path = temp_path("compact");
        let cache = ThoughtSignatures::load(path.clone());
        for i in 0..100 {
            cache.put(&format!("call_{i}"), &format!("sig-{i}"));
        }
        let mut inner = cache.lock();
        cache.compact(&mut inner);
        drop(inner);

        let reloaded = ThoughtSignatures::load(path.clone());
        assert_eq!(reloaded.len(), 100);
        assert_eq!(reloaded.get("call_99").as_deref(), Some("sig-99"));

        let _ = fs::remove_file(&path);
    }
}
