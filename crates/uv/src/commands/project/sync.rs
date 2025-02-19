use anyhow::{Context, Result};
use itertools::Itertools;

use distribution_types::{Dist, ResolvedDist, SourceDist};
use pep508_rs::MarkerTree;
use uv_auth::store_credentials_from_url;
use uv_cache::Cache;
use uv_client::{Connectivity, FlatIndexClient, RegistryClientBuilder};
use uv_configuration::{Concurrency, ExtrasSpecification, HashCheckingMode, InstallOptions};
use uv_dispatch::BuildDispatch;
use uv_fs::CWD;
use uv_installer::SitePackages;
use uv_normalize::{PackageName, DEV_DEPENDENCIES};
use uv_python::{PythonDownloads, PythonEnvironment, PythonPreference, PythonRequest};
use uv_resolver::{FlatIndex, Lock};
use uv_types::{BuildIsolation, HashStrategy};
use uv_workspace::{DiscoveryOptions, InstallTarget, MemberDiscovery, VirtualProject, Workspace};

use crate::commands::pip::loggers::{DefaultInstallLogger, DefaultResolveLogger, InstallLogger};
use crate::commands::pip::operations::Modifications;
use crate::commands::project::lock::do_safe_lock;
use crate::commands::project::{ProjectError, SharedState};
use crate::commands::{pip, project, ExitStatus};
use crate::printer::Printer;
use crate::settings::{InstallerSettingsRef, ResolverInstallerSettings};

/// Sync the project environment.
#[allow(clippy::fn_params_excessive_bools)]
pub(crate) async fn sync(
    locked: bool,
    frozen: bool,
    package: Option<PackageName>,
    extras: ExtrasSpecification,
    dev: bool,
    install_options: InstallOptions,
    modifications: Modifications,
    python: Option<String>,
    python_preference: PythonPreference,
    python_downloads: PythonDownloads,
    settings: ResolverInstallerSettings,
    connectivity: Connectivity,
    concurrency: Concurrency,
    native_tls: bool,
    cache: &Cache,
    printer: Printer,
) -> Result<ExitStatus> {
    // Identify the project.
    let project = if frozen {
        VirtualProject::discover(
            &CWD,
            &DiscoveryOptions {
                members: MemberDiscovery::None,
                ..DiscoveryOptions::default()
            },
        )
        .await?
    } else if let Some(package) = package.as_ref() {
        VirtualProject::Project(
            Workspace::discover(&CWD, &DiscoveryOptions::default())
                .await?
                .with_current_project(package.clone())
                .with_context(|| format!("Package `{package}` not found in workspace"))?,
        )
    } else {
        VirtualProject::discover(&CWD, &DiscoveryOptions::default()).await?
    };

    // Identify the target.
    let target = if let Some(package) = package.as_ref().filter(|_| frozen) {
        InstallTarget::frozen_member(&project, package)
    } else {
        InstallTarget::from(&project)
    };

    // Discover or create the virtual environment.
    let venv = project::get_or_init_environment(
        target.workspace(),
        python.as_deref().map(PythonRequest::parse),
        python_preference,
        python_downloads,
        connectivity,
        native_tls,
        cache,
        printer,
    )
    .await?;

    let lock = match do_safe_lock(
        locked,
        frozen,
        target.workspace(),
        venv.interpreter(),
        settings.as_ref().into(),
        Box::new(DefaultResolveLogger),
        connectivity,
        concurrency,
        native_tls,
        cache,
        printer,
    )
    .await
    {
        Ok(result) => result.into_lock(),
        Err(ProjectError::Operation(pip::operations::Error::Resolve(
            uv_resolver::ResolveError::NoSolution(err),
        ))) => {
            let report = miette::Report::msg(format!("{err}")).context(err.header());
            anstream::eprint!("{report:?}");
            return Ok(ExitStatus::Failure);
        }
        Err(err) => return Err(err.into()),
    };

    // Initialize any shared state.
    let state = SharedState::default();

    // Perform the sync operation.
    do_sync(
        target,
        &venv,
        &lock,
        &extras,
        dev,
        install_options,
        modifications,
        settings.as_ref().into(),
        &state,
        Box::new(DefaultInstallLogger),
        connectivity,
        concurrency,
        native_tls,
        cache,
        printer,
    )
    .await?;

    Ok(ExitStatus::Success)
}

