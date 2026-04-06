//! OS App Catalog — agent-installable pre-built application specs.
//!
//! OS apps are spec bundles (IOA TOML + CSDL + Cedar policies) loaded from
//! the `os-apps/` directory at runtime. Agents discover them via
//! `list_os_apps()` / `install_os_app()`.
//!
//! Backward-compatible skill aliases are preserved (`list_skills()`,
//! `install_skill()`) to avoid breaking older callers.
//!
//! Install reuses [`crate::bootstrap::bootstrap_tenant_specs`] so every app
//! goes through the same verification cascade as system specs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

use serde::Serialize;
use temper_runtime::tenant::TenantId;
use temper_spec::automaton;
use temper_spec::csdl::{emit_csdl_xml, merge_csdl, parse_csdl};

use crate::bootstrap;
use crate::state::PlatformState;

/// Result of an app installation, categorising each spec by what happened.
#[derive(Debug, Clone, Serialize)]
pub struct InstallResult {
    /// Entity types registered for the first time.
    pub added: Vec<String>,
    /// Entity types that already existed but whose IOA source changed.
    pub updated: Vec<String>,
    /// Entity types whose IOA source was byte-for-byte identical — skipped.
    pub skipped: Vec<String>,
    /// WASM modules compiled and registered.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub wasm_modules: Vec<String>,
}

/// Parsed app.toml manifest.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct AppManifest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub dependencies: Vec<String>,
}

fn default_version() -> String {
    "0.1.0".to_string()
}

fn read_app_manifest(app_dir: &Path) -> Option<AppManifest> {
    let path = app_dir.join("app.toml");
    let content = std::fs::read_to_string(&path).ok()?;
    toml::from_str(&content).ok()
}

/// Metadata for an app in the catalog.
#[derive(Debug, Clone, Serialize)]
pub struct AppEntry {
    /// Short name used in CLI flags and API calls (e.g. `"project-management"`).
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Entity types included in the app.
    pub entity_types: Vec<String>,
    /// Semantic version.
    pub version: String,
    /// Full app guide markdown (from `APP.md`/`app.md`/`skill.md`), if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_guide: Option<String>,
    /// Declared dependencies (from app.toml).
    #[serde(default)]
    pub dependencies: Vec<String>,
}

// Backward-compatible alias: SkillEntry → AppEntry.
pub type SkillEntry = AppEntry;

/// Full spec bundle for an app (owned, loaded from disk).
pub struct AppBundle {
    /// IOA spec sources as `(entity_type, ioa_toml_source)` pairs.
    pub specs: Vec<(String, String)>,
    /// CSDL XML source (None if app has no IOA specs).
    pub csdl: Option<String>,
    /// Cedar policy sources (may be empty).
    pub cedar_policies: Vec<String>,
    /// WASM module binaries as `(module_name, wasm_bytes)` pairs.
    pub wasm_modules: BTreeMap<String, Vec<u8>>,
}

// Backward-compatible alias: SkillBundle → AppBundle.
pub type SkillBundle = AppBundle;

// Backward-compatible type aliases.
pub type OsAppEntry = AppEntry;
pub type OsAppBundle = AppBundle;

// ── App Catalog (disk-loaded, cached) ───────────────────────────────

/// In-memory cache of discovered apps.
struct AppCatalog {
    /// Directory containing app bundles.
    apps_dir: PathBuf,
    /// Catalog entries (lightweight metadata).
    entries: Vec<AppEntry>,
    /// Mapping from app name to its directory path on disk.
    paths: BTreeMap<String, PathBuf>,
}

/// Global catalog, initialized on first access.
static CATALOG: OnceLock<RwLock<AppCatalog>> = OnceLock::new();

/// Get or initialize the global app catalog.
fn catalog() -> &'static RwLock<AppCatalog> {
    CATALOG.get_or_init(|| RwLock::new(AppCatalog::discover()))
}

/// Override the OS apps directory. Must be called before any catalog access.
///
/// If the catalog was already initialized, it is replaced.
pub fn set_os_apps_dir(dir: PathBuf) {
    let new_catalog = AppCatalog::from_dir(dir);
    match CATALOG.get() {
        Some(lock) => {
            *lock.write().unwrap() = new_catalog; // ci-ok: infallible lock
        }
        None => {
            let _ = CATALOG.set(RwLock::new(new_catalog));
        }
    }
}

