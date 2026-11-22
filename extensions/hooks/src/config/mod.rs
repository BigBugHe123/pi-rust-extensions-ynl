pub mod claude_policy;
pub mod codex;
mod codex_bundled;
mod codex_cloud;
mod codex_managed;
pub(crate) mod codex_plugin_bundle;
mod codex_plugins;
pub mod managed;
pub mod options;
pub mod policy_command;
pub mod policy_sources;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pi_ext_sdk::paths::project_path;
use serde_json::Value;

use crate::exec::http::HttpPolicy;
use crate::model::{HookDef, Layer, Plugin, Scope, Source};
use crate::paths::{Paths, project_root};
use crate::policy::{Conflict, cross_source_duplicates, find_conflicts};
use claude_policy::PolicyDoc;
pub use options::{Options, ScopeOption, read_json, write_option};

/// Options a project file cannot set, because the file can come from the repository.
const GLOBAL_ONLY: [&str; 2] = ["trustAll", "conflicts"];

pub struct LoadInput<'a> {
    pub paths: &'a Paths,
    pub agent_dir: &'a Path,
    pub cwd: &'a Path,
    pub project_trusted: bool,
}

#[derive(Debug, Default)]
pub struct Loaded {
    pub options: Options,
    pub hooks: Vec<HookDef>,
    /// Project files skipped because the project is not trusted.
    pub untrusted: Vec<PathBuf>,
    pub global_file: PathBuf,
    pub project_file: PathBuf,
    pub http: HttpPolicy,
    /// Event and matcher keys that different hooks from more than one layer define.
    pub conflicts: Vec<Conflict>,
    /// Commands that Claude or pi config and Codex config both register for one event.
    pub duplicates: Vec<String>,
}

pub fn options_files(agent_dir: &Path, cwd: &Path) -> (PathBuf, PathBuf) {
    let global = agent_dir.join("ext").join("hooks.json");
    let project = pi_ext_sdk::paths::project_ext_config(cwd, "hooks");
    (global, project)
}

pub fn load(input: &LoadInput) -> Loaded {
    let (global_file, project_file) = options_files(input.agent_dir, input.cwd);
    let mut loaded = Loaded {
        global_file: global_file.clone(),
        project_file: project_file.clone(),
        ..Loaded::default()
    };
    let global_doc = read_json(&global_file).unwrap_or(Value::Null);
    let mut project_doc =
        project_layer(input, &project_file, &mut loaded).and_then(|p| read_json(&p));
    if let Some(Value::Object(doc)) = project_doc.as_mut() {
        for key in GLOBAL_ONLY {
            if doc.remove(key).is_some() {
                pi_ext_sdk::info!("hooks: ignoring {key} in {}", project_file.display());
            }
        }
    }
    let layers: Vec<&Value> = [Some(&global_doc), project_doc.as_ref()]
        .into_iter()
        .flatten()
        .collect();
    loaded.options = Options::merge(&layers);
    loaded.options.switches = options::read_switches(&global_doc, options::SWITCHES);
    let options = loaded.options.clone();
    // A project file can come from the repository, so only the global file turns off managed hooks.
    let managed_on = Options::merge(&[&global_doc]);
    let policy = claude_policy::claude_policy_docs(input.paths, &options);
    let off = if options.claude || managed_on.claude {
        load_claude(input, &options, managed_on.claude, &policy, &mut loaded)
    } else {
        loaded.http = HttpPolicy::merge(policy.iter().map(|policy| &policy.doc));
        claude_hooks_off(&policy, std::iter::empty())
    };
    let plugin_gate = PluginGate {
        managed: claude_policy::enabled_plugins(&policy),
        off,
        forced: options.switch_on("managed/forceEnabledPlugins"),
    };
    if options.codex || managed_on.codex {
        load_codex(input, &options, managed_on.codex, &mut loaded);
    }
    if options.pi {
        let docs = [
            (Scope::Global, &global_file, Some(&global_doc)),
            (Scope::Project, &project_file, project_doc.as_ref()),
        ];
        for (scope, path, doc) in docs {
            let doc = doc.filter(|_| options.scope.allows(scope));
            if let Some(hooks) = doc.and_then(|d| d.get("hooks")) {
                let defs = parse_hooks(Source::Pi, scope, path, hooks);
                // Plugin hooks come from Claude Code plugins, so Claude hook gates cover them.
                loaded.hooks.extend(defs.into_iter().filter(|def| {
                    def.plugin
                        .as_ref()
                        .is_none_or(|plugin| plugin_gate.allows(plugin))
                }));
            }
        }
    }
    loaded.conflicts = find_conflicts(&loaded.hooks);
    loaded.duplicates = cross_source_duplicates(&loaded.hooks);
    loaded
}