/// Sync a lockfile with an environment.
#[allow(clippy::fn_params_excessive_bools)]
pub(super) async fn do_sync(
    target: InstallTarget<'_>,
    venv: &PythonEnvironment,
    lock: &Lock,
    extras: &ExtrasSpecification,
    dev: bool,
    install_options: InstallOptions,
    modifications: Modifications,
    settings: InstallerSettingsRef<'_>,
    state: &SharedState,
    logger: Box<dyn InstallLogger>,
    connectivity: Connectivity,
    concurrency: Concurrency,
    native_tls: bool,
    cache: &Cache,
    printer: Printer,
) -> Result<(), ProjectError> {
    // Extract the project settings.
    let InstallerSettingsRef {
        index_locations,
        index_strategy,
        keyring_provider,
        allow_insecure_host,
        config_setting,
        no_build_isolation,
        no_build_isolation_package,
        exclude_newer,
        link_mode,
        compile_bytecode,
        reinstall,
        build_options,
        sources,
    } = settings;

    // Validate that the Python version is supported by the lockfile.
    if !lock
        .requires_python()
        .contains(venv.interpreter().python_version())
    {
        return Err(ProjectError::LockedPythonIncompatibility(
            venv.interpreter().python_version().clone(),
            lock.requires_python().clone(),
        ));
    }

    // Determine the markers to use for resolution.
    let markers = venv.interpreter().resolver_markers();

    // Validate that the platform is supported by the lockfile.
    let environments = lock.supported_environments();
    if !environments.is_empty() {
        if !environments.iter().any(|env| env.evaluate(&markers, &[])) {
            return Err(ProjectError::LockedPlatformIncompatibility(
                // For error reporting, we use the "simplified"
                // supported environments, because these correspond to
                // what the end user actually wrote. The non-simplified
                // environments, by contrast, are explicitly
                // constrained by `requires-python`.
                lock.simplified_supported_environments()
                    .iter()
                    .filter_map(MarkerTree::contents)
                    .map(|env| format!("`{env}`"))
                    .join(", "),
            ));
        }
    }

    // Include development dependencies, if requested.
    let dev = if dev {
        vec![DEV_DEPENDENCIES.clone()]
    } else {
        vec![]
    };

    // Determine the tags to use for resolution.
    let tags = venv.interpreter().tags()?;

    // Read the lockfile.
    let resolution = lock.to_resolution(target, &markers, tags, extras, &dev)?;

    // Always skip virtual projects, which shouldn't be built or installed.
    let resolution = apply_no_virtual_project(resolution);

    // Filter resolution based on install-specific options.
    let resolution =
        install_options.filter_resolution(resolution, target.project_name(), lock.members());

    // Add all authenticated sources to the cache.
    for url in index_locations.urls() {
        store_credentials_from_url(url);
    }

    // Initialize the registry client.
    let client = RegistryClientBuilder::new(cache.clone())
        .native_tls(native_tls)
        .connectivity(connectivity)
        .index_urls(index_locations.index_urls())
        .index_strategy(index_strategy)
        .keyring(keyring_provider)
        .allow_insecure_host(allow_insecure_host.to_vec())
        .markers(venv.interpreter().markers())
        .platform(venv.interpreter().platform())
        .build();

    // Determine whether to enable build isolation.
    let build_isolation = if no_build_isolation {
        BuildIsolation::Shared(venv)
    } else if no_build_isolation_package.is_empty() {
        BuildIsolation::Isolated
    } else {
        BuildIsolation::SharedPackage(venv, no_build_isolation_package)
    };

    // TODO(charlie): These are all default values. We should consider whether we want to make them
    // optional on the downstream APIs.
    let build_constraints = [];
    let dry_run = false;

    // Extract the hashes from the lockfile.
    let hasher = HashStrategy::from_resolution(&resolution, HashCheckingMode::Verify)?;

    // Resolve the flat indexes from `--find-links`.
    let flat_index = {
        let client = FlatIndexClient::new(&client, cache);
        let entries = client.fetch(index_locations.flat_index()).await?;
        FlatIndex::from_entries(entries, Some(tags), &hasher, build_options)
    };

    // Create a build dispatch.
    let build_dispatch = BuildDispatch::new(
        &client,
        cache,
        &build_constraints,
        venv.interpreter(),
        index_locations,
        &flat_index,
        &state.index,
        &state.git,
        &state.in_flight,
        index_strategy,
        config_setting,
        build_isolation,
        link_mode,
        build_options,
        exclude_newer,
        sources,
        concurrency,
    );

    let site_packages = SitePackages::from_environment(venv)?;

    // Sync the environment.
    pip::operations::install(
        &resolution,
        site_packages,
        modifications,
        reinstall,
        build_options,
        link_mode,
        compile_bytecode,
        index_locations,
        &hasher,
        &markers,
        tags,
        &client,
        &state.in_flight,
        concurrency,
        &build_dispatch,
        cache,
        venv,
        logger,
        dry_run,
        printer,
    )
    .await?;

    Ok(())
}

/// Filter out any virtual workspace members.
fn apply_no_virtual_project(
    resolution: distribution_types::Resolution,
) -> distribution_types::Resolution {
    resolution.filter(|dist| {
        let ResolvedDist::Installable(dist) = dist else {
            return true;
        };

        let Dist::Source(dist) = dist else {
            return true;
        };

        let SourceDist::Directory(dist) = dist else {
            return true;
        };

        !dist.r#virtual
    })
}