/// Add an additional directory of apps to the catalog.
///
/// Scans the directory and merges discovered apps into the existing catalog.
/// Apps in the new directory do NOT replace existing apps with the same name.
/// Use this to register reference apps or project-specific apps alongside
/// the main os-apps directory.
pub fn add_os_apps_dir(dir: PathBuf) {
    let additional = AppCatalog::from_dir(dir);
    let cat = catalog();
    let mut lock = cat.write().unwrap(); // ci-ok: infallible lock
    for (name, path) in additional.paths {
        lock.paths.entry(name.clone()).or_insert(path);
    }
    for entry in additional.entries {
        if !lock.entries.iter().any(|e| e.name == entry.name) {
            lock.entries.push(entry);
        }
    }
}

/// Re-scan the OS apps directory and refresh the catalog.
///
/// Call this after modifying app files on disk to pick up changes
/// without restarting the server.
pub fn reload_os_apps() {
    let cat = catalog().read().unwrap(); // ci-ok: infallible lock
    let dir = cat.apps_dir.clone();
    drop(cat);
    let new = AppCatalog::from_dir(dir);
    *catalog().write().unwrap() = new; // ci-ok: infallible lock
}

/// Backward-compatible alias.
pub fn set_skills_dir(dir: PathBuf) {
    set_os_apps_dir(dir);
}

/// Backward-compatible alias.
pub fn reload_skills() {
    reload_os_apps();
}

impl AppCatalog {
    /// Discover the apps directory and scan it.
    fn discover() -> Self {
        // Priority 1: TEMPER_OS_APPS_DIR env var.
        if let Ok(dir) = std::env::var("TEMPER_OS_APPS_DIR") {
            // determinism-ok: env var read at startup for configuration
            let path = PathBuf::from(dir);
            if path.is_dir() {
                tracing::info!(
                    "Loading OS apps from TEMPER_OS_APPS_DIR: {}",
                    path.display()
                );
                return Self::from_dir(path);
            }
        }

        // Priority 1b: legacy TEMPER_SKILLS_DIR env var.
        if let Ok(dir) = std::env::var("TEMPER_SKILLS_DIR") {
            let path = PathBuf::from(dir);
            if path.is_dir() {
                tracing::info!(
                    "Loading OS apps from legacy TEMPER_SKILLS_DIR: {}",
                    path.display()
                );
                return Self::from_dir(path);
            }
        }

        // Priority 2: Relative to this crate's source (works in dev and cargo test).
        let compile_time_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("os-apps");
        if compile_time_dir.is_dir() {
            let canonical = compile_time_dir
                .canonicalize()
                .unwrap_or(compile_time_dir.clone());
            tracing::info!("Loading OS apps from workspace: {}", canonical.display());
            return Self::from_dir(canonical);
        }

        // Priority 3: ./os-apps/ relative to CWD.
        let cwd_dir = PathBuf::from("os-apps");
        if cwd_dir.is_dir() {
            let canonical = cwd_dir.canonicalize().unwrap_or(cwd_dir.clone());
            tracing::info!("Loading OS apps from CWD: {}", canonical.display());
            return Self::from_dir(canonical);
        }

        // Priority 4: ./skills/ (legacy fallback).
        let legacy_cwd_dir = PathBuf::from("skills");
        if legacy_cwd_dir.is_dir() {
            let canonical = legacy_cwd_dir
                .canonicalize()
                .unwrap_or(legacy_cwd_dir.clone());
            tracing::info!(
                "Loading OS apps from legacy CWD skills/: {}",
                canonical.display()
            );
            return Self::from_dir(canonical);
        }

        tracing::warn!(
            "No os-apps directory found. Set TEMPER_OS_APPS_DIR (or legacy TEMPER_SKILLS_DIR)."
        );
        Self {
            apps_dir: PathBuf::new(),
            entries: Vec::new(),
            paths: BTreeMap::new(),
        }
    }

