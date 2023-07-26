use std::collections::BTreeMap;
use std::collections::HashMap;
use std::hash::Hasher;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use anyhow::bail;
use anyhow::Result;
use dprint_core::async_runtime::FutureExt;
use dprint_core::async_runtime::LocalBoxFuture;
use dprint_core::configuration::ConfigKeyMap;
use dprint_core::plugins::process::HostFormatCallback;
use dprint_core::plugins::CancellationToken;
use dprint_core::plugins::CriticalFormatError;
use dprint_core::plugins::FileMatchingInfo;
use dprint_core::plugins::FormatRange;
use dprint_core::plugins::FormatResult;
use dprint_core::plugins::HostFormatRequest;
use dprint_core::plugins::PluginInfo;
use indexmap::IndexMap;
use thiserror::Error;

use crate::arg_parser::CliArgs;
use crate::arg_parser::FilePatternArgs;
use crate::configuration::get_global_config;
use crate::configuration::get_plugin_config_map;
use crate::configuration::resolve_config_from_args;
use crate::configuration::resolve_config_from_path;
use crate::configuration::GetGlobalConfigOptions;
use crate::configuration::ResolvedConfig;
use crate::configuration::ResolvedConfigPath;
use crate::environment::Environment;
use crate::paths::get_and_resolve_file_paths;
use crate::paths::get_file_paths_by_plugins_and_err_if_empty;
use crate::paths::PluginNames;
use crate::plugins::output_plugin_config_diagnostics;
use crate::plugins::FormatConfig;
use crate::plugins::InitializedPlugin;
use crate::plugins::InitializedPluginFormatRequest;
use crate::plugins::OutputPluginConfigDiagnosticsError;
use crate::plugins::PluginNameResolutionMaps;
use crate::plugins::PluginResolver;
use crate::plugins::PluginWrapper;
use crate::utils::FastInsecureHasher;
use crate::utils::ResolvedPath;

pub enum GetPluginResult {
  HadDiagnostics(usize),
  Success(InitializedPluginWithConfig),
}

pub struct PluginWithConfig {
  pub plugin: Rc<PluginWrapper>,
  pub associations: Option<Vec<String>>,
  pub format_config: Arc<FormatConfig>,
  pub file_matching: FileMatchingInfo,
  config_diagnostic_count: tokio::sync::Mutex<Option<usize>>,
}

impl PluginWithConfig {
  pub fn new(plugin: Rc<PluginWrapper>, associations: Option<Vec<String>>, format_config: Arc<FormatConfig>, file_matching: FileMatchingInfo) -> Self {
    Self {
      plugin,
      associations,
      format_config,
      config_diagnostic_count: Default::default(),
      file_matching,
    }
  }

  /// Gets a hash that represents the current state of the plugin.
  /// This is used for the "incremental" feature to tell if a plugin has changed state.
  pub fn incremental_hash(&self, hasher: &mut impl Hasher) {
    use std::hash::Hash;
    // list everything in here that would affect formatting
    hasher.write(self.info().name.as_bytes());
    hasher.write(self.info().version.as_bytes());

    // serialize the config keys in order to prevent the hash from changing
    let sorted_config = self.format_config.raw.iter().collect::<BTreeMap<_, _>>();
    for (key, value) in sorted_config {
      hasher.write(key.as_bytes());
      value.hash(hasher);
    }

    if let Some(associations) = &self.associations {
      for association in associations {
        hasher.write(association.as_bytes());
      }
    }
    self.format_config.global.hash(hasher);
  }

  pub fn name(&self) -> &str {
    &self.info().name
  }

  pub fn info(&self) -> &PluginInfo {
    self.plugin.info()
  }

  pub async fn initialize(self: &Rc<Self>) -> Result<InitializedPluginWithConfig> {
    let instance = self.plugin.initialize().await?;
    Ok(InitializedPluginWithConfig {
      instance,
      plugin: self.clone(),
    })
  }

  pub async fn get_or_create_checking_config_diagnostics<TEnvironment: Environment>(self: &Rc<Self>, environment: &TEnvironment) -> Result<GetPluginResult> {
    // only allow one thread to initialize and output the diagnostics (we don't want the messages being spammed)
    let instance = self.initialize().await?;
    let mut config_diagnostic_count = self.config_diagnostic_count.lock().await;
    match *config_diagnostic_count {
      Some(count) => {
        if count > 0 {
          return Ok(GetPluginResult::HadDiagnostics(count));
        }
        Ok(GetPluginResult::Success(instance))
      }
      None => {
        let result = instance.output_config_diagnostics(environment).await?;
        if let Err(err) = result {
          environment.log_stderr(&err.to_string());
          *config_diagnostic_count = Some(err.diagnostic_count);
          Ok(GetPluginResult::HadDiagnostics(err.diagnostic_count))
        } else {
          *config_diagnostic_count = Some(0);
          Ok(GetPluginResult::Success(instance))
        }
      }
    }
  }
}

