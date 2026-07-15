//! Ley-line VCS — jj sidecar for automatic versioning of the SQLite arena.
//!
//! Architecture: "The Sidecar Pattern"
//!   Hot Path:  Agent -> NFS -> HotSwapGraph -> SQLite  (microseconds)
//!   Notify:    VersionedGraph -> Channel               (async, non-blocking)
//!   Cold Path: Debouncer -> snapshot_to_jj -> jj       (milliseconds)
//!
//! jj never touches the hot path. The agent never needs to know it exists.
//! The `.leyline/` virtual directory in the mount exposes time-travel.

use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::io::AsyncReadExt as _;
use jj_lib::backend::{CopyId, TreeValue};
use jj_lib::config::StackedConfig;
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::settings::UserSettings;
use jj_lib::tree_builder::TreeBuilder;
use jj_lib::workspace::{Workspace, default_working_copy_factories};
use leyline_core::Controller;
use pollster::FutureExt as _;
use tokio::sync::mpsc;

use leyline_fs::graph::{Graph, Node};
use leyline_fs::staging::StagingGraph;

// ---------------------------------------------------------------------------
// DexTask — lightweight task tracker exposed via .dex/ virtual directory
// ---------------------------------------------------------------------------

/// A task in the dex tracker (JSONL-compatible with dex.rip).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct DexTask {
    id: String,
    description: String,
    status: String,
    /// Node IDs this task is editing (linked from .staging/).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    staged: Vec<String>,
    created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    completed_at: Option<i64>,
}

/// In-memory task store for the dex tracker.
#[derive(Debug, Default)]
struct DexStore {
    tasks: Vec<DexTask>,
    next_id: u64,
}

impl DexStore {
    fn new() -> Self {
        Self {
            tasks: Vec::new(),
            next_id: 1,
        }
    }

    fn create(&mut self, description: &str) -> &DexTask {
        let id = format!("dex-{}", self.next_id);
        self.next_id += 1;
        self.tasks.push(DexTask {
            id,
            description: description.to_string(),
            status: "pending".into(),
            staged: Vec::new(),
            created_at: now_nanos() / 1_000_000_000, // seconds
            completed_at: None,
        });
        self.tasks
            .last()
            .expect("just pushed above; last() must be Some")
    }

    fn find_mut(&mut self, id: &str) -> Option<&mut DexTask> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }

    fn current(&self) -> Option<&DexTask> {
        self.tasks
            .iter()
            .find(|t| t.status == "in_progress")
            .or_else(|| self.tasks.iter().find(|t| t.status == "pending"))
    }

    fn to_jsonl(&self) -> String {
        self.tasks
            .iter()
            .map(|t| serde_json::to_string(t).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n")
            + if self.tasks.is_empty() { "" } else { "\n" }
    }
}

// ---------------------------------------------------------------------------
// Ignore filter — avoid commit spam from temp/build artifacts
// ---------------------------------------------------------------------------

const IGNORED_PREFIXES: &[&str] = &["target/", ".git/", "build/", "tmp/", ".leyline/"];
const IGNORED_SUFFIXES: &[&str] = &[".log", ".DS_Store"];

fn is_ignored(id: &str) -> bool {
    let name = id.rsplit('/').next().unwrap_or(id);
    IGNORED_PREFIXES.iter().any(|p| id.starts_with(p))
        || IGNORED_SUFFIXES.iter().any(|s| name.ends_with(s))
}

// ---------------------------------------------------------------------------
// JjIntegration — init/open/commit/revert against a jj repo
// ---------------------------------------------------------------------------

/// Manages a jj repository for snapshotting SQLite graph state.
pub struct JjIntegration {
    jj_dir: PathBuf,
    settings: UserSettings,
}

impl JjIntegration {
    fn make_settings() -> Result<UserSettings> {
        UserSettings::from_config(StackedConfig::with_defaults())
            .map_err(|e| anyhow::anyhow!("settings: {e}"))
    }

    /// Create a new jj repo at the given directory.
    pub fn init(jj_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(jj_dir)?;
        let settings = Self::make_settings()?;

        Workspace::init_simple(&settings, jj_dir)
            .block_on()
            .map_err(|e| anyhow::anyhow!("jj init failed: {e}"))?;

        log::info!("initialized jj repo at {}", jj_dir.display());

        Ok(Self {
            jj_dir: jj_dir.to_path_buf(),
            settings,
        })
    }

    /// Open an existing jj repo.
    pub fn open(jj_dir: &Path) -> Result<Self> {
        let settings = Self::make_settings()?;

        // Verify we can load it
        Workspace::load(
            &settings,
            jj_dir,
            &Default::default(),
            &default_working_copy_factories(),
        )
        .map_err(|e| anyhow::anyhow!("jj open failed: {e}"))?;

        Ok(Self {
            jj_dir: jj_dir.to_path_buf(),
            settings,
        })
    }

    /// Init if new, open if exists.
    pub fn init_or_open(jj_dir: &Path) -> Result<Self> {
        if jj_dir.join(".jj").exists() {
            Self::open(jj_dir)
        } else {
            Self::init(jj_dir)
        }
    }

    /// Snapshot the current graph state into a jj commit.
    ///
    /// Walks all nodes from the Graph, builds a jj tree, and creates a commit.
    /// Returns the commit ID as a hex string.
    pub fn commit_snapshot(&self, graph: &dyn Graph, message: &str) -> Result<String> {
        let repo = self.load_repo()?;
        let store = repo.store().clone();

        // Build a jj tree from the graph
        let empty_tree_id = store.empty_tree_id().clone();
        let mut tree_builder = TreeBuilder::new(store.clone(), empty_tree_id);

        self.walk_graph_into_tree(graph, "", &mut tree_builder)?;

        let tree_id = tree_builder
            .write_tree()
            .block_on()
            .map_err(|e| anyhow::anyhow!("write tree: {e}"))?;

        // Wrap TreeId into MergedTree for new_commit
        let merged_tree = MergedTree::resolved(store.clone(), tree_id);

        // Linear history: parent on the current head (or root if first commit)
        let heads: Vec<_> = repo.view().heads().iter().cloned().collect();
        let parent_ids =
            if heads.is_empty() || (heads.len() == 1 && heads[0] == *store.root_commit_id()) {
                vec![store.root_commit_id().clone()]
            } else {
                // Filter out root commit from heads, use remaining as parents
                let non_root: Vec<_> = heads
                    .into_iter()
                    .filter(|id| id != store.root_commit_id())
                    .collect();
                if non_root.is_empty() {
                    vec![store.root_commit_id().clone()]
                } else {
                    non_root
                }
            };

        let mut tx = repo.start_transaction();
        let commit = tx
            .repo_mut()
            .new_commit(parent_ids, merged_tree)
            .set_description(message.to_string())
            .write()
            .block_on()
            .map_err(|e| anyhow::anyhow!("write commit: {e}"))?;

        let commit_id_hex = commit.id().hex();
        tx.commit(message)
            .block_on()
            .map_err(|e| anyhow::anyhow!("tx commit: {e}"))?;

        log::info!("jj snapshot: {} ({})", &commit_id_hex[..12], message);
        Ok(commit_id_hex)
    }