    /// Build catalog from a specific directory.
    fn from_dir(dir: PathBuf) -> Self {
        let mut entries = Vec::new();
        let mut paths = BTreeMap::new();

        let read_dir = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) => {
                tracing::warn!("Failed to read apps directory {}: {e}", dir.display());
                return Self {
                    apps_dir: dir,
                    entries,
                    paths,
                };
            }
        };

        let mut app_dirs: Vec<_> = read_dir
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
            .collect();
        // Deterministic ordering.
        app_dirs.sort_by_key(|e| e.file_name());

        for entry in app_dirs {
            let app_dir = entry.path();
            let dir_name = entry.file_name().to_string_lossy().to_string();

            // Try reading app.toml manifest.
            let manifest = read_app_manifest(&app_dir);

            let app_name = manifest
                .as_ref()
                .map(|m| m.name.clone())
                .unwrap_or_else(|| dir_name.clone());

            // Scan for IOA specs to determine entity types.
            let ioa_files = find_ioa_files(&app_dir);
            let entity_types: Vec<String> = ioa_files
                .iter()
                .filter_map(|(_, ioa_path)| {
                    let source = std::fs::read_to_string(ioa_path).ok()?;
                    let parsed = automaton::parse_automaton(&source).ok()?;
                    Some(parsed.automaton.name)
                })
                .collect();

            // Look for app guide.
            let app_guide = read_app_guide(&app_dir);

            // Determine description: manifest > app_guide > default.
            let description = manifest
                .as_ref()
                .filter(|m| !m.description.is_empty())
                .map(|m| m.description.clone())
                .or_else(|| {
                    app_guide
                        .as_ref()
                        .and_then(|guide| extract_description(guide))
                })
                .unwrap_or_else(|| format!("App: {app_name}"));

            let version = manifest
                .as_ref()
                .map(|m| m.version.clone())
                .unwrap_or_else(|| "0.1.0".to_string());

            let dependencies = manifest
                .as_ref()
                .map(|m| m.dependencies.clone())
                .unwrap_or_default();

            paths.insert(dir_name, app_dir);
            entries.push(AppEntry {
                name: app_name,
                description,
                entity_types,
                version,
                app_guide,
                dependencies,
            });
        }

        Self {
            apps_dir: dir,
            entries,
            paths,
        }
    }
}

/// Find all IOA spec files in an app directory.
///
/// Handles both layouts:
/// - Root-level: `app-name/*.ioa.toml` + `app-name/model.csdl.xml`
/// - Specs subdir: `app-name/specs/*.ioa.toml` + `app-name/specs/model.csdl.xml`
///
/// Returns `(entity_type_hint, path)` pairs. The entity type is extracted
/// from the IOA file's `[automaton] name` field, not the filename.
fn find_ioa_files(app_dir: &Path) -> Vec<(String, PathBuf)> {
    let mut results = Vec::new();
    let mut seen_names = std::collections::HashSet::new();

    // Scan root first (takes priority for dedup).
    scan_dir_for_ioa(app_dir, &mut results, &mut seen_names);

    // Then scan specs/ subdirectory.
    let specs_dir = app_dir.join("specs");
    if specs_dir.is_dir() {
        scan_dir_for_ioa(&specs_dir, &mut results, &mut seen_names);
    }

    results
}

/// Scan a single directory for `*.ioa.toml` files.
fn scan_dir_for_ioa(
    dir: &Path,
    results: &mut Vec<(String, PathBuf)>,
    seen: &mut std::collections::HashSet<String>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".ioa.toml"))
        .collect();
    files.sort_by_key(|e| e.file_name());

    for entry in files {
        let path = entry.path();
        let fname = entry.file_name().to_string_lossy().to_string();
        // Use filename as dedup key.
        if !seen.insert(fname) {
            continue;
        }
        results.push((String::new(), path));
    }
}