pub struct InitializedPluginWithConfigFormatRequest {
  pub file_path: PathBuf,
  pub file_text: String,
  pub range: FormatRange,
  pub override_config: ConfigKeyMap,
  pub on_host_format: HostFormatCallback,
  pub token: Arc<dyn CancellationToken>,
}

#[derive(Clone)]
pub struct InitializedPluginWithConfig {
  plugin: Rc<PluginWithConfig>,
  instance: Rc<dyn InitializedPlugin>,
}

impl InitializedPluginWithConfig {
  pub fn info(&self) -> &PluginInfo {
    self.plugin.info()
  }

  pub async fn resolved_config(&self) -> Result<String> {
    self.instance.resolved_config(self.plugin.format_config.clone()).await
  }

  pub async fn file_matching_info(&self) -> Result<FileMatchingInfo> {
    self.instance.file_matching_info(self.plugin.format_config.clone()).await
  }

  pub async fn license_text(&self) -> Result<String> {
    self.instance.license_text().await
  }

  pub async fn output_config_diagnostics<TEnvironment: Environment>(
    &self,
    environment: &TEnvironment,
  ) -> Result<Result<(), OutputPluginConfigDiagnosticsError>> {
    output_plugin_config_diagnostics(&self.info().name, &*self.instance, self.plugin.format_config.clone(), environment).await
  }

  pub async fn format_text(&self, request: InitializedPluginWithConfigFormatRequest) -> FormatResult {
    self
      .instance
      .format_text(InitializedPluginFormatRequest {
        file_path: request.file_path,
        file_text: request.file_text,
        range: request.range,
        config: self.plugin.format_config.clone(),
        override_config: request.override_config,
        on_host_format: request.on_host_format,
        token: request.token,
      })
      .await
  }
}

pub struct PluginsScope<TEnvironment: Environment> {
  environment: TEnvironment,
  pub config: Option<Rc<ResolvedConfig>>,
  pub plugins: IndexMap<String, Rc<PluginWithConfig>>,
  pub plugin_name_maps: PluginNameResolutionMaps,
}

impl<TEnvironment: Environment> PluginsScope<TEnvironment> {
  pub fn new(environment: TEnvironment, plugins: Vec<Rc<PluginWithConfig>>, config: Rc<ResolvedConfig>) -> Result<Self> {
    let plugin_name_maps = PluginNameResolutionMaps::from_plugins(plugins.iter().map(|p| p.as_ref()), &config.base_path)?;

    Ok(PluginsScope {
      environment,
      config: Some(config),
      plugin_name_maps,
      plugins: plugins.into_iter().map(|p| (p.name().to_string(), p)).collect(),
    })
  }

  pub fn process_plugin_count(&self) -> usize {
    self.plugins.values().filter(|p| p.plugin.is_process_plugin()).count()
  }

  pub fn get_plugin(&self, name: &str) -> Rc<PluginWithConfig> {
    self
      .plugins
      .get(name)
      .cloned()
      .unwrap_or_else(|| panic!("Expected to find plugin in collection: {}", name))
  }

  pub fn plugins_hash(&self) -> u64 {
    let mut hasher = FastInsecureHasher::default();
    for plugin in self.plugins.values() {
      plugin.incremental_hash(&mut hasher);
    }
    hasher.finish()
  }

  pub fn create_host_format_callback(self: &Rc<Self>) -> HostFormatCallback {
    let scope = self.clone();
    Rc::new(move |host_request| scope.format(host_request))
  }