fn last_bool<'a>(docs: impl Iterator<Item = &'a Value>, key: &str) -> Option<bool> {
    docs.filter_map(|doc| doc.get(key).and_then(Value::as_bool))
        .last()
}

fn with_layer(mut defs: Vec<HookDef>, layer: Layer) -> Vec<HookDef> {
    for def in &mut defs {
        def.layer = layer;
    }
    defs
}

/// Which Claude hooks the Claude settings turn off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HooksOff {
    None,
    /// All but managed hooks: `allowManagedHooksOnly`, or `disableAllHooks` in another file.
    NonManaged,
    /// Managed `disableAllHooks: true`.
    All,
}

/// `disableAllHooks` and `allowManagedHooksOnly` after Claude settings precedence. `user` holds
/// the user, project, and local docs, least specific first.
fn claude_hooks_off<'a>(policy: &[PolicyDoc], user: impl Iterator<Item = &'a Value>) -> HooksOff {
    let policy_disable = claude_policy::lock(policy, "disableAllHooks");
    if policy_disable == Some(true) {
        return HooksOff::All;
    }
    let managed_only = claude_policy::lock(policy, "allowManagedHooksOnly") == Some(true);
    let user_off = policy_disable.is_none() && last_bool(user, "disableAllHooks") == Some(true);
    if managed_only || user_off {
        HooksOff::NonManaged
    } else {
        HooksOff::None
    }
}

/// Which pi-native plugin groups the Claude settings let run.
struct PluginGate {
    /// Managed `enabledPlugins` by full `plugin@marketplace` id.
    managed: BTreeMap<String, bool>,
    off: HooksOff,
    /// `managed/forceEnabledPlugins`: force-enabled plugins run under `HooksOff::NonManaged`.
    forced: bool,
}

impl PluginGate {
    fn allows(&self, plugin: &Plugin) -> bool {
        let value = plugin.id.as_ref().and_then(|id| self.managed.get(id));
        match (self.off, value) {
            (_, Some(false)) | (HooksOff::All, _) => false,
            (HooksOff::None, _) => true,
            (HooksOff::NonManaged, Some(true)) => self.forced,
            (HooksOff::NonManaged, None) => false,
        }
    }
}

/// Claude user, project, local, and managed settings.
fn load_claude(
    input: &LoadInput,
    options: &Options,
    managed_on: bool,
    policy: &[PolicyDoc],
    loaded: &mut Loaded,
) -> HooksOff {
    let scoped = |scope: Scope| options.scope.allows(scope);
    let root = project_root(input.cwd, ".claude");
    let mut files = vec![(Layer::Global, input.paths.claude_dir.join("settings.json"))];
    if scoped(Scope::Project) {
        let names = [
            ("settings.json", Layer::Project(0)),
            ("settings.local.json", Layer::Local),
        ];
        for (name, layer) in names {
            let path = project_path(&root, Path::new(".claude").join(name));
            if same_path(&path, &files[0].1) {
                continue;
            }
            if let Some(path) = project_layer(input, &path, loaded) {
                files.push((layer, path));
            }
        }
    }
    let docs: Vec<(Layer, PathBuf, Value)> = files
        .into_iter()
        .filter(|_| options.claude)
        .filter_map(|(layer, path)| read_json(&path).map(|doc| (layer, path, doc)))
        .collect();
    let user_docs = || docs.iter().map(|(_, _, doc)| doc);
    let policy_docs = policy.iter().map(|policy| &policy.doc);
    loaded.http = HttpPolicy::merge(policy_docs.chain(user_docs()));
    let off = claude_hooks_off(policy, user_docs());
    match off {
        HooksOff::All => {
            pi_ext_sdk::info!("hooks: managed disableAllHooks is set; Claude hooks are off");
        }
        HooksOff::NonManaged => pi_ext_sdk::info!(
            "hooks: Claude settings allow only managed and force-enabled plugin Claude hooks"
        ),
        HooksOff::None => {}
    }
    if off != HooksOff::All && managed_on {
        for policy in policy.iter().filter(|policy| !policy.displaced) {
            if let Some(hooks) = policy.doc.get("hooks") {
                let defs = parse_hooks(Source::Claude, Scope::Global, &policy.path, hooks);
                let mut defs = with_layer(defs, Layer::Managed);
                for def in &mut defs {
                    def.user_writable = policy.source.user_writable();
                }
                loaded.hooks.extend(defs);
            }
        }
    }
    if off == HooksOff::None && options.claude {
        // The user file sets policy keys in every `scope`; only its hooks follow `scope`.
        let docs = docs
            .iter()
            .filter(|(layer, _, _)| *layer != Layer::Global || scoped(Scope::Global));
        for (layer, path, doc) in docs {
            if let Some(hooks) = doc.get("hooks") {
                let scope = if *layer == Layer::Global {
                    Scope::Global
                } else {
                    Scope::Project
                };
                let defs = parse_hooks(Source::Claude, scope, path, hooks);
                loaded.hooks.extend(with_layer(defs, *layer));
            }
        }
    }
    off
}