/// Find the CSDL model file in an app directory.
fn find_csdl(app_dir: &Path) -> Option<PathBuf> {
    // Root-level first.
    let root = app_dir.join("model.csdl.xml");
    if root.exists() {
        return Some(root);
    }
    // Then specs/.
    let specs = app_dir.join("specs").join("model.csdl.xml");
    if specs.exists() {
        return Some(specs);
    }
    // Then a dedicated csdl/ directory.
    let csdl_dir = app_dir.join("csdl");
    if csdl_dir.is_dir() {
        let Ok(entries) = std::fs::read_dir(&csdl_dir) else {
            return None;
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".csdl.xml"))
            .map(|e| e.path())
            .collect();
        files.sort();
        if let Some(first) = files.into_iter().next() {
            return Some(first);
        }
    }
    None
}

/// Find all Cedar policy files in an app directory.
fn find_cedar_policies(app_dir: &Path) -> Vec<PathBuf> {
    let policies_dir = app_dir.join("policies");
    if !policies_dir.is_dir() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(&policies_dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".cedar"))
        .map(|e| e.path())
        .collect();
    files.sort();
    files
}

/// Find compiled WASM module binaries in an app directory.
///
/// Scans `wasm/*/target/wasm32-unknown-unknown/release/{module_name}.wasm`
/// where `{module_name}` matches the directory name under `wasm/`.
fn find_wasm_modules(app_dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut modules = BTreeMap::new();
    let wasm_dir = app_dir.join("wasm");
    if !wasm_dir.is_dir() {
        return modules;
    }
    let Ok(entries) = std::fs::read_dir(&wasm_dir) else {
        return modules;
    };
    let mut dirs: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
        .collect();
    dirs.sort_by_key(|e| e.file_name());

    for entry in dirs {
        let module_name = entry.file_name().to_string_lossy().to_string();
        // Skip target directories that cargo creates.
        if module_name == "target" {
            continue;
        }
        let wasm_path = entry
            .path()
            .join("target")
            .join("wasm32-unknown-unknown")
            .join("release")
            .join(format!("{module_name}.wasm"));
        if wasm_path.exists() {
            match std::fs::read(&wasm_path) {
                Ok(bytes) => {
                    tracing::debug!(
                        module = %module_name,
                        size = bytes.len(),
                        "Found WASM module in OS app"
                    );
                    modules.insert(module_name, bytes);
                }
                Err(e) => {
                    tracing::warn!(
                        module = %module_name,
                        error = %e,
                        "Failed to read WASM module binary"
                    );
                }
            }
        }
    }
    modules
}

/// Read the app guide markdown (APP.md/app.md first, then skill.md/SKILL.md fallback).
fn read_app_guide(app_dir: &Path) -> Option<String> {
    for name in &["APP.md", "app.md", "skill.md", "SKILL.md"] {
        let path = app_dir.join(name);
        if let Ok(content) = std::fs::read_to_string(&path) {
            return Some(content);
        }
    }
    None
}

/// Extract a description from app guide markdown.
///
/// Looks for the first non-header, non-empty line, or a TOML frontmatter
/// `description` field.
fn extract_description(guide: &str) -> Option<String> {
    // Check for TOML frontmatter (+++...+++ delimited).
    if let Some(rest) = guide.strip_prefix("+++")
        && let Some(end) = rest.find("+++")
    {
        let frontmatter = &rest[..end];
        for line in frontmatter.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("description")
                && let Some(val) = trimmed.split('=').nth(1)
            {
                let val = val.trim().trim_matches('"');
                if !val.is_empty() {
                    return Some(val.to_string());
                }
            }
        }
    }
    // Fall back to first paragraph after any heading.
    for line in guide.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with("+++") {
            continue;
        }
        return Some(trimmed.to_string());
    }
    None
}

// ── Public API ──────────────────────────────────────────────────────

/// List all available OS apps.
pub fn list_os_apps() -> Vec<AppEntry> {
    let cat = catalog().read().unwrap(); // ci-ok: infallible lock
    cat.entries.clone()
}

/// Backward-compatible alias.
pub fn list_skills() -> Vec<AppEntry> {
    list_os_apps()
}

/// Get the full spec bundle for an OS app by name.
///
/// Reads IOA, CSDL, and Cedar files from disk on each call so changes
/// are picked up without a rebuild.
pub fn get_os_app(name: &str) -> Option<AppBundle> {
    let cat = catalog().read().unwrap(); // ci-ok: infallible lock
    let app_dir = cat.paths.get(name)?;
    load_app_bundle(app_dir)
}

