use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    collections::{hash_map::DefaultHasher, BTreeMap, HashMap, HashSet},
    env, fs,
    hash::{Hash, Hasher},
    io,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicU64, Ordering},
        Condvar, LazyLock, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    Emitter, Manager, WebviewUrl, WebviewWindowBuilder,
};

const SKILL_FILE_NAMES: &[&str] = &["SKILL.md", "skill.md"];
const DEEPSEEK_API_URL: &str = "https://api.deepseek.com/chat/completions";
const GITHUB_LATEST_RELEASE_API_URL: &str =
    "https://api.github.com/repos/zuozizhen/skills-box/releases/latest";
const SNAPSHOT_CACHE_TTL_MS: i64 = 60_000;
const AI_SOURCE_MARKDOWN_MAX_CHARS: usize = 4_000;
const AI_RESPONSE_MAX_TOKENS: usize = 1500;
const AI_REQUEST_TIMEOUT_SECS: u64 = 60;
const AI_CONNECT_TEST_TIMEOUT_SECS: u64 = 10;
const AI_CONNECT_TEST_MAX_TOKENS: usize = 8;
const AI_PROFILE_MAX_RETRY_ATTEMPTS: usize = 3;
const AI_PROFILE_RETRY_BASE_DELAY_MS: u64 = 1_200;
const WATCHER_REFRESH_MIN_INTERVAL_MS: u64 = 1_000;
const WATCHER_RESTART_BACKOFF_MS: u64 = 1_200;
const TRAY_FAVORITE_SUMMARY_MAX_CHARS: usize = 20;
const SKILLS_CLI_TIMEOUT_SECS: u64 = 8;
const UPDATE_CHECK_CACHE_TTL_MS: i64 = 10 * 60 * 1000;
const AI_SUMMARY_MAX_CONCURRENCY: usize = 10;
const AI_SUMMARY_QUEUE_WAIT_POLL_MS: u64 = 320;

#[derive(Debug, Clone)]
struct CachedSnapshot {
    cached_at: i64,
    snapshot: SkillsSnapshot,
}

#[derive(Debug, Clone)]
struct CachedUpdateCheck {
    checked_at: i64,
    result: UpdateCheckResult,
}

static SNAPSHOT_CACHE: LazyLock<Mutex<Option<CachedSnapshot>>> = LazyLock::new(|| Mutex::new(None));
static AI_SUMMARY_JOB_QUEUE: LazyLock<(Mutex<AiSummaryQueueState>, Condvar)> =
    LazyLock::new(|| (Mutex::new(AiSummaryQueueState::default()), Condvar::new()));
static OVERRIDES_FILE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static APP_CONFIG_FILE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static RESUMMARIZE_ALL_PROGRESS: LazyLock<Mutex<Option<ScanProgressPayload>>> =
    LazyLock::new(|| Mutex::new(None));
static AI_SUMMARY_STREAM_PROGRESS: LazyLock<Mutex<Option<AiSummaryStreamPayload>>> =
    LazyLock::new(|| Mutex::new(None));
static UPDATE_CHECK_CACHE: LazyLock<Mutex<Option<CachedUpdateCheck>>> =
    LazyLock::new(|| Mutex::new(None));
static TRAY_MENU_UPDATE_STATE: LazyLock<Mutex<TrayMenuUpdateState>> =
    LazyLock::new(|| Mutex::new(TrayMenuUpdateState::default()));
static AI_SUMMARY_CANCEL_EPOCH: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Default)]
struct AiSummaryQueueState {
    active_jobs: usize,
}

struct AiSummaryQueueGuard;

#[derive(Debug, Default)]
struct TrayMenuUpdateState {
    pending: bool,
    latest_snapshot: Option<SkillsSnapshot>,
}

impl Drop for AiSummaryQueueGuard {
    fn drop(&mut self) {
        let (state_lock, wait_cv) = &*AI_SUMMARY_JOB_QUEUE;
        let mut state = state_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.active_jobs = state.active_jobs.saturating_sub(1);
        wait_cv.notify_one();
    }
}

fn lock_ai_summary_job_queue() -> AiSummaryQueueGuard {
    let (state_lock, wait_cv) = &*AI_SUMMARY_JOB_QUEUE;
    let mut state = state_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    while state.active_jobs >= AI_SUMMARY_MAX_CONCURRENCY {
        state = wait_cv
            .wait(state)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
    state.active_jobs += 1;
    AiSummaryQueueGuard
}

fn try_lock_ai_summary_job_queue(cancel_epoch: Option<u64>) -> Result<AiSummaryQueueGuard, String> {
    let (state_lock, wait_cv) = &*AI_SUMMARY_JOB_QUEUE;
    let mut state = state_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    while state.active_jobs >= AI_SUMMARY_MAX_CONCURRENCY {
        ensure_ai_summary_not_cancelled(cancel_epoch)?;
        let waited = wait_cv
            .wait_timeout(state, Duration::from_millis(AI_SUMMARY_QUEUE_WAIT_POLL_MS))
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state = waited.0;
    }
    ensure_ai_summary_not_cancelled(cancel_epoch)?;
    state.active_jobs += 1;
    Ok(AiSummaryQueueGuard)
}

fn current_ai_summary_cancel_epoch() -> u64 {
    AI_SUMMARY_CANCEL_EPOCH.load(Ordering::Relaxed)
}

fn bump_ai_summary_cancel_epoch() -> u64 {
    AI_SUMMARY_CANCEL_EPOCH
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1)
}

fn is_ai_summary_cancelled(cancel_epoch: u64) -> bool {
    current_ai_summary_cancel_epoch() != cancel_epoch
}

fn ensure_ai_summary_not_cancelled(cancel_epoch: Option<u64>) -> Result<(), String> {
    if let Some(epoch) = cancel_epoch {
        if is_ai_summary_cancelled(epoch) {
            return Err("AI 总结任务已停止".to_string());
        }
    }
    Ok(())
}

fn is_ai_summary_cancel_error(message: &str) -> bool {
    message.contains("AI 总结任务已停止")
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
enum SkillStatus {
    #[default]
    Active,
    Update,
    Draft,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SkillData {
    id: String,
    name: String,
    source_name: String,
    source_usage: String,
    source_description: String,
    source_markdown: String,
    source_commands: Vec<String>,
    ai_brief: String,
    ai_detail: String,
    favorite: bool,
    status: SkillStatus,
    path: String,
    definition_path: String,
    installed_at: Option<i64>,
    first_seen_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PlatformData {
    id: String,
    name: String,
    root: String,
    skills: Vec<SkillData>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SkillsSnapshot {
    scanned_at: i64,
    ai_summarized_count: usize,
    ai_pending_count: usize,
    platforms: Vec<PlatformData>,
}

impl SkillsSnapshot {
    fn total_skills(&self) -> usize {
        self.platforms
            .iter()
            .map(|platform| platform.skills.len())
            .sum()
    }

    fn ai_summarized_count(&self) -> usize {
        self.platforms
            .iter()
            .flat_map(|platform| platform.skills.iter())
            .filter(|skill| !skill.ai_brief.trim().is_empty() && !skill.ai_detail.trim().is_empty())
            .count()
    }

    fn ai_pending_count(&self) -> usize {
        self.total_skills()
            .saturating_sub(self.ai_summarized_count())
    }
}

#[derive(Debug, Clone)]
struct PlatformSource {
    id: String,
    name: String,
    root: String,
    include_hidden: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct OverridesStore {
    #[serde(default)]
    entries: BTreeMap<String, SkillOverride>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppConfig {
    #[serde(default)]
    deepseek_api_key: Option<String>,
    #[serde(default)]
    onboarding_completed: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct SkillOverride {
    #[serde(default)]
    status: Option<SkillStatus>,
    #[serde(default)]
    ai_brief: Option<String>,
    #[serde(default)]
    ai_detail: Option<String>,
    #[serde(default)]
    favorite: Option<bool>,
    #[serde(default)]
    first_seen_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateSkillPayload {
    platform_id: String,
    skill_id: String,
    #[serde(default)]
    status: Option<SkillStatus>,
    #[serde(default)]
    favorite: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResummarizeSkillPayload {
    platform_id: String,
    skill_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AiSettingsStatus {
    has_key: bool,
    masked_key: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateCheckResult {
    current_version: String,
    latest_version: String,
    has_update: bool,
    release_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GithubLatestRelease {
    tag_name: String,
    html_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct AiSkillProfile {
    brief: String,
    detail: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScanProgressPayload {
    stage: String,
    message: String,
    new_skills_count: usize,
    summarized_count: usize,
    summarize_total: usize,
    current_skill: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AiSummaryStreamPayload {
    platform_id: String,
    skill_id: String,
    detail_markdown: String,
    done: bool,
}

#[derive(Debug, Clone)]
struct ResolvedDeepSeekKey {
    value: String,
    source: &'static str,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SkillsCliEntry {
    #[serde(default)]
    name: String,
    path: String,
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

fn resolve_home_path(input: &str) -> PathBuf {
    if let Some(rest) = input.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(input)
}

fn overrides_file_path() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".skillsbox").join("overrides.json"))
}

fn app_config_file_path() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".skillsbox").join("config.json"))
}

fn app_config_exists() -> bool {
    app_config_file_path().is_some_and(|path| path.exists())
}

fn normalize_version_value(input: &str) -> String {
    input.trim().trim_start_matches('v').trim().to_string()
}

fn override_key(platform_id: &str, skill_id: &str) -> String {
    format!("{platform_id}::{skill_id}")
}

fn write_text_atomic(path: &Path, contents: &str) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("data");
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let temp_path = parent.join(format!(".{file_name}.tmp-{}-{stamp}", std::process::id()));
    fs::write(&temp_path, contents)?;
    if let Err(err) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }
    Ok(())
}

fn load_overrides() -> OverridesStore {
    let _guard = OVERRIDES_FILE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(path) = overrides_file_path() else {
        return OverridesStore::default();
    };
    let Ok(contents) = fs::read_to_string(path) else {
        return OverridesStore::default();
    };
    serde_json::from_str(&contents).unwrap_or_default()
}

fn save_overrides(store: &OverridesStore) -> io::Result<()> {
    let _guard = OVERRIDES_FILE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(path) = overrides_file_path() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "HOME is not available for persistence",
        ));
    };
    let contents = serde_json::to_string_pretty(store)
        .map_err(|err| io::Error::other(format!("serialize overrides failed: {err}")))?;
    write_text_atomic(&path, &contents)
}

fn upsert_ai_override_entry(
    platform_id: &str,
    skill_id: &str,
    ai_brief: &str,
    ai_detail: &str,
) -> io::Result<()> {
    let _guard = OVERRIDES_FILE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(path) = overrides_file_path() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "HOME is not available for persistence",
        ));
    };

    let mut store = fs::read_to_string(&path)
        .ok()
        .and_then(|contents| serde_json::from_str::<OverridesStore>(&contents).ok())
        .unwrap_or_default();

    let key = override_key(platform_id, skill_id);
    let entry = store.entries.entry(key).or_default();
    entry.ai_brief = Some(ai_brief.to_string());
    entry.ai_detail = Some(ai_detail.to_string());

    let contents = serde_json::to_string_pretty(&store)
        .map_err(|err| io::Error::other(format!("serialize overrides failed: {err}")))?;
    write_text_atomic(&path, &contents)
}

fn load_app_config() -> AppConfig {
    let _guard = APP_CONFIG_FILE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(path) = app_config_file_path() else {
        return AppConfig::default();
    };
    let Ok(contents) = fs::read_to_string(path) else {
        return AppConfig::default();
    };
    serde_json::from_str(&contents).unwrap_or_default()
}

fn save_app_config(config: &AppConfig) -> io::Result<()> {
    let _guard = APP_CONFIG_FILE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(path) = app_config_file_path() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "HOME is not available for persistence",
        ));
    };
    let contents = serde_json::to_string_pretty(config)
        .map_err(|err| io::Error::other(format!("serialize app config failed: {err}")))?;
    write_text_atomic(&path, &contents)
}

fn is_hidden(name: &str) -> bool {
    name.starts_with('.')
}

fn file_stem_str(path: &Path) -> String {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("unknown")
        .to_string()
}

fn normalize_spaces(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn system_time_to_unix_millis(value: SystemTime) -> Option<i64> {
    let duration = value.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(duration.as_millis()).ok()
}

fn metadata_timestamp(metadata: &fs::Metadata) -> Option<SystemTime> {
    metadata.created().ok().or_else(|| metadata.modified().ok())
}

fn detect_installed_at(path: &Path) -> Option<i64> {
    let metadata = fs::metadata(path).ok()?;
    let timestamp = metadata_timestamp(&metadata)?;
    system_time_to_unix_millis(timestamp)
}

fn sanitize_id(input: &str) -> String {
    let mut id = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            id.push(ch.to_ascii_lowercase());
        } else {
            id.push('_');
        }
    }
    let id = id.trim_matches('_');
    if id.is_empty() {
        "skill".to_string()
    } else {
        id.to_string()
    }
}

