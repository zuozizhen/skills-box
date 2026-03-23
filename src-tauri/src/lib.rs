use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    env, fs, io,
    path::{Path, PathBuf},
    process::Command,
    sync::{LazyLock, Mutex},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
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
const GITHUB_RELEASES_API_URL: &str =
    "https://api.github.com/repos/zuozizhen/skills-box/releases?per_page=20";
const SNAPSHOT_CACHE_TTL_MS: i64 = 60_000;
const AI_SOURCE_MARKDOWN_MAX_CHARS: usize = 24_000;
const AI_RESPONSE_MAX_TOKENS: usize = 7500;
const AI_REQUEST_TIMEOUT_SECS: u64 = 60;
const AI_PROFILE_MAX_RETRY_ATTEMPTS: usize = 3;
const AI_PROFILE_RETRY_BASE_DELAY_MS: u64 = 1_200;

#[derive(Debug, Clone)]
struct CachedSnapshot {
    cached_at: i64,
    snapshot: SkillsSnapshot,
}

static SNAPSHOT_CACHE: LazyLock<Mutex<Option<CachedSnapshot>>> = LazyLock::new(|| Mutex::new(None));
static AI_PROFILE_QUEUE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static AI_SUMMARY_JOB_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static OVERRIDES_FILE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static APP_CONFIG_FILE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn lock_ai_summary_job_queue() -> std::sync::MutexGuard<'static, ()> {
    AI_SUMMARY_JOB_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TraySkillPayload {
    platform_id: String,
    skill_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AiSettingsStatus {
    has_key: bool,
    masked_key: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateCheckResult {
    current_version: String,
    latest_version: String,
    has_update: bool,
    release_url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TrayFavoriteSkill {
    platform_id: String,
    skill_id: String,
    name: String,
    summary: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GithubLatestRelease {
    tag_name: String,
    html_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GithubReleaseEntry {
    tag_name: String,
    html_url: Option<String>,
    #[serde(default)]
    draft: bool,
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
    home_dir().map(|home| home.join(".opcsoskills").join("overrides.json"))
}

fn app_config_file_path() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".opcsoskills").join("config.json"))
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
    let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or("data");
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
    unfenced
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .lines()
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
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

fn build_ai_prompt(platform_name: &str, skill: &SkillData) -> String {
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
        take_chars(source_markdown, AI_SOURCE_MARKDOWN_MAX_CHARS)
    };

    let payload = serde_json::json!({
        "platform": platform_name,
        "skill_id": skill.id,
        "skill_name": skill.source_name,
        "source_description": source_description,
        "source_usage": source_usage,
        "source_commands": commands,
        "skill_path": skill.path,
        "source_markdown_truncated_chars": AI_SOURCE_MARKDOWN_MAX_CHARS,
    });

    format!(
        "请基于下面的技能元数据和 SKILL.md 全文，生成“专业但易懂”的中文说明，目标是让用户快速上手并理解边界。\n\n元数据(JSON):\n{}\n\nSKILL.md 全文（可能已截断）:\n~~~markdown\n{}\n~~~\n\n输出要求:\n1) 严格输出 JSON: {{\"brief\":\"...\",\"detail\":\"...\"}}\n2) brief: 1 句话，16-32 字，直说核心价值，不要口号。\n3) detail: 必须是 Markdown，内容尽可能详尽、结构化、专业，建议使用以下二级标题（若信息不足可合并，但不要省略关键点）：\n   - ## 核心能力\n   - ## 适用场景\n   - ## 输入与输出\n   - ## 快速开始\n   - ## 常用命令与操作\n   - ## 注意事项与边界\n   - ## 常见问题与排查\n4) detail 要求：\n   - 先讲清“做什么”，再讲“何时用”，最后讲“怎么用”\n   - 步骤可执行，尽量具体到首个命令/入口\n   - 对参数、前置条件、失败场景给出明确提示\n   - 若文档有命令，必须整理成可直接复制的代码块\n5) 严禁编造输入里没有的能力；信息缺失时明确写“文档未提供”。\n6) 保留关键英文专有名词（框架名、命令名、API 字段名），其余用自然中文。\n7) 不要输出与该技能无关的背景知识，不要空泛。",
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
) -> Result<AiSkillProfile, String> {
    let prompt = build_ai_prompt(platform_name, skill);

    let body = serde_json::json!({
        "model": "deepseek-chat",
        "temperature": 0.1,
        "max_tokens": AI_RESPONSE_MAX_TOKENS,
        "response_format": { "type": "json_object" },
        "messages": [
            {
                "role": "system",
                "content": "你是“技能说明编辑器”。你要输出专业、准确、可执行、结构清晰的中文技能文档。必须忠于输入，不臆测，不输出多余字段。只返回 JSON，其中 detail 字段使用 Markdown。"
            },
            {
                "role": "user",
                "content": prompt
            }
        ]
    });

    let text = deepseek_post(api_key, &body, Duration::from_secs(AI_REQUEST_TIMEOUT_SECS))?;
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

fn call_deepseek_profile(
    platform_name: &str,
    skill: &SkillData,
    api_key: &str,
) -> Result<AiSkillProfile, String> {
    let _queue_guard = AI_PROFILE_QUEUE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let max_retries = AI_PROFILE_MAX_RETRY_ATTEMPTS.saturating_sub(1);
    let mut retried = 0usize;
    let mut last_error = String::new();

    for attempt in 1..=AI_PROFILE_MAX_RETRY_ATTEMPTS {
        match call_deepseek_profile_once(platform_name, skill, api_key) {
            Ok(profile) => return Ok(profile),
            Err(err) => {
                last_error = err.clone();
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

    Err(format!(
        "{last_error}（重试 {retried}/{max_retries}）"
    ))
}

fn build_deepseek_test_skill() -> SkillData {
    SkillData {
        id: "deepseek-connectivity-test".to_string(),
        name: "DeepSeek 连通测试".to_string(),
        source_name: "DeepSeek 连通测试".to_string(),
        source_usage: "用于验证当前 Key 是否可用于技能总结。".to_string(),
        source_description: "这是一个仅用于连通性和响应格式校验的测试技能。".to_string(),
        source_markdown: "# DeepSeek 连通测试\n\n这是一个仅用于连通性和响应格式校验的测试技能。"
            .to_string(),
        source_commands: vec!["npx skills list".to_string()],
        ai_brief: String::new(),
        ai_detail: String::new(),
        favorite: false,
        status: SkillStatus::Active,
        path: "/virtual/deepseek-connectivity-test".to_string(),
        definition_path: "/virtual/deepseek-connectivity-test/SKILL.md".to_string(),
        installed_at: None,
        first_seen_at: None,
    }
}

fn test_deepseek_api_key_request(api_key: &str) -> Result<(), String> {
    let probe = build_deepseek_test_skill();
    call_deepseek_profile("DeepSeek 测试", &probe, api_key).map(|_| ())
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

            let Ok(profile) = call_deepseek_profile(&platform.name, skill, api_key) else {
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
    let mut skills_args = vec!["list", "--json"];
    if global {
        skills_args.push("-g");
    }

    if let Ok(output) = Command::new("skills").args(&skills_args).output() {
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

    if let Ok(output) = Command::new("npx").args(&npx_args).output() {
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
    let _ = app.emit("scan_progress", payload);
}

fn emit_skills_snapshot<R: tauri::Runtime>(app: &tauri::AppHandle<R>, snapshot: &SkillsSnapshot) {
    let _ = app.emit("skills_snapshot_updated", snapshot.clone());
}

fn map_update_http_error(code: u16) -> String {
    match code {
        403 | 429 => "检查更新过于频繁，请稍后再试".to_string(),
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

    if status.is_success() {
        let release: GithubLatestRelease = serde_json::from_str(&body)
            .map_err(|_| "更新信息解析失败，请稍后重试".to_string())?;
        return Ok(Some(release));
    }

    if status.as_u16() != 404 {
        return Err(map_update_http_error(status.as_u16()));
    }

    let response = client
        .get(GITHUB_RELEASES_API_URL)
        .header(reqwest::header::USER_AGENT, "skills-box")
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .map_err(|err| format!("检查更新请求失败: {err}"))?;

    let status = response.status();
    let body = response
        .text()
        .map_err(|err| format!("检查更新响应读取失败: {err}"))?;

    if !status.is_success() {
        if status.as_u16() == 404 {
            return Ok(None);
        }
        return Err(map_update_http_error(status.as_u16()));
    }

    let releases: Vec<GithubReleaseEntry> =
        serde_json::from_str(&body).map_err(|_| "更新信息解析失败，请稍后重试".to_string())?;

    let latest = releases
        .into_iter()
        .find(|release| !release.draft)
        .map(|release| GithubLatestRelease {
            tag_name: release.tag_name,
            html_url: release.html_url,
        });

    Ok(latest)
}

fn check_for_updates_internal(current_version: String) -> Result<UpdateCheckResult, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(12))
        .build()
        .map_err(|err| format!("检查更新初始化失败: {err}"))?;

    let current_clean = normalize_version_value(&current_version);
    let Some(release) = fetch_latest_release(&client)? else {
        return Ok(UpdateCheckResult {
            current_version: current_clean.clone(),
            latest_version: current_clean,
            has_update: false,
            release_url: None,
        });
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

fn start_filesystem_watcher(app_handle: tauri::AppHandle) {
    thread::spawn(move || {
        use notify::{Config, RecommendedWatcher, Watcher};

        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = match RecommendedWatcher::new(
            move |result| {
                let _ = tx.send(result);
            },
            Config::default(),
        ) {
            Ok(watcher) => watcher,
            Err(_) => return,
        };

        let mut watched = BTreeMap::<String, (PathBuf, notify::RecursiveMode)>::new();
        reconcile_watch_targets(&mut watcher, &mut watched);

        loop {
            let Ok(first) = rx.recv() else {
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

            reconcile_watch_targets(&mut watcher, &mut watched);
            let snapshot = refresh_skills_with_auto_ai_internal(app_handle.clone());
            emit_skills_snapshot(&app_handle, &snapshot);
        }
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

fn build_tray_menu<R: tauri::Runtime, M: Manager<R>>(
    manager: &M,
) -> tauri::Result<Menu<R>> {
    let menu = Menu::new(manager)?;
    let dashboard_item = MenuItem::with_id(manager, "open_dashboard", "Dashboard", true, None::<&str>)?;
    let bottom_separator = PredefinedMenuItem::separator(manager)?;
    let quit_i = MenuItem::with_id(manager, "quit", "Quit", true, None::<&str>)?;
    menu.append_items(&[&dashboard_item, &bottom_separator, &quit_i])?;

    Ok(menu)
}

fn find_skill_path(snapshot: &SkillsSnapshot, platform_id: &str, skill_id: &str) -> Option<String> {
    snapshot
        .platforms
        .iter()
        .find(|platform| platform.id == platform_id)
        .and_then(|platform| platform.skills.iter().find(|skill| skill.id == skill_id))
        .map(|skill| skill.path.clone())
}

fn copy_text_to_clipboard(text: &str) -> Result<(), String> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|err| format!("剪贴板初始化失败: {err}"))?;
    clipboard
        .set_text(text.to_string())
        .map_err(|err| format!("复制到剪贴板失败: {err}"))
}

fn set_tray_menu_from_snapshot<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    _snapshot: &SkillsSnapshot,
) {
    let Ok(menu) = build_tray_menu(app) else {
        return;
    };
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_menu(Some(menu));
    }
}

fn list_tray_favorites_internal() -> Vec<TrayFavoriteSkill> {
    let snapshot = load_skills_snapshot_internal(false, false);
    let mut favorites = Vec::<TrayFavoriteSkill>::new();

    for platform in &snapshot.platforms {
        for skill in &platform.skills {
            if !skill.favorite {
                continue;
            }

            let summary = if !skill.ai_brief.trim().is_empty() {
                skill.ai_brief.trim().to_string()
            } else if !skill.source_description.trim().is_empty() {
                skill.source_description.trim().to_string()
            } else if !skill.source_usage.trim().is_empty() {
                skill.source_usage.trim().to_string()
            } else {
                "暂无一句话总结".to_string()
            };

            favorites.push(TrayFavoriteSkill {
                platform_id: platform.id.clone(),
                skill_id: skill.id.clone(),
                name: skill.name.clone(),
                summary,
            });
        }
    }

    favorites.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.skill_id.to_lowercase().cmp(&right.skill_id.to_lowercase()))
    });
    favorites
}

fn open_tray_panel<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    if let Some(window) = app.get_webview_window("tray_panel") {
        if window.is_visible().unwrap_or(false) {
            let _ = window.hide();
            return;
        }
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
        return;
    }

    let mut builder = WebviewWindowBuilder::new(app, "tray_panel", WebviewUrl::default())
        .title("Skills Box")
        .inner_size(420.0, 560.0)
        .min_inner_size(360.0, 420.0)
        .resizable(true)
        .visible(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .center();

    #[cfg(target_os = "macos")]
    {
        builder = builder
            .hidden_title(true)
            .traffic_light_position(tauri::LogicalPosition::new(14.0, 14.0));
    }

    if let Ok(window) = builder.build() {
        let _ = window.set_focus();
    }
}

fn open_dashboard<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    if let Some(window) = app.get_webview_window("dashboard") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
        return;
    }

    let mut builder = WebviewWindowBuilder::new(app, "dashboard", WebviewUrl::default())
        .title("Skills Box Dashboard")
        .inner_size(1024.0, 740.0)
        .min_inner_size(860.0, 600.0)
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

    let previous_snapshot = load_skills_snapshot_internal(false, false);
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

        let Ok(profile) = call_deepseek_profile(platform_name, skill, &api_key) else {
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
fn get_app_version(app: tauri::AppHandle) -> String {
    app.package_info().version.to_string()
}

#[tauri::command]
fn get_window_label(window: tauri::Window) -> String {
    window.label().to_string()
}

#[tauri::command]
fn list_tray_favorites() -> Vec<TrayFavoriteSkill> {
    list_tray_favorites_internal()
}

#[tauri::command]
fn copy_skill_path_for_tray(payload: TraySkillPayload) -> Result<String, String> {
    let snapshot = load_skills_snapshot_internal(false, false);
    let Some(path) = find_skill_path(&snapshot, &payload.platform_id, &payload.skill_id) else {
        return Err("未找到对应技能路径".to_string());
    };
    copy_text_to_clipboard(&path)?;
    Ok(path)
}

#[tauri::command]
fn open_dashboard_window(app: tauri::AppHandle) {
    open_dashboard(&app);
}

#[tauri::command]
fn hide_tray_panel_window(app: tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("tray_panel") {
        let _ = window.hide();
    }
}

#[tauri::command]
async fn check_for_updates(app: tauri::AppHandle) -> Result<UpdateCheckResult, String> {
    let current_version = app.package_info().version.to_string();
    tauri::async_runtime::spawn_blocking(move || check_for_updates_internal(current_version))
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
        let _job_queue_guard = lock_ai_summary_job_queue();
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
    let _job_queue_guard = lock_ai_summary_job_queue();
    let api_key = deepseek_api_key().ok_or_else(|| "请先在设置中填写 DeepSeek Key".to_string())?;

    let mut snapshot = load_skills_snapshot_internal(true, false);
    let mut targets = Vec::<(usize, usize, String)>::new();
    for (platform_index, platform) in snapshot.platforms.iter().enumerate() {
        for (skill_index, skill) in platform.skills.iter().enumerate() {
            if skill.source_description.trim().is_empty()
                && skill.source_usage.trim().is_empty()
                && skill.source_markdown.trim().is_empty()
            {
                continue;
            }
            targets.push((platform_index, skill_index, skill.name.clone()));
        }
    }

    let summarize_total = targets.len();
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

    if let Err(err) = test_deepseek_api_key_request(&api_key) {
        emit_scan_progress(
            &app,
            "resummarize_all_warning",
            format!("连通性预检失败，仍继续执行：{err}"),
            0,
            0,
            summarize_total,
            None,
        );
    }

    let mut overrides = load_overrides();
    let mut changed = false;
    let mut summarized_count = 0usize;
    let mut failed_count = 0usize;
    let mut first_error: Option<String> = None;

    for (index, (platform_index, skill_index, skill_name)) in targets.iter().enumerate() {
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
        let skill_id = snapshot.platforms[*platform_index].skills[*skill_index].id.clone();
        let skill_for_ai = snapshot.platforms[*platform_index].skills[*skill_index].clone();

        let profile = match call_deepseek_profile(&platform_name, &skill_for_ai, &api_key) {
            Ok(profile) => profile,
            Err(err) => {
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
        let error_message = first_error
            .unwrap_or_else(|| "全部请求失败，未拿到可用结果".to_string());
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
    Ok(snapshot)
}

fn resummarize_skill_internal(payload: ResummarizeSkillPayload) -> Result<SkillsSnapshot, String> {
    let _job_queue_guard = lock_ai_summary_job_queue();
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
    let skill_for_ai = snapshot.platforms[platform_index].skills[skill_index].clone();
    let profile = call_deepseek_profile(&platform_name, &skill_for_ai, &api_key)?;

    let skill = &mut snapshot.platforms[platform_index].skills[skill_index];
    skill.ai_brief = profile.brief.clone();
    skill.ai_detail = profile.detail.clone();

    let mut overrides = load_overrides();
    let key = override_key(&payload.platform_id, &payload.skill_id);
    let mut entry = overrides.entries.get(&key).cloned().unwrap_or_default();
    entry.ai_brief = Some(profile.brief);
    entry.ai_detail = Some(profile.detail);
    overrides.entries.insert(key, entry);
    save_overrides(&overrides).map_err(|err| format!("保存技能设置失败: {err}"))?;

    snapshot.ai_summarized_count = snapshot.ai_summarized_count();
    snapshot.ai_pending_count = snapshot.ai_pending_count();
    write_snapshot_cache(&snapshot);
    Ok(snapshot)
}

#[tauri::command]
async fn resummarize_skill(
    app: tauri::AppHandle,
    payload: ResummarizeSkillPayload,
) -> Result<SkillsSnapshot, String> {
    let app_for_task = app.clone();
    let snapshot =
        tauri::async_runtime::spawn_blocking(move || resummarize_skill_internal(payload))
            .await
            .unwrap_or_else(|err| Err(format!("后台任务失败: {err}")))?;

    set_tray_menu_from_snapshot(&app_for_task, &snapshot);
    emit_skills_snapshot(&app_for_task, &snapshot);
    Ok(snapshot)
}

#[tauri::command]
async fn resummarize_all_skills(app: tauri::AppHandle) -> Result<SkillsSnapshot, String> {
    let app_for_job = app.clone();
    let app_for_task = app.clone();
    let snapshot = tauri::async_runtime::spawn_blocking(move || {
        resummarize_all_skills_internal(app_for_job)
    })
        .await
        .unwrap_or_else(|err| Err(format!("后台任务失败: {err}")))?;

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
        .setup(|app| {
            let snapshot = load_skills_snapshot_internal(true, false);
            let tray_menu = build_tray_menu(app)?;
            let app_handle = app.handle().clone();
            
            // Build the main application menu
            let default_menu = Menu::default(app.handle())?;
            app.set_menu(default_menu)?;

            let tray_icon = tauri::image::Image::from_bytes(include_bytes!("../icons/tray/36x36.png"))?;

            TrayIconBuilder::with_id("main")
                .icon(tray_icon)
                .icon_as_template(true)
                .menu(&tray_menu)
                .show_menu_on_left_click(true)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open_dashboard" => {
                        open_dashboard(app);
                    }
                    "quit" => {
                        app.exit(0);
                    }
                    id if id == "tray_title"
                        || id == "tray_hint"
                        || id == "favorites_empty"
                        || id.starts_with("skill_note::") => {}
                    id if id.starts_with("skill::") => {
                        let mut parts = id.splitn(3, "::");
                        let _ = parts.next();
                        let platform_id = parts.next().unwrap_or_default();
                        let skill_id = parts.next().unwrap_or_default();
                        if platform_id.is_empty() || skill_id.is_empty() {
                            return;
                        }

                        let snapshot = load_skills_snapshot_internal(false, false);
                        if let Some(path) = find_skill_path(&snapshot, platform_id, skill_id) {
                            let _ = copy_text_to_clipboard(&path);
                        }
                    }
                    _ => {}
                })
                .build(app)?;

            start_filesystem_watcher(app_handle);

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            load_skills,
            refresh_skills,
            refresh_skills_with_auto_ai,
            update_skill,
            get_ai_settings_status,
            get_app_version,
            check_for_updates,
            set_deepseek_api_key,
            test_deepseek_api_key,
            summarize_pending_skills,
            resummarize_skill,
            resummarize_all_skills
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