/// Codex managed layers (system dir, legacy file, MDM, requirements), cloud, user, project,
/// and plugin layers. Each managed method, the cloud cache, and plugin loading have a switch.
fn load_codex(input: &LoadInput, options: &Options, managed_on: bool, loaded: &mut Loaded) {
    let scoped = |scope: Scope| options.scope.allows(scope);
    let mut sources = codex_managed::ManagedSources::load(input.paths, options);
    let mut layers: Vec<(Scope, Layer, codex::CodexLayer)> = Vec::new();
    if let Some(system) = input.paths.codex_system.as_deref()
        && options.switch_on(options::CODEX_SYSTEM)
    {
        layers.push((Scope::Global, Layer::Managed, codex::load_layer(system)));
    }
    let mut cloud_layers = layers.len()..layers.len();
    if scoped(Scope::Global) {
        let cloud = std::mem::take(&mut sources.cloud_configs);
        layers.extend(cloud.into_iter().map(|l| (Scope::Global, Layer::Global, l)));
        cloud_layers.end = layers.len();
        let user = codex::load_layer(&input.paths.codex_home);
        layers.push((Scope::Global, Layer::Global, user));
    } else {
        // The user `[features] hooks` applies in every `scope`; only its hooks follow `scope`.
        let feature = codex::load_layer(&input.paths.codex_home).feature;
        let user = codex::CodexLayer {
            feature,
            ..Default::default()
        };
        layers.push((Scope::Global, Layer::Global, user));
    }
    if scoped(Scope::Project) {
        let markers = managed::codex_root_markers(&input.paths.codex_home);
        for (depth, dir) in codex_project_dirs(input.cwd, &markers)
            .into_iter()
            .enumerate()
        {
            if same_path(&dir, &input.paths.codex_home) {
                continue;
            }
            let any = ["hooks.json", "config.toml"]
                .iter()
                .filter_map(|name| project_layer(input, &dir.join(name), loaded))
                .count();
            if any > 0 {
                let layer = codex::load_layer(&dir);
                layers.push((Scope::Project, Layer::Project(depth), layer));
            }
        }
    }
    for layer in std::mem::take(&mut sources.top_configs) {
        layers.push((Scope::Global, Layer::Managed, layer));
    }
    // Any local process can sign the cloud cache, so its `hooks = false` keeps managed hooks.
    let feature = |cloud: bool| {
        let requirements = sources.requirements.iter().filter(|r| cloud || !r.cloud);
        let configs = layers
            .iter()
            .enumerate()
            .filter(|(i, _)| cloud || !cloud_layers.contains(i));
        requirements
            .rev()
            .find_map(|r| r.feature)
            .or_else(|| configs.rev().find_map(|(_, (_, _, l))| l.feature))
    };
    let cloud_off = feature(true) == Some(false);
    if cloud_off && feature(false) == Some(false) {
        pi_ext_sdk::info!("hooks: Codex [features] hooks = false; Codex hooks are off");
        return;
    }
    if cloud_off {
        pi_ext_sdk::info!("hooks: cloud [features] hooks = false; only managed Codex hooks run");
    }
    let managed_only = cloud_off || sources.pin(|r| r.managed_only) == Some(true);
    for req in sources.requirements.iter().rev().filter(|_| managed_on) {
        if let Some(hooks) = &req.hooks {
            let defs = parse_hooks(Source::Codex, Scope::Global, &req.file, hooks);
            loaded.hooks.extend(with_layer(defs, Layer::Managed));
        }
    }
    let states = codex::hook_states(&input.paths.codex_home);
    let configs: Vec<&Value> = layers
        .iter()
        .filter_map(|(_, _, l)| l.config.as_ref())
        .collect();
    let plugins = (!managed_only
        && options.codex
        && scoped(Scope::Global)
        && options.switch_on(options::CODEX_PLUGINS))
    .then(|| {
        let pinned = sources.pin(|r| r.plugins);
        codex_plugins::load(&input.paths.codex_home, &configs, pinned, &states)
    });
    for (scope, layer, codex_layer) in layers {
        let on = if layer == Layer::Managed {
            managed_on
        } else {
            options.codex && !managed_only
        };
        if !on {
            continue;
        }
        for (path, hooks) in codex_layer.hooks {
            let defs = with_layer(parse_hooks(Source::Codex, scope, &path, &hooks), layer);
            loaded.hooks.extend(defs.into_iter().filter(|def| {
                layer == Layer::Managed || states.get(&def.codex_state_key()) != Some(&false)
            }));
        }
    }
    loaded.hooks.extend(plugins.into_iter().flatten());
}