    /// Resolve a commit by hex prefix (supports both short and full IDs).
    ///
    /// Walks from all heads backwards, returning the first commit whose hex ID
    /// starts with the given prefix. Skips the root commit.
    fn resolve_commit_by_prefix(
        &self,
        repo: &ReadonlyRepo,
        prefix: &str,
    ) -> Result<jj_lib::commit::Commit> {
        let store = repo.store();
        let mut seen = std::collections::HashSet::new();
        let mut queue: Vec<jj_lib::backend::CommitId> =
            repo.view().heads().iter().cloned().collect();

        while let Some(id) = queue.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            if id == *store.root_commit_id() {
                continue;
            }
            let commit = store
                .get_commit(&id)
                .map_err(|e| anyhow::anyhow!("get commit: {e}"))?;
            if commit.id().hex().starts_with(prefix) {
                return Ok(commit);
            }
            for parent_id in commit.parent_ids() {
                queue.push(parent_id.clone());
            }
        }

        anyhow::bail!("no commit matching prefix '{prefix}'")
    }

    /// Revert the graph to a specific jj commit.
    ///
    /// Walks the jj tree for the given commit and replaces the entire graph
    /// contents. This is a "naive revert" — deletes all nodes then re-creates
    /// from the jj tree. Not atomic across the Graph trait boundary (a future
    /// `Graph::restore_atomic` could wrap this in a SQLite transaction).
    pub fn revert_to_commit(&self, commit_id_prefix: &str, graph: &dyn Graph) -> Result<()> {
        let repo = self.load_repo()?;

        // Resolve commit by prefix (supports both short and full hex IDs)
        let commit = self.resolve_commit_by_prefix(&repo, commit_id_prefix)?;
        let tree = commit.tree();

        log::info!(
            "reverting to commit {} (resolved from '{}')",
            &commit.id().hex()[..12],
            commit_id_prefix,
        );

        // Clear current graph (skip .leyline virtual dir)
        for child in graph.list_children("")? {
            if child.name != ".leyline" {
                graph.remove_node(&child.id)?;
            }
        }

        // Walk jj tree and populate graph
        self.restore_tree_to_graph(graph, "", &tree)?;

        log::info!("revert complete");
        Ok(())
    }

    /// Walk a jj MergedTree and recreate all files/dirs in the graph.
    fn restore_tree_to_graph(
        &self,
        graph: &dyn Graph,
        _parent_id: &str,
        tree: &MergedTree,
    ) -> Result<()> {
        for (path, value_result) in tree.entries() {
            let value = match value_result {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("skipping entry {}: {e}", path.as_internal_file_string());
                    continue;
                }
            };

            // Only handle resolved (non-conflicted) values
            let resolved = match value.into_resolved() {
                Ok(v) => v,
                Err(_) => {
                    log::warn!(
                        "skipping conflicted entry {}",
                        path.as_internal_file_string()
                    );
                    continue;
                }
            };

            let Some(tree_value) = resolved else {
                continue; // deleted entry
            };

            if let TreeValue::File { id, .. } = tree_value {
                let path_str = path.as_internal_file_string();

                // Ensure parent directories exist
                self.ensure_parent_dirs(graph, path_str)?;

                // Read blob from jj store
                let name = path_str.rsplit('/').next().unwrap_or(path_str);
                let file_parent = if path_str.contains('/') {
                    &path_str[..path_str.len() - name.len() - 1]
                } else {
                    ""
                };

                let mut reader = tree
                    .store()
                    .read_file(&path, &id)
                    .block_on()
                    .map_err(|e| anyhow::anyhow!("read blob: {e}"))?;
                let mut content = Vec::new();
                let mut buf = [0u8; 8192];
                loop {
                    let n = reader
                        .read(&mut buf)
                        .block_on()
                        .map_err(|e| anyhow::anyhow!("read blob bytes: {e}"))?;
                    if n == 0 {
                        break;
                    }
                    content.extend_from_slice(&buf[..n]);
                }

                let node_id = graph.create_node(file_parent, name, false)?;
                if !content.is_empty() {
                    graph.write_content(&node_id, &content, 0)?;
                }
            }
        }
        Ok(())
    }

    /// Create parent directories for a path if they don't exist.
    fn ensure_parent_dirs(&self, graph: &dyn Graph, path: &str) -> Result<()> {
        let parts: Vec<&str> = path.split('/').collect();
        if parts.len() <= 1 {
            return Ok(()); // file at root, no parents needed
        }

        let mut current_parent = String::new();
        for &part in &parts[..parts.len() - 1] {
            let dir_id = if current_parent.is_empty() {
                part.to_string()
            } else {
                format!("{current_parent}/{part}")
            };

            // Only create if missing
            if graph.get_node(&dir_id)?.is_none() {
                graph.create_node(&current_parent, part, true)?;
            }

            current_parent = dir_id;
        }
        Ok(())
    }

    /// Get recent log entries as JSON (newest first, linear walk from head).
    pub fn log_json(&self, limit: usize) -> Result<String> {
        let repo = self.load_repo()?;
        let store = repo.store();

        // Find the single non-root head (linear history)
        let head_ids: Vec<_> = repo
            .view()
            .heads()
            .iter()
            .filter(|id| *id != store.root_commit_id())
            .cloned()
            .collect();

        let mut commits_json = Vec::new();

        // Walk backwards from head through parent chain
        let mut current = head_ids.into_iter().next();
        while let Some(commit_id) = current {
            if commits_json.len() >= limit {
                break;
            }
            if &commit_id == store.root_commit_id() {
                break;
            }

            let commit = match store.get_commit(&commit_id) {
                Ok(c) => c,
                Err(_) => break,
            };

            let id_hex = commit.id().hex();
            commits_json.push(serde_json::json!({
                "id": id_hex,
                "short_id": &id_hex[..id_hex.len().min(12)],
                "description": commit.description(),
                "timestamp": commit.author().timestamp.timestamp.0,
            }));

            // Follow first parent (linear history)
            current = commit.parent_ids().first().cloned();
        }

        Ok(serde_json::to_string_pretty(&commits_json)?)
    }

    fn load_repo(&self) -> Result<Arc<ReadonlyRepo>> {
        let workspace = Workspace::load(
            &self.settings,
            &self.jj_dir,
            &Default::default(),
            &default_working_copy_factories(),
        )
        .map_err(|e| anyhow::anyhow!("load workspace: {e}"))?;

        workspace
            .repo_loader()
            .load_at_head()
            .block_on()
            .map_err(|e| anyhow::anyhow!("load repo: {e}"))
    }

    /// Recursively walk the graph and insert file entries into the tree builder.
    fn walk_graph_into_tree(
        &self,
        graph: &dyn Graph,
        parent_id: &str,
        tree_builder: &mut TreeBuilder,
    ) -> Result<()> {
        let children = graph.list_children(parent_id)?;
        for child in &children {
            if is_ignored(&child.id) {
                continue;
            }
            if child.is_dir {
                self.walk_graph_into_tree(graph, &child.id, tree_builder)?;
            } else {
                // Read content
                let mut buf = vec![0u8; child.size.max(1) as usize];
                let n = graph.read_content(&child.id, &mut buf, 0)?;
                buf.truncate(n);

                let path = RepoPathBuf::from_internal_string(&child.id)
                    .map_err(|e| anyhow::anyhow!("invalid path '{}': {e}", child.id))?;

                // write_file is async — use pollster to block
                let blob_id = tree_builder
                    .store()
                    .write_file(&path, &mut buf.as_slice())
                    .block_on()
                    .map_err(|e| anyhow::anyhow!("write blob: {e}"))?;

                tree_builder.set_or_remove(
                    path,
                    Some(TreeValue::File {
                        id: blob_id,
                        executable: false,
                        copy_id: CopyId::placeholder(),
                    }),
                );
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// WriteEvent — notifications from VersionedGraph to commit_loop
// ---------------------------------------------------------------------------

/// Events emitted by VersionedGraph when the graph is mutated.
#[derive(Debug, Clone)]
pub enum WriteEvent {
    ContentChanged(String),
    NodeCreated(String),
    NodeRemoved(String),
    NodeRenamed { old: String, new: String },
}

// ---------------------------------------------------------------------------
// VersionedGraph — intercepts writes, sends events, handles .leyline/ virtuals
// ---------------------------------------------------------------------------

fn now_nanos() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock never before UNIX_EPOCH")
        .as_nanos() as i64
}

/// Wraps an inner Graph, forwarding all operations.
/// On mutations, sends WriteEvents to the commit_loop channel.
/// Intercepts `.leyline/*` paths to expose the virtual control surface.
/// Intercepts `.staging/*` paths to expose the CoW staging overlay.
pub struct VersionedGraph {
    inner: Arc<dyn Graph>,
    tx: mpsc::UnboundedSender<WriteEvent>,
    jj: Arc<Mutex<JjIntegration>>,
    /// CoW staging overlay — writes to `.staging/*` go here.
    staging: StagingGraph,
    /// Dex task tracker — exposed via `.dex/` virtual directory.
    dex: Mutex<DexStore>,
    /// Optional channel to signal the embedding loop on writes.
    embed_tx: Option<tokio::sync::mpsc::UnboundedSender<()>>,
}

impl VersionedGraph {
    pub fn new(
        inner: Arc<dyn Graph>,
        tx: mpsc::UnboundedSender<WriteEvent>,
        jj: Arc<Mutex<JjIntegration>>,
    ) -> Result<Self> {
        let staging = StagingGraph::new(inner.clone())?;
        Ok(Self {
            inner,
            tx,
            jj,
            staging,
            dex: Mutex::new(DexStore::new()),
            embed_tx: None,
        })
    }

    /// Attach an embedding channel sender. Writes will signal this channel
    /// so the embed_loop can incrementally re-embed changed nodes.
    pub fn with_embed_tx(mut self, tx: tokio::sync::mpsc::UnboundedSender<()>) -> Self {
        self.embed_tx = Some(tx);
        self
    }

    fn is_virtual(id: &str) -> bool {
        id == ".leyline" || id.starts_with(".leyline/")
    }

    fn is_staging(id: &str) -> bool {
        id == ".staging" || id.starts_with(".staging/")
    }

    /// Strip `.staging/` prefix to get the inner graph path.
    fn staging_path(id: &str) -> &str {
        id.strip_prefix(".staging/").unwrap_or("")
    }

    /// Control file names inside `.staging/`.
    const STAGING_CONTROLS: &[&str] = &[".dirty", ".commit", ".discard"];

    fn is_staging_control(name: &str) -> bool {
        Self::STAGING_CONTROLS.contains(&name)
    }

    fn read_staging_control(&self, name: &str, buf: &mut [u8], offset: u64) -> Result<usize> {
        let content = match name {
            ".dirty" => {
                let dirty = self.staging.dirty_nodes()?;
                let tombs = self.staging.tombstone_nodes()?;
                let mut lines = Vec::new();
                for id in &dirty {
                    lines.push(format!("+{id}"));
                }
                for id in &tombs {
                    lines.push(format!("-{id}"));
                }
                if lines.is_empty() {
                    "(clean)\n".to_string()
                } else {
                    lines.join("\n") + "\n"
                }
            }
            ".commit" | ".discard" => {
                // Read returns usage hint
                format!("echo 1 > .staging/{name}\n")
            }
            _ => return Ok(0),
        };

        let bytes = content.as_bytes();
        let off = offset as usize;
        if off >= bytes.len() {
            return Ok(0);
        }
        let end = (off + buf.len()).min(bytes.len());
        let n = end - off;
        buf[..n].copy_from_slice(&bytes[off..end]);
        Ok(n)
    }

    fn write_staging_control(&self, name: &str, _data: &[u8]) -> Result<usize> {
        match name {
            ".commit" => {
                log::info!("staging commit requested");
                self.staging.commit()?;
                let _ = self
                    .tx
                    .send(WriteEvent::ContentChanged("_staging_commit".to_string()));
                Ok(_data.len())
            }
            ".discard" => {
                log::info!("staging discard requested");
                self.staging.discard()?;
                Ok(_data.len())
            }
            _ => anyhow::bail!("unknown staging control file: {name}"),
        }
    }

    fn is_dex(id: &str) -> bool {
        id == ".dex" || id.starts_with(".dex/")
    }

    /// Virtual files inside `.dex/`.
    const DEX_FILES: &[&str] = &["tasks", "current", "complete"];

    fn read_dex(&self, name: &str, buf: &mut [u8], offset: u64) -> Result<usize> {
        let content = match name {
            "tasks" => self.dex.lock().to_jsonl(),
            "current" => {
                let store = self.dex.lock();
                match store.current() {
                    Some(task) => {
                        let mut obj = serde_json::to_value(task)?;
                        // Attach current staging dirty list
                        let dirty = self.staging.dirty_nodes().unwrap_or_default();
                        let tombs = self.staging.tombstone_nodes().unwrap_or_default();
                        obj["staged_dirty"] = serde_json::json!(dirty);
                        obj["staged_tombstones"] = serde_json::json!(tombs);
                        serde_json::to_string_pretty(&obj)? + "\n"
                    }
                    None => "{}\n".to_string(),
                }
            }
            "complete" => "echo <task-id> > .dex/complete\n".to_string(),
            _ => return Ok(0),
        };

        let bytes = content.as_bytes();
        let off = offset as usize;
        if off >= bytes.len() {
            return Ok(0);
        }
        let end = (off + buf.len()).min(bytes.len());
        let n = end - off;
        buf[..n].copy_from_slice(&bytes[off..end]);
        Ok(n)
    }

    fn write_dex(&self, name: &str, data: &[u8]) -> Result<usize> {
        let content = std::str::from_utf8(data)
            .context("dex write must be UTF-8")?
            .trim();

        match name {
            "tasks" => {
                // Write creates a new task; content is the description
                if content.is_empty() {
                    anyhow::bail!("task description cannot be empty");
                }
                let mut store = self.dex.lock();
                let task = store.create(content);
                log::info!("dex: created task {} — {}", task.id, task.description);
                Ok(data.len())
            }
            "current" => {
                // Write a task ID to set it as in_progress
                let mut store = self.dex.lock();
                if let Some(task) = store.find_mut(content) {
                    task.status = "in_progress".into();
                    // Snapshot current dirty nodes into the task
                    task.staged = self.staging.dirty_nodes().unwrap_or_default();
                    log::info!("dex: started task {}", content);
                } else {
                    anyhow::bail!("task '{content}' not found");
                }
                Ok(data.len())
            }
            "complete" => {
                // Write task ID to commit staging + mark task done
                let task_id = content.to_string();

                // Snapshot dirty nodes into the task before committing
                {
                    let mut store = self.dex.lock();
                    if let Some(task) = store.find_mut(&task_id) {
                        task.staged = self.staging.dirty_nodes().unwrap_or_default();
                    } else {
                        anyhow::bail!("task '{task_id}' not found");
                    }
                }

                // Commit staging → batch splice into live
                self.staging.commit()?;
                let _ = self
                    .tx
                    .send(WriteEvent::ContentChanged("_dex_complete".to_string()));

                // Mark task completed
                {
                    let mut store = self.dex.lock();
                    if let Some(task) = store.find_mut(&task_id) {
                        task.status = "completed".into();
                        task.completed_at = Some(now_nanos() / 1_000_000_000);
                    }
                }

                log::info!("dex: completed task {task_id}");
                Ok(data.len())
            }
            _ => anyhow::bail!("unknown dex file: {name}"),
        }
    }

    fn read_virtual(&self, id: &str, buf: &mut [u8], offset: u64) -> Result<usize> {
        let content = match id {
            ".leyline/status" => "{\"status\":\"ok\"}\n".to_string(),
            ".leyline/log" => {
                let jj = self.jj.lock();
                jj.log_json(20)?
            }
            _ => return Ok(0),
        };

        let bytes = content.as_bytes();
        let off = offset as usize;
        if off >= bytes.len() {
            return Ok(0);
        }
        let end = (off + buf.len()).min(bytes.len());
        let n = end - off;
        buf[..n].copy_from_slice(&bytes[off..end]);
        Ok(n)
    }

    fn write_virtual(&self, id: &str, data: &[u8], _offset: u64) -> Result<usize> {
        let content = std::str::from_utf8(data)
            .context("virtual write must be UTF-8")?
            .trim();

        match id {
            ".leyline/revert" => {
                log::info!("revert requested: {content}");
                let jj = self.jj.lock();
                jj.revert_to_commit(content, self.inner.as_ref())?;
                Ok(data.len())
            }
            _ => anyhow::bail!("unknown control file: {id}"),
        }
    }
}

impl Graph for VersionedGraph {
    fn get_node(&self, id: &str) -> Result<Option<Node>> {
        if id == ".leyline" {
            return Ok(Some(Node {
                id: ".leyline".into(),
                name: ".leyline".into(),
                is_dir: true,
                size: 0,
                mtime_nanos: now_nanos(),
            }));
        }
        if Self::is_virtual(id) {
            let name = id.rsplit('/').next().unwrap_or(id);
            return Ok(Some(Node {
                id: id.into(),
                name: name.into(),
                is_dir: false,
                size: 65536,
                mtime_nanos: now_nanos(),
            }));
        }
        if id == ".dex" {
            return Ok(Some(Node {
                id: ".dex".into(),
                name: ".dex".into(),
                is_dir: true,
                size: 0,
                mtime_nanos: now_nanos(),
            }));
        }
        if Self::is_dex(id) {
            let name = id.rsplit('/').next().unwrap_or(id);
            if Self::DEX_FILES.contains(&name) {
                return Ok(Some(Node {
                    id: id.into(),
                    name: name.into(),
                    is_dir: false,
                    size: 65536,
                    mtime_nanos: now_nanos(),
                }));
            }
            return Ok(None);
        }
        if id == ".staging" {
            return Ok(Some(Node {
                id: ".staging".into(),
                name: ".staging".into(),
                is_dir: true,
                size: 0,
                mtime_nanos: now_nanos(),
            }));
        }
        if Self::is_staging(id) {
            let inner_path = Self::staging_path(id);
            let name = id.rsplit('/').next().unwrap_or(id);
            // Control files
            if Self::is_staging_control(name) {
                return Ok(Some(Node {
                    id: id.into(),
                    name: name.into(),
                    is_dir: false,
                    size: 65536,
                    mtime_nanos: now_nanos(),
                }));
            }
            // Delegate to staging graph
            return match self.staging.get_node(inner_path)? {
                Some(node) => Ok(Some(Node {
                    id: id.into(),
                    name: node.name,
                    is_dir: node.is_dir,
                    size: node.size,
                    mtime_nanos: node.mtime_nanos,
                })),
                None => Ok(None),
            };
        }
        self.inner.get_node(id)
    }

    fn lookup_child(&self, parent_id: &str, name: &str) -> Result<Option<Node>> {
        if parent_id.is_empty() && name == ".leyline" {
            return Ok(Some(Node {
                id: ".leyline".into(),
                name: ".leyline".into(),
                is_dir: true,
                size: 0,
                mtime_nanos: now_nanos(),
            }));
        }
        if parent_id == ".leyline" {
            let id = format!(".leyline/{name}");
            return match name {
                "status" | "log" | "revert" => Ok(Some(Node {
                    id,
                    name: name.into(),
                    is_dir: false,
                    size: 65536,
                    mtime_nanos: now_nanos(),
                })),
                _ => Ok(None),
            };
        }
        if parent_id.is_empty() && name == ".dex" {
            return Ok(Some(Node {
                id: ".dex".into(),
                name: ".dex".into(),
                is_dir: true,
                size: 0,
                mtime_nanos: now_nanos(),
            }));
        }
        if parent_id == ".dex" {
            if Self::DEX_FILES.contains(&name) {
                return Ok(Some(Node {
                    id: format!(".dex/{name}"),
                    name: name.into(),
                    is_dir: false,
                    size: 65536,
                    mtime_nanos: now_nanos(),
                }));
            }
            return Ok(None);
        }
        if parent_id.is_empty() && name == ".staging" {
            return Ok(Some(Node {
                id: ".staging".into(),
                name: ".staging".into(),
                is_dir: true,
                size: 0,
                mtime_nanos: now_nanos(),
            }));
        }
        if Self::is_staging(parent_id) || parent_id == ".staging" {
            let inner_parent = if parent_id == ".staging" {
                ""
            } else {
                Self::staging_path(parent_id)
            };
            // Control files live directly under .staging/
            if inner_parent.is_empty() && Self::is_staging_control(name) {
                let id = format!(".staging/{name}");
                return Ok(Some(Node {
                    id,
                    name: name.into(),
                    is_dir: false,
                    size: 65536,
                    mtime_nanos: now_nanos(),
                }));
            }
            // Delegate to staging graph
            return match self.staging.lookup_child(inner_parent, name)? {
                Some(node) => {
                    let staged_id = if parent_id == ".staging" {
                        format!(".staging/{}", node.id)
                    } else {
                        format!("{parent_id}/{}", node.name)
                    };
                    Ok(Some(Node {
                        id: staged_id,
                        name: node.name,
                        is_dir: node.is_dir,
                        size: node.size,
                        mtime_nanos: node.mtime_nanos,
                    }))
                }
                None => Ok(None),
            };
        }
        self.inner.lookup_child(parent_id, name)
    }

    fn list_children(&self, parent_id: &str) -> Result<Vec<Node>> {
        let now = now_nanos();
        if parent_id == ".leyline" {
            return Ok(vec![
                Node {
                    id: ".leyline/status".into(),
                    name: "status".into(),
                    is_dir: false,
                    size: 65536,
                    mtime_nanos: now,
                },
                Node {
                    id: ".leyline/log".into(),
                    name: "log".into(),
                    is_dir: false,
                    size: 65536,
                    mtime_nanos: now,
                },
                Node {
                    id: ".leyline/revert".into(),
                    name: "revert".into(),
                    is_dir: false,
                    size: 65536,
                    mtime_nanos: now,
                },
            ]);
        }
        if parent_id == ".dex" {
            let now = now_nanos();
            return Ok(Self::DEX_FILES
                .iter()
                .map(|&name| Node {
                    id: format!(".dex/{name}"),
                    name: name.into(),
                    is_dir: false,
                    size: 65536,
                    mtime_nanos: now,
                })
                .collect());
        }
        if parent_id == ".staging" || Self::is_staging(parent_id) {
            let inner_parent = if parent_id == ".staging" {
                ""
            } else {
                Self::staging_path(parent_id)
            };
            let inner_children = self.staging.list_children(inner_parent)?;
            let mut children: Vec<Node> = inner_children
                .into_iter()
                .map(|node| {
                    let staged_id = format!(".staging/{}", node.id);
                    Node {
                        id: staged_id,
                        name: node.name,
                        is_dir: node.is_dir,
                        size: node.size,
                        mtime_nanos: node.mtime_nanos,
                    }
                })
                .collect();
            // Inject control files at .staging/ root
            if inner_parent.is_empty() {
                for &name in Self::STAGING_CONTROLS {
                    children.push(Node {
                        id: format!(".staging/{name}"),
                        name: name.into(),
                        is_dir: false,
                        size: 65536,
                        mtime_nanos: now,
                    });
                }
            }
            return Ok(children);
        }
        let mut children = self.inner.list_children(parent_id)?;
        if parent_id.is_empty() {
            children.push(Node {
                id: ".leyline".into(),
                name: ".leyline".into(),
                is_dir: true,
                size: 0,
                mtime_nanos: now,
            });
            children.push(Node {
                id: ".staging".into(),
                name: ".staging".into(),
                is_dir: true,
                size: 0,
                mtime_nanos: now,
            });
            children.push(Node {
                id: ".dex".into(),
                name: ".dex".into(),
                is_dir: true,
                size: 0,
                mtime_nanos: now,
            });
        }
        Ok(children)
    }

    fn read_content(&self, id: &str, buf: &mut [u8], offset: u64) -> Result<usize> {
        if Self::is_virtual(id) {
            return self.read_virtual(id, buf, offset);
        }
        if Self::is_dex(id) {
            let name = id.rsplit('/').next().unwrap_or("");
            return self.read_dex(name, buf, offset);
        }
        if Self::is_staging(id) {
            let inner_path = Self::staging_path(id);
            let name = id.rsplit('/').next().unwrap_or("");
            if Self::is_staging_control(name) {
                return self.read_staging_control(name, buf, offset);
            }
            return self.staging.read_content(inner_path, buf, offset);
        }
        self.inner.read_content(id, buf, offset)
    }

    fn write_content(&self, id: &str, data: &[u8], offset: u64) -> Result<usize> {
        if Self::is_virtual(id) {
            return self.write_virtual(id, data, offset);
        }
        if Self::is_dex(id) {
            let name = id.rsplit('/').next().unwrap_or("");
            return self.write_dex(name, data);
        }
        if Self::is_staging(id) {
            let inner_path = Self::staging_path(id);
            let name = id.rsplit('/').next().unwrap_or("");
            if Self::is_staging_control(name) {
                return self.write_staging_control(name, data);
            }
            return self.staging.write_content(inner_path, data, offset);
        }
        let result = self.inner.write_content(id, data, offset)?;
        let _ = self.tx.send(WriteEvent::ContentChanged(id.to_string()));
        if let Some(ref etx) = self.embed_tx {
            let _ = etx.send(());
        }
        Ok(result)
    }

    fn create_node(&self, parent_id: &str, name: &str, is_dir: bool) -> Result<String> {
        if Self::is_staging(parent_id) || parent_id == ".staging" {
            let inner_parent = if parent_id == ".staging" {
                ""
            } else {
                Self::staging_path(parent_id)
            };
            let inner_id = self.staging.create_node(inner_parent, name, is_dir)?;
            return Ok(format!(".staging/{inner_id}"));
        }
        let id = self.inner.create_node(parent_id, name, is_dir)?;
        let _ = self.tx.send(WriteEvent::NodeCreated(id.clone()));
        if let Some(ref etx) = self.embed_tx {
            let _ = etx.send(());
        }
        Ok(id)
    }

    fn remove_node(&self, id: &str) -> Result<()> {
        if Self::is_staging(id) {
            let inner_path = Self::staging_path(id);
            return self.staging.remove_node(inner_path);
        }
        self.inner.remove_node(id)?;
        let _ = self.tx.send(WriteEvent::NodeRemoved(id.to_string()));
        if let Some(ref etx) = self.embed_tx {
            let _ = etx.send(());
        }
        Ok(())
    }

    fn truncate(&self, id: &str) -> Result<()> {
        if Self::is_staging(id) {
            let inner_path = Self::staging_path(id);
            return self.staging.truncate(inner_path);
        }
        self.inner.truncate(id)
    }

    fn rename_node(&self, id: &str, new_parent_id: &str, new_name: &str) -> Result<()> {
        // Staging paths are read-through + CoW; rename not supported in staging overlay
        if Self::is_staging(id) || Self::is_staging(new_parent_id) {
            anyhow::bail!("rename not supported in .staging/ overlay");
        }
        let new_id = if new_parent_id.is_empty() {
            new_name.to_string()
        } else {
            format!("{new_parent_id}/{new_name}")
        };
        self.inner.rename_node(id, new_parent_id, new_name)?;
        let _ = self.tx.send(WriteEvent::NodeRenamed {
            old: id.to_string(),
            new: new_id,
        });
        if let Some(ref etx) = self.embed_tx {
            let _ = etx.send(());
        }
        Ok(())
    }

    fn flush_node(&self, id: &str) -> Result<()> {
        self.inner.flush_node(id)?;
        let _ = self.tx.send(WriteEvent::ContentChanged(id.to_string()));
        Ok(())
    }

    fn batch_splice(&self, edits: &[(String, Option<String>)]) -> Result<()> {
        self.inner.batch_splice(edits)?;
        let _ = self
            .tx
            .send(WriteEvent::ContentChanged("_batch".to_string()));
        Ok(())
    }

    fn serialize(&self) -> Result<Vec<u8>> {
        self.inner.serialize()
    }

    fn flush_to_arena(&self) -> Result<()> {
        self.inner.flush_to_arena()
    }
}

// ---------------------------------------------------------------------------
// jj_commit_loop — debounced background snapshotting
// ---------------------------------------------------------------------------

/// Debounced commit loop: collects write events, waits for a quiet period,
/// then snapshots the graph into a jj commit.
///
/// If `control_path` is provided, also polls the control block generation
/// every second. This catches external mutations (e.g. `leyline load`) that
/// bypass the Graph trait and therefore emit no WriteEvents.
pub async fn jj_commit_loop(
    mut rx: mpsc::UnboundedReceiver<WriteEvent>,
    jj: Arc<Mutex<JjIntegration>>,
    graph: Arc<dyn Graph>,
    quiet_period: Duration,
    control_path: Option<PathBuf>,
) {
    let mut batch: Vec<WriteEvent> = Vec::new();
    // T2.4: track Σ root (BLAKE3 of arena bytes) instead of the dropped
    // public `generation` counter. `[0u8; 32]` (Hash::ZERO) is the sentinel
    // for "no current root yet" — safe initial value: any real root differs.
    let mut last_root: [u8; 32] = control_path
        .as_ref()
        .and_then(|p| Controller::open_or_create(p).ok())
        .map(|c| c.current_root())
        .unwrap_or([0u8; 32]);

    let mut poll_interval = tokio::time::interval(Duration::from_secs(1));
    poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else { break };
                batch.push(event);

                // Collect more events during the quiet period
                loop {
                    match tokio::time::timeout(quiet_period, rx.recv()).await {
                        Ok(Some(e)) => batch.push(e),
                        Ok(None) => return,
                        Err(_) => break, // quiet period elapsed
                    }
                }

                let message = format_commit_message(&batch);
                batch.clear();

                match jj.lock().commit_snapshot(graph.as_ref(), &message) {
                    Ok(id) => {
                        log::debug!("committed snapshot {}", &id[..12]);
                        if let Err(e) = graph.flush_to_arena() {
                            log::warn!("arena flush failed: {e}");
                        }
                        // Update last_root so we don't re-snapshot our own flush.
                        if let Some(ref p) = control_path
                            && let Ok(ctrl) = Controller::open_or_create(p)
                        {
                            last_root = ctrl.current_root();
                        }
                    }
                    Err(e) => log::warn!("snapshot failed: {e}"),
                }
            }
            _ = poll_interval.tick(), if control_path.is_some() => {
                let ctrl_path = control_path
                    .as_ref()
                    .expect("select arm guard `control_path.is_some()` above");
                let current_root = match Controller::open_or_create(ctrl_path) {
                    Ok(c) => c.current_root(),
                    Err(_) => continue,
                };
                // Sentinel `[0u8; 32]` = no root yet. Skip until a real root publishes.
                if current_root != last_root && current_root != [0u8; 32] {
                    last_root = current_root;
                    log::info!("Σ root change detected ({}…), snapshotting", hex::encode(&current_root[..6]));
                    match jj.lock().commit_snapshot(graph.as_ref(), "root change") {
                        Ok(id) => log::info!("root-change snapshot {}", &id[..id.len().min(12)]),
                        Err(e) => log::warn!("root-change snapshot failed: {e}"),
                    }
                }
            }
        }
    }
}

