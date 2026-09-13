//! Thread-safe C ABI. All JSON operations share the same engine used by the GUI and CLI.
//! Callers must not close an engine handle while another thread is entering filesearch_engine_call.
mod clone_identity;
#[cfg(all(test, target_os = "macos"))]
mod content_read_tests;
mod duplicates;
mod file_events;
mod file_identity;
mod filesystem;
pub mod index_store;
mod metadata_postings;
mod numeric_columns;
#[cfg(test)]
mod numeric_columns_tests;
#[cfg(test)]
mod numeric_profile_tests;
pub mod query;
#[cfg(test)]
mod query_cache_tests;
mod relations;
mod result_order;
mod scan_resume;
pub mod scanner;
mod snapshot_cache;
mod snapshot_leases;
mod status_signal;
use arc_swap::ArcSwap;
use index_store::{IndexStore, IndexedFile, SearchSnapshot};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    ffi::{c_char, c_void, CStr, CString},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, RwLock, Weak,
    },
    time::{Duration, Instant},
};

pub const PROTOCOL_VERSION: u32 = 2;

// Bare generation lookup is a short convenience window. Explicit leases and
// active queries own their own Arc; structural rebuilds must not retain another
// two independent complete indexes just to serve that convenience window.
struct HistoricalSnapshot {
    generation: u64,
    snapshot: Weak<SearchSnapshot>,
    retained: Option<Arc<SearchSnapshot>>,
}
impl HistoricalSnapshot {
    fn new(snapshot: Arc<SearchSnapshot>, retain: bool) -> Self {
        Self {
            generation: snapshot.generation,
            snapshot: Arc::downgrade(&snapshot),
            retained: retain.then_some(snapshot),
        }
    }
}
pub struct SearchEngine {
    index_store: Mutex<IndexStore>,
    preferences: RwLock<Value>,
    offline: AtomicBool,
    snapshot: ArcSwap<SearchSnapshot>,
    previous: Mutex<VecDeque<HistoricalSnapshot>>,
    scanning: AtomicBool,
    active: AtomicBool,
    stop: AtomicBool,
    scan_cancel: AtomicBool,
    generation: AtomicU64,
    published_revision: AtomicU64,
    needs_cache_rebuild: AtomicBool,
    requests: Mutex<HashMap<String, Arc<AtomicBool>>>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
    restart_lock: Mutex<()>,
    refresh_lock: Mutex<()>,
    state: status_signal::StatusSignal,
    index_directory: PathBuf,
    leases: snapshot_leases::SnapshotLeases,
}
impl SearchEngine {
    pub fn open(path: &Path) -> Result<Arc<Self>, String> {
        let index_store = IndexStore::open(path)?;
        // FSEvents reports physical paths (/private/tmp, not /tmp). Keep the
        // exclusion in the same namespace so our own WAL/cache writes cannot
        // continuously invalidate the index when its location uses an alias.
        let index_directory = path
            .parent()
            .unwrap_or(Path::new("."))
            .canonicalize()
            .map_err(|e| format!("Index directory: {e}"))?;
        let index_directory =
            PathBuf::from(scanner::visible_path(&index_directory.to_string_lossy()));
        let generation = index_store
            .get("generation", json!(0))
            .as_u64()
            .unwrap_or(0);
        let revision = index_store.get("revision", json!(0)).as_u64().unwrap_or(0);
        let content_revision = index_store
            .get("content_revision", json!(0))
            .as_u64()
            .unwrap_or(0);
        let (mut snapshot, needs_cache_rebuild) =
            if let Some((snapshot, _)) = index_store.cache_read() {
                (snapshot, false)
            } else {
                // Cache damage or removal must schedule a replacement even when no
                // file metadata changed since the last successful checkpoint.
                if !index_store.cache_is_dirty() {
                    index_store.set("cache_dirty", &json!(true))?;
                }
                let snapshot = SearchSnapshot::new(index_store.entries()?, generation);
                let needs_rebuild = !snapshot.is_empty();
                (snapshot, needs_rebuild)
            };
        index_store.clear_changes(revision)?;
        let roots = index_store.get("roots", json!([]));
        let uncovered = index_store.get("uncovered", json!([]));
        let resume = index_store
            .get("watch_enabled", json!(false))
            .as_bool()
            .unwrap_or(false);
        let offline = index_store
            .get("offline", json!(false))
            .as_bool()
            .unwrap_or(false);
        let mut preferences = json!({});
        for key in ["macros", "bookmarks", "exclusions", "history", "settings"] {
            preferences[key] = index_store.get(
                key,
                if key == "macros" || key == "settings" {
                    json!({})
                } else {
                    json!([])
                },
            )
        }
        snapshot.content_revision = content_revision;
        let engine = Arc::new(Self {
            snapshot: ArcSwap::from_pointee(snapshot),
            previous: Mutex::new(VecDeque::new()),
            index_store: Mutex::new(index_store),
            preferences: RwLock::new(preferences),
            offline: AtomicBool::new(offline),
            scanning: AtomicBool::new(false),
            active: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            scan_cancel: AtomicBool::new(false),
            generation: AtomicU64::new(generation),
            published_revision: AtomicU64::new(revision),
            needs_cache_rebuild: AtomicBool::new(needs_cache_rebuild),
            requests: Mutex::new(HashMap::new()),
            worker: Mutex::new(None),
            restart_lock: Mutex::new(()),
            refresh_lock: Mutex::new(()),
            state: status_signal::StatusSignal::new(
                json!({"roots":roots,"uncovered":uncovered,"state":"idle","errors":[],"event_id":0}),
            ),
            index_directory,
            leases: snapshot_leases::SnapshotLeases::default(),
        });
        if resume && roots.as_array().is_some_and(|r| !r.is_empty()) {
            if let Err(error) = engine.start_worker(&json!({"roots":roots,"watch":true}), true) {
                engine.state.lock().unwrap()["errors"] = json!([error]);
            }
        }
        if needs_cache_rebuild && !(resume && roots.as_array().is_some_and(|r| !r.is_empty())) {
            // Make SQL-restored results available before rebuilding the large
            // disposable cache. Watched indexes checkpoint after history replay.
            let cache_engine = engine.clone();
            std::thread::spawn(move || cache_engine.rebuild_missing_cache());
        }
        Ok(engine)
    }
    fn rebuild_missing_cache(&self) {
        if !self.needs_cache_rebuild.swap(false, Ordering::SeqCst) {
            return;
        }
        if let Err(error) = self.refresh(true) {
            self.state.lock().unwrap()["cache_error"] = json!(error);
        }
        if self.index_store.lock().unwrap().cache_is_dirty() {
            self.needs_cache_rebuild.store(true, Ordering::SeqCst);
        }
    }
    fn refresh(&self, cache: bool) -> Result<(), String> {
        let _publication = self.refresh_lock.lock().unwrap();
        let previous_snapshot = self.snapshot.load_full();
        let (changes, mut revision, mut content_revision) = {
            let index_store = self.index_store.lock().unwrap();
            let revision = index_store.get("revision", json!(0)).as_u64().unwrap_or(0);
            if revision == self.published_revision.load(Ordering::Relaxed) {
                if cache && index_store.cache_is_dirty() {
                    self.persist_snapshot_cache(&index_store, &previous_snapshot, revision);
                }
                return Ok(());
            }
            (
                index_store.changes_since(
                    self.published_revision.load(Ordering::Relaxed),
                    SearchSnapshot::incremental_limit(previous_snapshot.len()),
                )?,
                revision,
                index_store
                    .get("content_revision", json!(0))
                    .as_u64()
                    .unwrap_or(0),
            )
        };
        let generation = self.generation.load(Ordering::Relaxed) + 1;
        let change_count = changes.as_ref().map(Vec::len);
        let incremental_limit = SearchSnapshot::incremental_limit(previous_snapshot.len());
        let remap_reason = changes
            .as_ref()
            .and_then(|changes| SearchSnapshot::remap_reason(changes, &previous_snapshot));
        let (mut mode, reason) = match change_count {
            None => ("full", Some("delta_unavailable")),
            Some(count) if count > incremental_limit => {
                ("full", Some("delta_exceeds_incremental_limit"))
            }
            _ if remap_reason.is_some() => ("remapped", remap_reason),
            _ => ("incremental", None),
        };
        self.state.lock().unwrap()["last_refresh"] = json!({
            "mode": mode,
            "reason": reason,
            "previous_generation": previous_snapshot.generation,
            "generation": generation,
            "published_revision": self.published_revision.load(Ordering::Relaxed),
            "target_revision": revision,
            "changes": change_count,
            "previous_live_entries": previous_snapshot.len(),
            "previous_slots": previous_snapshot.entries.len(),
            "incremental_limit": incremental_limit,
        });
        if mode != "incremental" {
            self.release_previous_snapshots();
        }
        let update = changes.ok_or("delta_unavailable").and_then(|changes| {
            SearchSnapshot::from_changes_with_reason(changes, generation, &previous_snapshot)
        });
        let mut snapshot = match update {
            Ok(snapshot) => snapshot,
            Err(reason) => {
                // A failed incremental attempt still uses the complete SQL
                // recovery path. Do not retain unrelated old generations during
                // construction of another independent set of derived indexes.
                if mode == "incremental" {
                    self.release_previous_snapshots();
                }
                mode = "full";
                {
                    let mut state = self.state.lock().unwrap();
                    state["last_refresh"]["mode"] = json!(mode);
                    state["last_refresh"]["reason"] = json!(reason);
                }
                let index_store = self.index_store.lock().unwrap();
                revision = index_store.get("revision", json!(0)).as_u64().unwrap_or(0);
                content_revision = index_store
                    .get("content_revision", json!(0))
                    .as_u64()
                    .unwrap_or(0);
                SearchSnapshot::new(index_store.entries()?, generation)
            }
        };
        snapshot.content_revision = content_revision;
        let snapshot = Arc::new(snapshot);
        {
            let index_store = self.index_store.lock().unwrap();
            // Generation describes an actual immutable search snapshot. Seen
            // epochs and journal cursors never increment it on their own.
            index_store.set("generation", &json!(generation))?;
            index_store.clear_changes(revision)?;
        }
        // Publish useful results before serializing the disposable disk cache.
        let old = self.snapshot.swap(snapshot.clone());
        self.generation.store(generation, Ordering::Relaxed);
        self.published_revision.store(revision, Ordering::Relaxed);
        self.state.notify();
        let retired = {
            let mut previous = self.previous.lock().unwrap();
            previous.push_back(HistoricalSnapshot::new(old, mode == "incremental"));
            if previous.len() > 2 {
                previous.pop_front()
            } else {
                None
            }
        };
        // Destruction can release large posting maps; do it outside the history
        // lock, before an optional cache checkpoint. Leased/inflight Arcs remain.
        drop(retired);
        drop(previous_snapshot);
        if cache {
            let index_store = self.index_store.lock().unwrap();
            self.persist_snapshot_cache(&index_store, &snapshot, revision);
        }
        Ok(())
    }
    fn release_previous_snapshots(&self) {
        self.leases.discard_expired();
        let retained: Vec<_> = self
            .previous
            .lock()
            .unwrap()
            .iter_mut()
            .filter_map(|entry| entry.retained.take())
            .collect();
        drop(retained);
    }
    fn persist_snapshot_cache(
        &self,
        index_store: &IndexStore,
        snapshot: &SearchSnapshot,
        revision: u64,
    ) {
        match index_store.cache_write(snapshot, revision) {
            Ok(()) if !index_store.cache_is_dirty() => {
                self.state
                    .lock()
                    .unwrap()
                    .as_object_mut()
                    .unwrap()
                    .remove("cache_error");
            }
            Ok(()) => (), // A newer SQLite revision deferred this checkpoint.
            Err(error) => {
                // SQLite and the in-memory snapshot remain authoritative when
                // this disposable acceleration file cannot be replaced.
                let dirty_error = if index_store.cache_is_dirty() {
                    None
                } else {
                    index_store.set("cache_dirty", &json!(true)).err()
                };
                self.state.lock().unwrap()["cache_error"] = json!(match dirty_error {
                    Some(dirty_error) =>
                        format!("{error}; unable to schedule cache repair: {dirty_error}"),
                    None => error,
                });
            }
        }
    }
    pub fn call(self: &Arc<Self>, request: Value) -> Value {
        match self.dispatch(&request) {
            Ok(mut response) => {
                response["success"] = json!(true);
                response["protocol_version"] = json!(PROTOCOL_VERSION);
                response
            }
            Err(error) => {
                json!({"success":false,"error":error,"protocol_version":PROTOCOL_VERSION})
            }
        }
    }
    fn status_reply(&self) -> Value {
        let (mut status, revision) = self.state.snapshot();
        let snapshot = self.snapshot.load();
        status["count"] = json!(snapshot.len());
        status["generation"] = json!(snapshot.generation);
        status["scanning"] = json!(self.scanning.load(Ordering::Relaxed));
        status["updating"] = json!(status["state"] == "updating");
        status["status_revision"] = json!(revision);
        status
    }
    fn wait_status(&self, request: &Value) -> Result<Value, String> {
        if let Some(token) = request["snapshot_lease"].as_str() {
            self.leases.get(token)?;
        }
        let after = request["after"].as_u64().ok_or("Missing status revision")?;
        let id = request["request_id"]
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= 128)
            .ok_or("Status observation requires a bounded request_id")?
            .to_owned();
        let cancelled = Arc::new(AtomicBool::new(false));
        if let Some(previous) = self
            .requests
            .lock()
            .unwrap()
            .insert(id.clone(), cancelled.clone())
        {
            previous.store(true, Ordering::Release);
            self.state.wake_cancelled();
        }
        let _request = RequestGuard {
            requests: &self.requests,
            id: Some(id),
            token: cancelled.clone(),
        };
        self.state.wait(
            after,
            Duration::from_millis(request["timeout_ms"].as_u64().unwrap_or(30_000)),
            &cancelled,
        )?;
        Ok(self.status_reply())
    }
    fn dispatch(self: &Arc<Self>, request: &Value) -> Result<Value, String> {
        match request
            .get("op")
            .and_then(Value::as_str)
            .ok_or("Missing op")?
        {
            "status" => Ok(self.status_reply()),
            "wait_status" => self.wait_status(request),
            "query" => self.query(request),
            "directory_info" => self.directory_info(request),
            "retain_snapshot" => {
                let snapshot = self.pin(request["generation"].as_u64())?;
                let generation = snapshot.generation;
                let mut preferences = self.preferences.read().unwrap().clone();
                preferences["_snapshot_coverage"] = self.relation_coverage();
                preferences["_snapshot_query_time_millis"] =
                    json!(chrono::Local::now().timestamp_millis());
                let token = self.leases.retain(snapshot, preferences)?;
                Ok(json!({"snapshot_lease":token,"generation":generation}))
            }
            "release_snapshot" => {
                let token = request["snapshot_lease"]
                    .as_str()
                    .ok_or("Missing snapshot_lease")?;
                Ok(json!({"released":self.leases.release(token)}))
            }
            "renew_snapshot" => {
                let token = request["snapshot_lease"]
                    .as_str()
                    .ok_or("Missing snapshot_lease")?;
                let context = self.leases.get(token)?;
                Ok(json!({"generation":context.snapshot.generation}))
            }
            "query_plan" => {
                let preferences = if let Some(token) = request["snapshot_lease"].as_str() {
                    self.leases.get(token)?.preferences
                } else {
                    self.preferences.read().unwrap().clone()
                };
                let macros: HashMap<String, String> =
                    serde_json::from_value(preferences["macros"].clone())
                        .map_err(|_| "Invalid macros")?;
                let parsed_query = query::parse(request["text"].as_str().unwrap_or(""), &macros)?;
                let mut relational = parsed_query.requires_relations();
                let mut content = parsed_query.requires_content();
                let mut properties = parsed_query.requires_properties();
                if let Some(exclusions) = preferences["exclusions"].as_array() {
                    for exclusion in exclusions {
                        let parsed_query =
                            query::parse(exclusion.as_str().ok_or("Invalid exclusion")?, &macros)?;
                        relational |= parsed_query.requires_relations();
                        content |= parsed_query.requires_content();
                        properties |= parsed_query.requires_properties();
                    }
                }
                Ok(
                    json!({"requires_content":content,"requires_properties":properties,"requires_extraction":(content||properties)&&!relational,"indexed_relations_only":relational}),
                )
            }
            "volumes" | "volumes_changed" => Ok(
                json!({"volumes":scanner::volumes()?,"watcher":"Disk Arbitration changes are reconciled by the active filesystem watcher"}),
            ),
            "content_candidates" => {
                let mut candidate_request = request.clone();
                candidate_request["metadata_only"] = json!(true);
                self.query(&candidate_request)
            }
            "publish_content" => {
                self.refresh(false)?;
                Ok(json!({}))
            }
            "scan" | "watch" => self.start_scan(request),
            "quiesce" => {
                let _restart = self.restart_lock.lock().unwrap();
                self.stop.store(true, Ordering::Release);
                self.scan_cancel.store(true, Ordering::Release);
                if let Some(worker) = self.worker.lock().unwrap().take() {
                    worker
                        .join()
                        .map_err(|_| "Index worker failed during update preparation")?;
                }
                Ok(json!({"quiescent":true}))
            }
            "stop" => {
                self.stop.store(true, Ordering::Relaxed);
                self.scan_cancel.store(true, Ordering::Relaxed);
                Ok(json!({"stopping":true}))
            }
            "cancel" => {
                let id = request["request_id"].as_str().ok_or("Missing request_id")?;
                if id == "scan" {
                    self.scan_cancel.store(true, Ordering::Relaxed);
                }
                if let Some(token) = self.requests.lock().unwrap().get(id) {
                    token.store(true, Ordering::Relaxed);
                }
                self.state.wake_cancelled();
                Ok(json!({"cancelled":id}))
            }
            "put_content" => {
                let path = request["path"].as_str().ok_or("Missing path")?;
                let text = request["text"].as_str().unwrap_or("");
                if text.len() > 32 * 1024 * 1024 {
                    return Err("Content extraction exceeds 32 MiB per file".into());
                }
                if let Some(expected) = request.get("expected") {
                    validate_content_identity(path, expected)?;
                }
                self.index_store.lock().unwrap().put_content(
                    path,
                    text,
                    &request.get("properties").cloned().unwrap_or(json!({})),
                )?;
                if !request["defer_publish"].as_bool().unwrap_or(false) {
                    self.refresh(false)?;
                }
                Ok(json!({"path":path}))
            }
            "preferences" => self.preferences(request),
            "duplicates" => self.duplicates(request),
            "import_file_list" => self.import_file_list(request),
            "export" => {
                let mut request = request.clone();
                request["limit"] = json!(10000);
                self.query(&request)
            }
            op => Err(format!("Unknown operation '{op}'")),
        }
    }
    fn preferences(&self, request: &Value) -> Result<Value, String> {
        let index_store = self.index_store.lock().unwrap();
        let allowed = ["macros", "bookmarks", "exclusions", "history", "settings"];
        if let Some(values) = request
            .get("set")
            .or_else(|| {
                if request["action"] == "set" {
                    request.get("values")
                } else {
                    None
                }
            })
            .and_then(Value::as_object)
        {
            for (key, value) in values {
                if !allowed.contains(&key.as_str()) {
                    return Err(format!("Unknown preference '{key}'"));
                }
                if key == "macros" && !value.is_object() {
                    return Err("macros must be an object of strings".into());
                }
                index_store.set(key, value)?;
            }
        }
        let mut response = json!({});
        for key in allowed {
            response[key] = index_store.get(
                key,
                if key == "macros" || key == "settings" {
                    json!({})
                } else {
                    json!([])
                },
            )
        }
        *self.preferences.write().unwrap() = response.clone();
        Ok(response)
    }
    fn pin(&self, generation: Option<u64>) -> Result<Arc<SearchSnapshot>, String> {
        let current = self.snapshot.load_full();
        if generation.is_none_or(|g| g == current.generation) {
            return Ok(current);
        }
        self.previous
            .lock()
            .unwrap()
            .iter()
            .find(|snapshot| Some(snapshot.generation) == generation)
            .and_then(|snapshot| snapshot.snapshot.upgrade())
            .ok_or("Requested generation expired; restart query at offset 0".into())
    }
    fn relation_coverage(&self) -> Value {
        let state = self.state.lock().unwrap();
        json!({"roots":state["roots"], "uncovered":state["uncovered"],
            "complete":!self.offline.load(Ordering::Relaxed) && !self.scanning.load(Ordering::Relaxed)
                && state["initial_scan_complete"] == true})
    }
    fn directory_info(&self, request: &Value) -> Result<Value, String> {
        let paths: Vec<String> = serde_json::from_value(request["paths"].clone())
            .map_err(|_| "paths must be an array")?;
        if paths.len() > 200 {
            return Err("Directory information is limited to 200 paths per request".into());
        }
        let lease = request["snapshot_lease"]
            .as_str()
            .map(|token| self.leases.get(token))
            .transpose()?;
        let snapshot = if let Some(context) = &lease {
            if request["generation"]
                .as_u64()
                .is_some_and(|value| value != context.snapshot.generation)
            {
                return Err("Snapshot lease generation mismatch".into());
            }
            context.snapshot.clone()
        } else {
            self.pin(request["generation"].as_u64())?
        };
        let coverage = lease
            .as_ref()
            .map(|context| context.preferences["_snapshot_coverage"].clone())
            .unwrap_or_else(|| self.relation_coverage());
        let cancelled = Arc::new(AtomicBool::new(false));
        let id = request["request_id"].as_str().map(str::to_owned);
        if let Some(id) = &id {
            if let Some(previous) = self
                .requests
                .lock()
                .unwrap()
                .insert(id.clone(), cancelled.clone())
            {
                previous.store(true, Ordering::Relaxed);
            }
        }
        let _request = RequestGuard {
            requests: &self.requests,
            id,
            token: cancelled.clone(),
        };
        snapshot
            .directory_hierarchy(&cancelled)?
            .info(&snapshot, &coverage, &paths, &cancelled)
    }
    fn query(&self, request: &Value) -> Result<Value, String> {
        if request["retain_snapshot"].as_bool() != Some(true)
            || request["snapshot_lease"].is_string()
        {
            return self.query_inner(request);
        }
        // Pin the first page's preferences and relative-date clock together with
        // its metadata. Retaining just its generation later would let changed
        // macros or midnight reorder an otherwise same-sized cross-page result.
        let snapshot = self.pin(request["generation"].as_u64())?;
        let mut preferences = self.preferences.read().unwrap().clone();
        preferences["_snapshot_query_time_millis"] = json!(chrono::Local::now().timestamp_millis());
        preferences["_snapshot_coverage"] = self.relation_coverage();
        let token = self.leases.retain(snapshot, preferences)?;
        let mut leased = request.clone();
        leased["snapshot_lease"] = json!(token);
        match self.query_inner(&leased) {
            Ok(mut response) => {
                response["snapshot_lease"] = json!(token);
                Ok(response)
            }
            Err(error) => {
                self.leases.release(&token);
                Err(error)
            }
        }
    }
    fn query_inner(&self, request: &Value) -> Result<Value, String> {
        let started = Instant::now();
        let text = request["text"].as_str().unwrap_or("");
        if text.len() > 16384 {
            return Err("Query is limited to 16 KiB".into());
        }
        let lease = request["snapshot_lease"]
            .as_str()
            .map(|token| self.leases.get(token))
            .transpose()?;
        let snapshot = if let Some(context) = &lease {
            if request["generation"]
                .as_u64()
                .is_some_and(|generation| generation != context.snapshot.generation)
            {
                return Err(
                    "SearchSnapshot lease generation does not match the requested generation"
                        .into(),
                );
            }
            context.snapshot.clone()
        } else {
            self.pin(request["generation"].as_u64())?
        };
        let preferences = lease
            .as_ref()
            .map(|context| context.preferences.clone())
            .unwrap_or_else(|| self.preferences.read().unwrap().clone());
        let (macros, exclusions) = {
            (
                serde_json::from_value::<HashMap<String, String>>(preferences["macros"].clone())
                    .map_err(|_| "Invalid macro preference values")?,
                serde_json::from_value::<Vec<String>>(preferences["exclusions"].clone())
                    .map_err(|_| "exclusions must be an array of query strings")?,
            )
        };
        let query_time = preferences["_snapshot_query_time_millis"]
            .as_i64()
            .and_then(chrono::DateTime::from_timestamp_millis)
            .map(|time| time.with_timezone(&chrono::Local))
            .unwrap_or_else(chrono::Local::now);
        let mut parsed_query = query::parse_at(text, &macros, query_time)?;
        let mut excludes: Vec<_> = exclusions
            .iter()
            .map(|s| query::parse_at(s, &macros, query_time))
            .collect::<Result<_, _>>()?;
        let relational = parsed_query.requires_relations()
            || excludes.iter().any(query::Query::requires_relations);
        let coverage = if relational {
            lease
                .as_ref()
                .map(|context| context.preferences["_snapshot_coverage"].clone())
                .unwrap_or_else(|| self.relation_coverage())
        } else {
            Value::Null
        };
        let offline = self.offline.load(Ordering::Relaxed);
        let mut offset = request["offset"].as_u64().unwrap_or(0) as usize;
        let limit = request["limit"].as_u64().unwrap_or(200).min(10000) as usize;
        let result_order = result_order::ResultOrder::parse(&request["sort"])?;
        let request_id = request["request_id"].as_str().map(str::to_string);
        let cancelled = Arc::new(AtomicBool::new(false));
        if let Some(id) = &request_id {
            let mut requests = self.requests.lock().unwrap();
            if let Some(old) = requests.insert(id.clone(), cancelled.clone()) {
                old.store(true, Ordering::Relaxed)
            }
        }
        let guard = RequestGuard {
            requests: &self.requests,
            id: request_id,
            token: cancelled.clone(),
        };
        let mut warnings = HashSet::new();
        let metadata_only = request["metadata_only"].as_bool().unwrap_or(false);
        if relational {
            warnings.insert("Directory relationships and sizes use indexed scope. Missing coverage, content or properties remain unknown; sizes are logical, not reclaimable bytes".to_string());
            if metadata_only {
                // Extraction needs children rather than only their matching
                // parents. Returning all entries is conservative, never a false
                // negative. Native relation searches do not extract implicitly.
                parsed_query = query::Query::All;
                excludes.clear();
            }
        }

        let needs_content = !metadata_only
            && (parsed_query.requires_content()
                || excludes.iter().any(query::Query::requires_content));
        if needs_content {
            self.validate_content_revision(snapshot.content_revision)?;
        }
        let all_files =
            matches!(parsed_query, query::Query::All) && (metadata_only || excludes.is_empty());
        let query_key = (!needs_content).then(|| {
            let clock = (parsed_query.may_depend_on_clock()
                || excludes.iter().any(query::Query::may_depend_on_clock))
            .then_some((
                query_time.timestamp(),
                query_time.offset().local_minus_utc(),
            ));
            json!([
                text,
                preferences["macros"],
                preferences["exclusions"],
                metadata_only,
                clock,
                coverage
            ])
            .to_string()
        });
        let cache_key = query_key.clone().filter(|_| !all_files);
        let page_key = query_key.map(|key| {
            json!([
                key,
                request["sort"],
                offset,
                limit,
                request["anchor_path"],
                request["anchor_id"],
                request["anchor_delta"]
            ])
            .to_string()
        });
        if let Some(page) = page_key
            .as_deref()
            .and_then(|key| snapshot.cached_page(key))
        {
            if cancelled.load(Ordering::Relaxed) {
                return Err("Query cancelled".into());
            }
            let rows: Vec<&IndexedFile> = page
                .slots
                .iter()
                .map(|slot| snapshot.entries[*slot as usize].as_ref())
                .collect();
            return Ok(
                json!({"rows":rows,"offline":offline,"total":page.total,"generation":snapshot.generation,"elapsed_ms":started.elapsed().as_secs_f64()*1000.,"warnings":warnings.iter().cloned().collect::<Vec<_>>(),"offset":page.offset,"limit":limit,"anchor_index":page.anchor_index,"anchor_found":page.anchor_index.is_some()}),
            );
        }
        let mut matching = cache_key
            .as_deref()
            .and_then(|key| snapshot.cached_matches(key));
        if !all_files && matching.is_none() && relational && !metadata_only {
            let tree = snapshot.directory_hierarchy(&cancelled)?;
            let mut content =
                |file: &IndexedFile| self.index_store.lock().unwrap().content_for(&file.path);
            relations::resolve(
                &mut parsed_query,
                &snapshot,
                tree,
                &coverage,
                &cancelled,
                &mut content,
            )?;
            for exclusion in &mut excludes {
                relations::resolve(
                    exclusion,
                    &snapshot,
                    tree,
                    &coverage,
                    &cancelled,
                    &mut content,
                )?;
            }
        }
        if !all_files && matching.is_none() && !needs_content {
            if let Some(mut matched) = snapshot.exact_matches(&parsed_query, &cancelled)? {
                let mut complete = true;
                if !metadata_only {
                    for exclusion in &excludes {
                        if let Some(excluded) = snapshot.exact_matches(exclusion, &cancelled)? {
                            matched -= excluded;
                        } else {
                            complete = false;
                            break;
                        }
                    }
                }
                if complete {
                    matching = Some(Arc::new(matched));
                }
            }
            if let (Some(key), Some(matched)) = (&cache_key, &matching) {
                snapshot.cache_matches(key.clone(), matched.clone());
            }
        }
        if !all_files && matching.is_none() {
            let row_needs_content = !metadata_only
                && (parsed_query.requires_content()
                    || excludes.iter().any(query::Query::requires_content));
            let mut matched = roaring::RoaringBitmap::new();
            let candidates = snapshot.candidates_with_cancellation(&parsed_query, &cancelled)?;
            let iterator: Box<dyn Iterator<Item = u32> + '_> =
                if let Some(candidate_set) = candidates {
                    Box::new(candidate_set.into_iter())
                } else {
                    Box::new(snapshot.live.iter())
                };
            for slot in iterator {
                if cancelled.load(Ordering::Relaxed) {
                    return Err("Query cancelled".into());
                }
                // Old postings can only be used with the live mask of this same snapshot.
                if !snapshot.live.contains(slot) {
                    continue;
                }
                let file = &snapshot.entries[slot as usize];
                if row_needs_content && !parsed_query.may_match_without_content(file)? {
                    continue;
                }
                let stored = if row_needs_content {
                    self.index_store.lock().unwrap().content_for(&file.path)?
                } else {
                    None
                };
                let extracted;
                let body = if let Some(body) = stored.as_deref() {
                    Some(body)
                } else if row_needs_content && !offline {
                    extracted = read_text(file, &mut warnings);
                    extracted.as_deref()
                } else {
                    None
                };
                let mut excluded = false;
                for exclusion in &excludes {
                    if !metadata_only && exclusion.matches_available(file, body)? {
                        excluded = true;
                        break;
                    }
                }
                if (if metadata_only {
                    parsed_query.may_match_before_extraction(file)?
                } else {
                    parsed_query.matches_available(file, body)?
                }) && !excluded
                {
                    matched.insert(slot);
                }
            }
            let matched = Arc::new(matched);
            if let Some(key) = cache_key {
                snapshot.cache_matches(key, matched.clone());
            }
            matching = Some(matched);
        }
        let total = matching
            .as_ref()
            .map_or_else(|| snapshot.len(), |set| set.len() as usize);
        let anchor_requested = request["anchor_path"].as_str();
        let anchor_delta = request["anchor_delta"].as_i64().unwrap_or(0);
        let mut anchor_index = None;
        let mut matches = Vec::<u32>::new();
        if total != 0 && (limit != 0 || anchor_requested.is_some()) {
            let ordered = if limit >= 10000
                || snapshot.len() <= 10000
                || (result_order.primary_path_direction().is_some()
                    && (all_files || total >= 10000))
            {
                Some(snapshot.result_order(&result_order, &cancelled)?)
            } else {
                snapshot
                    .cached_order(&result_order)
                    .filter(|_| all_files || total >= 10000)
            };
            if let Some(order) = ordered {
                if let Some(anchor) = anchor_requested {
                    // An unchanged viewport usually retains its rank. Validate this
                    // exact path first; a rename/insertion falls back to the same
                    // ordered matching stream, without another full path scan.
                    let expected = (offset as i128 + anchor_delta as i128).max(0) as usize;
                    if all_files
                        && order
                            .get(expected)
                            .is_some_and(|slot| snapshot.entries[*slot as usize].path == anchor)
                    {
                        anchor_index = Some(expected);
                    } else if all_files {
                        // Stable row ids locate the object in logarithmic time;
                        // the same exact comparator locates its ordered rank.
                        // An id is only a hint and never overrides the path.
                        let indexed_anchor = request["anchor_id"]
                            .as_i64()
                            .and_then(|id| snapshot.slot_for_id(id))
                            .filter(|slot| {
                                snapshot.live.contains(*slot as u32)
                                    && snapshot.entries[*slot].path == anchor
                            });
                        if let Some(slot) = indexed_anchor {
                            anchor_index = order
                                .binary_search_by(|candidate| {
                                    result_order.compare(
                                        &snapshot.entries[*candidate as usize],
                                        &snapshot.entries[slot],
                                    )
                                })
                                .ok();
                        } else {
                            for (rank, &slot) in order.iter().enumerate() {
                                if rank % 1024 == 0 && cancelled.load(Ordering::Relaxed) {
                                    return Err("Query cancelled".into());
                                }
                                if snapshot.entries[slot as usize].path == anchor {
                                    anchor_index = Some(rank);
                                    break;
                                }
                            }
                        }
                    } else {
                        let mut rank = 0;
                        for (position, &slot) in order.iter().enumerate() {
                            if position % 1024 == 0 && cancelled.load(Ordering::Relaxed) {
                                return Err("Query cancelled".into());
                            }
                            if matching.as_ref().is_none_or(|set| set.contains(slot)) {
                                if snapshot.entries[slot as usize].path == anchor {
                                    anchor_index = Some(rank);
                                    break;
                                }
                                rank += 1;
                            }
                        }
                    }
                }
                offset = anchored_offset(
                    offset,
                    limit,
                    total,
                    anchor_requested.is_some(),
                    anchor_index,
                    anchor_delta,
                );
                if all_files {
                    let start = offset.min(order.len());
                    matches.extend_from_slice(
                        &order[start..start.saturating_add(limit).min(order.len())],
                    );
                } else {
                    let mut rank = 0;
                    for (position, &slot) in order.iter().enumerate() {
                        if position % 1024 == 0 && cancelled.load(Ordering::Relaxed) {
                            return Err("Query cancelled".into());
                        }
                        if matching.as_ref().unwrap().contains(slot) {
                            if rank >= offset && matches.len() < limit {
                                matches.push(slot);
                            }
                            rank += 1;
                            if matches.len() == limit {
                                break;
                            }
                        }
                    }
                }
            } else {
                matches = matching.as_ref().map_or_else(
                    || snapshot.live.iter().collect(),
                    |set| set.iter().collect(),
                );
                let compare = |a, b| {
                    result_order
                        .compare(&snapshot.entries[a as usize], &snapshot.entries[b as usize])
                };
                if let Some(anchor) = anchor_requested {
                    let indexed_anchor = request["anchor_id"]
                        .as_i64()
                        .and_then(|id| snapshot.slot_for_id(id))
                        .filter(|slot| {
                            snapshot.live.contains(*slot as u32)
                                && snapshot.entries[*slot].path == anchor
                        })
                        .map(|slot| slot as u32)
                        .or_else(|| {
                            matches
                                .iter()
                                .copied()
                                .find(|slot| snapshot.entries[*slot as usize].path == anchor)
                        });
                    if let Some(anchor) = indexed_anchor
                        .filter(|slot| matching.as_ref().is_none_or(|set| set.contains(*slot)))
                    {
                        let mut rank = 0;
                        for (position, &slot) in matches.iter().enumerate() {
                            if position % 1024 == 0 && cancelled.load(Ordering::Relaxed) {
                                return Err("Query cancelled".into());
                            }
                            if compare(slot, anchor).is_lt() {
                                rank += 1;
                            }
                        }
                        anchor_index = Some(rank);
                    }
                }
                offset = anchored_offset(
                    offset,
                    limit,
                    total,
                    anchor_requested.is_some(),
                    anchor_index,
                    anchor_delta,
                );
                result_order::select_prefix(
                    &mut matches,
                    offset.saturating_add(limit),
                    compare,
                    &cancelled,
                )?;
                let end = offset.saturating_add(limit).min(matches.len());
                matches = matches.get(offset..end).unwrap_or(&[]).to_vec();
            }
        } else {
            offset = anchored_offset(
                offset,
                limit,
                total,
                anchor_requested.is_some(),
                None,
                anchor_delta,
            );
        }
        if cancelled.load(Ordering::Relaxed) {
            return Err("Query cancelled".into());
        }
        if needs_content {
            self.validate_content_revision(snapshot.content_revision)?;
        }
        if let Some(key) = page_key {
            snapshot.cache_page(
                key,
                Arc::new(index_store::ResultPage {
                    slots: matches.clone(),
                    total,
                    offset,
                    anchor_index,
                }),
            );
        }
        let rows: Vec<&IndexedFile> = matches
            .iter()
            .map(|i| snapshot.entries[*i as usize].as_ref())
            .collect();
        drop(guard);
        Ok(
            json!({"rows":rows,"offline":offline,"total":total,"generation":snapshot.generation,"elapsed_ms":started.elapsed().as_secs_f64()*1000.,"warnings":warnings.into_iter().collect::<Vec<_>>(),"offset":offset,"limit":limit,"anchor_index":anchor_index,"anchor_found":anchor_index.is_some()}),
        )
    }
    fn validate_content_revision(&self, expected: u64) -> Result<(), String> {
        if self
            .index_store
            .lock()
            .unwrap()
            .get("content_revision", json!(0))
            .as_u64()
            .unwrap_or(0)
            != expected
        {
            return Err(
                "Content revision changed; restart the content query with a new snapshot".into(),
            );
        }
        Ok(())
    }
    fn start_scan(self: &Arc<Self>, request: &Value) -> Result<Value, String> {
        self.start_worker(request, false)
    }
    fn start_worker(self: &Arc<Self>, request: &Value, resume: bool) -> Result<Value, String> {
        let roots: Vec<String> = serde_json::from_value(request["roots"].clone())
            .map_err(|_| "roots must be an array of absolute paths")?;
        if roots.is_empty() {
            return Err("At least one root is required".into());
        }
        let roots = normalize_roots(roots)?;
        let _restart = self.restart_lock.lock().unwrap();
        if self.active.load(Ordering::SeqCst) {
            self.stop.store(true, Ordering::Relaxed);
            self.scan_cancel.store(true, Ordering::Relaxed);
        }
        if let Some(worker) = self.worker.lock().unwrap().take() {
            worker.join().map_err(|_| "Index worker failed")?;
        }
        if self.offline.load(Ordering::Relaxed) {
            return Err("Offline file-list indexes cannot scan local files".into());
        }
        let watch = request["watch"].as_bool().unwrap_or(true);
        let wait = request["wait"].as_bool().unwrap_or(false);
        let same_roots = self.index_store.lock().unwrap().get("roots", json!([])) == json!(roots);
        if !same_roots {
            self.index_store.lock().unwrap().retain_roots(&roots)?;
            self.index_store
                .lock()
                .unwrap()
                .set("roots", &json!(roots))?;
        }
        self.index_store
            .lock()
            .unwrap()
            .set("watch_enabled", &json!(watch))?;
        self.active.store(true, Ordering::SeqCst);
        self.scanning.store(true, Ordering::Relaxed);
        self.stop.store(false, Ordering::Relaxed);
        self.scan_cancel.store(false, Ordering::Relaxed);
        *self.state.lock().unwrap() = json!({"roots":roots,"uncovered":[],"errors":[],"state":"scanning","event_id":0,"initial_scan_complete":false,"scan_processed_entries":0,"reconciled_entries":0,"enumerated_entries":0,"verified_link_entries":0,"reused_link_objects":0,"resumed_recheck_roots":[]});
        let engine = self.clone();
        let roots_result = roots.clone();
        let worker = move || engine.scan_worker(roots, watch, resume && same_roots);
        if wait && !watch {
            worker();
        } else {
            *self.worker.lock().unwrap() = Some(std::thread::spawn(worker));
        }
        Ok(json!({"started":true,"roots":roots_result,"watch":watch}))
    }
    fn reconcile(
        &self,
        roots: &[String],
        configured: &[String],
        event_id: u64,
        recursive: bool,
        initial: bool,
    ) -> Result<bool, String> {
        if self.stop.load(Ordering::Relaxed) {
            return Ok(false);
        }
        self.state.lock().unwrap()["state"] = json!(if initial { "scanning" } else { "updating" });
        let mut observed = roaring::RoaringTreemap::new();
        let mut error = None;
        let mut last_publish = Instant::now();
        let mut publish_delay = Duration::from_secs(2);
        let mut changed_count = 0usize;
        let mut processed_count = 0usize;
        let mut linked_uncovered = Vec::new();
        let mut linked_errors = Vec::new();
        let mut batch_cancelled = false;
        let mut verified_objects = index_store::VerifiedFileObjects::default();
        let mut publish_threshold =
            progressive_publish_threshold(self.snapshot.load().entries.len());
        let on_batch = |mut batch: Vec<scanner::ScannedFile>| {
            batch.retain(|e| !Path::new(&e.path).starts_with(&self.index_directory));
            if error.is_some() || batch_cancelled {
                return;
            }
            let mut linked_count = 0usize;
            let mut links_cancelled = false;
            let mut check_links =
                |paths: Vec<String>| -> Result<index_store::LinkVerification, String> {
                    let mut verified = index_store::LinkVerification::default();
                    for path in paths {
                        if self.scan_cancel.load(Ordering::Relaxed) {
                            links_cancelled = true;
                            return Err("Hard-link reconciliation cancelled".into());
                        }
                        match scanner::stat_entry(&path) {
                            Ok(entry) => verified.entries.push(entry),
                            Err(error) => {
                                let removed =
                                    std::fs::symlink_metadata(&path).is_err_and(|error| {
                                        error.kind() == std::io::ErrorKind::NotFound
                                    }) && Path::new(&path)
                                        .parent()
                                        .and_then(Path::to_str)
                                        .is_some_and(scanner::directory_accessible);
                                if removed {
                                    verified.removed.push(path);
                                } else {
                                    verified.unavailable.push(path.clone());
                                    linked_uncovered.push(path);
                                    linked_errors.push(error);
                                }
                            }
                        }
                    }
                    // Every alias was independently re-statted above. A write
                    // can occur between those reads: use the last actual object
                    // metadata for all paths whose fresh identities agree.
                    let mut object_metadata = HashMap::new();
                    for file in &verified.entries {
                        if !file.is_dir && file.file_id != 0 {
                            object_metadata.insert(
                                (file.volume_id.clone(), file.file_id),
                                (
                                    file.size,
                                    file.modified,
                                    file.created,
                                    file.changed,
                                    file.modified_ns,
                                    file.changed_ns,
                                    file.flags,
                                ),
                            );
                        }
                    }
                    for file in &mut verified.entries {
                        if let Some(&(
                            size,
                            modified,
                            created,
                            changed,
                            modified_ns,
                            changed_ns,
                            flags,
                        )) = object_metadata.get(&(file.volume_id.clone(), file.file_id))
                        {
                            file.size = size;
                            file.modified = modified;
                            file.created = created;
                            file.changed = changed;
                            file.modified_ns = modified_ns;
                            file.changed_ns = changed_ns;
                            file.flags = flags;
                        }
                    }
                    linked_count += verified.entries.len();
                    Ok(verified)
                };
            // Old/new inode associations and verified peers share one SQLite
            // transaction. The callback only stats paths, never locks the store.
            let reused_before = verified_objects.reused_objects();
            match self.index_store.lock().unwrap().observe_batch(
                &batch,
                recursive.then_some(&mut observed),
                if initial {
                    None
                } else {
                    Some(&mut check_links)
                },
                (!initial).then_some(&mut verified_objects),
            ) {
                Ok(changed) => changed_count += changed,
                Err(_) if links_cancelled => {
                    // The callback returned this cancellation before the batch
                    // could commit. Preserve that explicit outcome; a database
                    // error remains an error even when Stop was also requested.
                    batch_cancelled = true;
                    return;
                }
                Err(e) => {
                    error = Some(e);
                    self.scan_cancel.store(true, Ordering::Relaxed);
                    return;
                }
            }
            processed_count += batch.len() + linked_count;
            {
                let mut state = self.state.lock().unwrap();
                let previous = state["reconciled_entries"].as_u64().unwrap_or(0);
                state["reconciled_entries"] = json!(previous + (batch.len() + linked_count) as u64);
                for (key, added) in [
                    ("enumerated_entries", batch.len() as u64),
                    ("verified_link_entries", linked_count as u64),
                    (
                        "reused_link_objects",
                        verified_objects.reused_objects() - reused_before,
                    ),
                ] {
                    state[key] = json!(state[key].as_u64().unwrap_or(0) + added);
                }
            }
            if initial {
                self.state.lock().unwrap()["scan_processed_entries"] = json!(processed_count);
            }
            // A progressively larger index is useful during initial enumeration,
            // but rebuilding all prior rows every 10k entries blocks the scanner.
            // Grow publication batches with the index and budget construction to
            // at most one fifth of the following enumeration interval.
            if initial
                && changed_count >= publish_threshold
                && last_publish.elapsed() >= publish_delay
            {
                let started = Instant::now();
                if let Err(e) = self.refresh(false) {
                    error = Some(e);
                    self.scan_cancel.store(true, Ordering::Relaxed);
                }
                publish_delay = (started.elapsed() * 5).max(Duration::from_secs(2));
                publish_threshold =
                    progressive_publish_threshold(self.snapshot.load().entries.len());
                last_publish = Instant::now();
                changed_count = 0;
            }
        };
        let mut report = if recursive {
            scanner::scan_excluding_in_namespace(
                roots,
                configured,
                &[self.index_directory.to_string_lossy().into_owned()],
                &self.scan_cancel,
                on_batch,
            )
        } else {
            scanner::scan_metadata(roots, &self.scan_cancel, on_batch)
        };
        if let Some(e) = error {
            return Err(e);
        }
        // Cancellation can arrive during the final metadata callback, after the
        // scanner's last token check. Never finish coverage or acknowledge it.
        report.cancelled |= batch_cancelled;
        report.uncovered.extend(linked_uncovered);
        report.errors.extend(linked_errors);
        // An incremental removed/renamed entry is a deletion, not a permanently
        // inaccessible directory. A missing configured root remains uncovered
        // (for example an unplugged volume). Verify the parent is enumerable.
        let mut removed = Vec::new();
        if !initial {
            report.uncovered.retain(|path| {
                let missing = !configured.contains(path)
                    && std::fs::symlink_metadata(path)
                        .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                    && Path::new(path)
                        .ancestors()
                        .skip(1)
                        .take_while(|ancestor| {
                            configured.iter().any(|root| ancestor.starts_with(root))
                        })
                        .find(|ancestor| std::fs::symlink_metadata(ancestor).is_ok())
                        .is_some_and(|ancestor| std::fs::read_dir(ancestor).is_ok());
                if missing {
                    removed.push(path.clone());
                }
                !missing
            });
            report.errors.retain(|error| {
                !removed
                    .iter()
                    .any(|path| error.starts_with(&format!("{path}: ")))
            });
        }
        let mut index_store = self.index_store.lock().unwrap();
        if !report.cancelled {
            let mut finish_roots = if recursive {
                roots.to_vec()
            } else {
                Vec::new()
            };
            finish_roots.extend(removed);
            // A remote or snapshot mount may now cover previously indexed local
            // files. Hide those stale rows, retaining them until local coverage
            // returns instead of misinterpreting the intentional skip as deletion.
            let mut protected = report.uncovered.clone();
            protected.extend(report.excluded_mounts.iter().cloned());
            index_store.finish_observed(&finish_roots, &protected, &observed, event_id)?;
        }
        let mut uncovered: Vec<String> =
            serde_json::from_value(index_store.get("uncovered", json!([]))).unwrap_or_default();
        // Only a recursive traversal proves coverage was regained below a path.
        uncovered.retain(|p| {
            !(!report.cancelled && recursive && roots.iter().any(|r| Path::new(p).starts_with(r)))
        });
        uncovered.extend(report.uncovered);
        uncovered.sort();
        uncovered.dedup();
        index_store.set("uncovered", &json!(uncovered))?;
        drop(index_store);
        // Stop keeps the last usable snapshot. Committed batches remain in the
        // durable change journal for the next reconciliation or reopen; building
        // a replacement here can otherwise keep Stop busy for minutes.
        if !(report.cancelled && self.stop.load(Ordering::Relaxed)) {
            self.refresh(initial && !report.cancelled)?;
        }
        {
            let mut state = self.state.lock().unwrap();
            state["uncovered"] = json!(uncovered);
            state["errors"] = json!(report.errors);
            state["excluded_mounts"] = json!(report.excluded_mounts);
            state["event_id"] = json!(event_id);
            if report.cancelled {
                state["state"] = json!("cancelled");
            } else if initial {
                state["state"] = json!("ready");
                state["initial_scan_complete"] = json!(true);
            }
        }
        Ok(!report.cancelled)
    }
    fn resolve_directory_checks(&self, plan: &mut scanner::Reconciliation) -> Result<(), String> {
        let uncovered: Vec<String> =
            serde_json::from_value(self.index_store.lock().unwrap().get("uncovered", json!([])))
                .unwrap_or_default();
        let mut accessible_directories = scanner::PathScopes::default();
        for path in std::mem::take(&mut plan.directory_checks) {
            if self.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            let covered = self
                .index_store
                .lock()
                .unwrap()
                .directory_is_covered(&path)?;
            if !covered || !scanner::directory_accessible(&path) {
                // A new directory or a lost traversal permission needs the
                // ordinary coverage reconciliation, including protected rows.
                plan.recursive.push(path);
                continue;
            }
            accessible_directories.insert(Path::new(&path));
            plan.metadata.push(path);
        }
        // Check each old gap once, instead of testing it against every directory
        // notification. Exact directory checks already established access.
        for denied in uncovered {
            if self.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            let path = Path::new(&denied);
            if accessible_directories.contains(path)
                || (accessible_directories.covers(path) && scanner::scope_accessible(&denied))
            {
                plan.recursive.push(denied);
            }
        }
        let Some(recursive) =
            scanner::normalize_roots_until(&plan.recursive, || self.stop.load(Ordering::Relaxed))
        else {
            return Ok(());
        };
        plan.recursive = recursive;
        let recursive_scopes = scanner::PathScopes::from_paths(&plan.recursive);
        plan.metadata
            .retain(|path| !recursive_scopes.covers(Path::new(path)));
        plan.metadata.sort();
        plan.metadata.dedup();
        Ok(())
    }
    fn checkpoint_resume_proof(
        &self,
        roots: &[String],
        before: &scan_resume::ResumeProof,
    ) -> Result<bool, String> {
        let uncovered: Vec<String> =
            serde_json::from_value(self.index_store.lock().unwrap().get("uncovered", json!([])))
                .unwrap_or_default();
        match scan_resume::inspect(roots, &uncovered) {
            Ok(after) if before.same_namespace(&after.proof) => {
                self.index_store.lock().unwrap().commit_resume_proof(
                    serde_json::to_value(before).map_err(|error| error.to_string())?,
                )?;
                return Ok(before.same_mount_namespace(&after.proof));
            }
            Ok(after) => {
                self.state.lock().unwrap()["resume_checkpoint_error"] =
                    json!(before.rejection_reason(&after.proof));
            }
            Err(error) => {
                self.state.lock().unwrap()["resume_checkpoint_error"] = json!(error);
            }
        }
        Ok(false)
    }
    fn invalidate_wrapped_history(&self, events: &[scanner::ChangeEvent]) -> Result<bool, String> {
        if !events
            .iter()
            .any(|event| event.flags & file_events::EVENT_IDS_WRAPPED != 0)
        {
            return Ok(false);
        }
        self.index_store
            .lock()
            .unwrap()
            .invalidate_event_history("event_ids_wrapped")?;
        self.scanning.store(true, Ordering::SeqCst);
        let mut state = self.state.lock().unwrap();
        state["initial_scan_complete"] = json!(false);
        state["resume_reason"] = json!("event_ids_wrapped");
        state["event_id"] = json!(0);
        Ok(true)
    }
    fn prune_index_namespace(&self, configured: &[String]) -> Result<bool, String> {
        let mut store = self.index_store.lock().unwrap();
        let mut candidates = store.indexed_child_scopes(scanner::SYSTEM_VOLUMES)?;
        let uncovered: Vec<String> =
            serde_json::from_value(store.get("uncovered", json!([]))).unwrap_or_default();
        candidates.extend(uncovered);
        let mut scopes = Vec::new();
        let mut visited = HashSet::new();
        while let Some(path) = candidates.pop() {
            if !visited.insert(path.clone()) {
                continue;
            }
            if !scanner::path_in_namespace(&path, configured) {
                scopes.push(path);
            } else if !scanner::subtree_in_namespace(&path, configured) {
                // An explicitly selected descendant protects its own branch,
                // not every previously indexed sibling on the auxiliary volume.
                candidates.extend(store.indexed_child_scopes(&path)?);
            }
        }
        // These are stored database keys, not filesystem scan selections.
        // Canonicalizing or expanding Data firmlinks would change the deletion
        // namespace and could remove separately configured logical roots.
        scopes.sort_unstable();
        scopes.dedup();
        let mut collapsed = Vec::<String>::new();
        for scope in scopes {
            if !collapsed
                .iter()
                .any(|parent| Path::new(&scope).starts_with(parent))
            {
                collapsed.push(scope);
            }
        }
        let scopes = collapsed;
        let Some(removed) = store.prune_namespace_scopes(&scopes, &self.scan_cancel)? else {
            self.state.lock().unwrap()["state"] = json!("cancelled");
            return Ok(false);
        };
        drop(store);
        {
            let mut state = self.state.lock().unwrap();
            state["namespace_pruned_entries"] = json!(removed);
            state["namespace_pruned_scopes"] = json!(scopes);
        }
        // A completed cleanup is already durable. Stop must not start a new
        // snapshot build after its commit; the next resume restores its delta.
        if self.scan_cancel.load(Ordering::Relaxed) || self.stop.load(Ordering::Relaxed) {
            self.state.lock().unwrap()["state"] = json!("cancelled");
            return Ok(false);
        }
        if removed != 0 {
            self.refresh(false)?;
        }
        Ok(true)
    }
    fn scan_worker(self: Arc<Self>, roots: Vec<String>, watch: bool, resume: bool) {
        let result = loop {
            let mut restart_stream = false;
            let pass = (|| -> Result<(), String> {
                if !self.prune_index_namespace(&roots)? {
                    return Ok(());
                }
                let (saved_cursor, saved_proof_value, uncovered, invalid_history) = {
                    let store = self.index_store.lock().unwrap();
                    let cursor = store.get("event_id", json!(0)).as_u64().unwrap_or(0);
                    let proof = store.get("scan_resume_proof", Value::Null);
                    let uncovered =
                        serde_json::from_value::<Vec<String>>(store.get("uncovered", json!([])))
                            .unwrap_or_default();
                    (
                        cursor,
                        proof,
                        uncovered,
                        store.get("event_history_invalid", Value::Null),
                    )
                };
                let saved_proof =
                    serde_json::from_value::<scan_resume::ResumeProof>(saved_proof_value.clone());
                let inspection = scan_resume::inspect(&roots, &uncovered);
                let current_cursor = scanner::Watcher::current_event_id();
                let reason = if !watch {
                    "watch_disabled"
                } else if !resume {
                    "explicit_baseline_requested"
                } else if invalid_history.as_str() == Some("event_ids_wrapped") {
                    "event_ids_wrapped"
                } else if saved_cursor == 0 {
                    "missing_event_cursor"
                } else if saved_cursor > current_cursor {
                    "event_cursor_out_of_range"
                } else if saved_proof_value.is_null() {
                    "missing_completed_baseline_proof"
                } else {
                    match (&saved_proof, &inspection) {
                        (Err(_), _) => "invalid_baseline_proof",
                        (_, Err(_)) => "inspection_failed",
                        (Ok(previous), Ok(current)) => previous
                            .rejection_reason(&current.proof)
                            .unwrap_or("journal_resume"),
                    }
                };
                let restore = reason == "journal_resume";
                let recheck = if restore {
                    inspection
                        .as_ref()
                        .unwrap()
                        .recheck_against(saved_proof.as_ref().unwrap())
                } else {
                    Vec::new()
                };
                let decision = json!({"reason":reason,"saved_cursor":saved_cursor,"current_cursor":current_cursor,
                "inspection_error":inspection.as_ref().err(),"proof_error":saved_proof.as_ref().err().map(ToString::to_string),
                "saved":saved_proof.as_ref().ok().map(scan_resume::ResumeProof::summary),
                "current":inspection.as_ref().ok().map(|current| current.proof.summary()),
                "recheck_roots":recheck});
                self.index_store
                    .lock()
                    .unwrap()
                    .set("resume_decision", &decision)?;
                {
                    let mut state = self.state.lock().unwrap();
                    state["resume_reason"] = json!(reason);
                    state["resume_decision"] = decision;
                }
                // Capture the initial baseline before starting the watcher. Committing
                // this anchor after enumeration leaves all concurrent events replayable.
                let since = if restore {
                    saved_cursor
                } else {
                    current_cursor
                };
                // Retain the old completed proof while rebuilding. Until finish
                // commits a new cursor, it remains the recovery/diagnostic anchor.
                let watcher = if watch {
                    Some(scanner::Watcher::start(&roots, since)?)
                } else {
                    None
                };
                let mut completed_mount_namespace = false;
                if restore {
                    {
                        let mut state = self.state.lock().unwrap();
                        state["event_id"] = json!(since);
                        state["resumed"] = json!(true);
                        state["resumed_recheck_roots"] = json!(recheck);
                        state["uncovered"] = json!(uncovered);
                    }
                    // External/non-journal volumes and previous permission gaps do
                    // not inherit the boot volume's proof. Recheck only those paths.
                    if !recheck.is_empty()
                        && !self.reconcile(&recheck, &roots, since, true, false)?
                    {
                        return Ok(());
                    }
                    completed_mount_namespace =
                        self.checkpoint_resume_proof(&roots, &inspection.as_ref().unwrap().proof)?;
                    self.state.lock().unwrap()["initial_scan_complete"] = json!(true);
                } else {
                    self.state.lock().unwrap()["resumed"] = json!(false);
                    if !self.reconcile(&roots, &roots, since, true, true)? {
                        return Ok(());
                    }
                    if watch {
                        if let Ok(before) = &inspection {
                            completed_mount_namespace =
                                self.checkpoint_resume_proof(&roots, &before.proof)?;
                        }
                    }
                }
                self.scanning.store(false, Ordering::SeqCst);
                self.state.lock().unwrap()["covered_history_mount_cursor"] =
                    json!(completed_mount_namespace.then_some(since));
                if let Some(watcher) = watcher {
                    let mut replaying_history = true;
                    self.state.lock().unwrap()["state"] = json!("catching_up");
                    let mut pending_events = Vec::new();
                    while !self.stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(150));
                        pending_events.extend(watcher.drain());
                        if pending_events.is_empty() {
                            continue;
                        }
                        let events = std::mem::take(&mut pending_events);
                        if self.invalidate_wrapped_history(&events)? {
                            restart_stream = true;
                            break;
                        }
                        let persisted = self
                            .index_store
                            .lock()
                            .unwrap()
                            .get("event_id", json!(0))
                            .as_u64()
                            .unwrap_or(0);
                        let history_done = events
                            .iter()
                            .any(|event| event.flags & file_events::EVENT_HISTORY_DONE != 0);
                        let events: Vec<_> = events
                            .into_iter()
                            .filter(|event| {
                                event.flags & file_events::EVENT_HISTORY_DONE == 0
                                    && (event.flags & (0x02 | 0x04 | 0x08) != 0
                                        || !Path::new(&event.path)
                                            .starts_with(&self.index_directory))
                            })
                            .collect();
                        // Only delivered filesystem events acknowledge this stream.
                        // Disk Arbitration/root-change controls have ID zero.
                        let max_id = events
                            .iter()
                            .map(|event| event.event_id)
                            .max()
                            .unwrap_or(persisted)
                            .max(persisted);
                        // Saving the cursor writes our WAL and creates another
                        // event. An all-self event drain must not save a cursor or
                        // it becomes a permanent 150ms disk-write feedback loop.
                        if events.is_empty() {
                            if history_done {
                                replaying_history = false;
                            }
                            if !replaying_history {
                                self.state.lock().unwrap()["state"] = json!("watching");
                            }
                            if history_done {
                                self.rebuild_missing_cache();
                            }
                            continue;
                        }
                        let historical_mount_cursor =
                            (replaying_history && completed_mount_namespace).then_some(since);
                        let mut plan = scanner::reconciliation_plan_with_history(
                            &roots,
                            &events,
                            historical_mount_cursor,
                        );
                        self.resolve_directory_checks(&mut plan)?;
                        if self.stop.load(Ordering::Relaxed) {
                            break;
                        }
                        self.scan_cancel.store(false, Ordering::Relaxed);
                        // Event progress is committed only after all paths in this
                        // drain have been reconciled; a crash replays an unfinished drain.
                        if !plan.recursive.is_empty()
                            && !self.reconcile(&plan.recursive, &roots, persisted, true, false)?
                        {
                            pending_events = events;
                            if history_done {
                                pending_events.push(scanner::ChangeEvent::from_flags(
                                    "/",
                                    0,
                                    file_events::EVENT_HISTORY_DONE,
                                ));
                            }
                            continue;
                        }
                        if !plan.metadata.is_empty()
                            && !self.reconcile(&plan.metadata, &roots, persisted, false, false)?
                        {
                            pending_events = events;
                            if history_done {
                                pending_events.push(scanner::ChangeEvent::from_flags(
                                    "/",
                                    0,
                                    file_events::EVENT_HISTORY_DONE,
                                ));
                            }
                            continue;
                        }
                        if max_id > persisted {
                            self.index_store.lock().unwrap().advance_event_id(max_id)?;
                        }
                        if history_done {
                            replaying_history = false;
                        }
                        let mut state = self.state.lock().unwrap();
                        state["event_id"] = json!(max_id);
                        state["state"] = json!(if replaying_history {
                            "catching_up"
                        } else {
                            "watching"
                        });
                        drop(state);
                        if history_done {
                            self.rebuild_missing_cache();
                        }
                    }
                }
                Ok(())
            })();
            // All old-stream callbacks and queued old-epoch IDs are drained by
            // Watcher's Drop before a fresh stream captures the new baseline.
            if pass.is_ok() && restart_stream && !self.stop.load(Ordering::Relaxed) {
                continue;
            }
            break pass;
        };
        if let Err(e) = result {
            let mut state = self.state.lock().unwrap();
            state["state"] = json!("error");
            state["errors"] = json!([e]);
        } else if self.stop.load(Ordering::Relaxed) {
            self.state.lock().unwrap()["state"] = json!("stopped");
        }
        self.scanning.store(false, Ordering::SeqCst);
        self.active.store(false, Ordering::SeqCst);
        self.state.notify();
    }
    fn import_file_list(&self, request: &Value) -> Result<Value, String> {
        if self.active.load(Ordering::SeqCst) {
            return Err("Use a separate offline database for imported file lists".into());
        }
        let rows = request["rows"].as_array().ok_or("rows must be an array")?;
        let mut entries = Vec::new();
        for (i, row) in rows.iter().enumerate() {
            let path = row["path"]
                .as_str()
                .ok_or("Every imported row needs path")?;
            let file_path = Path::new(path);
            let is_dir = row["is_dir"].as_bool().unwrap_or(false);
            entries.push(scanner::ScannedFile {
                path: path.into(),
                name: row["name"].as_str().map(str::to_string).unwrap_or_else(|| {
                    file_path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                }),
                extension: if is_dir {
                    String::new()
                } else {
                    file_path
                        .extension()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_lowercase()
                },
                size: row["size"].as_u64().unwrap_or(0),
                modified: row["modified"].as_i64().unwrap_or(0),
                created: row["created"].as_i64().unwrap_or(0),
                is_dir,
                file_id: i as u64 + 1,
                volume_id: "offline:filelist".into(),
                ..Default::default()
            });
        }
        {
            let mut index_store = self.index_store.lock().unwrap();
            index_store.clear()?;
            index_store.batch(&entries, 1)?;
            index_store.set("offline", &json!(true))?;
            index_store.set("watch_enabled", &json!(false))?;
        }
        self.offline.store(true, Ordering::Relaxed);
        self.state.lock().unwrap()["offline"] = json!(true);
        self.state.lock().unwrap()["state"] = json!("offline");
        self.refresh(true)?;
        Ok(json!({"imported":entries.len(),"offline":true}))
    }
    fn duplicates(&self, request: &Value) -> Result<Value, String> {
        if self.offline.load(Ordering::Relaxed) {
            return Err("Content duplicate detection requires online files".into());
        }
        let request_id = request["request_id"].as_str().map(str::to_string);
        let cancelled = Arc::new(AtomicBool::new(false));
        if let Some(id) = &request_id {
            self.requests
                .lock()
                .unwrap()
                .insert(id.clone(), cancelled.clone());
        }
        let _guard = RequestGuard {
            requests: &self.requests,
            id: request_id,
            token: cancelled.clone(),
        };
        let mode = request["mode"].as_str().unwrap_or("content");
        if !["content", "name", "size"].contains(&mode) {
            return Err("Duplicate mode must be content, name, or size".into());
        }
        let snapshot = self.pin(request["generation"].as_u64())?;
        duplicates::find(&snapshot, mode, &cancelled)
    }
}
fn validate_content_identity(path: &str, expected: &Value) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("Content source unavailable: {error}"))?;
    let identity = file_identity::FileIdentity::from_metadata(&metadata);
    let matches = metadata.file_type().is_file()
        && expected["file_id"].as_u64() == Some(identity.file_id)
        && expected["device_id"].as_u64() == Some(identity.device_id)
        && expected["size"].as_u64() == Some(identity.size)
        && expected["modified_ns"].as_i64() == Some(identity.modified_ns())
        && expected["changed_ns"].as_i64() == Some(identity.changed_ns());
    if !matches {
        return Err("Content source changed during extraction; retry with current metadata".into());
    }
    Ok(())
}
fn anchored_offset(
    offset: usize,
    limit: usize,
    total: usize,
    requested: bool,
    anchor: Option<usize>,
    delta: i64,
) -> usize {
    if !requested {
        return offset;
    }
    anchor
        .map(|rank| {
            (rank as i128 - delta as i128)
                .max(0)
                .min(total.saturating_sub(1) as i128) as usize
        })
        .unwrap_or_else(|| offset.min(total.saturating_sub(limit.max(1))))
}
fn read_text(indexed_file: &IndexedFile, warnings: &mut HashSet<String>) -> Option<String> {
    if indexed_file.is_dir || indexed_file.is_symlink {
        return None;
    }
    if indexed_file.flags & 0x40000000 != 0 {
        warnings.insert("Cloud placeholders were skipped without downloading".into());
        return None;
    }
    if indexed_file.size > 16 * 1024 * 1024 {
        warnings.insert("Uncached content larger than 16 MiB was skipped".into());
        return None;
    }
    if ![
        "txt", "md", "rs", "swift", "c", "h", "cpp", "hpp", "py", "js", "ts", "tsx", "jsx", "json",
        "toml", "yaml", "yml", "xml", "html", "css", "csv", "log", "sh", "sql", "ini", "cfg",
        "plist", "tex",
    ]
    .contains(&indexed_file.extension.as_str())
    {
        warnings.insert("Some files need the macOS content extractor or have unsupported formats; content results are partial".into());
        return None;
    }
    let result = (|| -> Result<String, String> {
        let fresh = scanner::stat_entry(&indexed_file.path)?;
        if fresh.is_symlink || fresh.is_dataless() || fresh.is_dir {
            return Err("File became a symlink, directory or cloud placeholder".into());
        }
        if fresh.volume_id != indexed_file.volume_id
            || fresh.file_id != indexed_file.file_id
            || fresh.size != indexed_file.size
            || fresh.modified_ns != indexed_file.modified_ns
            || fresh.changed_ns != indexed_file.changed_ns
        {
            return Err("File changed since this query snapshot; refresh the index".into());
        }
        let metadata =
            std::fs::symlink_metadata(&indexed_file.path).map_err(|error| error.to_string())?;
        let expected = file_identity::FileIdentity::from_metadata(&metadata);
        if !metadata.file_type().is_file() || !expected.matches_entry(indexed_file) {
            return Err("File identity changed before reading content".into());
        }
        let mut file = open_regular(&indexed_file.path)?;
        read_verified_text(&mut file, &indexed_file.path, &expected)
    })();
    match result {
        Ok(text) => Some(text),
        Err(error) => {
            warnings.insert(format!("Content results are partial: {error}"));
            None
        }
    }
}
fn read_verified_text(
    file: &mut std::fs::File,
    path: &str,
    expected: &file_identity::FileIdentity,
) -> Result<String, String> {
    use std::io::Read;
    let matches_descriptor = |file: &std::fs::File| -> Result<(), String> {
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        if !metadata.file_type().is_file()
            || file_identity::FileIdentity::from_metadata(&metadata) != *expected
        {
            return Err("Opened file identity or contents changed during reading".into());
        }
        #[cfg(target_os = "macos")]
        {
            use std::os::macos::fs::MetadataExt;
            if metadata.st_flags() & 0x40000000 != 0 {
                return Err("Cloud placeholder skipped".into());
            }
        }
        Ok(())
    };
    matches_descriptor(file)?;
    let mut bytes = Vec::new();
    (&mut *file)
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > 16 * 1024 * 1024 {
        return Err("Content exceeds bounded read".into());
    }
    if bytes.len() as u64 != expected.size {
        return Err("File size changed during content reading".into());
    }
    matches_descriptor(file)?;
    let final_metadata = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !final_metadata.file_type().is_file()
        || file_identity::FileIdentity::from_metadata(&final_metadata) != *expected
    {
        return Err("Content path was replaced while its original descriptor remained open".into());
    }
    String::from_utf8(bytes).map_err(|error| error.to_string())
}