const PROJECT_SKILLS_REL_DIRS: &[&str] = &[
    "skills",
    "skills/.curated",
    "skills/.experimental",
    "skills/.system",
    ".agent/skills",
    ".agents/skills",
    ".augment/skills",
    ".claude/skills",
    ".cline/skills",
    ".codebuddy/skills",
    ".codex/skills",
    ".commandcode/skills",
    ".continue/skills",
    ".cortex/skills",
    ".crush/skills",
    ".factory/skills",
    ".github/skills",
    ".goose/skills",
    ".iflow/skills",
    ".junie/skills",
    ".kilocode/skills",
    ".kiro/skills",
    ".kode/skills",
    ".mcpjam/skills",
    ".mux/skills",
    ".neovate/skills",
    ".opencode/skills",
    ".openhands/skills",
    ".pi/skills",
    ".pochi/skills",
    ".qoder/skills",
    ".qwen/skills",
    ".roo/skills",
    ".trae/skills",
    ".vibe/skills",
    ".windsurf/skills",
    ".zencoder/skills",
    ".adal/skills",
];

fn config_home_dir() -> PathBuf {
    if let Some(value) = env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(value);
    }
    home_dir()
        .map(|home| home.join(".config"))
        .unwrap_or_else(|| PathBuf::from(".config"))
}

fn codex_home_dir() -> PathBuf {
    if let Some(value) = env::var_os("CODEX_HOME") {
        if !value.is_empty() {
            return PathBuf::from(value);
        }
    }
    home_dir()
        .map(|home| home.join(".codex"))
        .unwrap_or_else(|| PathBuf::from(".codex"))
}

fn claude_config_dir() -> PathBuf {
    if let Some(value) = env::var_os("CLAUDE_CONFIG_DIR") {
        if !value.is_empty() {
            return PathBuf::from(value);
        }
    }
    home_dir()
        .map(|home| home.join(".claude"))
        .unwrap_or_else(|| PathBuf::from(".claude"))
}

fn detect_project_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(cwd) = env::current_dir() {
        roots.push(cwd.clone());
        if cwd
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "src-tauri")
        {
            if let Some(parent) = cwd.parent() {
                roots.push(parent.to_path_buf());
            }
        }
    }

    let mut seen = HashSet::new();
    roots
        .into_iter()
        .filter(|path| seen.insert(path.to_string_lossy().to_lowercase()))
        .collect()
}

fn add_source(
    sources: &mut Vec<PlatformSource>,
    seen_paths: &mut HashSet<String>,
    id: impl Into<String>,
    name: impl Into<String>,
    root: PathBuf,
    include_hidden: bool,
) {
    let root_str = root.to_string_lossy().to_string();
    if root_str.trim().is_empty() {
        return;
    }
    let normalized = root_str.to_lowercase();
    if !seen_paths.insert(normalized) {
        return;
    }
    sources.push(PlatformSource {
        id: id.into(),
        name: name.into(),
        root: root_str,
        include_hidden,
    });
}