/// `.codex` dirs from the project root down to `cwd`; Codex loads each one. The root is the
/// nearest dir with one of `markers` (Codex `project_root_markers`); no markers means `cwd`.
fn codex_project_dirs(cwd: &Path, markers: &[String]) -> Vec<PathBuf> {
    let root = cwd
        .ancestors()
        .find(|dir| markers.iter().any(|marker| dir.join(marker).exists()))
        .unwrap_or(cwd);
    let mut dirs: Vec<PathBuf> = cwd
        .ancestors()
        .take_while(|dir| dir.starts_with(root))
        .map(|dir| project_path(dir, ".codex"))
        .filter(|dir| dir.is_dir())
        .collect();
    dirs.reverse();
    dirs
}

/// `Some(path)` when the project file exists and the project is trusted; records skipped files.
fn project_layer(input: &LoadInput, path: &Path, loaded: &mut Loaded) -> Option<PathBuf> {
    if !path.is_file() {
        return None;
    }
    if input.project_trusted {
        Some(path.to_path_buf())
    } else {
        loaded.untrusted.push(path.to_path_buf());
        None
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Prefix of the only variables a group's `pluginEnv` can set.
pub const PLUGIN_OPTION_PREFIX: &str = "CLAUDE_PLUGIN_OPTION_";

/// The string values of `pluginEnv` whose names start with [`PLUGIN_OPTION_PREFIX`].
fn plugin_env(group: &Value) -> Vec<(String, String)> {
    let map = group.get("pluginEnv").and_then(Value::as_object);
    map.into_iter()
        .flatten()
        .filter(|(key, _)| key.starts_with(PLUGIN_OPTION_PREFIX))
        .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
        .collect()
}

/// Reads the Claude settings shape: event -> matcher groups -> handlers.
pub fn parse_hooks(source: Source, scope: Scope, file: &Path, hooks: &Value) -> Vec<HookDef> {
    let Some(events) = hooks.as_object() else {
        return Vec::new();
    };
    let mut defs = Vec::new();
    for (event, groups) in events {
        for (group_index, group) in groups.as_array().into_iter().flatten().enumerate() {
            let matcher = group
                .get("matcher")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let field = |key: &str| group.get(key).and_then(Value::as_str).map(str::to_owned);
            let plugin = Plugin {
                id: field("plugin"),
                root: field("pluginRoot"),
                data: field("pluginData"),
                env: plugin_env(group),
            };
            let has_plugin = plugin.id.is_some() || plugin.root.is_some() || plugin.data.is_some();
            for (handler_index, handler) in group
                .get("hooks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
            {
                if !handler.is_object() {
                    continue;
                }
                let mut def = HookDef::new(
                    source,
                    scope,
                    file.to_path_buf(),
                    event,
                    matcher.clone(),
                    handler.clone(),
                );
                def.plugin = has_plugin.then(|| plugin.clone());
                def.position = (group_index, handler_index);
                defs.push(def);
            }
        }
    }
    defs
}

#[cfg(test)]
mod tests;