fn open_regular(path: &str) -> Result<std::fs::File, String> {
    use std::os::unix::fs::OpenOptionsExt;
    // Refuse a replaced symlink at any component of an indexed physical path.
    #[cfg(target_os = "macos")]
    let flags = libc::O_NOFOLLOW_ANY | libc::O_NONBLOCK;
    #[cfg(not(target_os = "macos"))]
    let flags = libc::O_NOFOLLOW | libc::O_NONBLOCK;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(path)
        .map_err(|e| e.to_string())?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("Only regular files can be read for content".into());
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt;
        if meta.st_flags() & 0x40000000 != 0 {
            return Err("Cloud placeholder skipped".into());
        }
    }
    Ok(file)
}
fn normalize_roots(roots: Vec<String>) -> Result<Vec<String>, String> {
    for root in &roots {
        if !Path::new(root).is_absolute() {
            return Err("Index roots must be absolute paths".into());
        }
    }
    Ok(scanner::normalize_roots(&roots))
}
struct RequestGuard<'a> {
    requests: &'a Mutex<HashMap<String, Arc<AtomicBool>>>,
    id: Option<String>,
    token: Arc<AtomicBool>,
}
impl Drop for RequestGuard<'_> {
    fn drop(&mut self) {
        if let Some(id) = &self.id {
            let mut active_requests = self.requests.lock().unwrap();
            if active_requests
                .get(id)
                .is_some_and(|v| Arc::ptr_eq(v, &self.token))
            {
                active_requests.remove(id);
            }
        }
    }
}