/// Backward-compatible alias.
pub fn get_skill(name: &str) -> Option<AppBundle> {
    get_os_app(name)
}

/// Get the full app guide markdown for an app by name.
pub fn get_app_guide(name: &str) -> Option<String> {
    let cat = catalog().read().unwrap(); // ci-ok: infallible lock
    cat.entries
        .iter()
        .find(|e| e.name == name)
        .and_then(|e| e.app_guide.clone())
}

/// Backward-compatible alias.
pub fn get_skill_guide(name: &str) -> Option<String> {
    get_app_guide(name)
}

/// Load a complete app bundle from a directory on disk.
fn load_app_bundle(app_dir: &Path) -> Option<AppBundle> {
    let ioa_files = find_ioa_files(app_dir);

    // Read IOA specs, extracting entity type from the parsed automaton name.
    let mut specs = Vec::new();
    for (_hint, path) in &ioa_files {
        let source = std::fs::read_to_string(path).ok()?;
        let parsed = automaton::parse_automaton(&source).ok()?;
        specs.push((parsed.automaton.name, source));
    }

    // Read CSDL (optional — apps without specs won't have CSDL).
    let csdl = find_csdl(app_dir).and_then(|p| std::fs::read_to_string(&p).ok());

    // Read Cedar policies.
    let cedar_policies: Vec<String> = find_cedar_policies(app_dir)
        .into_iter()
        .filter_map(|p| std::fs::read_to_string(&p).ok())
        .collect();

    // Read WASM module binaries from wasm/*/target/wasm32-unknown-unknown/release/*.wasm.
    let wasm_modules = find_wasm_modules(app_dir);

    // Read app guide to check if there's anything at all.
    let app_guide = read_app_guide(app_dir);

    // Return None only if the app has nothing at all.
    if specs.is_empty()
        && cedar_policies.is_empty()
        && wasm_modules.is_empty()
        && app_guide.is_none()
        && csdl.is_none()
    {
        return None;
    }

    Some(AppBundle {
        specs,
        csdl,
        cedar_policies,
        wasm_modules,
    })
}

fn os_app_dependencies(name: &str) -> Vec<String> {
    // Check manifest first.
    let cat = catalog().read().unwrap(); // ci-ok: infallible lock
    if let Some(entry) = cat.entries.iter().find(|e| e.name == name)
        && !entry.dependencies.is_empty()
    {
        return entry.dependencies.clone();
    }
    // Hardcoded fallback.
    match name {
        // TemperAgent persists conversation/files in TemperFS entities.
        "temper-agent" => vec!["temper-fs".to_string()],
        _ => vec![],
    }
}

/// Install an OS app into a tenant (workspace).
///
/// Reads app files from disk, runs the verification cascade, registers
/// specs in the SpecRegistry, loads Cedar policies, and **persists
/// everything to the platform DB** so specs survive redeployments.
///
/// **Write ordering:** Turso first, then memory. If Turso persistence fails
/// the operation returns an error *before* touching in-memory state, so the
/// registry and Cedar engine stay consistent with the durable store.
pub async fn install_os_app(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
) -> Result<InstallResult, String> {
    for dependency in os_app_dependencies(app_name) {
        install_os_app_without_dependencies(state, tenant, &dependency).await?;
    }
    install_os_app_without_dependencies(state, tenant, app_name).await
}