  pub fn format(self: &Rc<Self>, request: HostFormatRequest) -> LocalBoxFuture<'static, FormatResult> {
    let plugin_names = self.plugin_name_maps.get_plugin_names_from_file_path(&request.file_path);
    log_verbose!(
      self.environment,
      "Host formatting {} - File length: {} - Plugins: [{}] - Range: {:?}",
      request.file_path.display(),
      request.file_text.len(),
      plugin_names.join(", "),
      request.range,
    );
    let scope = self.clone();
    async move {
      let mut file_text = request.file_text;
      let mut had_change = false;
      for plugin_name in plugin_names {
        let plugin = scope.get_plugin(&plugin_name);
        match plugin.get_or_create_checking_config_diagnostics(&scope.environment).await {
          Ok(GetPluginResult::Success(initialized_plugin)) => {
            let result = initialized_plugin
              .format_text(InitializedPluginWithConfigFormatRequest {
                file_path: request.file_path.clone(),
                file_text: file_text.clone(),
                range: request.range.clone(),
                override_config: request.override_config.clone(),
                on_host_format: scope.create_host_format_callback(),
                token: request.token.clone(),
              })
              .await;
            if let Some(new_text) = result? {
              file_text = new_text;
              had_change = true;
            }
          }
          Ok(GetPluginResult::HadDiagnostics(count)) => bail!("Had {} configuration errors.", count),
          Err(err) => return Err(CriticalFormatError(err).into()),
        }
      }

      Ok(if had_change { Some(file_text) } else { None })
    }
    .boxed_local()
  }
}

pub struct PluginsScopeAndPaths<TEnvironment: Environment> {
  pub scope: PluginsScope<TEnvironment>,
  pub file_paths_by_plugins: HashMap<PluginNames, Vec<PathBuf>>,
}

pub async fn resolve_plugins_scope_and_paths<TEnvironment: Environment>(
  args: &CliArgs,
  patterns: &FilePatternArgs,
  environment: &TEnvironment,
  plugin_resolver: &Rc<PluginResolver<TEnvironment>>,
) -> Result<Vec<PluginsScopeAndPaths<TEnvironment>>> {
  let resolve_plugins_options = ResolvePluginsOptions {
    // Skip checking these diagnostics when the user provides
    // plugins from the CLI args. They may be doing this to filter
    // to only specific plugins.
    check_top_level_unknown_property_diagnostics: args.plugins.is_empty(),
  };
  let resolver = PluginsAndPathsResolver {
    args,
    patterns,
    environment,
    plugin_resolver,
    resolve_plugins_options: &resolve_plugins_options,
  };

  resolver.resolve_config().await
}

struct PluginsAndPathsResolver<'a, TEnvironment: Environment> {
  args: &'a CliArgs,
  patterns: &'a FilePatternArgs,
  environment: &'a TEnvironment,
  plugin_resolver: &'a Rc<PluginResolver<TEnvironment>>,
  resolve_plugins_options: &'a ResolvePluginsOptions,
}

impl<'a, TEnvironment: Environment> PluginsAndPathsResolver<'a, TEnvironment> {
  pub async fn resolve_config(&self) -> Result<Vec<PluginsScopeAndPaths<TEnvironment>>> {
    let config = Rc::new(resolve_config_from_args(self.args, self.environment).await?);
    let scope = resolve_plugins_scope_and_err_if_empty(config.clone(), self.environment, self.plugin_resolver, self.resolve_plugins_options).await?;
    let glob_output = get_and_resolve_file_paths(&config, self.patterns, scope.plugins.values().map(|p| p.as_ref()), self.environment).await?;
    let file_paths_by_plugins = get_file_paths_by_plugins_and_err_if_empty(&config.base_path, &scope.plugin_name_maps, glob_output.file_paths)?;

    let mut result = vec![PluginsScopeAndPaths { scope, file_paths_by_plugins }];
    for config_file_path in glob_output.config_files {
      result.extend(self.resolve_sub_config(config_file_path, &config).await?);
    }

    Ok(result)
  }

  fn resolve_sub_config(
    &'a self,
    config_file_path: PathBuf,
    parent_config: &'a ResolvedConfig,
  ) -> LocalBoxFuture<'a, Result<Vec<PluginsScopeAndPaths<TEnvironment>>>> {
    async move {
      log_verbose!(self.environment, "Analyzing config file {}", config_file_path.display());
      let config_file_path = self.environment.canonicalize(&config_file_path)?;
      let config_path = ResolvedConfigPath {
        base_path: config_file_path.parent().unwrap(),
        resolved_path: ResolvedPath::local(config_file_path),
      };
      let mut config = resolve_config_from_path(&config_path, self.environment).await?;
      if !self.args.plugins.is_empty() {
        config.plugins = parent_config.plugins.clone();
      }
      let config = Rc::new(config);
      let scope = resolve_plugins_scope_and_err_if_empty(config.clone(), self.environment, self.plugin_resolver, self.resolve_plugins_options).await?;
      let glob_output = get_and_resolve_file_paths(&config, self.patterns, scope.plugins.values().map(|p| p.as_ref()), self.environment).await?;
      let file_paths_by_plugins = get_file_paths_by_plugins_and_err_if_empty(&config.base_path, &scope.plugin_name_maps, glob_output.file_paths)?;

      let mut result = vec![PluginsScopeAndPaths { scope, file_paths_by_plugins }];
      for config_file_path in glob_output.config_files {
        result.extend(self.resolve_sub_config(config_file_path, &config).await?);
      }

      Ok(result)
    }
    .boxed_local()
  }
}