/// Open an opaque engine handle for the Swift/CLI C ABI.
///
/// # Safety
/// `path` must be null or a readable NUL-terminated string that remains valid
/// for this call. Close a returned non-null handle exactly once.
#[no_mangle]
pub unsafe extern "C" fn filesearch_engine_open(path: *const c_char) -> *mut c_void {
    if path.is_null() {
        return std::ptr::null_mut();
    }
    let result = std::panic::catch_unwind(|| {
        let path = CStr::from_ptr(path).to_str().map_err(|e| e.to_string())?;
        SearchEngine::open(Path::new(path))
    });
    match result {
        Ok(Ok(engine)) => Box::into_raw(Box::new(engine)) as *mut c_void,
        _ => std::ptr::null_mut(),
    }
}
/// Execute one JSON operation and return a caller-owned response string.
///
/// # Safety
/// A non-null `handle` must come from `filesearch_engine_open` and remain open throughout
/// this call. `request` must be null or a readable NUL-terminated string. Release
/// the result with `filesearch_engine_free_string`, never another allocator.
#[no_mangle]
pub unsafe extern "C" fn filesearch_engine_call(
    handle: *mut c_void,
    request: *const c_char,
) -> *mut c_char {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if handle.is_null() || request.is_null() {
            return json!({"success":false,"error":"Null engine or request","protocol_version":PROTOCOL_VERSION});
        }
        let engine = &*(handle as *const Arc<SearchEngine>);
        match serde_json::from_slice::<Value>(CStr::from_ptr(request).to_bytes()) {
            Ok(request) => engine.call(request),
            Err(e) => {
                json!({"success":false,"error":format!("Invalid JSON: {e}"),"protocol_version":PROTOCOL_VERSION})
            }
        }
    }));
    let response = result
        .unwrap_or_else(|_| json!({"success":false,"error":"Search engine panic contained at FFI boundary","protocol_version":PROTOCOL_VERSION}));
    CString::new(response.to_string()).unwrap().into_raw()
}
/// Release a response allocated by `filesearch_engine_call`.
///
/// # Safety
/// `value` must be null or an unmodified, not-yet-freed pointer returned by
/// `filesearch_engine_call`. No other thread may read it during or after this call.
#[no_mangle]
pub unsafe extern "C" fn filesearch_engine_free_string(value: *mut c_char) {
    if !value.is_null() {
        drop(CString::from_raw(value));
    }
}
/// Close an engine handle and signal its background worker to stop.
///
/// # Safety
/// `handle` must be null or a live handle from `filesearch_engine_open`, closed exactly
/// once. No thread may enter or remain in `filesearch_engine_call` with this handle.
#[no_mangle]
pub unsafe extern "C" fn filesearch_engine_close(handle: *mut c_void) {
    if !handle.is_null() {
        let engine = Box::from_raw(handle as *mut Arc<SearchEngine>);
        engine.stop.store(true, Ordering::Relaxed);
        engine.scan_cancel.store(true, Ordering::Relaxed);
        drop(engine);
    }
}