fn format_commit_message(events: &[WriteEvent]) -> String {
    let mut created = 0usize;
    let mut changed = 0usize;
    let mut removed = 0usize;
    let mut renamed = 0usize;

    for e in events {
        match e {
            WriteEvent::ContentChanged(_) => changed += 1,
            WriteEvent::NodeCreated(_) => created += 1,
            WriteEvent::NodeRemoved(_) => removed += 1,
            WriteEvent::NodeRenamed { .. } => renamed += 1,
        }
    }

    let mut parts = Vec::new();
    if created > 0 {
        parts.push(format!("{created} created"));
    }
    if changed > 0 {
        parts.push(format!("{changed} changed"));
    }
    if removed > 0 {
        parts.push(format!("{removed} removed"));
    }
    if renamed > 0 {
        parts.push(format!("{renamed} renamed"));
    }

    if parts.is_empty() {
        "snapshot".to_string()
    } else {
        format!("snapshot: {}", parts.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leyline_fs::graph::MemoryGraph;
    #[cfg(feature = "sqlite")]
    use leyline_fs::graph::SqliteGraphAdapter;

    #[cfg(feature = "sqlite")]
    /// Create a writable SqliteGraphAdapter with an empty schema.
    fn writable_graph() -> SqliteGraphAdapter {
        use rusqlite::Connection;
        let source = Connection::open_in_memory().unwrap();
        source
            .execute_batch(
                "CREATE TABLE nodes (
                    id TEXT PRIMARY KEY,
                    parent_id TEXT,
                    name TEXT NOT NULL,
                    kind INTEGER NOT NULL,
                    size INTEGER DEFAULT 0,
                    mtime INTEGER NOT NULL,
                    record JSON
                );
                CREATE INDEX idx_parent_name ON nodes(parent_id, name);",
            )
            .unwrap();
        let data = source.serialize("main").unwrap();
        SqliteGraphAdapter::new_writable(data.as_ref()).unwrap()
    }

    #[test]
    fn is_ignored_filters_correctly() {
        assert!(is_ignored("target/debug/foo"));
        assert!(is_ignored(".git/config"));
        assert!(is_ignored("build/output"));
        assert!(is_ignored("app.log"));
        assert!(is_ignored("dir/.DS_Store"));
        assert!(is_ignored(".leyline/status"));

        assert!(!is_ignored("functions/main/source"));
        assert!(!is_ignored("src/main.go"));
        assert!(!is_ignored("docs/readme"));
    }

    #[test]
    fn format_commit_message_batches() {
        let events = vec![
            WriteEvent::ContentChanged("a".into()),
            WriteEvent::ContentChanged("b".into()),
            WriteEvent::NodeCreated("c".into()),
        ];
        let msg = format_commit_message(&events);
        assert!(msg.contains("1 created"));
        assert!(msg.contains("2 changed"));
    }

    #[test]
    fn jj_init_creates_repo() {
        let dir = tempfile::tempdir().unwrap();
        let _jj = JjIntegration::init(dir.path()).unwrap();
        assert!(dir.path().join(".jj").exists());

        // Can open again
        let _jj2 = JjIntegration::open(dir.path()).unwrap();

        // init_or_open detects existing
        let _jj3 = JjIntegration::init_or_open(dir.path()).unwrap();
    }

    #[test]
    fn snapshot_creates_commit() {
        let dir = tempfile::tempdir().unwrap();
        let jj = JjIntegration::init(dir.path()).unwrap();

        let mut g = MemoryGraph::new();
        g.add_node(
            Node {
                id: "hello.txt".into(),
                name: "hello.txt".into(),
                is_dir: false,
                size: 5,
                mtime_nanos: 0,
            },
            "",
            Some(b"hello".to_vec()),
        );

        let commit_id = jj.commit_snapshot(&g, "test snapshot").unwrap();
        assert!(!commit_id.is_empty());
        assert!(commit_id.len() >= 12);
    }

    #[test]
    fn snapshot_deduplicates() {
        let dir = tempfile::tempdir().unwrap();
        let jj = JjIntegration::init(dir.path()).unwrap();

        let mut g = MemoryGraph::new();
        g.add_node(
            Node {
                id: "f.txt".into(),
                name: "f.txt".into(),
                is_dir: false,
                size: 3,
                mtime_nanos: 0,
            },
            "",
            Some(b"abc".to_vec()),
        );

        let id1 = jj.commit_snapshot(&g, "snap 1").unwrap();
        let id2 = jj.commit_snapshot(&g, "snap 2").unwrap();

        // Same content should produce different commits (different descriptions)
        // but both should succeed
        assert_ne!(id1, id2);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn revert_restores_state() {
        let dir = tempfile::tempdir().unwrap();
        let jj = JjIntegration::init(dir.path()).unwrap();

        let g = writable_graph();
        g.create_node("", "hello.txt", false).unwrap();
        g.write_content("hello.txt", b"version 1", 0).unwrap();
        g.create_node("", "other.txt", false).unwrap();
        g.write_content("other.txt", b"keep me", 0).unwrap();

        let commit1 = jj.commit_snapshot(&g, "v1").unwrap();

        // Modify: change content and add a file
        g.truncate("hello.txt").unwrap();
        g.write_content("hello.txt", b"version 2", 0).unwrap();
        g.create_node("", "new.txt", false).unwrap();
        g.write_content("new.txt", b"added", 0).unwrap();

        let _commit2 = jj.commit_snapshot(&g, "v2").unwrap();

        // Verify current state has 3 files
        let children = g.list_children("").unwrap();
        assert_eq!(children.len(), 3);

        // Revert to commit1
        jj.revert_to_commit(&commit1, &g).unwrap();

        // Should be back to 2 files with original content
        let children = g.list_children("").unwrap();
        assert_eq!(
            children.len(),
            2,
            "expected 2 files after revert, got {}",
            children.len()
        );

        let mut buf = [0u8; 256];
        let n = g.read_content("hello.txt", &mut buf, 0).unwrap();
        assert_eq!(&buf[..n], b"version 1");

        let n = g.read_content("other.txt", &mut buf, 0).unwrap();
        assert_eq!(&buf[..n], b"keep me");

        // new.txt should be gone
        assert!(g.get_node("new.txt").unwrap().is_none());
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn log_json_returns_entries() {
        let dir = tempfile::tempdir().unwrap();
        let jj = JjIntegration::init(dir.path()).unwrap();

        let g = writable_graph();
        g.create_node("", "f.txt", false).unwrap();
        g.write_content("f.txt", b"data", 0).unwrap();

        jj.commit_snapshot(&g, "first commit").unwrap();
        jj.commit_snapshot(&g, "second commit").unwrap();

        let log = jj.log_json(10).unwrap();
        let entries: Vec<serde_json::Value> = serde_json::from_str(&log).unwrap();

        // jj creates an initial working-copy commit on init, so 3 total
        assert!(
            entries.len() >= 2,
            "expected at least 2 entries, got {}",
            entries.len()
        );
        assert!(
            entries
                .iter()
                .any(|e| e["description"].as_str().unwrap().contains("first"))
        );
        assert!(
            entries
                .iter()
                .any(|e| e["description"].as_str().unwrap().contains("second"))
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn revert_with_nested_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let jj = JjIntegration::init(dir.path()).unwrap();

        let g = writable_graph();
        g.create_node("", "src", true).unwrap();
        g.create_node("src", "main.go", false).unwrap();
        g.write_content("src/main.go", b"package main", 0).unwrap();

        let commit_id = jj.commit_snapshot(&g, "with dirs").unwrap();

        // Clear and revert
        g.remove_node("src").unwrap();
        assert!(g.list_children("").unwrap().is_empty());

        jj.revert_to_commit(&commit_id, &g).unwrap();

        // Verify directory structure restored
        let root = g.list_children("").unwrap();
        assert!(root.iter().any(|c| c.name == "src" && c.is_dir));

        let src_children = g.list_children("src").unwrap();
        assert!(src_children.iter().any(|c| c.name == "main.go"));

        let mut buf = [0u8; 256];
        let n = g.read_content("src/main.go", &mut buf, 0).unwrap();
        assert_eq!(&buf[..n], b"package main");
    }

    #[test]
    fn versioned_graph_injects_leyline_dir() {
        let mut g = MemoryGraph::new();
        g.add_node(
            Node {
                id: "file1".into(),
                name: "file1".into(),
                is_dir: false,
                size: 5,
                mtime_nanos: 0,
            },
            "",
            Some(b"hello".to_vec()),
        );

        let (tx, _rx) = mpsc::unbounded_channel();
        let jj_dir = tempfile::tempdir().unwrap();
        let jj = JjIntegration::init(jj_dir.path()).unwrap();

        let vg = VersionedGraph::new(Arc::new(g), tx, Arc::new(Mutex::new(jj))).unwrap();

        // Root listing includes .leyline
        let children = vg.list_children("").unwrap();
        assert!(children.iter().any(|c| c.name == ".leyline"));
        assert!(children.iter().any(|c| c.name == "file1"));

        // .leyline/ has virtual files
        let leyline_children = vg.list_children(".leyline").unwrap();
        assert_eq!(leyline_children.len(), 3);
        assert!(leyline_children.iter().any(|c| c.name == "status"));
        assert!(leyline_children.iter().any(|c| c.name == "log"));
        assert!(leyline_children.iter().any(|c| c.name == "revert"));

        // Can read status
        let mut buf = [0u8; 256];
        let n = vg.read_content(".leyline/status", &mut buf, 0).unwrap();
        let content = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(content.contains("ok"));

        // Lookup .leyline from root
        let node = vg.lookup_child("", ".leyline").unwrap().unwrap();
        assert!(node.is_dir);

        // Lookup virtual file
        let node = vg.lookup_child(".leyline", "status").unwrap().unwrap();
        assert!(!node.is_dir);
    }

    /// Helper: create a VersionedGraph wrapping a MemoryGraph with some files.
    fn test_versioned_graph() -> (VersionedGraph, tempfile::TempDir) {
        let mut g = MemoryGraph::new();
        g.add_node(
            Node {
                id: "docs".into(),
                name: "docs".into(),
                is_dir: true,
                size: 0,
                mtime_nanos: 1000,
            },
            "",
            None,
        );
        g.add_node(
            Node {
                id: "docs/readme".into(),
                name: "readme".into(),
                is_dir: false,
                size: 5,
                mtime_nanos: 2000,
            },
            "docs",
            Some(b"hello".to_vec()),
        );
        g.add_node(
            Node {
                id: "docs/notes".into(),
                name: "notes".into(),
                is_dir: false,
                size: 5,
                mtime_nanos: 3000,
            },
            "docs",
            Some(b"world".to_vec()),
        );

        let (tx, _rx) = mpsc::unbounded_channel();
        let jj_dir = tempfile::tempdir().unwrap();
        let jj = JjIntegration::init(jj_dir.path()).unwrap();

        let vg = VersionedGraph::new(Arc::new(g), tx, Arc::new(Mutex::new(jj))).unwrap();
        (vg, jj_dir)
    }

    #[test]
    fn staging_dir_appears_in_root() {
        let (vg, _jj_dir) = test_versioned_graph();

        let children = vg.list_children("").unwrap();
        assert!(children.iter().any(|c| c.name == ".staging"));
        assert!(children.iter().any(|c| c.name == ".leyline"));
        assert!(children.iter().any(|c| c.name == "docs"));
    }

    #[test]
    fn staging_get_node_returns_dir() {
        let (vg, _jj_dir) = test_versioned_graph();

        let node = vg.get_node(".staging").unwrap().unwrap();
        assert!(node.is_dir);
        assert_eq!(node.name, ".staging");
    }

    #[test]
    fn staging_lookup_from_root() {
        let (vg, _jj_dir) = test_versioned_graph();

        let node = vg.lookup_child("", ".staging").unwrap().unwrap();
        assert!(node.is_dir);
        assert_eq!(node.id, ".staging");
    }

    #[test]
    fn staging_mirrors_live_tree() {
        let (vg, _jj_dir) = test_versioned_graph();

        // .staging/ root should show live graph's children + control files
        let children = vg.list_children(".staging").unwrap();
        assert!(children.iter().any(|c| c.name == "docs"));
        assert!(children.iter().any(|c| c.name == ".dirty"));
        assert!(children.iter().any(|c| c.name == ".commit"));
        assert!(children.iter().any(|c| c.name == ".discard"));

        // .staging/docs should mirror live docs/
        let docs = vg.list_children(".staging/docs").unwrap();
        assert_eq!(docs.len(), 2);
        assert!(docs.iter().any(|c| c.name == "readme"));
        assert!(docs.iter().any(|c| c.name == "notes"));
    }

    #[test]
    fn staging_read_through_to_live() {
        let (vg, _jj_dir) = test_versioned_graph();

        let mut buf = [0u8; 64];
        let n = vg
            .read_content(".staging/docs/readme", &mut buf, 0)
            .unwrap();
        assert_eq!(&buf[..n], b"hello");
    }

    #[test]
    fn staging_write_shadows_live() {
        let (vg, _jj_dir) = test_versioned_graph();

        // Write to staging
        vg.write_content(".staging/docs/readme", b"staged", 0)
            .unwrap();

        // Staging returns modified content
        let mut buf = [0u8; 64];
        let n = vg
            .read_content(".staging/docs/readme", &mut buf, 0)
            .unwrap();
        assert_eq!(&buf[..n], b"staged");

        // Live is unchanged
        let n = vg.read_content("docs/readme", &mut buf, 0).unwrap();
        assert_eq!(&buf[..n], b"hello");
    }

    #[test]
    fn staging_dirty_control_file() {
        let (vg, _jj_dir) = test_versioned_graph();

        // Initially clean
        let mut buf = [0u8; 256];
        let n = vg.read_content(".staging/.dirty", &mut buf, 0).unwrap();
        let content = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(content.contains("(clean)"));

        // Stage an edit
        vg.write_content(".staging/docs/readme", b"staged", 0)
            .unwrap();

        let n = vg.read_content(".staging/.dirty", &mut buf, 0).unwrap();
        let content = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(content.contains("+docs/readme"));
    }

    #[test]
    fn staging_discard_clears_edits() {
        let (vg, _jj_dir) = test_versioned_graph();

        // Stage an edit
        vg.write_content(".staging/docs/readme", b"staged", 0)
            .unwrap();

        // Discard
        vg.write_content(".staging/.discard", b"1", 0).unwrap();

        // Back to live content
        let mut buf = [0u8; 64];
        let n = vg
            .read_content(".staging/docs/readme", &mut buf, 0)
            .unwrap();
        assert_eq!(&buf[..n], b"hello");

        // Dirty is clean again
        let n = vg.read_content(".staging/.dirty", &mut buf, 0).unwrap();
        let content = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(content.contains("(clean)"));
    }

    #[test]
    fn staging_remove_shows_tombstone_in_dirty() {
        let (vg, _jj_dir) = test_versioned_graph();

        vg.remove_node(".staging/docs/notes").unwrap();

        let mut buf = [0u8; 256];
        let n = vg.read_content(".staging/.dirty", &mut buf, 0).unwrap();
        let content = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(content.contains("-docs/notes"));

        // Tombstoned node not visible in staging listing
        let docs = vg.list_children(".staging/docs").unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].name, "readme");
    }

    #[test]
    fn staging_create_node() {
        let (vg, _jj_dir) = test_versioned_graph();

        let id = vg.create_node(".staging/docs", "new.txt", false).unwrap();
        assert_eq!(id, ".staging/docs/new.txt");

        let node = vg.get_node(".staging/docs/new.txt").unwrap().unwrap();
        assert_eq!(node.name, "new.txt");
        assert!(!node.is_dir);

        // Visible in staging listing
        let docs = vg.list_children(".staging/docs").unwrap();
        assert_eq!(docs.len(), 3);
    }

    #[test]
    fn staging_lookup_control_files() {
        let (vg, _jj_dir) = test_versioned_graph();

        let dirty = vg.lookup_child(".staging", ".dirty").unwrap().unwrap();
        assert_eq!(dirty.id, ".staging/.dirty");
        assert!(!dirty.is_dir);

        let commit = vg.lookup_child(".staging", ".commit").unwrap().unwrap();
        assert_eq!(commit.id, ".staging/.commit");

        let discard = vg.lookup_child(".staging", ".discard").unwrap().unwrap();
        assert_eq!(discard.id, ".staging/.discard");
    }

    #[test]
    fn staging_get_node_control_files() {
        let (vg, _jj_dir) = test_versioned_graph();

        let dirty = vg.get_node(".staging/.dirty").unwrap().unwrap();
        assert_eq!(dirty.name, ".dirty");
        assert!(!dirty.is_dir);

        let commit = vg.get_node(".staging/.commit").unwrap().unwrap();
        assert_eq!(commit.name, ".commit");
    }

    // -----------------------------------------------------------------------
    // .dex/ virtual directory tests
    // -----------------------------------------------------------------------

    #[test]
    fn dex_dir_appears_in_root() {
        let (vg, _jj_dir) = test_versioned_graph();
        let children = vg.list_children("").unwrap();
        assert!(children.iter().any(|c| c.name == ".dex"));
    }

    #[test]
    fn dex_get_node_returns_dir() {
        let (vg, _jj_dir) = test_versioned_graph();
        let node = vg.get_node(".dex").unwrap().unwrap();
        assert!(node.is_dir);
        assert_eq!(node.name, ".dex");
    }

    #[test]
    fn dex_lookup_from_root() {
        let (vg, _jj_dir) = test_versioned_graph();
        let node = vg.lookup_child("", ".dex").unwrap().unwrap();
        assert!(node.is_dir);
        assert_eq!(node.id, ".dex");
    }

    #[test]
    fn dex_lists_virtual_files() {
        let (vg, _jj_dir) = test_versioned_graph();
        let children = vg.list_children(".dex").unwrap();
        assert_eq!(children.len(), 3);
        assert!(children.iter().any(|c| c.name == "tasks"));
        assert!(children.iter().any(|c| c.name == "current"));
        assert!(children.iter().any(|c| c.name == "complete"));
    }

    #[test]
    fn dex_lookup_virtual_files() {
        let (vg, _jj_dir) = test_versioned_graph();
        let tasks = vg.lookup_child(".dex", "tasks").unwrap().unwrap();
        assert_eq!(tasks.id, ".dex/tasks");
        assert!(!tasks.is_dir);

        let current = vg.lookup_child(".dex", "current").unwrap().unwrap();
        assert_eq!(current.id, ".dex/current");

        // Unknown file returns None
        assert!(vg.lookup_child(".dex", "unknown").unwrap().is_none());
    }

    #[test]
    fn dex_get_node_virtual_files() {
        let (vg, _jj_dir) = test_versioned_graph();
        let tasks = vg.get_node(".dex/tasks").unwrap().unwrap();
        assert_eq!(tasks.name, "tasks");
        assert!(!tasks.is_dir);

        // Unknown returns None
        assert!(vg.get_node(".dex/unknown").unwrap().is_none());
    }

    #[test]
    fn dex_create_and_read_task() {
        let (vg, _jj_dir) = test_versioned_graph();

        // Initially empty
        let mut buf = [0u8; 512];
        let n = vg.read_content(".dex/tasks", &mut buf, 0).unwrap();
        assert_eq!(n, 0); // empty JSONL

        // Create a task
        vg.write_content(".dex/tasks", b"fix the login bug", 0)
            .unwrap();

        // Read tasks — should have one JSONL line
        let n = vg.read_content(".dex/tasks", &mut buf, 0).unwrap();
        let content = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(content.contains("fix the login bug"));
        assert!(content.contains("dex-1"));
        assert!(content.contains("pending"));
    }

    #[test]
    fn dex_current_shows_active_task() {
        let (vg, _jj_dir) = test_versioned_graph();

        // No tasks — empty object
        let mut buf = [0u8; 1024];
        let n = vg.read_content(".dex/current", &mut buf, 0).unwrap();
        let content = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(content.contains("{}"));

        // Create and start a task
        vg.write_content(".dex/tasks", b"refactor auth", 0).unwrap();
        vg.write_content(".dex/current", b"dex-1", 0).unwrap();

        let n = vg.read_content(".dex/current", &mut buf, 0).unwrap();
        let content = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(content.contains("in_progress"));
        assert!(content.contains("refactor auth"));
    }

    #[test]
    fn dex_complete_snapshots_staged_nodes() {
        let (vg, _jj_dir) = test_versioned_graph();

        // Stage an edit
        vg.write_content(".staging/docs/readme", b"edited", 0)
            .unwrap();

        // Create and start task
        vg.write_content(".dex/tasks", b"update readme", 0).unwrap();
        vg.write_content(".dex/current", b"dex-1", 0).unwrap();

        // Complete fails (MemoryGraph doesn't support batch_splice),
        // but staged nodes were captured into the task before the attempt
        let result = vg.write_content(".dex/complete", b"dex-1", 0);
        assert!(
            result.is_err(),
            "expected batch_splice error from MemoryGraph"
        );

        // Task stays in_progress (commit failed before status update)
        let mut buf = [0u8; 1024];
        let n = vg.read_content(".dex/tasks", &mut buf, 0).unwrap();
        let content = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(content.contains("in_progress"));
        // Staged nodes were captured
        assert!(content.contains("docs/readme"));
    }

    #[test]
    fn dex_empty_description_rejected() {
        let (vg, _jj_dir) = test_versioned_graph();
        let result = vg.write_content(".dex/tasks", b"  ", 0);
        assert!(result.is_err());
    }

    #[test]
    fn dex_complete_nonexistent_task_rejected() {
        let (vg, _jj_dir) = test_versioned_graph();
        let result = vg.write_content(".dex/complete", b"dex-999", 0);
        assert!(result.is_err());
    }
}