fn build_platform_sources() -> Vec<PlatformSource> {
    let home = home_dir().unwrap_or_else(|| PathBuf::from("."));
    let config_home = config_home_dir();
    let codex_home = codex_home_dir();
    let claude_home = claude_config_dir();
    let mut sources = Vec::new();
    let mut seen_paths = HashSet::new();

    add_source(
        &mut sources,
        &mut seen_paths,
        "claude_global",
        "Claude (Global)",
        claude_home.join("skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "codex_global",
        "Codex (Global)",
        codex_home.join("skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "codex_system",
        "Codex System",
        codex_home.join("skills").join(".system"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "cursor_global",
        "Cursor (Global)",
        home.join(".cursor/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "cursor_legacy",
        "Cursor (Legacy)",
        home.join(".cursor/skills-cursor"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "global_agents_config",
        "Universal Agents (Global)",
        config_home.join("agents/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "global_agents_home",
        "Agents Home (Global)",
        home.join(".agents/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "openclaw_global",
        "OpenClaw (Global)",
        home.join(".openclaw/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "clawdbot_global",
        "ClawdBot (Global)",
        home.join(".clawdbot/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "moltbot_global",
        "MoltBot (Global)",
        home.join(".moltbot/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "amp_global",
        "Amp (Global)",
        config_home.join("agents/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "antigravity_global",
        "Antigravity (Global)",
        home.join(".gemini/antigravity/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "augment_global",
        "Augment (Global)",
        home.join(".augment/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "codebuddy_global",
        "CodeBuddy (Global)",
        home.join(".codebuddy/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "commandcode_global",
        "Command Code (Global)",
        home.join(".commandcode/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "continue_global",
        "Continue (Global)",
        home.join(".continue/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "cortex_global",
        "Cortex (Global)",
        home.join(".snowflake/cortex/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "crush_global",
        "Crush (Global)",
        config_home.join("crush/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "droid_global",
        "Droid (Global)",
        home.join(".factory/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "gemini_global",
        "Gemini CLI (Global)",
        home.join(".gemini/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "copilot_global",
        "GitHub Copilot (Global)",
        home.join(".copilot/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "goose_global",
        "Goose (Global)",
        config_home.join("goose/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "junie_global",
        "Junie (Global)",
        home.join(".junie/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "iflow_global",
        "iFlow (Global)",
        home.join(".iflow/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "kilo_global",
        "Kilo (Global)",
        home.join(".kilocode/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "kiro_global",
        "Kiro (Global)",
        home.join(".kiro/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "kode_global",
        "Kode (Global)",
        home.join(".kode/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "mcpjam_global",
        "MCPJam (Global)",
        home.join(".mcpjam/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "vibe_global",
        "Mistral Vibe (Global)",
        home.join(".vibe/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "mux_global",
        "Mux (Global)",
        home.join(".mux/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "opencode_global",
        "OpenCode (Global)",
        config_home.join("opencode/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "openhands_global",
        "OpenHands (Global)",
        home.join(".openhands/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "pi_global",
        "Pi (Global)",
        home.join(".pi/agent/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "qoder_global",
        "Qoder (Global)",
        home.join(".qoder/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "qwen_global",
        "Qwen (Global)",
        home.join(".qwen/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "roo_global",
        "Roo (Global)",
        home.join(".roo/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "trae_global",
        "Trae (Global)",
        home.join(".trae/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "traecn_global",
        "Trae CN (Global)",
        home.join(".trae-cn/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "windsurf_global",
        "Windsurf (Global)",
        home.join(".codeium/windsurf/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "zencoder_global",
        "Zencoder (Global)",
        home.join(".zencoder/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "neovate_global",
        "Neovate (Global)",
        home.join(".neovate/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "pochi_global",
        "Pochi (Global)",
        home.join(".pochi/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "adal_global",
        "AdaL (Global)",
        home.join(".adal/skills"),
        false,
    );
    add_source(
        &mut sources,
        &mut seen_paths,
        "local_prompts",
        "Local Prompts",
        home.join("prompts"),
        false,
    );

    for project_root in detect_project_roots() {
        for rel in PROJECT_SKILLS_REL_DIRS {
            add_source(
                &mut sources,
                &mut seen_paths,
                format!("project_{}", sanitize_id(rel)),
                format!("Project: {rel}"),
                project_root.join(rel),
                false,
            );
        }
    }

    sources
}

fn humanize_identifier(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut capitalize_next = true;
    for ch in input.chars() {
        if matches!(ch, '-' | '_' | '.') {
            output.push(' ');
            capitalize_next = true;
            continue;
        }
        if capitalize_next {
            output.extend(ch.to_uppercase());
            capitalize_next = false;
        } else {
            output.push(ch);
        }
    }
    normalize_spaces(&output)
}

fn take_chars(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    input.chars().take(max_chars).collect()
}

fn strip_frontmatter_markdown(contents: &str) -> String {
    let normalized = contents.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = normalized.lines().collect();
    if lines.is_empty() {
        return String::new();
    }
    if lines[0].trim() != "---" {
        return normalized.trim().to_string();
    }

    for (index, line) in lines.iter().enumerate().skip(1) {
        if line.trim() != "---" {
            continue;
        }
        let body = lines
            .iter()
            .skip(index + 1)
            .copied()
            .collect::<Vec<_>>()
            .join("\n");
        return body.trim().to_string();
    }

    normalized.trim().to_string()
}

fn parse_frontmatter_description(contents: &str) -> Option<String> {
    let mut lines = contents.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }

    let mut collecting_block = false;
    let mut block_lines: Vec<String> = Vec::new();

    for line in lines {
        let trimmed = line.trim_end();
        let trimmed_start = trimmed.trim_start();
        if trimmed_start == "---" {
            break;
        }

        if collecting_block {
            if line.starts_with(' ') || line.starts_with('\t') {
                let value = trimmed_start.trim_matches('"').trim_matches('\'');
                if !value.is_empty() {
                    block_lines.push(value.to_string());
                }
                continue;
            }
            break;
        }

        if let Some(rest) = trimmed_start.strip_prefix("description:") {
            let value = rest.trim().trim_matches('"').trim_matches('\'').trim();
            if matches!(value, ">" | "|") {
                collecting_block = true;
                continue;
            }
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }

    if block_lines.is_empty() {
        None
    } else {
        Some(normalize_spaces(&block_lines.join(" ")))
    }
}

fn parse_first_paragraph(contents: &str) -> Option<String> {
    let mut in_frontmatter = false;
    let mut frontmatter_closed = false;
    let mut in_code = false;
    let mut paragraph = Vec::new();

    for (index, raw_line) in contents.lines().enumerate() {
        let line = raw_line.trim();
        if index == 0 && line == "---" {
            in_frontmatter = true;
            continue;
        }
        if in_frontmatter {
            if line == "---" {
                in_frontmatter = false;
                frontmatter_closed = true;
            }
            continue;
        }

        if !frontmatter_closed && line == "---" && paragraph.is_empty() {
            continue;
        }

        if line.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        if line.is_empty() {
            if !paragraph.is_empty() {
                break;
            }
            continue;
        }
        paragraph.push(line.to_string());
    }

    if paragraph.is_empty() {
        None
    } else {
        Some(normalize_spaces(&paragraph.join(" ")))
    }
}

fn looks_like_command(input: &str) -> bool {
    let first = input.split_whitespace().next().unwrap_or_default();
    matches!(
        first,
        "python"
            | "python3"
            | "node"
            | "npm"
            | "npx"
            | "pnpm"
            | "yarn"
            | "uv"
            | "bash"
            | "sh"
            | "skills"
            | "codex"
            | "claude"
            | "git"
            | "cargo"
            | "go"
            | "curl"
    )
}

fn extract_command_lines(contents: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let mut seen = HashSet::new();
    let mut in_code = false;

    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if !in_code {
            continue;
        }

        let mut candidate = line.trim_start_matches('$').trim().to_string();
        if candidate.is_empty() {
            continue;
        }

        if candidate.starts_with('#') {
            continue;
        }

        if candidate.starts_with("./") {
            candidate = format!("bash {}", candidate);
        }

        if !looks_like_command(&candidate) {
            continue;
        }

        let normalized = normalize_spaces(&candidate);
        if seen.insert(normalized.clone()) {
            commands.push(normalized);
        }
    }

    commands
}

fn build_source_usage(description: &str, commands: &[String], fallback_id: &str) -> String {
    let mut parts = Vec::new();
    if !description.trim().is_empty() {
        parts.push(description.trim().to_string());
    }
    if !commands.is_empty() {
        let mut commands_part = String::from("Commands: ");
        for (index, command) in commands.iter().take(3).enumerate() {
            if index > 0 {
                commands_part.push_str(" ; ");
            }
            commands_part.push_str(command);
        }
        parts.push(commands_part);
    }

    if parts.is_empty() {
        format!(
            "Use {} by reading SKILL.md and following its usage section.",
            fallback_id
        )
    } else {
        parts.join(" ")
    }
}

fn parse_json_array_from_text<T: DeserializeOwned>(text: &str) -> Vec<T> {
    let trimmed = text.trim();
    if let Ok(items) = serde_json::from_str::<Vec<T>>(trimmed) {
        return items;
    }

    let Some(start) = trimmed.find('[') else {
        return vec![];
    };
    let Some(end) = trimmed.rfind(']') else {
        return vec![];
    };
    if end < start {
        return vec![];
    }

    serde_json::from_str::<Vec<T>>(&trimmed[start..=end]).unwrap_or_default()
}

fn resolve_deepseek_api_key() -> Option<ResolvedDeepSeekKey> {
    let config = load_app_config();
    if let Some(value) = config.deepseek_api_key {
        let value = value.trim().to_string();
        if !value.is_empty() {
            return Some(ResolvedDeepSeekKey {
                value,
                source: "config",
            });
        }
    }

    if let Some(value) = env::var_os("DEEPSEEK_API_KEY") {
        let value = value.to_string_lossy().trim().to_string();
        if !value.is_empty() {
            return Some(ResolvedDeepSeekKey {
                value,
                source: "env",
            });
        }
    }
    None
}

fn deepseek_api_key() -> Option<String> {
    resolve_deepseek_api_key().map(|resolved| resolved.value)
}

fn key_tail(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let len = chars.len();
    let start = len.saturating_sub(4);
    chars[start..].iter().collect()
}

fn masked_key(value: &str) -> Option<String> {
    let key = value.trim();
    if key.is_empty() {
        return None;
    }

    let chars: Vec<char> = key.chars().collect();
    let len = chars.len();
    if len == 1 {
        return Some("·".to_string());
    }
    if len == 2 {
        let prefix: String = chars[..1].iter().collect();
        return Some(format!("{prefix}·"));
    }

    let mut prefix_len = len.min(6);
    let mut suffix_len = len.saturating_sub(prefix_len).min(4);
    if prefix_len + suffix_len >= len {
        if suffix_len > 0 {
            suffix_len -= 1;
        } else if prefix_len > 1 {
            prefix_len -= 1;
        }
    }
    let hidden_len = len.saturating_sub(prefix_len + suffix_len).max(1);

    let prefix: String = chars[..prefix_len].iter().collect();
    let suffix: String = if suffix_len > 0 {
        chars[len - suffix_len..].iter().collect()
    } else {
        String::new()
    };
    Some(format!("{prefix}{}{suffix}", "·".repeat(hidden_len)))
}

fn deepseek_api_key_mask() -> Option<String> {
    resolve_deepseek_api_key().and_then(|resolved| masked_key(&resolved.value))
}

fn trim_ai_brief(input: &str) -> String {
    normalize_spaces(input).trim().to_string()
}

fn unwrap_markdown_fence(input: &str) -> String {
    let trimmed = input.trim();
    if !trimmed.starts_with("```") {
        return trimmed.to_string();
    }

    let mut lines = trimmed.lines();
    let Some(first_line) = lines.next() else {
        return String::new();
    };
    if !first_line.trim_start().starts_with("```") {
        return trimmed.to_string();
    }

    let mut body: Vec<&str> = lines.collect();
    if body.last().is_some_and(|line| line.trim() == "```") {
        body.pop();
    }
    body.join("\n").trim().to_string()
}

fn trim_ai_markdown(input: &str) -> String {
    let unfenced = unwrap_markdown_fence(input);
    let normalized = unfenced
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .lines()
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();

    remove_markdown_section_by_h2(
        &normalized,
        &["注意事项与边界", "注意事项与限制", "注意事项", "边界"],
    )
}

fn remove_markdown_section_by_h2(input: &str, headings: &[&str]) -> String {
    if input.trim().is_empty() {
        return String::new();
    }

    let mut output: Vec<String> = Vec::new();
    let mut skipping = false;

    for line in input.lines() {
        let trimmed = line.trim();
        let lower = trimmed.to_lowercase();
        let is_h2 = lower.starts_with("## ");
        if is_h2 {
            let title = trimmed.trim_start_matches('#').trim();
            let should_skip = headings
                .iter()
                .any(|candidate| title.starts_with(candidate));
            if should_skip {
                skipping = true;
                continue;
            }
            skipping = false;
        }
        if skipping {
            continue;
        }
        output.push(line.to_string());
    }

    output.join("\n").trim().to_string()
}

fn parse_ai_profile_text(text: &str) -> Option<AiSkillProfile> {
    let trimmed = text.trim();

    if let Ok(profile) = serde_json::from_str::<AiSkillProfile>(trimmed) {
        return Some(profile);
    }

    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    if end <= start {
        return None;
    }

    serde_json::from_str::<AiSkillProfile>(&trimmed[start..=end]).ok()
}

fn extract_deepseek_error_message(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;

    if let Some(message) = value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(|message| message.as_str())
    {
        let message = message.trim();
        if !message.is_empty() {
            return Some(message.to_string());
        }
    }

    if let Some(message) = value.get("message").and_then(|message| message.as_str()) {
        let message = message.trim();
        if !message.is_empty() {
            return Some(message.to_string());
        }
    }

    None
}

fn deepseek_post(
    api_key: &str,
    body: &serde_json::Value,
    timeout: Duration,
) -> Result<String, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|err| format!("DeepSeek 客户端初始化失败: {err}"))?;

    let response = client
        .post(DEEPSEEK_API_URL)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {api_key}"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(body)
        .send()
        .map_err(|err| format!("DeepSeek 请求失败: {err}"))?;

    let status = response.status();
    let text = response
        .text()
        .map_err(|err| format!("DeepSeek 响应读取失败: {err}"))?;

    if status.is_success() {
        return Ok(text);
    }

    if let Some(message) = extract_deepseek_error_message(&text) {
        Err(format!(
            "DeepSeek 返回异常: HTTP {} - {message}",
            status.as_u16()
        ))
    } else {
        Err(format!("DeepSeek 返回异常: HTTP {}", status.as_u16()))
    }
}

fn extract_deepseek_content(text: &str) -> Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|err| format!("DeepSeek 响应解析失败: {err}"))?;
    value
        .get("choices")
        .and_then(|choices| choices.get(0))
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_str())
        .map(|content| content.to_string())
        .ok_or_else(|| "DeepSeek 响应缺少 content".to_string())
}

fn build_ai_prompt(
    platform_name: &str,
    skill: &SkillData,
    source_markdown_max_chars: usize,
) -> String {
    let source_description = skill.source_description.trim();
    let source_usage = skill.source_usage.trim();
    let source_markdown = skill.source_markdown.trim();
    let commands = if skill.source_commands.is_empty() {
        vec!["(none)".to_string()]
    } else {
        skill.source_commands.clone()
    };
    let source_markdown_for_prompt = if source_markdown.is_empty() {
        String::new()
    } else {
        take_chars(source_markdown, source_markdown_max_chars)
    };

    let payload = serde_json::json!({
        "platform": platform_name,
        "skill_id": skill.id,
        "skill_name": skill.source_name,
        "source_description": source_description,
        "source_usage": source_usage,
        "source_commands": commands,
        "skill_path": skill.path,
        "source_markdown_truncated_chars": source_markdown_max_chars,
    });

    format!(
        "请基于下面的技能元数据和 SKILL.md 全文，生成“精炼、易懂、重点明确”的中文说明。\n\n元数据(JSON):\n{}\n\nSKILL.md 全文（可能已截断）:\n~~~markdown\n{}\n~~~\n\n输出要求:\n1) 严格输出 JSON: {{\"brief\":\"...\",\"detail\":\"...\"}}，字段顺序必须先 brief，再 detail。\n2) brief: 1 句话，16-30 字，直说核心价值，不要口号。\n3) detail: 必须是 Markdown，仅保留这两个二级标题（按顺序输出）：\n   - ## 什么时候用\n   - ## 怎么用\n4) detail 写作约束：\n   - 突出重点，不要废话，不要背景铺垫\n   - 总长度建议 400-900 字\n   - “什么时候用”写清典型场景、触发条件、不适用边界\n   - “怎么用”写清最短可执行步骤、关键参数/输入输出、常见失败处理\n   - 有命令就给可直接复制的代码块\n   - 严禁编造输入里没有的能力；信息缺失时写“文档未提供”\n   - 不要输出“注意事项与边界”章节\n5) 保留关键英文专有名词（框架名、命令名、API 字段名），其余用自然中文。",
        payload, source_markdown_for_prompt
    )
}

fn should_retry_deepseek_error(message: &str) -> bool {
    let lower = message.to_lowercase();
    if lower.contains("http 401")
        || lower.contains("http 400")
        || lower.contains("http 403")
        || lower.contains("http 404")
        || lower.contains("invalid api key")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
    {
        return false;
    }

    lower.contains("http 408")
        || lower.contains("http 409")
        || lower.contains("http 429")
        || lower.contains("http 500")
        || lower.contains("http 502")
        || lower.contains("http 503")
        || lower.contains("http 504")
        || lower.contains("timeout")
        || lower.contains("timed out")
        || lower.contains("network")
        || lower.contains("connection")
        || lower.contains("dns")
        || lower.contains("temporarily")
        || lower.contains("请求失败")
        || lower.contains("连接失败")
        || lower.contains("响应解析失败")
        || lower.contains("格式解析失败")
        || lower.contains("响应缺少")
        || lower.contains("内容为空")
}

fn call_deepseek_profile_once(
    platform_name: &str,
    skill: &SkillData,
    api_key: &str,
    source_markdown_max_chars: usize,
    response_max_tokens: usize,
    cancel_epoch: Option<u64>,
) -> Result<AiSkillProfile, String> {
    ensure_ai_summary_not_cancelled(cancel_epoch)?;
    let prompt = build_ai_prompt(platform_name, skill, source_markdown_max_chars);

    let body = serde_json::json!({
        "model": "deepseek-chat",
        "temperature": 0.1,
        "max_tokens": response_max_tokens,
        "response_format": { "type": "json_object" },
        "messages": [
            {
                "role": "system",
                "content": "你是“技能说明编辑器”。输出必须精炼、易懂、重点明确、可执行，不说废话。必须忠于输入，不臆测，不输出多余字段。只返回 JSON，字段顺序 brief 再 detail，其中 detail 使用 Markdown。"
            },
            {
                "role": "user",
                "content": prompt
            }
        ]
    });

    let text = deepseek_post(api_key, &body, Duration::from_secs(AI_REQUEST_TIMEOUT_SECS))?;
    ensure_ai_summary_not_cancelled(cancel_epoch)?;
    let content = extract_deepseek_content(&text)?;

    let profile =
        parse_ai_profile_text(&content).ok_or_else(|| "AI 描述格式解析失败".to_string())?;
    let brief = trim_ai_brief(&profile.brief);
    let detail = trim_ai_markdown(&profile.detail);

    if brief.is_empty() || detail.is_empty() {
        return Err("AI 描述内容为空".to_string());
    }

    Ok(AiSkillProfile { brief, detail })
}

fn call_deepseek_profile_with_limits(
    platform_name: &str,
    skill: &SkillData,
    api_key: &str,
    source_markdown_max_chars: usize,
    response_max_tokens: usize,
    cancel_epoch: Option<u64>,
) -> Result<AiSkillProfile, String> {
    let max_retries = AI_PROFILE_MAX_RETRY_ATTEMPTS.saturating_sub(1);
    let mut retried = 0usize;
    let mut last_error = String::new();

    for attempt in 1..=AI_PROFILE_MAX_RETRY_ATTEMPTS {
        ensure_ai_summary_not_cancelled(cancel_epoch)?;
        match call_deepseek_profile_once(
            platform_name,
            skill,
            api_key,
            source_markdown_max_chars,
            response_max_tokens,
            cancel_epoch,
        ) {
            Ok(profile) => return Ok(profile),
            Err(err) => {
                last_error = err.clone();
                if is_ai_summary_cancel_error(&err) {
                    return Err(err);
                }
                let is_last_attempt = attempt >= AI_PROFILE_MAX_RETRY_ATTEMPTS;
                if is_last_attempt || !should_retry_deepseek_error(&err) {
                    return Err(format!("{err}（重试 {retried}/{max_retries}）"));
                }

                retried += 1;
                let exp = (attempt - 1).min(6) as u32;
                let delay_ms = AI_PROFILE_RETRY_BASE_DELAY_MS.saturating_mul(1u64 << exp);
                thread::sleep(Duration::from_millis(delay_ms));
            }
        }
    }

    Err(format!("{last_error}（重试 {retried}/{max_retries}）"))
}

fn call_deepseek_profile(
    platform_name: &str,
    skill: &SkillData,
    api_key: &str,
    cancel_epoch: Option<u64>,
) -> Result<AiSkillProfile, String> {
    call_deepseek_profile_with_limits(
        platform_name,
        skill,
        api_key,
        AI_SOURCE_MARKDOWN_MAX_CHARS,
        AI_RESPONSE_MAX_TOKENS,
        cancel_epoch,
    )
}

fn extract_partial_detail_from_json_like(raw: &str) -> Option<String> {
    let key_pos = raw.find("\"detail\"")?;
    let mut tail = &raw[key_pos + "\"detail\"".len()..];

    let colon_pos = tail.find(':')?;
    tail = &tail[colon_pos + 1..];
    tail = tail.trim_start();
    if !tail.starts_with('"') {
        return None;
    }
    tail = &tail[1..];

    let mut output = String::new();
    let mut escaped = false;

    for ch in tail.chars() {
        if escaped {
            match ch {
                'n' => output.push('\n'),
                'r' => output.push('\r'),
                't' => output.push('\t'),
                '"' => output.push('"'),
                '\\' => output.push('\\'),
                '/' => output.push('/'),
                _ => output.push(ch),
            }
            escaped = false;
            continue;
        }

        if ch == '\\' {
            escaped = true;
            continue;
        }

        if ch == '"' {
            return Some(output);
        }

        output.push(ch);
    }

    Some(output)
}

fn call_deepseek_profile_streaming<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    platform_id: &str,
    skill_id: &str,
    platform_name: &str,
    skill: &SkillData,
    api_key: &str,
    cancel_epoch: Option<u64>,
) -> Result<AiSkillProfile, String> {
    use std::io::BufRead;
    ensure_ai_summary_not_cancelled(cancel_epoch)?;

    let prompt = build_ai_prompt(platform_name, skill, AI_SOURCE_MARKDOWN_MAX_CHARS);
    let body = serde_json::json!({
        "model": "deepseek-chat",
        "temperature": 0.1,
        "max_tokens": AI_RESPONSE_MAX_TOKENS,
        "stream": true,
        "messages": [
            {
                "role": "system",
                "content": "你是“技能说明编辑器”。输出必须精炼、易懂、重点明确、可执行，不说废话。必须忠于输入，不臆测，不输出多余字段。只返回 JSON，字段顺序 brief 再 detail，其中 detail 使用 Markdown。"
            },
            {
                "role": "user",
                "content": prompt
            }
        ]
    });

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(AI_REQUEST_TIMEOUT_SECS))
        .build()
        .map_err(|err| format!("DeepSeek 客户端初始化失败: {err}"))?;

    let response = client
        .post(DEEPSEEK_API_URL)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {api_key}"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(&body)
        .send()
        .map_err(|err| format!("DeepSeek 请求失败: {err}"))?;
    ensure_ai_summary_not_cancelled(cancel_epoch)?;

    let status = response.status();
    if !status.is_success() {
        let text = response
            .text()
            .map_err(|err| format!("DeepSeek 响应读取失败: {err}"))?;
        if let Some(message) = extract_deepseek_error_message(&text) {
            return Err(format!(
                "DeepSeek 返回异常: HTTP {} - {message}",
                status.as_u16()
            ));
        }
        return Err(format!("DeepSeek 返回异常: HTTP {}", status.as_u16()));
    }

    let mut reader = io::BufReader::new(response);
    let mut line = String::new();
    let mut content = String::new();
    let mut last_emit_at = Instant::now();
    let mut saw_delta = false;

    loop {
        ensure_ai_summary_not_cancelled(cancel_epoch)?;
        line.clear();
        let read_bytes = reader
            .read_line(&mut line)
            .map_err(|err| format!("DeepSeek 流读取失败: {err}"))?;
        if read_bytes == 0 {
            break;
        }

        let trimmed = line.trim();
        if !trimmed.starts_with("data:") {
            continue;
        }
        let data = trimmed.trim_start_matches("data:").trim();
        if data.is_empty() {
            continue;
        }
        if data == "[DONE]" {
            break;
        }

        let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
            continue;
        };

        let delta = value
            .get("choices")
            .and_then(|choices| choices.get(0))
            .and_then(|choice| choice.get("delta"))
            .and_then(|delta| delta.get("content"))
            .and_then(|content| content.as_str());

        let Some(delta_text) = delta else {
            continue;
        };

        if delta_text.is_empty() {
            continue;
        }
        saw_delta = true;
        content.push_str(delta_text);

        if last_emit_at.elapsed() >= Duration::from_millis(120) || delta_text.contains('\n') {
            if let Some(detail_markdown) = extract_partial_detail_from_json_like(&content) {
                emit_ai_summary_stream(
                    app,
                    AiSummaryStreamPayload {
                        platform_id: platform_id.to_string(),
                        skill_id: skill_id.to_string(),
                        detail_markdown,
                        done: false,
                    },
                );
                last_emit_at = Instant::now();
            }
        }
    }

    if !saw_delta {
        return Err("DeepSeek 流式响应为空".to_string());
    }

    let profile =
        parse_ai_profile_text(&content).ok_or_else(|| "AI 描述格式解析失败".to_string())?;
    ensure_ai_summary_not_cancelled(cancel_epoch)?;
    let brief = trim_ai_brief(&profile.brief);
    let detail = trim_ai_markdown(&profile.detail);
    if brief.is_empty() || detail.is_empty() {
        return Err("AI 描述内容为空".to_string());
    }

    emit_ai_summary_stream(
        app,
        AiSummaryStreamPayload {
            platform_id: platform_id.to_string(),
            skill_id: skill_id.to_string(),
            detail_markdown: detail.clone(),
            done: true,
        },
    );

    Ok(AiSkillProfile { brief, detail })
}

fn test_deepseek_api_key_request(api_key: &str) -> Result<(), String> {
    let body = serde_json::json!({
        "model": "deepseek-chat",
        "temperature": 0,
        "max_tokens": AI_CONNECT_TEST_MAX_TOKENS,
        "messages": [
            { "role": "system", "content": "You are a connectivity probe. Reply with a short text." },
            { "role": "user", "content": "ping" }
        ]
    });

    let text = deepseek_post(
        api_key,
        &body,
        Duration::from_secs(AI_CONNECT_TEST_TIMEOUT_SECS),
    )?;
    let content = extract_deepseek_content(&text)?;
    if content.trim().is_empty() {
        return Err("DeepSeek 连通测试返回空内容".to_string());
    }
    Ok(())
}

fn enrich_missing_ai_profiles(
    platforms: &mut [PlatformData],
    overrides: &mut OverridesStore,
    api_key: &str,
) -> bool {
    let mut changed = false;

    for platform in platforms {
        for skill in &mut platform.skills {
            if !skill.ai_brief.trim().is_empty() && !skill.ai_detail.trim().is_empty() {
                continue;
            }
            if skill.source_description.trim().is_empty()
                && skill.source_usage.trim().is_empty()
                && skill.source_markdown.trim().is_empty()
            {
                continue;
            }

            let Ok(profile) = call_deepseek_profile(&platform.name, skill, api_key, None) else {
                continue;
            };

            skill.ai_brief = profile.brief.clone();
            skill.ai_detail = profile.detail.clone();

            let key = override_key(&platform.id, &skill.id);
            let entry = overrides.entries.entry(key).or_default();
            entry.ai_brief = Some(profile.brief);
            entry.ai_detail = Some(profile.detail);
            changed = true;
        }
    }

    changed
}

fn ensure_first_seen_records(
    platforms: &mut [PlatformData],
    overrides: &mut OverridesStore,
) -> bool {
    let now_millis = system_time_to_unix_millis(SystemTime::now());
    let mut changed = false;

    for platform in platforms {
        for skill in &mut platform.skills {
            let key = override_key(&platform.id, &skill.id);
            let entry = overrides.entries.entry(key).or_default();
            let resolved_first_seen = entry
                .first_seen_at
                .or(skill.first_seen_at)
                .or(skill.installed_at)
                .or(now_millis);

            skill.first_seen_at = resolved_first_seen;
            if entry.first_seen_at != resolved_first_seen {
                entry.first_seen_at = resolved_first_seen;
                changed = true;
            }
        }
    }

    changed
}

fn run_skills_cli_list_json(global: bool) -> Vec<SkillsCliEntry> {
    fn run_command_with_timeout(program: &str, args: &[&str]) -> Option<std::process::Output> {
        use std::process::Stdio;

        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .ok()?;

        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return child.wait_with_output().ok(),
                Ok(None) => {
                    if start.elapsed() >= Duration::from_secs(SKILLS_CLI_TIMEOUT_SECS) {
                        let _ = child.kill();
                        let _ = child.wait();
                        return None;
                    }
                    thread::sleep(Duration::from_millis(120));
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        }
    }

    let mut skills_args = vec!["list", "--json"];
    if global {
        skills_args.push("-g");
    }

    if let Some(output) = run_command_with_timeout("skills", &skills_args) {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let parsed = parse_json_array_from_text::<SkillsCliEntry>(&stdout);
            if !parsed.is_empty() {
                return parsed;
            }
        }
    }

    let mut npx_args = vec!["--yes", "skills", "list", "--json"];
    if global {
        npx_args.push("-g");
    }

    if let Some(output) = run_command_with_timeout("npx", &npx_args) {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            return parse_json_array_from_text::<SkillsCliEntry>(&stdout);
        }
    }

    vec![]
}

#[derive(Debug, Clone)]
struct ParsedSkillMeta {
    source_name: String,
    source_usage: String,
    source_description: String,
    source_markdown: String,
    source_commands: Vec<String>,
}

fn parse_skill_meta(file_path: &Path, fallback: &str) -> ParsedSkillMeta {
    let Ok(contents) = fs::read_to_string(file_path) else {
        let source_name = humanize_identifier(fallback);
        let source_usage = build_source_usage("", &[], fallback);
        return ParsedSkillMeta {
            source_name,
            source_usage,
            source_description: String::new(),
            source_markdown: String::new(),
            source_commands: vec![],
        };
    };

    let source_name = parse_frontmatter_name(&contents)
        .or_else(|| parse_heading_name(&contents))
        .unwrap_or_else(|| humanize_identifier(fallback));
    let description = parse_frontmatter_description(&contents)
        .or_else(|| parse_first_paragraph(&contents))
        .unwrap_or_default();
    let source_markdown = strip_frontmatter_markdown(&contents);
    let commands = extract_command_lines(&contents);
    let source_usage = build_source_usage(&description, &commands, fallback);

    ParsedSkillMeta {
        source_name,
        source_usage,
        source_description: description,
        source_markdown,
        source_commands: commands,
    }
}

fn parse_frontmatter_name(contents: &str) -> Option<String> {
    let mut lines = contents.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }

    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("name:") {
            let value = rest.trim().trim_matches('"').trim_matches('\'').trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn parse_heading_name(contents: &str) -> Option<String> {
    for line in contents.lines() {
        let trimmed = line.trim();
        if let Some(title) = trimmed.strip_prefix("# ") {
            let title = title.trim();
            if !title.is_empty() {
                return Some(title.to_string());
            }
        }
    }
    None
}

fn find_skill_definition_file(dir_path: &Path) -> Option<PathBuf> {
    for candidate in SKILL_FILE_NAMES {
        let full = dir_path.join(candidate);
        if full.is_file() {
            return Some(full);
        }
    }
    None
}

fn discover_skills_for_platform(
    source: &PlatformSource,
    overrides: &OverridesStore,
) -> io::Result<Vec<SkillData>> {
    let root = resolve_home_path(&source.root);
    if !root.exists() || !root.is_dir() {
        return Ok(vec![]);
    }

    let mut skills = Vec::new();
    let mut seen_ids = HashSet::new();
    let mut entries: Vec<_> = fs::read_dir(&root)?.filter_map(Result::ok).collect();
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let entry_name = entry.file_name();
        let entry_name = entry_name.to_string_lossy();
        if !source.include_hidden && is_hidden(&entry_name) {
            continue;
        }

        let entry_path = entry.path();
        if entry_path.is_dir() {
            if let Some(skill_file) = find_skill_definition_file(&entry_path) {
                let local_id = entry_name.to_string();
                if !seen_ids.insert(local_id.clone()) {
                    continue;
                }
                let key = override_key(&source.id, &local_id);
                let override_entry = overrides.entries.get(&key);
                let meta = parse_skill_meta(&skill_file, &local_id);
                let name = meta.source_name.clone();
                let status = override_entry
                    .and_then(|entry| entry.status)
                    .unwrap_or_default();
                let ai_brief = override_entry
                    .and_then(|entry| entry.ai_brief.clone())
                    .unwrap_or_default();
                let ai_detail = override_entry
                    .and_then(|entry| entry.ai_detail.clone())
                    .unwrap_or_default();
                let favorite = override_entry
                    .and_then(|entry| entry.favorite)
                    .unwrap_or(false);
                let first_seen_at = override_entry.and_then(|entry| entry.first_seen_at);
                let installed_at =
                    detect_installed_at(&entry_path).or_else(|| detect_installed_at(&skill_file));

                skills.push(SkillData {
                    id: local_id,
                    name,
                    source_name: meta.source_name,
                    source_usage: meta.source_usage,
                    source_description: meta.source_description,
                    source_markdown: meta.source_markdown,
                    source_commands: meta.source_commands,
                    ai_brief,
                    ai_detail,
                    favorite,
                    status,
                    path: entry_path.to_string_lossy().to_string(),
                    definition_path: skill_file.to_string_lossy().to_string(),
                    installed_at,
                    first_seen_at,
                });
            }
            continue;
        }

        if entry_path.is_file()
            && entry_path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
        {
            let local_id = file_stem_str(&entry_path);
            if !seen_ids.insert(local_id.clone()) {
                continue;
            }
            let key = override_key(&source.id, &local_id);
            let override_entry = overrides.entries.get(&key);
            let meta = parse_skill_meta(&entry_path, &local_id);
            let name = meta.source_name.clone();
            let status = override_entry
                .and_then(|entry| entry.status)
                .unwrap_or_default();
            let ai_brief = override_entry
                .and_then(|entry| entry.ai_brief.clone())
                .unwrap_or_default();
            let ai_detail = override_entry
                .and_then(|entry| entry.ai_detail.clone())
                .unwrap_or_default();
            let favorite = override_entry
                .and_then(|entry| entry.favorite)
                .unwrap_or(false);
            let first_seen_at = override_entry.and_then(|entry| entry.first_seen_at);
            let installed_at = detect_installed_at(&entry_path);

            skills.push(SkillData {
                id: local_id,
                name,
                source_name: meta.source_name,
                source_usage: meta.source_usage,
                source_description: meta.source_description,
                source_markdown: meta.source_markdown,
                source_commands: meta.source_commands,
                ai_brief,
                ai_detail,
                favorite,
                status,
                path: entry_path.to_string_lossy().to_string(),
                definition_path: entry_path.to_string_lossy().to_string(),
                installed_at,
                first_seen_at,
            });
        }
    }

    skills.sort_by(|a, b| a.id.to_lowercase().cmp(&b.id.to_lowercase()));
    Ok(skills)
}

fn discover_skills_from_skills_cli(
    platform_id: &str,
    platform_name: &str,
    global: bool,
    overrides: &OverridesStore,
    excluded_paths: &mut HashSet<String>,
) -> Option<PlatformData> {
    let mut entries = run_skills_cli_list_json(global);
    if entries.is_empty() {
        return None;
    }

    entries.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    let mut skills = Vec::new();
    let mut seen_ids = HashSet::new();

    for entry in entries {
        let path = entry.path.trim().to_string();
        if path.is_empty() || excluded_paths.contains(&path) {
            continue;
        }

        excluded_paths.insert(path.clone());
        let base_id = Path::new(&path)
            .file_name()
            .and_then(|name| name.to_str())
            .map(sanitize_id)
            .unwrap_or_else(|| "skill".to_string());
        let mut skill_id = base_id.clone();
        let mut index = 2usize;
        while seen_ids.contains(&skill_id) {
            skill_id = format!("{base_id}_{index}");
            index += 1;
        }
        seen_ids.insert(skill_id.clone());

        let key = override_key(platform_id, &skill_id);
        let override_entry = overrides.entries.get(&key);
        let path_ref = Path::new(&path);
        let parsed_meta = if path_ref.is_dir() {
            find_skill_definition_file(path_ref).map(|file| parse_skill_meta(&file, &skill_id))
        } else if path_ref.is_file() {
            Some(parse_skill_meta(path_ref, &skill_id))
        } else {
            None
        };
        let source_name = if entry.name.trim().is_empty() {
            parsed_meta
                .as_ref()
                .map(|meta| meta.source_name.clone())
                .unwrap_or_else(|| {
                    let fallback = Path::new(&path)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("skill");
                    humanize_identifier(fallback)
                })
        } else {
            entry.name.trim().to_string()
        };
        let name = source_name.clone();
        let source_usage = parsed_meta
            .as_ref()
            .map(|meta| meta.source_usage.clone())
            .unwrap_or_else(|| {
                format!(
                    "Use {} according to its SKILL.md instructions in {}.",
                    source_name, path
                )
            });
        let source_description = parsed_meta
            .as_ref()
            .map(|meta| meta.source_description.clone())
            .unwrap_or_default();
        let source_markdown = parsed_meta
            .as_ref()
            .map(|meta| meta.source_markdown.clone())
            .unwrap_or_default();
        let source_commands = parsed_meta
            .as_ref()
            .map(|meta| meta.source_commands.clone())
            .unwrap_or_default();
        let definition_path = if path_ref.is_dir() {
            find_skill_definition_file(path_ref)
                .map(|file| file.to_string_lossy().to_string())
                .unwrap_or_else(|| path.clone())
        } else {
            path.clone()
        };
        let status = override_entry
            .and_then(|entry| entry.status)
            .unwrap_or_default();
        let ai_brief = override_entry
            .and_then(|entry| entry.ai_brief.clone())
            .unwrap_or_default();
        let ai_detail = override_entry
            .and_then(|entry| entry.ai_detail.clone())
            .unwrap_or_default();
        let favorite = override_entry
            .and_then(|entry| entry.favorite)
            .unwrap_or(false);
        let first_seen_at = override_entry.and_then(|entry| entry.first_seen_at);
        let installed_at = detect_installed_at(Path::new(&path));

        skills.push(SkillData {
            id: skill_id,
            name,
            source_name,
            source_usage,
            source_description,
            source_markdown,
            source_commands,
            ai_brief,
            ai_detail,
            favorite,
            status,
            definition_path,
            path,
            installed_at,
            first_seen_at,
        });
    }

    if skills.is_empty() {
        return None;
    }

    Some(PlatformData {
        id: platform_id.to_string(),
        name: platform_name.to_string(),
        root: if global {
            "skills list -g --json".to_string()
        } else {
            "skills list --json".to_string()
        },
        skills,
    })
}

fn build_skills_snapshot(summarize_pending: bool) -> SkillsSnapshot {
    let mut overrides = load_overrides();
    let mut platforms = Vec::new();
    let mut known_paths = HashSet::new();

    for source in build_platform_sources() {
        let Ok(skills) = discover_skills_for_platform(&source, &overrides) else {
            continue;
        };
        if skills.is_empty() {
            continue;
        }
        for skill in &skills {
            known_paths.insert(skill.path.clone());
        }
        platforms.push(PlatformData {
            id: source.id.clone(),
            name: source.name.clone(),
            root: resolve_home_path(&source.root)
                .to_string_lossy()
                .to_string(),
            skills,
        });
    }

    if let Some(platform) = discover_skills_from_skills_cli(
        "skills_cli_project",
        "Project (skills.sh)",
        false,
        &overrides,
        &mut known_paths,
    ) {
        platforms.push(platform);
    }

    if let Some(platform) = discover_skills_from_skills_cli(
        "skills_cli_global",
        "Global (skills.sh)",
        true,
        &overrides,
        &mut known_paths,
    ) {
        platforms.push(platform);
    }

    dedupe_skills_across_platforms(&mut platforms);

    let mut changed = ensure_first_seen_records(&mut platforms, &mut overrides);
    if summarize_pending {
        if let Some(api_key) = deepseek_api_key() {
            if enrich_missing_ai_profiles(&mut platforms, &mut overrides, &api_key) {
                changed = true;
            }
        }
    }
    if changed {
        let _ = save_overrides(&overrides);
    }

    let scanned_at = system_time_to_unix_millis(SystemTime::now()).unwrap_or_default();
    let mut snapshot = SkillsSnapshot {
        scanned_at,
        ai_summarized_count: 0,
        ai_pending_count: 0,
        platforms,
    };
    snapshot.ai_summarized_count = snapshot.ai_summarized_count();
    snapshot.ai_pending_count = snapshot.ai_pending_count();
    snapshot
}

fn load_skills_snapshot_internal(force_scan: bool, summarize_pending: bool) -> SkillsSnapshot {
    let now = system_time_to_unix_millis(SystemTime::now()).unwrap_or_default();

    if !force_scan && !summarize_pending {
        let cache = SNAPSHOT_CACHE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(entry) = cache.as_ref() {
            let age = now.saturating_sub(entry.cached_at);
            if age <= SNAPSHOT_CACHE_TTL_MS {
                return entry.snapshot.clone();
            }
        }
    }

    let snapshot = build_skills_snapshot(summarize_pending);
    write_snapshot_cache(&snapshot);
    snapshot
}

fn skill_dedupe_signature(skill: &SkillData) -> String {
    let normalized_name =
        collapse_whitespace_to_single_line(skill.source_name.trim()).to_lowercase();
    let fingerprint_source_raw = if !skill.source_markdown.trim().is_empty() {
        collapse_whitespace_to_single_line(skill.source_markdown.trim()).to_lowercase()
    } else if !skill.source_description.trim().is_empty() {
        collapse_whitespace_to_single_line(skill.source_description.trim()).to_lowercase()
    } else if !skill.source_usage.trim().is_empty() {
        collapse_whitespace_to_single_line(skill.source_usage.trim()).to_lowercase()
    } else {
        Path::new(&skill.definition_path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(skill.id.as_str())
            .to_lowercase()
    };
    let fingerprint_source = take_chars(&fingerprint_source_raw, 4000);

    let mut hasher = DefaultHasher::new();
    normalized_name.hash(&mut hasher);
    fingerprint_source.hash(&mut hasher);
    format!("{normalized_name}::{:016x}", hasher.finish())
}

fn merge_duplicate_skill_data(target: &mut SkillData, incoming: &SkillData) {
    if target.name.trim().is_empty() && !incoming.name.trim().is_empty() {
        target.name = incoming.name.clone();
    }
    if target.source_name.trim().is_empty() && !incoming.source_name.trim().is_empty() {
        target.source_name = incoming.source_name.clone();
    }
    if target.source_usage.trim().is_empty() && !incoming.source_usage.trim().is_empty() {
        target.source_usage = incoming.source_usage.clone();
    }
    if target.source_description.trim().is_empty() && !incoming.source_description.trim().is_empty()
    {
        target.source_description = incoming.source_description.clone();
    }
    if target.source_markdown.trim().is_empty() && !incoming.source_markdown.trim().is_empty() {
        target.source_markdown = incoming.source_markdown.clone();
    }
    if target.source_commands.is_empty() && !incoming.source_commands.is_empty() {
        target.source_commands = incoming.source_commands.clone();
    }
    if target.ai_brief.trim().is_empty() && !incoming.ai_brief.trim().is_empty() {
        target.ai_brief = incoming.ai_brief.clone();
    }
    if target.ai_detail.trim().is_empty() && !incoming.ai_detail.trim().is_empty() {
        target.ai_detail = incoming.ai_detail.clone();
    }
    if target.path.trim().is_empty() && !incoming.path.trim().is_empty() {
        target.path = incoming.path.clone();
    }
    if target.definition_path.trim().is_empty() && !incoming.definition_path.trim().is_empty() {
        target.definition_path = incoming.definition_path.clone();
    }
    match (target.installed_at, incoming.installed_at) {
        (Some(current), Some(next)) if next > current => target.installed_at = Some(next),
        (None, Some(next)) => target.installed_at = Some(next),
        _ => {}
    }
    match (target.first_seen_at, incoming.first_seen_at) {
        (Some(current), Some(next)) if next < current => target.first_seen_at = Some(next),
        (None, Some(next)) => target.first_seen_at = Some(next),
        _ => {}
    }
}

fn dedupe_skills_across_platforms(platforms: &mut [PlatformData]) {
    let mut signature_to_index = HashMap::<String, usize>::new();
    let mut canonical = Vec::<(usize, SkillData)>::new();

    for (platform_index, platform) in platforms.iter().enumerate() {
        for skill in &platform.skills {
            let signature = skill_dedupe_signature(skill);
            if let Some(existing_index) = signature_to_index.get(&signature).copied() {
                if let Some((_, kept_skill)) = canonical.get_mut(existing_index) {
                    merge_duplicate_skill_data(kept_skill, skill);
                }
                continue;
            }
            signature_to_index.insert(signature, canonical.len());
            canonical.push((platform_index, skill.clone()));
        }
    }

    for platform in platforms.iter_mut() {
        platform.skills.clear();
    }

    for (platform_index, skill) in canonical {
        if let Some(platform) = platforms.get_mut(platform_index) {
            platform.skills.push(skill);
        }
    }
}

fn load_cached_snapshot_any_age() -> Option<SkillsSnapshot> {
    let cache = SNAPSHOT_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.as_ref().map(|entry| entry.snapshot.clone())
}

fn write_snapshot_cache(snapshot: &SkillsSnapshot) {
    let now = system_time_to_unix_millis(SystemTime::now()).unwrap_or_default();
    let mut cache = SNAPSHOT_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *cache = Some(CachedSnapshot {
        cached_at: now,
        snapshot: snapshot.clone(),
    });
}

fn skill_keys(snapshot: &SkillsSnapshot) -> HashSet<String> {
    snapshot
        .platforms
        .iter()
        .flat_map(|platform| {
            platform
                .skills
                .iter()
                .map(move |skill| override_key(&platform.id, &skill.id))
        })
        .collect()
}

fn emit_scan_progress<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    stage: &str,
    message: String,
    new_skills_count: usize,
    summarized_count: usize,
    summarize_total: usize,
    current_skill: Option<String>,
) {
    let payload = ScanProgressPayload {
        stage: stage.to_string(),
        message,
        new_skills_count,
        summarized_count,
        summarize_total,
        current_skill,
    };
    let should_log = stage.starts_with("resummarize_all");
    if should_log {
        let mut progress = RESUMMARIZE_ALL_PROGRESS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *progress = Some(payload.clone());
    }
    let mut delivered = false;

    if let Some(window) = app.get_webview_window("dashboard") {
        if let Err(err) = window.emit("scan_progress", payload.clone()) {
            eprintln!("[scan_progress] emit failed window=dashboard stage={stage}: {err}");
        } else {
            delivered = true;
        }
    }

    if let Some(window) = app.get_webview_window("main") {
        if let Err(err) = window.emit("scan_progress", payload.clone()) {
            eprintln!("[scan_progress] emit failed window=main stage={stage}: {err}");
        } else {
            delivered = true;
        }
    }

    if !delivered {
        if let Err(err) = app.emit("scan_progress", payload) {
            eprintln!("[scan_progress] emit fallback failed stage={stage}: {err}");
        } else if should_log {
            eprintln!(
                "[scan_progress] emitted via app stage={stage}, summarized={summarized_count}, total={summarize_total}"
            );
        }
    } else if should_log {
        eprintln!(
            "[scan_progress] emitted stage={stage}, summarized={summarized_count}, total={summarize_total}"
        );
    }
}

fn emit_ai_summary_stream<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    payload: AiSummaryStreamPayload,
) {
    {
        let mut stream = AI_SUMMARY_STREAM_PROGRESS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *stream = Some(payload.clone());
    }

    eprintln!(
        "[ai_summary_stream] emit {}::{} len={} done={}",
        payload.platform_id,
        payload.skill_id,
        payload.detail_markdown.len(),
        payload.done
    );
    let mut delivered = false;

    if let Some(window) = app.get_webview_window("dashboard") {
        if window.emit("ai_summary_stream", payload.clone()).is_ok() {
            delivered = true;
        }
    }

    if let Some(window) = app.get_webview_window("main") {
        if window.emit("ai_summary_stream", payload.clone()).is_ok() {
            delivered = true;
        }
    }

    if !delivered {
        let _ = app.emit("ai_summary_stream", payload);
    }
}

#[tauri::command]
fn get_ai_summary_stream_latest() -> Option<AiSummaryStreamPayload> {
    let stream = AI_SUMMARY_STREAM_PROGRESS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    stream.clone()
}

#[tauri::command]
fn get_resummarize_all_progress() -> Option<ScanProgressPayload> {
    let progress = RESUMMARIZE_ALL_PROGRESS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    progress.clone()
}

#[tauri::command]
fn cancel_ai_summary_jobs() -> usize {
    let _ = bump_ai_summary_cancel_epoch();
    let (state_lock, wait_cv) = &*AI_SUMMARY_JOB_QUEUE;
    let state = state_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let active_jobs = state.active_jobs;
    drop(state);
    wait_cv.notify_all();
    active_jobs
}

fn emit_skills_snapshot<R: tauri::Runtime>(app: &tauri::AppHandle<R>, snapshot: &SkillsSnapshot) {
    let _ = app.emit("skills_snapshot_updated", snapshot.clone());
}

fn map_update_http_error(code: u16) -> String {
    match code {
        403 | 429 => "更新服务暂时不可用，请稍后重试".to_string(),
        _ => format!("更新服务暂时不可用 (HTTP {code})"),
    }
}

fn fetch_latest_release(
    client: &reqwest::blocking::Client,
) -> Result<Option<GithubLatestRelease>, String> {
    let response = client
        .get(GITHUB_LATEST_RELEASE_API_URL)
        .header(reqwest::header::USER_AGENT, "skills-box")
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .map_err(|err| format!("检查更新请求失败: {err}"))?;

    let status = response.status();
    let body = response
        .text()
        .map_err(|err| format!("检查更新响应读取失败: {err}"))?;

    if status.as_u16() == 404 {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(map_update_http_error(status.as_u16()));
    }

    let release: GithubLatestRelease =
        serde_json::from_str(&body).map_err(|_| "更新信息解析失败，请稍后重试".to_string())?;
    Ok(Some(release))
}

fn check_for_updates_internal(current_version: String) -> Result<UpdateCheckResult, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(12))
        .build()
        .map_err(|err| format!("检查更新初始化失败: {err}"))?;

    let current_clean = normalize_version_value(&current_version);
    let Some(release) = fetch_latest_release(&client)? else {
        return Err("没有可用正式更新".to_string());
    };

    let latest_clean = normalize_version_value(&release.tag_name);
    if latest_clean.is_empty() {
        return Err("未获取到有效版本信息，请稍后重试".to_string());
    }

    let has_update = match (
        semver::Version::parse(&latest_clean),
        semver::Version::parse(&current_clean),
    ) {
        (Ok(latest), Ok(current)) => latest > current,
        _ => latest_clean != current_clean,
    };

    Ok(UpdateCheckResult {
        current_version: current_clean,
        latest_version: latest_clean,
        has_update,
        release_url: release.html_url,
    })
}

fn check_for_updates_cached_internal(current_version: String) -> Result<UpdateCheckResult, String> {
    let now = system_time_to_unix_millis(SystemTime::now()).unwrap_or_default();
    let cached = {
        let cache = UPDATE_CHECK_CACHE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cache.clone()
    };

    if let Some(entry) = cached.as_ref() {
        let age = now.saturating_sub(entry.checked_at);
        if age <= UPDATE_CHECK_CACHE_TTL_MS {
            return Ok(entry.result.clone());
        }
    }

    match check_for_updates_internal(current_version) {
        Ok(result) => {
            let mut cache = UPDATE_CHECK_CACHE
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *cache = Some(CachedUpdateCheck {
                checked_at: now,
                result: result.clone(),
            });
            Ok(result)
        }
        Err(err) => {
            let lower = err.to_lowercase();
            if (err.contains("过于频繁") || lower.contains("rate")) && cached.is_some() {
                if let Some(entry) = cached {
                    return Ok(entry.result);
                }
            }
            Err(err)
        }
    }
}

fn should_process_notify_event(event: &notify::Event) -> bool {
    !matches!(event.kind, notify::EventKind::Access(_))
}

fn collect_watch_targets() -> Vec<(PathBuf, notify::RecursiveMode)> {
    use notify::RecursiveMode;

    let mut targets = BTreeMap::<String, (PathBuf, RecursiveMode)>::new();

    let mut register = |path: PathBuf, mode: RecursiveMode| {
        let key = path.to_string_lossy().to_lowercase();
        let effective_mode = if let Some((_, old_mode)) = targets.get(&key) {
            if matches!(
                (old_mode, mode),
                (RecursiveMode::NonRecursive, RecursiveMode::Recursive)
            ) {
                RecursiveMode::Recursive
            } else {
                *old_mode
            }
        } else {
            mode
        };
        targets.insert(key, (path, effective_mode));
    };

    let mut register_root_or_parent = |root: PathBuf| {
        if root.exists() {
            register(root, RecursiveMode::Recursive);
            return;
        }

        let mut candidate = root;
        let mut found = None;
        for _ in 0..4 {
            if candidate.exists() {
                found = Some(candidate.clone());
                break;
            }
            if !candidate.pop() {
                break;
            }
        }
        if let Some(existing_parent) = found {
            register(existing_parent, RecursiveMode::NonRecursive);
        }
    };

    for source in build_platform_sources() {
        register_root_or_parent(resolve_home_path(&source.root));
    }

    for entry in run_skills_cli_list_json(false)
        .into_iter()
        .chain(run_skills_cli_list_json(true))
    {
        let path = PathBuf::from(entry.path.trim());
        if path.as_os_str().is_empty() {
            continue;
        }
        register_root_or_parent(path);
    }

    targets.into_values().collect()
}

fn reconcile_watch_targets(
    watcher: &mut notify::RecommendedWatcher,
    watched: &mut BTreeMap<String, (PathBuf, notify::RecursiveMode)>,
) {
    use notify::Watcher;

    let desired_vec = collect_watch_targets();
    let mut desired = BTreeMap::<String, (PathBuf, notify::RecursiveMode)>::new();
    for (path, mode) in desired_vec {
        desired.insert(path.to_string_lossy().to_lowercase(), (path, mode));
    }

    for (key, (path, mode)) in &desired {
        let should_rewatch = match watched.get(key) {
            Some((old_path, old_mode)) => old_path != path || old_mode != mode,
            None => true,
        };
        if should_rewatch {
            if let Some((old_path, _)) = watched.get(key) {
                let _ = watcher.unwatch(old_path);
            }
            let _ = watcher.watch(path, *mode);
        }
    }

    for (key, (old_path, _)) in watched.iter() {
        if !desired.contains_key(key) {
            let _ = watcher.unwatch(old_path);
        }
    }

    *watched = desired;
}

fn run_filesystem_watcher_loop(app_handle: tauri::AppHandle) {
    use notify::{Config, RecommendedWatcher, Watcher};
    use std::sync::mpsc::RecvTimeoutError;

    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = match RecommendedWatcher::new(
        move |result| {
            let _ = tx.send(result);
        },
        Config::default(),
    ) {
        Ok(watcher) => watcher,
        Err(err) => {
            eprintln!("[watcher] init failed: {err}");
            return;
        }
    };

    let mut watched = BTreeMap::<String, (PathBuf, notify::RecursiveMode)>::new();
    reconcile_watch_targets(&mut watcher, &mut watched);
    let min_refresh_interval = Duration::from_millis(WATCHER_REFRESH_MIN_INTERVAL_MS);
    let mut last_refresh_started_at: Option<Instant> = None;

    loop {
        let Ok(first) = rx.recv() else {
            eprintln!("[watcher] event channel disconnected");
            break;
        };

        let mut changed = match first {
            Ok(event) => should_process_notify_event(&event),
            Err(_) => false,
        };

        while let Ok(next) = rx.recv_timeout(Duration::from_millis(280)) {
            if let Ok(event) = next {
                if should_process_notify_event(&event) {
                    changed = true;
                }
            }
        }

        if !changed {
            continue;
        }

        if let Some(last_started_at) = last_refresh_started_at {
            let elapsed = last_started_at.elapsed();
            if elapsed < min_refresh_interval {
                let wait_for = min_refresh_interval - elapsed;
                let deadline = Instant::now() + wait_for;
                loop {
                    let timeout = deadline.saturating_duration_since(Instant::now());
                    if timeout.is_zero() {
                        break;
                    }
                    match rx.recv_timeout(timeout.min(Duration::from_millis(180))) {
                        Ok(Ok(event)) => {
                            if should_process_notify_event(&event) {
                                changed = true;
                            }
                        }
                        Ok(Err(_)) => {}
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => {
                            eprintln!("[watcher] event channel disconnected while waiting");
                            return;
                        }
                    }
                }
            }
        }
        if !changed {
            continue;
        }

        reconcile_watch_targets(&mut watcher, &mut watched);
        last_refresh_started_at = Some(Instant::now());
        let snapshot = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            refresh_skills_with_auto_ai_internal(app_handle.clone())
        })) {
            Ok(snapshot) => snapshot,
            Err(_) => {
                eprintln!("[watcher] refresh_skills_with_auto_ai_internal panicked");
                continue;
            }
        };
        emit_skills_snapshot(&app_handle, &snapshot);
    }
}

fn start_filesystem_watcher(app_handle: tauri::AppHandle) {
    thread::spawn(move || loop {
        let loop_handle = app_handle.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_filesystem_watcher_loop(loop_handle);
        }));

        if result.is_err() {
            eprintln!("[watcher] watcher loop panicked, restarting");
        } else {
            eprintln!("[watcher] watcher loop exited, restarting");
        }

        thread::sleep(Duration::from_millis(WATCHER_RESTART_BACKOFF_MS));
    });
}

fn apply_update_to_cached_snapshot(payload: &UpdateSkillPayload) -> Option<SkillsSnapshot> {
    let mut cache = SNAPSHOT_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let entry = cache.as_mut()?;

    let mut updated = false;
    for platform in &mut entry.snapshot.platforms {
        if platform.id != payload.platform_id {
            continue;
        }
        for skill in &mut platform.skills {
            if skill.id != payload.skill_id {
                continue;
            }
            if let Some(status) = payload.status {
                skill.status = status;
            }
            if let Some(favorite) = payload.favorite {
                skill.favorite = favorite;
            }
            updated = true;
            break;
        }
        if updated {
            break;
        }
    }

    if !updated {
        return None;
    }

    entry.cached_at = system_time_to_unix_millis(SystemTime::now()).unwrap_or_default();
    Some(entry.snapshot.clone())
}

fn apply_ai_update_to_cached_snapshot(
    platform_id: &str,
    skill_id: &str,
    ai_brief: &str,
    ai_detail: &str,
) -> Option<SkillsSnapshot> {
    let mut cache = SNAPSHOT_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let entry = cache.as_mut()?;

    let mut updated = false;
    for platform in &mut entry.snapshot.platforms {
        if platform.id != platform_id {
            continue;
        }
        for skill in &mut platform.skills {
            if skill.id != skill_id {
                continue;
            }
            skill.ai_brief = ai_brief.to_string();
            skill.ai_detail = ai_detail.to_string();
            updated = true;
            break;
        }
        if updated {
            break;
        }
    }

    if !updated {
        return None;
    }

    entry.snapshot.ai_summarized_count = entry.snapshot.ai_summarized_count();
    entry.snapshot.ai_pending_count = entry.snapshot.ai_pending_count();
    entry.cached_at = system_time_to_unix_millis(SystemTime::now()).unwrap_or_default();
    Some(entry.snapshot.clone())
}

fn collapse_whitespace_to_single_line(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_with_ellipsis(input: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }

    let chars = input.chars().collect::<Vec<_>>();
    if chars.len() <= max_chars {
        return input.to_string();
    }

    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }

    let keep = max_chars - 3;
    let mut output = chars.into_iter().take(keep).collect::<String>();
    output.push_str("...");
    output
}

fn tray_favorite_summary(skill: &SkillData) -> String {
    let raw = if !skill.ai_brief.trim().is_empty() {
        skill.ai_brief.trim()
    } else if !skill.source_description.trim().is_empty() {
        skill.source_description.trim()
    } else if !skill.source_usage.trim().is_empty() {
        skill.source_usage.trim()
    } else {
        "暂无一句话解释"
    };
    let normalized = collapse_whitespace_to_single_line(raw);
    if normalized.is_empty() {
        return "暂无一句话解释".to_string();
    }
    truncate_with_ellipsis(&normalized, TRAY_FAVORITE_SUMMARY_MAX_CHARS)
}

fn find_skill_definition_path(
    snapshot: &SkillsSnapshot,
    platform_id: &str,
    skill_id: &str,
) -> Option<String> {
    snapshot
        .platforms
        .iter()
        .find(|platform| platform.id == platform_id)
        .and_then(|platform| platform.skills.iter().find(|skill| skill.id == skill_id))
        .map(|skill| skill.definition_path.clone())
}

fn copy_text_to_clipboard(text: &str) -> Result<(), String> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|err| format!("剪贴板初始化失败: {err}"))?;
    clipboard
        .set_text(text.to_string())
        .map_err(|err| format!("复制到剪贴板失败: {err}"))
}

fn build_tray_menu<R: tauri::Runtime, M: Manager<R>>(
    manager: &M,
    snapshot: &SkillsSnapshot,
) -> tauri::Result<Menu<R>> {
    let menu = Menu::new(manager)?;
    let dashboard_item = MenuItem::with_id(
        manager,
        "open_dashboard",
        "SkillsBox 面板",
        true,
        None::<&str>,
    )?;
    let hint_top_separator = PredefinedMenuItem::separator(manager)?;
    let hint_item = MenuItem::with_id(
        manager,
        "favorites_hint",
        "点击下面 skills 复制技能路径，粘贴给 AI 调用",
        false,
        None::<&str>,
    )?;
    let favorites_separator = PredefinedMenuItem::separator(manager)?;
    let bottom_separator = PredefinedMenuItem::separator(manager)?;
    let quit_i = MenuItem::with_id(manager, "quit", "Quit", true, None::<&str>)?;
    menu.append(&dashboard_item)?;
    menu.append(&hint_top_separator)?;
    menu.append(&hint_item)?;
    menu.append(&favorites_separator)?;

    let mut favorites = snapshot
        .platforms
        .iter()
        .flat_map(|platform| {
            platform
                .skills
                .iter()
                .filter(|skill| skill.favorite)
                .map(move |skill| (platform.id.as_str(), skill))
        })
        .collect::<Vec<_>>();

    favorites.sort_by(|(platform_id_a, skill_a), (platform_id_b, skill_b)| {
        skill_a
            .name
            .to_lowercase()
            .cmp(&skill_b.name.to_lowercase())
            .then_with(|| skill_a.id.to_lowercase().cmp(&skill_b.id.to_lowercase()))
            .then_with(|| platform_id_a.cmp(platform_id_b))
    });

    if favorites.is_empty() {
        let empty_item =
            MenuItem::with_id(manager, "favorites_empty", "暂无收藏", false, None::<&str>)?;
        menu.append(&empty_item)?;
    } else {
        for (platform_id, skill) in favorites {
            let item_id = format!("favorite::{platform_id}::{}", skill.id);
            let item_label = format!("{}（{}）", skill.name, tray_favorite_summary(skill));
            let item = MenuItem::with_id(manager, item_id, item_label, true, None::<&str>)?;
            menu.append(&item)?;
        }
    }

    menu.append(&bottom_separator)?;
    menu.append(&quit_i)?;

    Ok(menu)
}

fn flush_tray_menu_updates_on_main<R: tauri::Runtime>(app_handle: &tauri::AppHandle<R>) {
    loop {
        let snapshot = {
            let mut state = TRAY_MENU_UPDATE_STATE
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match state.latest_snapshot.take() {
                Some(snapshot) => Some(snapshot),
                None => {
                    state.pending = false;
                    None
                }
            }
        };

        let Some(snapshot) = snapshot else {
            break;
        };

        let Ok(menu) = build_tray_menu(app_handle, &snapshot) else {
            continue;
        };
        if let Some(tray) = app_handle.tray_by_id("main") {
            let _ = tray.set_menu(Some(menu));
        }
    }
}

fn set_tray_menu_from_snapshot<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    snapshot: &SkillsSnapshot,
) {
    let should_schedule = {
        let mut state = TRAY_MENU_UPDATE_STATE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.latest_snapshot = Some(snapshot.clone());
        if state.pending {
            false
        } else {
            state.pending = true;
            true
        }
    };

    if !should_schedule {
        return;
    }

    let app_handle = app.clone();
    if app_handle
        .clone()
        .run_on_main_thread(move || flush_tray_menu_updates_on_main(&app_handle))
        .is_err()
    {
        let mut state = TRAY_MENU_UPDATE_STATE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.pending = false;
    }
}

fn open_dashboard<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    if let Some(window) = app.get_webview_window("dashboard") {
        let _ = window.set_title("");
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
        return;
    }

    let mut builder = WebviewWindowBuilder::new(app, "dashboard", WebviewUrl::default())
        .title("")
        .inner_size(1024.0, 820.0)
        .min_inner_size(860.0, 640.0)
        .resizable(true)
        .decorations(true)
        .visible(true)
        .center();

    #[cfg(target_os = "macos")]
    {
        builder = builder
            .hidden_title(true)
            .traffic_light_position(tauri::LogicalPosition::new(16.0, 16.0));
    }

    if let Ok(window) = builder.build() {
        let _ = window.set_focus();
    }
}

fn refresh_skills_with_auto_ai_internal(app: tauri::AppHandle) -> SkillsSnapshot {
    let _job_queue_guard = lock_ai_summary_job_queue();

    emit_scan_progress(
        &app,
        "scanning",
        "正在扫描 skill...".to_string(),
        0,
        0,
        0,
        None,
    );

    let previous_snapshot = load_cached_snapshot_any_age()
        .unwrap_or_else(|| load_skills_snapshot_internal(false, false));
    let mut snapshot = load_skills_snapshot_internal(true, false);

    let previous_keys = skill_keys(&previous_snapshot);
    let current_keys = skill_keys(&snapshot);
    let new_keys: HashSet<String> = current_keys
        .difference(&previous_keys)
        .cloned()
        .collect::<HashSet<_>>();
    let new_skills_count = new_keys.len();

    emit_scan_progress(
        &app,
        "scanned",
        format!("扫描完成，发现 {new_skills_count} 个新 skill。"),
        new_skills_count,
        0,
        0,
        None,
    );

    if new_skills_count == 0 {
        set_tray_menu_from_snapshot(&app, &snapshot);
        write_snapshot_cache(&snapshot);
        emit_scan_progress(&app, "done", "未发现新 skill。".to_string(), 0, 0, 0, None);
        return snapshot;
    }

    let Some(api_key) = deepseek_api_key() else {
        set_tray_menu_from_snapshot(&app, &snapshot);
        write_snapshot_cache(&snapshot);
        emit_scan_progress(
            &app,
            "warning",
            format!("发现 {new_skills_count} 个新 skill，但未配置 Key，未自动总结。"),
            new_skills_count,
            0,
            0,
            None,
        );
        return snapshot;
    };

    let mut targets = Vec::<(String, String, String, String)>::new();
    for platform in &snapshot.platforms {
        for skill in &platform.skills {
            let key = override_key(&platform.id, &skill.id);
            if !new_keys.contains(&key) {
                continue;
            }
            if !skill.ai_brief.trim().is_empty() && !skill.ai_detail.trim().is_empty() {
                continue;
            }
            targets.push((
                platform.id.clone(),
                platform.name.clone(),
                skill.id.clone(),
                skill.name.clone(),
            ));
        }
    }

    let summarize_total = targets.len();
    if summarize_total == 0 {
        set_tray_menu_from_snapshot(&app, &snapshot);
        write_snapshot_cache(&snapshot);
        emit_scan_progress(
            &app,
            "done",
            format!("发现 {new_skills_count} 个新 skill，均已具备总结内容。"),
            new_skills_count,
            0,
            0,
            None,
        );
        return snapshot;
    }

    let mut overrides = load_overrides();
    let mut summarized_count = 0usize;
    let mut changed = false;

    for (index, (platform_id, platform_name, skill_id, skill_name)) in targets.iter().enumerate() {
        emit_scan_progress(
            &app,
            "summarizing",
            format!("正在总结 {}/{}：{}", index + 1, summarize_total, skill_name),
            new_skills_count,
            summarized_count,
            summarize_total,
            Some(skill_name.clone()),
        );

        let Some(platform) = snapshot
            .platforms
            .iter_mut()
            .find(|platform| platform.id == *platform_id)
        else {
            continue;
        };
        let Some(skill) = platform
            .skills
            .iter_mut()
            .find(|skill| skill.id == *skill_id)
        else {
            continue;
        };

        let Ok(profile) = call_deepseek_profile(platform_name, skill, &api_key, None) else {
            continue;
        };

        skill.ai_brief = profile.brief.clone();
        skill.ai_detail = profile.detail.clone();

        let key = override_key(platform_id, skill_id);
        let mut entry = overrides.entries.get(&key).cloned().unwrap_or_default();
        entry.ai_brief = Some(profile.brief);
        entry.ai_detail = Some(profile.detail);
        overrides.entries.insert(key, entry);
        summarized_count += 1;
        changed = true;
    }

    if changed {
        let _ = save_overrides(&overrides);
    }

    snapshot.ai_summarized_count = snapshot.ai_summarized_count();
    snapshot.ai_pending_count = snapshot.ai_pending_count();

    write_snapshot_cache(&snapshot);
    set_tray_menu_from_snapshot(&app, &snapshot);
    emit_scan_progress(
        &app,
        "done",
        format!(
            "发现 {new_skills_count} 个新 skill，已完成 {summarized_count}/{summarize_total} 个总结。"
        ),
        new_skills_count,
        summarized_count,
        summarize_total,
        None,
    );

    snapshot
}

#[tauri::command]
async fn load_skills() -> SkillsSnapshot {
    tauri::async_runtime::spawn_blocking(|| load_skills_snapshot_internal(false, false))
        .await
        .unwrap_or_else(|_| load_skills_snapshot_internal(false, false))
}

#[tauri::command]
async fn refresh_skills(app: tauri::AppHandle) -> SkillsSnapshot {
    let snapshot =
        tauri::async_runtime::spawn_blocking(|| load_skills_snapshot_internal(true, false))
            .await
            .unwrap_or_else(|_| load_skills_snapshot_internal(true, false));
    set_tray_menu_from_snapshot(&app, &snapshot);
    snapshot
}

#[tauri::command]
async fn refresh_skills_with_auto_ai(app: tauri::AppHandle) -> SkillsSnapshot {
    let app_for_task = app.clone();
    let snapshot = tauri::async_runtime::spawn_blocking(move || {
        refresh_skills_with_auto_ai_internal(app_for_task)
    })
    .await
    .unwrap_or_else(|_| load_skills_snapshot_internal(true, false));
    set_tray_menu_from_snapshot(&app, &snapshot);
    snapshot
}

#[tauri::command]
fn get_ai_settings_status() -> AiSettingsStatus {
    AiSettingsStatus {
        has_key: deepseek_api_key().is_some(),
        masked_key: deepseek_api_key_mask(),
    }
}

#[tauri::command]
fn get_onboarding_status() -> bool {
    let config = load_app_config();
    match config.onboarding_completed {
        Some(completed) => !completed,
        None => !app_config_exists(),
    }
}

#[tauri::command]
fn complete_onboarding() -> Result<(), String> {
    let mut config = load_app_config();
    if config.onboarding_completed == Some(true) {
        return Ok(());
    }
    config.onboarding_completed = Some(true);
    save_app_config(&config).map_err(|err| format!("保存引导状态失败: {err}"))
}

#[tauri::command]
fn get_app_version(app: tauri::AppHandle) -> String {
    app.package_info().version.to_string()
}

#[tauri::command]
fn restart_app(app: tauri::AppHandle) {
    app.request_restart();
}

#[tauri::command]
async fn check_for_updates(app: tauri::AppHandle) -> Result<UpdateCheckResult, String> {
    let current_version = app.package_info().version.to_string();
    tauri::async_runtime::spawn_blocking(move || check_for_updates_cached_internal(current_version))
        .await
        .unwrap_or_else(|err| Err(format!("后台任务失败: {err}")))
}

#[tauri::command]
async fn set_deepseek_api_key(api_key: String) -> Result<AiSettingsStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let value = api_key.trim().to_string();
        let mut config = load_app_config();
        let previous_value = config
            .deepseek_api_key
            .as_deref()
            .map(|item| item.trim().to_string());
        let next_value = if value.is_empty() { None } else { Some(value) };

        if previous_value != next_value {
            config.deepseek_api_key = next_value;
            save_app_config(&config).map_err(|err| format!("保存 Key 失败: {err}"))?;
        }

        let resolved = resolve_deepseek_api_key();
        Ok(AiSettingsStatus {
            has_key: resolved.is_some(),
            masked_key: resolved.and_then(|item| masked_key(&item.value)),
        })
    })
    .await
    .unwrap_or_else(|err| Err(format!("后台任务失败: {err}")))
}

#[tauri::command]
async fn test_deepseek_api_key() -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let resolved = resolve_deepseek_api_key()
            .ok_or_else(|| "请先在设置中填写 DeepSeek Key".to_string())?;
        let source_label = match resolved.source {
            "config" => "本地设置",
            "env" => "环境变量",
            _ => resolved.source,
        };
        let key_tail = key_tail(&resolved.value);
        match test_deepseek_api_key_request(&resolved.value) {
            Ok(()) => Ok(format!(
                "DeepSeek 连接正常（来源: {source_label}，Key 尾号: {key_tail}）"
            )),
            Err(err) => Err(format!(
                "{err}（来源: {source_label}，Key 尾号: {key_tail}）"
            )),
        }
    })
    .await
    .unwrap_or_else(|err| Err(format!("后台任务失败: {err}")))
}

#[tauri::command]
async fn summarize_pending_skills(app: tauri::AppHandle) -> Result<SkillsSnapshot, String> {
    let app_for_task = app.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let cancel_epoch = current_ai_summary_cancel_epoch();
        let _job_queue_guard = try_lock_ai_summary_job_queue(Some(cancel_epoch))?;
        ensure_ai_summary_not_cancelled(Some(cancel_epoch))?;
        let api_key =
            deepseek_api_key().ok_or_else(|| "请先在设置中填写 DeepSeek Key".to_string())?;
        test_deepseek_api_key_request(&api_key)?;
        let snapshot = load_skills_snapshot_internal(true, true);
        Ok::<SkillsSnapshot, String>(snapshot)
    })
    .await
    .unwrap_or_else(|err| Err(format!("后台任务失败: {err}")))?;

    set_tray_menu_from_snapshot(&app_for_task, &result);
    Ok(result)
}

fn resummarize_all_skills_internal<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<SkillsSnapshot, String> {
    let job_started_at = Instant::now();
    eprintln!("[resummarize_all] internal start");
    let cancel_epoch = current_ai_summary_cancel_epoch();
    let _job_queue_guard = try_lock_ai_summary_job_queue(Some(cancel_epoch))?;
    ensure_ai_summary_not_cancelled(Some(cancel_epoch))?;
    eprintln!("[resummarize_all] lock acquired");
    let api_key = deepseek_api_key().ok_or_else(|| "请先在设置中填写 DeepSeek Key".to_string())?;
    eprintln!("[resummarize_all] api key loaded");

    emit_scan_progress(
        &app,
        "resummarize_all_start",
        "正在整理待总结技能...".to_string(),
        0,
        0,
        0,
        None,
    );

    let mut snapshot = load_cached_snapshot_any_age()
        .unwrap_or_else(|| load_skills_snapshot_internal(false, false));
    eprintln!(
        "[resummarize_all] snapshot ready in {:?}",
        job_started_at.elapsed()
    );
    let mut targets = Vec::<(usize, usize, String, i64, String)>::new();
    for (platform_index, platform) in snapshot.platforms.iter().enumerate() {
        for (skill_index, skill) in platform.skills.iter().enumerate() {
            if skill.source_description.trim().is_empty()
                && skill.source_usage.trim().is_empty()
                && skill.source_markdown.trim().is_empty()
            {
                continue;
            }
            let order_time = skill
                .first_seen_at
                .or(skill.installed_at)
                .unwrap_or_default();
            targets.push((
                platform_index,
                skill_index,
                skill.name.clone(),
                order_time,
                skill.id.clone(),
            ));
        }
    }

    targets.sort_by(
        |(_p1, _s1, name1, time1, id1), (_p2, _s2, name2, time2, id2)| {
            time2
                .cmp(time1)
                .then_with(|| name1.to_lowercase().cmp(&name2.to_lowercase()))
                .then_with(|| id1.to_lowercase().cmp(&id2.to_lowercase()))
        },
    );

    let summarize_total = targets.len();
    eprintln!("[resummarize_all] targets={summarize_total}");
    emit_scan_progress(
        &app,
        "resummarize_all_start",
        format!("准备重新总结 {summarize_total} 个 skill。"),
        0,
        0,
        summarize_total,
        None,
    );
    if summarize_total == 0 {
        eprintln!("[resummarize_all] nothing to summarize");
        emit_scan_progress(
            &app,
            "resummarize_all_done",
            "没有可重新总结的 skill。".to_string(),
            0,
            0,
            0,
            None,
        );
        return Ok(snapshot);
    }

    let mut overrides = load_overrides();
    let mut changed = false;
    let mut summarized_count = 0usize;
    let mut failed_count = 0usize;
    let mut first_error: Option<String> = None;

    for (index, (platform_index, skill_index, skill_name, _order_time, _skill_id)) in
        targets.iter().enumerate()
    {
        ensure_ai_summary_not_cancelled(Some(cancel_epoch))?;
        if index == 0 {
            eprintln!("[resummarize_all] enter summarize loop");
        }
        emit_scan_progress(
            &app,
            "resummarize_all_progress",
            format!(
                "正在重新总结 {}/{}：{}",
                index + 1,
                summarize_total,
                skill_name
            ),
            0,
            summarized_count,
            summarize_total,
            Some(skill_name.clone()),
        );

        let platform_name = snapshot.platforms[*platform_index].name.clone();
        let platform_id = snapshot.platforms[*platform_index].id.clone();
        let skill_id = snapshot.platforms[*platform_index].skills[*skill_index]
            .id
            .clone();
        let skill_for_ai = snapshot.platforms[*platform_index].skills[*skill_index].clone();

        let profile = match call_deepseek_profile(
            &platform_name,
            &skill_for_ai,
            &api_key,
            Some(cancel_epoch),
        ) {
            Ok(profile) => profile,
            Err(err) => {
                if is_ai_summary_cancel_error(&err) {
                    emit_scan_progress(
                        &app,
                        "resummarize_all_stopped",
                        "任务已停止。".to_string(),
                        0,
                        summarized_count,
                        summarize_total,
                        None,
                    );
                    return Err(err);
                }
                eprintln!("[resummarize_all] summarize failed: {skill_name}: {err}");
                failed_count += 1;
                if first_error.is_none() {
                    first_error = Some(err.clone());
                }
                emit_scan_progress(
                    &app,
                    "resummarize_all_progress",
                    format!("总结失败：{skill_name}（{err}）"),
                    0,
                    summarized_count,
                    summarize_total,
                    Some(skill_name.clone()),
                );
                continue;
            }
        };

        let skill = &mut snapshot.platforms[*platform_index].skills[*skill_index];
        skill.ai_brief = profile.brief.clone();
        skill.ai_detail = profile.detail.clone();

        let key = override_key(&platform_id, &skill_id);
        let entry = overrides.entries.entry(key).or_default();
        entry.ai_brief = Some(profile.brief);
        entry.ai_detail = Some(profile.detail);
        summarized_count += 1;
        changed = true;
    }

    if changed {
        save_overrides(&overrides).map_err(|err| format!("保存技能设置失败: {err}"))?;
    }

    snapshot.ai_summarized_count = snapshot.ai_summarized_count();
    snapshot.ai_pending_count = snapshot.ai_pending_count();
    write_snapshot_cache(&snapshot);

    if summarized_count == 0 && failed_count > 0 {
        let error_message =
            first_error.unwrap_or_else(|| "全部请求失败，未拿到可用结果".to_string());
        let final_message = format!(
            "全部重新总结失败：成功 0/{summarize_total}，失败 {failed_count}。首个错误：{error_message}"
        );
        emit_scan_progress(
            &app,
            "resummarize_all_error",
            final_message.clone(),
            0,
            summarized_count,
            summarize_total,
            None,
        );
        eprintln!(
            "[resummarize_all] all failed after {:?}: {}",
            job_started_at.elapsed(),
            final_message
        );
        return Err(final_message);
    }

    emit_scan_progress(
        &app,
        "resummarize_all_done",
        format!(
            "全部重新总结完成：成功 {summarized_count}/{summarize_total}，失败 {failed_count}。"
        ),
        0,
        summarized_count,
        summarize_total,
        None,
    );
    eprintln!(
        "[resummarize_all] done in {:?}, success={}, failed={}",
        job_started_at.elapsed(),
        summarized_count,
        failed_count
    );
    Ok(snapshot)
}

fn resummarize_skill_internal<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    payload: ResummarizeSkillPayload,
) -> Result<SkillsSnapshot, String> {
    let cancel_epoch = current_ai_summary_cancel_epoch();
    let _job_queue_guard = try_lock_ai_summary_job_queue(Some(cancel_epoch))?;
    ensure_ai_summary_not_cancelled(Some(cancel_epoch))?;
    let api_key = deepseek_api_key().ok_or_else(|| "请先在设置中填写 DeepSeek Key".to_string())?;

    let mut snapshot = load_skills_snapshot_internal(false, false);
    let mut location = snapshot
        .platforms
        .iter()
        .enumerate()
        .find(|(_, platform)| platform.id == payload.platform_id)
        .and_then(|(platform_index, platform)| {
            platform
                .skills
                .iter()
                .position(|skill| skill.id == payload.skill_id)
                .map(|skill_index| (platform_index, skill_index))
        });

    if location.is_none() {
        snapshot = load_skills_snapshot_internal(true, false);
        location = snapshot
            .platforms
            .iter()
            .enumerate()
            .find(|(_, platform)| platform.id == payload.platform_id)
            .and_then(|(platform_index, platform)| {
                platform
                    .skills
                    .iter()
                    .position(|skill| skill.id == payload.skill_id)
                    .map(|skill_index| (platform_index, skill_index))
            });
    }

    let Some((platform_index, skill_index)) = location else {
        return Err("未找到对应 skill，可能已被移除".to_string());
    };

    let platform_name = snapshot.platforms[platform_index].name.clone();
    let platform_id = payload.platform_id.clone();
    let skill_id = payload.skill_id.clone();
    let skill_for_ai = snapshot.platforms[platform_index].skills[skill_index].clone();
    let profile = match call_deepseek_profile_streaming(
        &app,
        &platform_id,
        &skill_id,
        &platform_name,
        &skill_for_ai,
        &api_key,
        Some(cancel_epoch),
    ) {
        Ok(profile) => profile,
        Err(err) => {
            if is_ai_summary_cancel_error(&err) {
                return Err(err);
            }
            eprintln!("[resummarize_skill] stream fallback: {err}");
            call_deepseek_profile(&platform_name, &skill_for_ai, &api_key, Some(cancel_epoch))?
        }
    };

    let skill = &mut snapshot.platforms[platform_index].skills[skill_index];
    skill.ai_brief = profile.brief.clone();
    skill.ai_detail = profile.detail.clone();

    upsert_ai_override_entry(&platform_id, &skill_id, &profile.brief, &profile.detail)
        .map_err(|err| format!("保存技能设置失败: {err}"))?;

    snapshot.ai_summarized_count = snapshot.ai_summarized_count();
    snapshot.ai_pending_count = snapshot.ai_pending_count();
    if let Some(merged) =
        apply_ai_update_to_cached_snapshot(&platform_id, &skill_id, &profile.brief, &profile.detail)
    {
        return Ok(merged);
    }
    write_snapshot_cache(&snapshot);
    Ok(snapshot)
}

#[tauri::command]
async fn resummarize_skill(
    app: tauri::AppHandle,
    payload: ResummarizeSkillPayload,
) -> Result<SkillsSnapshot, String> {
    let app_for_job = app.clone();
    let app_for_task = app.clone();
    let snapshot = tauri::async_runtime::spawn_blocking(move || {
        resummarize_skill_internal(app_for_job, payload)
    })
    .await
    .unwrap_or_else(|err| Err(format!("后台任务失败: {err}")))?;

    set_tray_menu_from_snapshot(&app_for_task, &snapshot);
    emit_skills_snapshot(&app_for_task, &snapshot);
    Ok(snapshot)
}

#[tauri::command]
async fn resummarize_all_skills(app: tauri::AppHandle) -> Result<SkillsSnapshot, String> {
    eprintln!("[resummarize_all] command received");
    emit_scan_progress(
        &app,
        "resummarize_all_start",
        "任务已提交，等待后台执行...".to_string(),
        0,
        0,
        0,
        None,
    );

    let app_for_job = app.clone();
    let app_for_task = app.clone();
    let snapshot =
        tauri::async_runtime::spawn_blocking(move || resummarize_all_skills_internal(app_for_job))
            .await
            .unwrap_or_else(|err| Err(format!("后台任务失败: {err}")))?;

    eprintln!("[resummarize_all] command completed");
    set_tray_menu_from_snapshot(&app_for_task, &snapshot);
    emit_skills_snapshot(&app_for_task, &snapshot);
    Ok(snapshot)
}

#[tauri::command]
fn update_skill(
    app: tauri::AppHandle,
    payload: UpdateSkillPayload,
) -> Result<SkillsSnapshot, String> {
    let mut store = load_overrides();
    let key = override_key(&payload.platform_id, &payload.skill_id);
    let mut entry = store.entries.get(&key).cloned().unwrap_or_default();

    if let Some(status) = payload.status {
        entry.status = Some(status);
    }
    if let Some(favorite) = payload.favorite {
        entry.favorite = if favorite { Some(true) } else { None };
    }

    let has_status = entry.status.is_some();
    let has_favorite = entry.favorite.unwrap_or(false);
    let has_ai_brief = entry
        .ai_brief
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty());
    let has_ai_detail = entry
        .ai_detail
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty());
    let has_first_seen = entry.first_seen_at.unwrap_or_default() > 0;

    if !has_status && !has_favorite && !has_ai_brief && !has_ai_detail && !has_first_seen {
        store.entries.remove(&key);
    } else {
        store.entries.insert(key, entry);
    }

    save_overrides(&store).map_err(|err| format!("保存技能设置失败: {err}"))?;
    let snapshot = apply_update_to_cached_snapshot(&payload)
        .unwrap_or_else(|| load_skills_snapshot_internal(true, false));
    set_tray_menu_from_snapshot(&app, &snapshot);
    Ok(snapshot)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            #[cfg(target_os = "macos")]
            {
                let _ = app
                    .handle()
                    .set_activation_policy(tauri::ActivationPolicy::Accessory);
            }

            let snapshot = load_skills_snapshot_internal(true, false);
            let tray_menu = build_tray_menu(app, &snapshot)?;
            let app_handle = app.handle().clone();

            // Build the main application menu
            let default_menu = Menu::default(app.handle())?;
            app.set_menu(default_menu)?;

            let tray_icon =
                tauri::image::Image::from_bytes(include_bytes!("../icons/tray/36x36.png"))?;

            TrayIconBuilder::with_id("main")
                .icon(tray_icon)
                .icon_as_template(true)
                .menu(&tray_menu)
                .show_menu_on_left_click(true)
                .on_menu_event(|app, event| {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match event
                        .id
                        .as_ref()
                    {
                        "open_dashboard" => {
                            open_dashboard(app);
                        }
                        "quit" => {
                            app.exit(0);
                        }
                        id if id.starts_with("favorite::") => {
                            let mut parts = id.splitn(3, "::");
                            let _ = parts.next();
                            let platform_id = parts.next().unwrap_or_default();
                            let skill_id = parts.next().unwrap_or_default();
                            if platform_id.is_empty() || skill_id.is_empty() {
                                return;
                            }

                            let Some(snapshot) = load_cached_snapshot_any_age() else {
                                return;
                            };
                            if let Some(path) =
                                find_skill_definition_path(&snapshot, platform_id, skill_id)
                            {
                                let _ = copy_text_to_clipboard(&path);
                            }
                        }
                        _ => {}
                    }));
                })
                .build(app)?;

            if get_onboarding_status() {
                open_dashboard(app.handle());
            }

            start_filesystem_watcher(app_handle);

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            load_skills,
            refresh_skills,
            refresh_skills_with_auto_ai,
            update_skill,
            get_ai_settings_status,
            get_onboarding_status,
            complete_onboarding,
            get_app_version,
            restart_app,
            check_for_updates,
            set_deepseek_api_key,
            test_deepseek_api_key,
            summarize_pending_skills,
            resummarize_skill,
            resummarize_all_skills,
            cancel_ai_summary_jobs,
            get_resummarize_all_progress,
            get_ai_summary_stream_latest
        ])
        .run(tauri::generate_context!())
        .unwrap_or_else(|err| eprintln!("error while running tauri application: {err}"));
}