// Publish approximately logarithmically during a growing initial index.
fn progressive_publish_threshold(indexed_count: usize) -> usize {
    (indexed_count / 2).max(10_000)
}

#[cfg(all(test, target_os = "macos"))]
mod reconciliation_tests {
    use super::*;
    #[test]
    fn clean_persisted_cache_is_not_rewritten_without_a_new_revision() {
        use std::os::unix::fs::MetadataExt;
        let fixture = tempfile::tempdir().unwrap();
        let engine = SearchEngine::open(&fixture.path().join("index.sqlite")).unwrap();
        engine.refresh(true).unwrap();
        let path = engine.index_store.lock().unwrap().cache_path.clone();
        let original = std::fs::metadata(&path).unwrap().ino();
        assert_eq!(
            engine
                .index_store
                .lock()
                .unwrap()
                .get("cache_dirty", json!(null)),
            false
        );
        engine.refresh(true).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().ino(),
            original,
            "A clean cache was unnecessarily replaced"
        );
        engine
            .index_store
            .lock()
            .unwrap()
            .set("cache_dirty", &json!(true))
            .unwrap();
        engine.refresh(true).unwrap();
        assert_ne!(
            std::fs::metadata(&path).unwrap().ino(),
            original,
            "The existing persisted dirty key was ignored"
        );
        assert_eq!(
            engine
                .index_store
                .lock()
                .unwrap()
                .get("cache_dirty", json!(null)),
            false
        );
    }
    #[test]
    fn incremental_directory_events_keep_scanning_false_and_preserve_descendants() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("files");
        std::fs::create_dir_all(root.join("before/subfolder")).unwrap();
        std::fs::write(root.join("before/subfolder/child.txt"), "inside").unwrap();
        let root = root.canonicalize().unwrap().to_string_lossy().into_owned();
        let engine = SearchEngine::open(&temp.path().join("db/index.sqlite")).unwrap();
        let result = engine.call(json!({"op":"scan","roots":[root],"watch":false,"wait":true}));
        assert_eq!(result["success"], true);
        let roots = [root.clone()];
        let before = format!("{root}/before");
        let status = engine.call(json!({"op":"status"}));
        assert_eq!(status["initial_scan_complete"], true);
        assert_eq!(status["scanning"], false);
        // Updating only a directory's own metadata must not delete its children.
        assert!(engine
            .reconcile(std::slice::from_ref(&before), &roots, 40, false, false)
            .unwrap());
        assert_eq!(
            engine.call(json!({"op":"query","text":"child.txt"}))["total"],
            1
        );
        let updating = engine.call(json!({"op":"status"}));
        assert_eq!(updating["scanning"], false);
        assert_eq!(updating["updating"], true);
        assert_eq!(updating["generation"], status["generation"]);
        let after = format!("{root}/after");
        std::fs::rename(&before, &after).unwrap();
        assert!(engine
            .reconcile(&[before.clone(), after.clone()], &roots, 41, true, false)
            .unwrap());
        let result = engine.call(json!({"op":"query","text":"child.txt"}));
        assert_eq!(result["total"], 1);
        assert_eq!(
            result["rows"][0]["path"],
            format!("{after}/subfolder/child.txt")
        );
        assert_eq!(
            engine.call(json!({"op":"query","text":"path:before"}))["total"],
            0
        );
        assert_eq!(engine.call(json!({"op":"status"}))["uncovered"], json!([]));
        std::fs::remove_dir_all(&after).unwrap();
        assert!(engine.reconcile(&[after], &roots, 42, true, false).unwrap());
        assert_eq!(
            engine.call(json!({"op":"query","text":"child.txt"}))["total"],
            0
        );
        assert_eq!(engine.call(json!({"op":"status"}))["scanning"], false);
    }
    #[test]
    fn one_hardlink_event_refreshes_each_existing_directory_entry() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("files");
        std::fs::create_dir_all(root.join("one")).unwrap();
        std::fs::create_dir(root.join("two")).unwrap();
        let first = root.join("one/first.txt");
        let second = root.join("two/second.txt");
        std::fs::write(&first, "old").unwrap();
        std::fs::hard_link(&first, &second).unwrap();
        let root = root.canonicalize().unwrap().to_string_lossy().into_owned();
        let first = first.canonicalize().unwrap().to_string_lossy().into_owned();
        let second = second
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let engine = SearchEngine::open(&temp.path().join("db/index.sqlite")).unwrap();
        assert_eq!(
            engine.call(json!({"op":"scan","roots":[root],"watch":false,"wait":true}))["success"],
            true
        );
        engine.call(
            json!({"op":"put_content","path":second,"text":"old","properties":{"cached":"old"}}),
        );
        std::fs::write(&first, "new longer contents").unwrap();
        engine
            .reconcile(
                std::slice::from_ref(&first),
                std::slice::from_ref(&root),
                44,
                true,
                false,
            )
            .unwrap();
        let query = engine.call(json!({"op":"query","text":"ext:txt"}));
        assert_eq!(query["total"], 2);
        for row in query["rows"].as_array().unwrap() {
            assert_eq!(row["size"], 19);
            assert_eq!(row["content_indexed"], false);
        }
        // A stale inode association must not be copied onto a replaced path.
        std::fs::remove_file(&second).unwrap();
        std::fs::write(&second, "replacement").unwrap();
        std::fs::write(&first, "again").unwrap();
        engine
            .reconcile(
                std::slice::from_ref(&first),
                std::slice::from_ref(&root),
                45,
                true,
                false,
            )
            .unwrap();
        let query = engine.call(json!({"op":"query","text":"ext:txt"}));
        let rows = query["rows"].as_array().unwrap();
        assert_eq!(
            rows.iter().find(|row| row["path"] == first).unwrap()["size"],
            5
        );
        assert_eq!(
            rows.iter().find(|row| row["path"] == second).unwrap()["size"],
            11
        );
    }
    #[test]
    fn directory_stat_alone_does_not_claim_denied_descendants_are_covered() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("files");
        let blocked = root.join("blocked");
        std::fs::create_dir_all(&blocked).unwrap();
        std::fs::write(blocked.join("secret.txt"), "secret").unwrap();
        let root = root.canonicalize().unwrap().to_string_lossy().into_owned();
        let blocked = blocked
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let engine = SearchEngine::open(&temp.path().join("db/index.sqlite")).unwrap();
        engine.call(json!({"op":"scan","roots":[root],"watch":false,"wait":true}));
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let recursive = engine.reconcile(
            std::slice::from_ref(&blocked),
            std::slice::from_ref(&root),
            46,
            true,
            false,
        );
        let metadata = engine.reconcile(std::slice::from_ref(&blocked), &[root], 47, false, false);
        let status = engine.call(json!({"op":"status"}));
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(recursive.unwrap());
        assert!(metadata.unwrap());
        assert_eq!(status["uncovered"], json!([blocked]));
        assert_eq!(
            engine.call(json!({"op":"query","text":"secret.txt"}))["total"],
            0
        );
    }
    #[test]
    fn vanished_configured_root_stays_uncovered() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("files");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("file.txt"), "data").unwrap();
        let root = root.canonicalize().unwrap().to_string_lossy().into_owned();
        let roots = [root.clone()];
        let engine = SearchEngine::open(&temp.path().join("db/index.sqlite")).unwrap();
        assert_eq!(
            engine.call(json!({"op":"scan","roots":[root],"watch":false,"wait":true}))["success"],
            true
        );
        std::fs::remove_dir_all(&root).unwrap();
        engine.reconcile(&roots, &roots, 43, true, false).unwrap();
        assert_eq!(
            engine.call(json!({"op":"status"}))["uncovered"],
            json!(roots)
        );
        assert_eq!(
            engine.call(json!({"op":"query","text":"file.txt"}))["total"],
            0
        );
    }

    #[test]
    fn database_directory_alias_is_excluded_from_its_own_scan() {
        let temp = tempfile::Builder::new()
            .prefix("apf-alias-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::write(temp.path().join("visible.txt"), "visible").unwrap();
        let engine = SearchEngine::open(&temp.path().join("cache/index.sqlite")).unwrap();
        let root = temp.path().canonicalize().unwrap();
        assert_eq!(
            engine.call(json!({"op":"scan","roots":[root],"watch":false,"wait":true}))["success"],
            true
        );
        let result = engine.call(json!({"op":"query","text":"","limit":100}));
        assert_eq!(
            result["total"], 2,
            "Only the root and visible.txt belong in the index: {result}"
        );
    }
    #[test]
    fn delta_slots_preserve_old_pages_and_reused_rowids() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("files");
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap().to_string_lossy().into_owned();
        for name in ["alpha.txt", "beta.txt", "last.txt"] {
            std::fs::write(format!("{root}/{name}"), name).unwrap();
        }
        let engine = SearchEngine::open(&temp.path().join("db/index.sqlite")).unwrap();
        engine.call(json!({"op":"scan","roots":[root],"watch":false,"wait":true}));
        let lease = engine.call(json!({"op":"retain_snapshot"}));
        let old =
            engine.call(json!({"op":"query","text":"","snapshot_lease":lease["snapshot_lease"]}));
        let last = format!("{root}/last.txt");
        std::fs::remove_file(&last).unwrap();
        engine
            .reconcile(
                std::slice::from_ref(&last),
                std::slice::from_ref(&root),
                7,
                true,
                false,
            )
            .unwrap();
        let new = format!("{root}/new-file.txt");
        std::fs::write(&new, "new row").unwrap();
        engine
            .reconcile(
                std::slice::from_ref(&new),
                std::slice::from_ref(&root),
                8,
                true,
                false,
            )
            .unwrap();
        assert_eq!(engine.call(json!({"op":"query","text":"last"}))["total"], 0);
        assert_eq!(
            engine.call(json!({"op":"query","text":"new-file"}))["total"],
            1
        );
        assert_eq!(
            engine.call(json!({"op":"query","text":"","snapshot_lease":lease["snapshot_lease"]}))
                ["rows"],
            old["rows"]
        );
        let snapshot = engine.snapshot.load();
        let reference = engine.index_store.lock().unwrap().entries().unwrap();
        assert_eq!(
            serde_json::to_value(snapshot.visible_entries().collect::<Vec<_>>()).unwrap(),
            serde_json::to_value(reference).unwrap()
        );
        assert_eq!(snapshot.len(), 4);
        engine.refresh(true).unwrap();
        let reopened = SearchEngine::open(&temp.path().join("db/index.sqlite")).unwrap();
        assert_eq!(
            reopened.call(json!({"op":"query","text":"last"}))["total"],
            0
        );
        assert_eq!(
            reopened.call(json!({"op":"query","text":"new-file"}))["total"],
            1
        );
    }
    #[test]
    fn removed_intermediate_directories_do_not_become_false_permission_gaps() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("files");
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/b/child.txt"), "child").unwrap();
        let root = root.canonicalize().unwrap().to_string_lossy().into_owned();
        let engine = SearchEngine::open(&temp.path().join("db/index.sqlite")).unwrap();
        engine.call(json!({"op":"scan","roots":[root],"watch":false,"wait":true}));
        std::fs::remove_dir_all(format!("{root}/a")).unwrap();
        engine
            .reconcile(
                &[format!("{root}/a/b")],
                std::slice::from_ref(&root),
                9,
                true,
                false,
            )
            .unwrap();
        assert_eq!(engine.call(json!({"op":"status"}))["uncovered"], json!([]));
        assert_eq!(
            engine.call(json!({"op":"query","text":"child"}))["total"],
            0
        );
    }
    #[test]
    fn own_wal_events_do_not_restart_scan_or_write_forever() {
        use std::os::unix::fs::MetadataExt;
        let temp = tempfile::Builder::new()
            .prefix("apf-watch-self-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::write(temp.path().join("visible.txt"), "visible").unwrap();
        let dbpath = temp.path().join("cache/index.sqlite");
        let engine = SearchEngine::open(&dbpath).unwrap();
        let root = temp.path().canonicalize().unwrap();
        assert_eq!(
            engine.call(json!({"op":"watch","roots":[root],"watch":true}))["success"],
            true
        );
        let until = Instant::now() + Duration::from_secs(5);
        while engine.call(json!({"op":"status"}))["state"] != "watching" && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(700));
        let before = engine.call(json!({"op":"status"}));
        let stamp = |path: &Path| {
            let metadata = std::fs::metadata(path).unwrap();
            (metadata.mtime(), metadata.mtime_nsec(), metadata.len())
        };
        let wal = dbpath.with_file_name("index.sqlite-wal");
        let before_wal = stamp(&wal);
        std::thread::sleep(Duration::from_millis(800));
        let after = engine.call(json!({"op":"status"}));
        let after_wal = stamp(&wal);
        engine.call(json!({"op":"stop"}));
        assert_eq!(before["state"], "watching");
        assert_eq!(after["state"], "watching");
        assert_eq!(before["generation"], after["generation"]);
        assert_eq!(after["count"], 2);
        assert_eq!(after["scanning"], false);
        assert_eq!(
            before_wal, after_wal,
            "Ignored WAL events must not write another WAL event"
        );
    }
    #[test]
    fn dropped_event_reconciliation_repairs_unobserved_create_rename_and_delete() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("files");
        std::fs::create_dir_all(root.join("before/subfolder")).unwrap();
        std::fs::write(root.join("before/subfolder/child.txt"), "child").unwrap();
        std::fs::write(root.join("remove.txt"), "gone").unwrap();
        let root = root.canonicalize().unwrap().to_string_lossy().into_owned();
        let engine = SearchEngine::open(&temp.path().join("db/index.sqlite")).unwrap();
        engine.call(json!({"op":"scan","roots":[root],"watch":false,"wait":true}));
        std::fs::rename(format!("{root}/before"), format!("{root}/after")).unwrap();
        std::fs::remove_file(format!("{root}/remove.txt")).unwrap();
        std::fs::write(format!("{root}/new.txt"), "new").unwrap();
        let plan = scanner::reconciliation_plan(
            std::slice::from_ref(&root),
            &[scanner::ChangeEvent::from_flags(
                &format!("{root}/after"),
                55,
                0x04,
            )],
        );
        engine
            .reconcile(
                &plan.recursive,
                std::slice::from_ref(&root),
                55,
                true,
                false,
            )
            .unwrap();
        let rows = engine.call(json!({"op":"query","text":"","limit":100}));
        let got: HashSet<_> = rows["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["path"].as_str().unwrap().to_string())
            .collect();
        let mut want = HashSet::new();
        let mut pending = vec![PathBuf::from(&root)];
        while let Some(path) = pending.pop() {
            want.insert(path.to_string_lossy().into_owned());
            if path.is_dir() {
                for entry in std::fs::read_dir(path).unwrap() {
                    pending.push(entry.unwrap().path());
                }
            }
        }
        assert_eq!(got, want);
        assert_eq!(engine.call(json!({"op":"status"}))["event_id"], 55);
    }
    #[test]
    #[ignore = "million-row real-file update benchmark; run explicitly in release"]
    fn million_row_incremental_updates() {
        let source = std::env::var("APF_INCREMENTAL_SOURCE")
            .expect("Set APF_INCREMENTAL_SOURCE to an isolated synthetic benchmark database");
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("live-file-for-incremental-benchmark.txt");
        std::fs::write(&path, "00000000").unwrap();
        let path = path.canonicalize().unwrap().to_string_lossy().into_owned();
        let dbpath = temp.path().join("db/index.sqlite");
        std::fs::create_dir(dbpath.parent().unwrap()).unwrap();
        std::fs::copy(&source, &dbpath).unwrap();
        {
            let mut db = IndexStore::open(&dbpath).unwrap();
            db.set("offline", &json!(false)).unwrap();
            db.set("watch_enabled", &json!(false)).unwrap();
            db.batch(&[scanner::stat_entry(&path).unwrap()], 1).unwrap();
        }
        let engine = SearchEngine::open(&dbpath).unwrap();
        let mut samples = Vec::new();
        let mut create_samples = Vec::new();
        let mut rename_samples = Vec::new();
        let mut delete_samples = Vec::new();
        let root = temp
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let runs = std::env::var("APF_INCREMENTAL_RUNS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(3);
        for i in 0..runs {
            let started = Instant::now();
            std::fs::write(&path, format!("{i:08}")).unwrap();
            engine
                .reconcile(
                    std::slice::from_ref(&path),
                    std::slice::from_ref(&path),
                    0,
                    false,
                    false,
                )
                .unwrap();
            let elapsed = started.elapsed().as_secs_f64() * 1000.;
            let real = scanner::stat_entry(&path).unwrap();
            let snapshot = engine.snapshot.load();
            let entry = snapshot.entries.iter().find(|e| e.path == path).unwrap();
            assert_eq!(entry.modified_ns, real.modified_ns);
            assert_eq!(entry.changed_ns, real.changed_ns);
            samples.push(elapsed);
            eprintln!("Million-row real-file update {}: {:.2} ms", i + 1, elapsed);
            drop(snapshot);
            let created = format!("{root}/added-{i}.txt");
            let renamed = format!("{root}/renamed-{i}.txt");
            let started = Instant::now();
            std::fs::write(&created, "created").unwrap();
            engine
                .reconcile(
                    std::slice::from_ref(&created),
                    std::slice::from_ref(&root),
                    0,
                    true,
                    false,
                )
                .unwrap();
            create_samples.push(started.elapsed().as_secs_f64() * 1000.);
            assert_eq!(
                engine.call(json!({"op":"query","text":format!("name:added-{i}.txt")}))["total"],
                1
            );
            let started = Instant::now();
            std::fs::rename(&created, &renamed).unwrap();
            engine
                .reconcile(
                    &[created.clone(), renamed.clone()],
                    std::slice::from_ref(&root),
                    0,
                    true,
                    false,
                )
                .unwrap();
            rename_samples.push(started.elapsed().as_secs_f64() * 1000.);
            assert_eq!(
                engine.call(json!({"op":"query","text":format!("name:added-{i}.txt")}))["total"],
                0
            );
            assert_eq!(
                engine.call(json!({"op":"query","text":format!("name:renamed-{i}.txt")}))["total"],
                1
            );
            let started = Instant::now();
            std::fs::remove_file(&renamed).unwrap();
            engine
                .reconcile(
                    std::slice::from_ref(&renamed),
                    std::slice::from_ref(&root),
                    0,
                    true,
                    false,
                )
                .unwrap();
            delete_samples.push(started.elapsed().as_secs_f64() * 1000.);
            assert_eq!(
                engine.call(json!({"op":"query","text":format!("name:renamed-{i}.txt")}))["total"],
                0
            );
        }
        let current = engine.snapshot.load();
        let reference = engine.index_store.lock().unwrap().entries().unwrap();
        let fingerprint = |entries: Vec<&IndexedFile>| {
            let mut hash = blake3::Hasher::new();
            for entry in entries {
                hash.update(&serde_json::to_vec(entry).unwrap());
            }
            hash.finalize().to_hex().to_string()
        };
        let observed_hash = fingerprint(current.visible_entries().collect());
        let reference_hash = fingerprint(reference.iter().collect());
        assert_eq!(
            observed_hash, reference_hash,
            "All searchable metadata must match an independent full SQLite reload"
        );
        let mut sorted = samples.clone();
        sorted.sort_by(f64::total_cmp);
        let p95 = sorted[(sorted.len() * 95).div_ceil(100) - 1];
        let stats = |values: &Vec<f64>| {
            let mut sorted = values.clone();
            sorted.sort_by(f64::total_cmp);
            json!({"samples_ms":values,"p95_ms":sorted[(sorted.len()*95).div_ceil(100)-1],"max_ms":sorted.last()})
        };
        let report = json!({"create":stats(&create_samples),"rename":stats(&rename_samples),"delete":stats(&delete_samples),"entries":current.len(),"runs":runs,"samples_ms":samples,"p95_ms":p95,"p95_under_1000ms":p95<=1000.,"all_metadata_matches_full_sqlite_reload":true,"blake3":observed_hash,"boundary":"Actual file write + SearchEngine::reconcile + publication in a million-row synthetic index; excludes watcher delivery, XPC and GUI","source_database":source});
        if let Ok(output) = std::env::var("APF_INCREMENTAL_OUTPUT") {
            std::fs::write(output, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
        }
        eprintln!("{report}");
    }
}

#[cfg(test)]
mod scan_resume_tests;

#[cfg(test)]
mod event_scope_tests;

#[cfg(test)]
mod stop_publication_tests;

#[cfg(test)]
mod link_pass_tests;

#[cfg(test)]
mod path_scope_tests;

#[cfg(test)]
mod snapshot_remap_tests;

#[cfg(test)]
mod snapshot_history_tests;