async fn install_os_app_without_dependencies(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
) -> Result<InstallResult, String> {
    let bundle =
        get_os_app(app_name).ok_or_else(|| format!("OS app '{app_name}' not found in catalog"))?;
    let tenant_id = TenantId::new(tenant);

    // Classify each bundle spec as added / updated / skipped, and compute the
    // merged CSDL — both require the registry read lock, so we do them together.
    let (mut added, mut updated, mut skipped, merged_csdl) = {
        let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
        let mut added = Vec::new();
        let mut updated = Vec::new();
        let mut skipped = Vec::new();
        for (entity_type, ioa_source) in &bundle.specs {
            let incoming_hash = temper_store_turso::spec_content_hash(ioa_source);
            match registry.get_spec(&tenant_id, entity_type) {
                Some(existing) => {
                    let existing_hash = temper_store_turso::spec_content_hash(&existing.ioa_source);
                    if incoming_hash == existing_hash {
                        skipped.push(entity_type.to_string());
                    } else {
                        updated.push(entity_type.to_string());
                    }
                }
                None => {
                    added.push(entity_type.to_string());
                }
            }
        }
        // App installs must preserve existing tenant types.
        let merged_csdl = if let Some(ref csdl) = bundle.csdl {
            if let Some(existing) = registry.get_tenant(&tenant_id) {
                let incoming = parse_csdl(csdl)
                    .map_err(|e| format!("Failed to parse CSDL for os-app '{app_name}': {e}"))?;
                Some(emit_csdl_xml(&merge_csdl(&existing.csdl, &incoming)))
            } else {
                Some(csdl.clone())
            }
        } else {
            // No CSDL in bundle; keep existing if any.
            registry
                .get_tenant(&tenant_id)
                .map(|t| emit_csdl_xml(&t.csdl))
        };
        (added, updated, skipped, merged_csdl)
    };
    // Sort for deterministic output.
    added.sort();
    updated.sort();
    skipped.sort();

    // Build the full Cedar policy text for this tenant (existing + new).
    let combined_policy = if !bundle.cedar_policies.is_empty() {
        let combined: String = bundle.cedar_policies.join("\n");
        let policies = state.server.tenant_policies.read().unwrap(); // ci-ok: infallible lock
        let existing = policies.get(tenant).cloned().unwrap_or_default();
        let full_text = if existing.is_empty() {
            combined
        } else {
            format!("{existing}\n{combined}")
        };
        Some(full_text)
    } else {
        None
    };

    // ── Step 1: Persist to Turso FIRST (if available). ──────────────
    // If any write fails, bail before touching in-memory state.
    if let Some(ref store) = state.server.event_store
        && let Some(turso) = store.platform_turso_store()
    {
        let mut spec_sources: BTreeMap<String, String> = turso
            .load_specs()
            .await
            .map_err(|e| format!("Failed to load existing specs for tenant '{tenant}': {e}"))?
            .into_iter()
            .filter(|row| row.tenant == tenant)
            .map(|row| (row.entity_type, row.ioa_source))
            .collect();

        for (entity_type, ioa_source) in &bundle.specs {
            spec_sources.insert(entity_type.clone(), ioa_source.clone());
        }

        if let Some(ref merged) = merged_csdl {
            for (entity_type, ioa_source) in spec_sources {
                let hash = temper_store_turso::spec_content_hash(&ioa_source);
                turso
                    .upsert_spec(tenant, &entity_type, &ioa_source, merged, &hash)
                    .await
                    .map_err(|e| format!("Failed to persist spec {entity_type}: {e}"))?;
            }
        }
        if let Some(ref policy_text) = combined_policy {
            turso
                .upsert_tenant_policy(tenant, policy_text)
                .await
                .map_err(|e| format!("Failed to persist Cedar policy: {e}"))?;
        }
        turso
            .record_installed_app(tenant, app_name)
            .await
            .map_err(|e| format!("Failed to record os-app installation: {e}"))?;
        // Commit all specs atomically after all writes succeed.
        turso
            .commit_specs(tenant)
            .await
            .map_err(|e| format!("Failed to commit specs: {e}"))?;
    } else if let Some(ref store) = state.server.event_store
        && let Some(ps) = store.platform_store()
    {
        if let Some(ref merged) = merged_csdl {
            for (entity_type, ioa_source) in &bundle.specs {
                let hash = temper_store_turso::spec_content_hash(ioa_source);
                ps.upsert_spec(tenant, entity_type, ioa_source, merged, &hash)
                    .await
                    .map_err(|e| format!("Failed to persist spec {entity_type}: {e}"))?;
            }
        }
        if let Some(ref policy_text) = combined_policy {
            ps.upsert_tenant_policy(tenant, policy_text)
                .await
                .map_err(|e| format!("Failed to persist Cedar policy: {e}"))?;
        }
        ps.record_installed_app(tenant, app_name)
            .await
            .map_err(|e| format!("Failed to record os-app installation: {e}"))?;
        // Commit all specs atomically after all writes succeed.
        ps.commit_specs(tenant)
            .await
            .map_err(|e| format!("Failed to commit specs: {e}"))?;
    }

    // ── Step 2: Bootstrap into memory (verification + registry). ────
    // Only process specs whose content has changed (added or updated);
    // skipped specs are already loaded with identical content.
    if !bundle.specs.is_empty() {
        let specs_to_bootstrap: Vec<(&str, &str)> = bundle
            .specs
            .iter()
            .filter(|(entity_type, _)| !skipped.contains(entity_type))
            .map(|(et, src)| (et.as_str(), src.as_str()))
            .collect();

        if !specs_to_bootstrap.is_empty() {
            let verified_cache = if let Some(ref store) = state.server.event_store
                && let Some(turso) = store.platform_turso_store()
            {
                turso
                    .load_verification_cache(tenant)
                    .await
                    .unwrap_or_default()
            } else if let Some(ref store) = state.server.event_store
                && let Some(ps) = store.platform_store()
            {
                ps.load_verification_cache(tenant).await.unwrap_or_default()
            } else {
                std::collections::BTreeMap::new()
            };

            if let Some(ref merged) = merged_csdl {
                bootstrap::bootstrap_tenant_specs(
                    state,
                    tenant,
                    merged,
                    &specs_to_bootstrap,
                    true,
                    &format!("OsApp({app_name})"),
                    &verified_cache,
                );
            }
        }
    }

    // ── Step 3: Load Cedar policies into memory. ────────────────────
    if let Some(ref policy_text) = combined_policy {
        if let Err(e) = state
            .server
            .authz
            .reload_tenant_policies(tenant, policy_text)
        {
            tracing::warn!(
                tenant,
                error = %e,
                "Failed to reload tenant Cedar policies after os-app install"
            );
        } else {
            let mut policies = state.server.tenant_policies.write().unwrap(); // ci-ok: infallible lock
            policies.insert(tenant.to_string(), policy_text.clone());
        }
    }

    // ── Step 4: Compile and register WASM modules. ──────────────────
    let mut wasm_registered = Vec::new();
    for (module_name, wasm_bytes) in &bundle.wasm_modules {
        match state.server.wasm_engine.compile_and_cache(wasm_bytes) {
            Ok(hash) => {
                // Persist to Turso FIRST for durability.
                if let Err(e) = state
                    .server
                    .upsert_wasm_module(tenant, module_name, wasm_bytes, &hash)
                    .await
                {
                    tracing::warn!(
                        tenant,
                        module = %module_name,
                        error = %e,
                        "Failed to persist WASM module to durable store (continuing in-memory only)"
                    );
                }
                // Register in module registry.
                {
                    let mut wasm_reg = state.server.wasm_module_registry.write().unwrap(); // ci-ok: infallible lock
                    wasm_reg.register(&tenant_id, module_name, &hash);
                }
                tracing::info!(
                    tenant,
                    module = %module_name,
                    hash = %hash,
                    size = wasm_bytes.len(),
                    "WASM module loaded from OS app"
                );
                wasm_registered.push(module_name.clone());
            }
            Err(e) => {
                tracing::warn!(
                    tenant,
                    module = %module_name,
                    error = %e,
                    "Failed to compile WASM module from OS app"
                );
            }
        }
    }

    tracing::info!(
        "Installed os-app '{app_name}' for tenant '{tenant}': \
         added={:?} updated={:?} skipped={:?} wasm={:?}",
        added,
        updated,
        skipped,
        wasm_registered,
    );

    Ok(InstallResult {
        added,
        updated,
        skipped,
        wasm_modules: wasm_registered,
    })
}

/// Backward-compatible alias.
pub async fn install_skill(
    state: &PlatformState,
    tenant: &str,
    skill_name: &str,
) -> Result<InstallResult, String> {
    install_os_app(state, tenant, skill_name).await
}

#[cfg(test)]
mod mod_test;