pub async fn get_plugins_scope_from_args<TEnvironment: Environment>(
  args: &CliArgs,
  environment: &TEnvironment,
  plugin_resolver: &Rc<PluginResolver<TEnvironment>>,
) -> Result<PluginsScope<TEnvironment>, ResolvePluginsError> {
  match resolve_config_from_args(args, environment).await {
    Ok(config) => {
      resolve_plugins_scope(
        Rc::new(config),
        environment,
        plugin_resolver,
        &ResolvePluginsOptions {
          // Skip checking these diagnostics when the user provides
          // plugins from the CLI args. They may be doing this to filter
          // to only specific plugins.
          check_top_level_unknown_property_diagnostics: args.plugins.is_empty(),
        },
      )
      .await
    }
    // ignore
    Err(_) => Ok(PluginsScope {
      environment: environment.clone(),
      config: None,
      plugin_name_maps: Default::default(),
      plugins: Default::default(),
    }),
  }
}

#[derive(Debug, Error)]
#[error("No formatting plugins found. Ensure at least one is specified in the 'plugins' array of the configuration file.")]
pub struct NoPluginsFoundError;

#[derive(Debug, Error)]
#[error(transparent)]
pub struct ResolvePluginsError(#[from] anyhow::Error);

pub struct ResolvePluginsOptions {
  pub check_top_level_unknown_property_diagnostics: bool,
}

pub async fn resolve_plugins_scope_and_err_if_empty<TEnvironment: Environment>(
  config: Rc<ResolvedConfig>,
  environment: &TEnvironment,
  plugin_resolver: &Rc<PluginResolver<TEnvironment>>,
  options: &ResolvePluginsOptions,
) -> Result<PluginsScope<TEnvironment>> {
  let scope = resolve_plugins_scope(config, environment, plugin_resolver, options).await?;
  if scope.plugins.is_empty() {
    Err(NoPluginsFoundError.into())
  } else {
    Ok(scope)
  }
}

pub async fn resolve_plugins_scope<TEnvironment: Environment>(
  config: Rc<ResolvedConfig>,
  environment: &TEnvironment,
  plugin_resolver: &Rc<PluginResolver<TEnvironment>>,
  options: &ResolvePluginsOptions,
) -> Result<PluginsScope<TEnvironment>, ResolvePluginsError> {
  // resolve the plugins
  let plugins = plugin_resolver.resolve_plugins(config.plugins.clone()).await?;
  let mut config_map = config.config_map.clone();

  // resolve each plugin's configuration
  let mut plugins_with_config = Vec::new();
  for plugin in plugins.into_iter() {
    plugins_with_config.push((get_plugin_config_map(&plugin, &mut config_map)?, plugin));
  }

  // now get global config
  let global_config = get_global_config(
    config_map,
    environment,
    &GetGlobalConfigOptions {
      check_unknown_property_diagnostics: options.check_top_level_unknown_property_diagnostics,
    },
  )?;

  // create the scope
  let plugins = plugins_with_config.into_iter().map(|(plugin_config, plugin)| {
    let global_config = global_config.clone();
    let next_config_id = plugin_resolver.next_config_id();
    async move {
      let instance = plugin.initialize().await?;
      let format_config = Arc::new(FormatConfig {
        id: next_config_id,
        global: global_config,
        raw: plugin_config.properties,
      });
      let file_matching_info = instance.file_matching_info(format_config.clone()).await?;
      Ok::<_, anyhow::Error>(Rc::new(PluginWithConfig::new(
        plugin,
        plugin_config.associations,
        format_config,
        file_matching_info,
      )))
    }
    .boxed_local()
  });
  let plugin_results = dprint_core::async_runtime::future::join_all(plugins).await;
  let mut plugins = Vec::with_capacity(plugin_results.len());
  for result in plugin_results {
    plugins.push(result?);
  }

  Ok(PluginsScope::new(environment.clone(), plugins, config)?)
}
