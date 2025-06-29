mod error;
#[cfg(feature = "indicatif")]
mod indicatif;
mod reporter;
use std::{
    collections::{HashMap, HashSet},
    future::ready,
    path::{Path, PathBuf},
    sync::Arc,
};

pub use error::InstallerError;
use futures::{stream::FuturesUnordered, FutureExt, StreamExt, TryFutureExt};
#[cfg(feature = "indicatif")]
pub use indicatif::{
    DefaultProgressFormatter, IndicatifReporter, IndicatifReporterBuilder, Placement,
    ProgressFormatter,
};
use itertools::Itertools;
use rattler_cache::package_cache::{CacheLock, CacheReporter};
use rattler_conda_types::{
    prefix_record::{Link, LinkType},
    PackageName, Platform, PrefixRecord, RepoDataRecord,
};
use rattler_networking::retry_policies::default_retry_policy;
pub use reporter::Reporter;
use reqwest::Client;
use simple_spawn_blocking::tokio::run_blocking_task;
use tokio::{sync::Semaphore, task::JoinError};

use super::{
    unlink_package, AppleCodeSignBehavior, InstallDriver, InstallOptions, Prefix, Transaction,
};
use crate::{
    default_cache_dir,
    install::{
        clobber_registry::ClobberedPath,
        link_script::{LinkScriptError, PrePostLinkResult},
    },
    package_cache::PackageCache,
};

#[derive(Default)]
pub struct LinkOptions {
    pub allow_symbolic_links: Option<bool>,
    pub allow_hard_links: Option<bool>,
    pub allow_ref_links: Option<bool>,
}

/// An installer that can install packages into a prefix.
#[derive(Default)]
pub struct Installer {
    installed: Option<Vec<PrefixRecord>>,
    package_cache: Option<PackageCache>,
    downloader: Option<reqwest_middleware::ClientWithMiddleware>,
    execute_link_scripts: bool,
    io_semaphore: Option<Arc<Semaphore>>,
    reporter: Option<Arc<dyn Reporter>>,
    target_platform: Option<Platform>,
    apple_code_sign_behavior: AppleCodeSignBehavior,
    alternative_target_prefix: Option<PathBuf>,
    reinstall_packages: Option<HashSet<PackageName>>,
    // TODO: Determine upfront if these are possible.
    link_options: LinkOptions,
}

#[derive(Debug)]
pub struct InstallationResult {
    /// The transaction that was applied
    pub transaction: Transaction<PrefixRecord, RepoDataRecord>,

    /// The result of running pre link scripts. `None` if no
    /// pre-processing was performed, possibly because link scripts were
    /// disabled.
    pub pre_link_script_result: Option<PrePostLinkResult>,

    /// The result of running post link scripts. `None` if no
    /// post-processing was performed, possibly because link scripts were
    /// disabled.
    pub post_link_script_result: Option<Result<PrePostLinkResult, LinkScriptError>>,

    /// The paths that were clobbered during the installation process.
    pub clobbered_paths: HashMap<PathBuf, ClobberedPath>,
}

impl Installer {
    /// Constructs a new installer
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets an optional IO concurrency limit. This is used to make sure
    /// that the system doesn't acquire more IO resources than the system has
    /// available.
    #[must_use]
    pub fn with_io_concurrency_limit(self, limit: usize) -> Self {
        Self {
            io_semaphore: Some(Arc::new(Semaphore::new(limit))),
            ..self
        }
    }

    /// Sets an optional IO concurrency limit.
    ///
    /// This function is similar to [`Self::with_io_concurrency_limit`],
    /// but modifies an existing instance.
    pub fn set_io_concurrency_limit(&mut self, limit: usize) -> &mut Self {
        self.io_semaphore = Some(Arc::new(Semaphore::new(limit)));
        self
    }

    /// Sets an optional IO concurrency semaphore. This is used to make sure
    /// that the system doesn't acquire more IO resources than the system has
    /// available.
    #[must_use]
    pub fn with_io_concurrency_semaphore(self, io_concurrency_semaphore: Arc<Semaphore>) -> Self {
        Self {
            io_semaphore: Some(io_concurrency_semaphore),
            ..self
        }
    }

    /// Sets an optional IO concurrency semaphore.
    ///
    /// This function is similar to [`Self::with_io_concurrency_semaphore`], but
    /// modifies an existing instance.
    pub fn set_io_concurrency_semaphore(&mut self, limit: usize) -> &mut Self {
        self.io_semaphore = Some(Arc::new(Semaphore::new(limit)));
        self
    }

    /// Sets whether to execute link scripts or not.
    ///
    /// By default, link scripts are not executed. Link scripts can run
    /// arbitrary code during the installation phase which makes them a security
    /// risk.
    #[must_use]
    pub fn with_execute_link_scripts(self, execute: bool) -> Self {
        Self {
            execute_link_scripts: execute,
            ..self
        }
    }

    /// Sets whether to execute link scripts or not.
    ///
    /// By default, link scripts are not executed. Link scripts can run
    /// arbitrary code during the installation phase which makes them a security
    /// risk.
    pub fn set_execute_link_scripts(&mut self, execute: bool) -> &mut Self {
        self.execute_link_scripts = execute;
        self
    }

    /// Sets the package cache to use.
    #[must_use]
    pub fn with_package_cache(self, package_cache: PackageCache) -> Self {
        Self {
            package_cache: Some(package_cache),
            ..self
        }
    }

    /// Sets the package cache to use.
    ///
    /// This function is similar to [`Self::with_package_cache`],but modifies an
    /// existing instance.
    pub fn set_package_cache(&mut self, package_cache: PackageCache) -> &mut Self {
        self.package_cache = Some(package_cache);
        self
    }

    /// Sets the download client to use
    #[must_use]
    pub fn with_download_client(
        self,
        downloader: reqwest_middleware::ClientWithMiddleware,
    ) -> Self {
        Self {
            downloader: Some(downloader),
            ..self
        }
    }

    /// Sets the download client to use
    ///
    /// This function is similar to [`Self::with_download_client`], but modifies
    /// an existing instance.
    pub fn set_download_client(
        &mut self,
        downloader: reqwest_middleware::ClientWithMiddleware,
    ) -> &mut Self {
        self.downloader = Some(downloader);
        self
    }

    /// Sets a reporter that will receive events during the installation
    /// process.
    #[must_use]
    pub fn with_reporter<R: Reporter + 'static>(self, reporter: R) -> Self {
        Self {
            reporter: Some(Arc::new(reporter)),
            ..self
        }
    }

    /// Sets a reporter that will receive events during the installation
    /// process.
    ///
    /// This function is similar to [`Self::with_reporter`],but modifies an
    /// existing instance.
    pub fn set_reporter<R: Reporter + 'static>(&mut self, reporter: R) -> &mut Self {
        self.reporter = Some(Arc::new(reporter));
        self
    }

    /// Sets the packages that are currently installed in the prefix. If this
    /// is not set, the installation process will first figure this out.
    #[must_use]
    pub fn with_installed_packages(self, installed: Vec<PrefixRecord>) -> Self {
        Self {
            installed: Some(installed),
            ..self
        }
    }

    /// Set the packages that we want explicitly to be reinstalled.
    #[must_use]
    pub fn with_reinstall_packages(self, reinstall: HashSet<PackageName>) -> Self {
        Self {
            reinstall_packages: Some(reinstall),
            ..self
        }
    }

    /// Set the packages that we want explicitly to be reinstalled.
    /// This function is similar to [`Self::with_reinstall_packages`],but
    /// modifies an existing instance.
    pub fn set_reinstall_packages(&mut self, reinstall: HashSet<PackageName>) -> &mut Self {
        self.reinstall_packages = Some(reinstall);
        self
    }

    /// Sets the packages that are currently installed in the prefix. If this
    /// is not set, the installation process will first figure this out.
    ///
    /// This function is similar to [`Self::with_installed_packages`],but
    /// modifies an existing instance.
    pub fn set_installed_packages(&mut self, installed: Vec<PrefixRecord>) -> &mut Self {
        self.installed = Some(installed);
        self
    }

    /// Sets the target platform of the installation. If not specifically set
    /// this will default to the current platform.
    #[must_use]
    pub fn with_target_platform(self, target_platform: Platform) -> Self {
        Self {
            target_platform: Some(target_platform),
            ..self
        }
    }

    /// Sets the target platform of the installation. If not specifically set
    /// this will default to the current platform.
    ///
    /// This function is similar to [`Self::with_target_platform`], but modifies
    /// an existing instance.
    pub fn set_target_platform(&mut self, target_platform: Platform) -> &mut Self {
        self.target_platform = Some(target_platform);
        self
    }

    /// Determines how to handle Apple code signing behavior.
    #[must_use]
    pub fn with_apple_code_signing_behavior(self, behavior: AppleCodeSignBehavior) -> Self {
        Self {
            apple_code_sign_behavior: behavior,
            ..self
        }
    }

    /// Determines how to handle Apple code signing behavior.
    ///
    /// This function is similar to
    /// [`Self::with_apple_code_signing_behavior`],but modifies an existing
    /// instance.
    pub fn set_apple_code_signing_behavior(
        &mut self,
        behavior: AppleCodeSignBehavior,
    ) -> &mut Self {
        self.apple_code_sign_behavior = behavior;
        self
    }

    /// Sets the link options for the installer.
    pub fn with_link_options(self, options: LinkOptions) -> Self {
        Self {
            link_options: options,
            ..self
        }
    }

    /// Sets the link options for the installer.
    pub fn set_link_options(&mut self, options: LinkOptions) -> &mut Self {
        self.link_options = options;
        self
    }

    /// Install the packages in the given prefix.
    pub async fn install(
        self,
        prefix: impl AsRef<Path>,
        records: impl IntoIterator<Item = RepoDataRecord>,
    ) -> Result<InstallationResult, InstallerError> {
        let downloader = self
            .downloader
            .unwrap_or_else(|| reqwest_middleware::ClientWithMiddleware::from(Client::default()));
        let package_cache = self.package_cache.unwrap_or_else(|| {
            PackageCache::new(
                default_cache_dir()
                    .expect("failed to determine default cache directory")
                    .join(rattler_cache::PACKAGE_CACHE_DIR),
            )
        });

        let prefix = Prefix::create(prefix.as_ref().to_path_buf()).map_err(|err| {
            InstallerError::FailedToCreatePrefix(prefix.as_ref().to_path_buf(), err)
        })?;

        // Create a future to determine the currently installed packages. We
        // can start this in parallel with the other operations and resolve it
        // when we need it.
        let installed: Vec<PrefixRecord> = if let Some(installed) = self.installed {
            installed
        } else {
            let prefix = prefix.clone();
            // TODO: Should we add progress reporting here?
            run_blocking_task(move || {
                PrefixRecord::collect_from_prefix(&prefix)
                    .map_err(InstallerError::FailedToDetectInstalledPackages)
            })
            .await?
        };

        // Construct a driver.
        let driver = InstallDriver::builder()
            .execute_link_scripts(self.execute_link_scripts)
            .with_io_concurrency_semaphore(
                self.io_semaphore.unwrap_or(Arc::new(Semaphore::new(100))),
            )
            .with_prefix_records(&installed)
            .finish();

        // Construct a transaction from the current and desired situation.
        let target_platform = self.target_platform.unwrap_or_else(Platform::current);
        let transaction = Transaction::from_current_and_desired(
            installed.clone(),
            records.into_iter().collect::<Vec<_>>(),
            self.reinstall_packages,
            target_platform,
        )?;

        let remaining = installed
            .into_iter()
            .filter(|pr| !transaction.removed_packages().contains(pr))
            .collect::<Vec<_>>();

        // If the transaction is empty we can short-circuit the installation
        if transaction.operations.is_empty() {
            return Ok(InstallationResult {
                transaction,
                pre_link_script_result: None,
                post_link_script_result: None,
                clobbered_paths: HashMap::default(),
            });
        }

        // Determine base installer options.
        let base_install_options = InstallOptions {
            target_prefix: self.alternative_target_prefix.clone(),
            platform: Some(target_platform),
            python_info: transaction.python_info.clone(),
            apple_codesign_behavior: self.apple_code_sign_behavior,
            allow_symbolic_links: self.link_options.allow_symbolic_links,
            allow_hard_links: self.link_options.allow_hard_links,
            allow_ref_links: self.link_options.allow_ref_links,
            ..InstallOptions::default()
        };

        // Preprocess the transaction
        let pre_process_result = driver
            .pre_process(&transaction, &prefix)
            .map_err(InstallerError::PreProcessingFailed)?;

        if let Some(reporter) = &self.reporter {
            reporter.on_transaction_start(&transaction);
        }

        let mut pending_unlink_futures = FuturesUnordered::new();
        // Execute the operations (remove) in the transaction.
        for (operation_idx, operation) in transaction.operations.iter().enumerate() {
            let reporter = self.reporter.clone();
            let driver = &driver;
            let prefix = &prefix;

            let op = async move {
                // Uninstall the package if it was removed.
                if let Some(record) = operation.record_to_remove() {
                    if let Some(reporter) = &reporter {
                        reporter.on_transaction_operation_start(operation_idx);
                    }

                    let reporter = reporter
                        .as_deref()
                        .map(move |r| (r, r.on_unlink_start(operation_idx, record)));
                    driver.clobber_registry().unregister_paths(record);
                    unlink_package(prefix, record).await.map_err(|e| {
                        InstallerError::UnlinkError(record.repodata_record.file_name.clone(), e)
                    })?;
                    if let Some((reporter, index)) = reporter {
                        reporter.on_unlink_complete(index);
                        if operation.record_to_install().is_none() {
                            reporter.on_transaction_operation_complete(operation_idx);
                        }
                    }
                }
                Ok::<(), InstallerError>(())
            };
            pending_unlink_futures.push(op);
        }

        let mut pending_link_futures = FuturesUnordered::new();
        // Execute the operations (install) in the transaction.
        for (operation_idx, operation) in transaction
            .operations
            .iter()
            .enumerate()
            .sorted_by_key(|(_, op)| {
                op.record_to_install()
                    .and_then(|r| r.package_record.size)
                    .unwrap_or(0)
            })
            .rev()
        {
            let downloader = &downloader;
            let package_cache = &package_cache;
            let reporter = self.reporter.clone();
            let base_install_options = &base_install_options;
            let driver = &driver;
            let prefix = &prefix;
            let operation_future = async move {
                if let Some(reporter) = &reporter {
                    if operation.record_to_remove().is_none() {
                        reporter.on_transaction_operation_start(operation_idx);
                    }
                }

                // Start populating the cache with the package if it's not already there.
                let package_to_install = if let Some(record) = operation.record_to_install() {
                    let record = record.clone();
                    let downloader = downloader.clone();
                    let reporter = reporter.clone();
                    let package_cache = package_cache.clone();
                    tokio::spawn(async move {
                        let populate_cache_report = reporter.clone().map(|r| {
                            let cache_index = r.on_populate_cache_start(operation_idx, &record);
                            (r, cache_index)
                        });
                        let cache_lock = populate_cache(
                            &record,
                            downloader,
                            &package_cache,
                            populate_cache_report.clone(),
                        )
                        .await?;
                        if let Some((reporter, index)) = populate_cache_report {
                            reporter.on_populate_cache_complete(index);
                        }
                        Ok((cache_lock, record))
                    })
                    .map_err(JoinError::try_into_panic)
                    .map(|res| match res {
                        Ok(Ok(result)) => Ok(Some(result)),
                        Ok(Err(e)) => Err(e),
                        Err(Ok(payload)) => std::panic::resume_unwind(payload),
                        Err(Err(_err)) => Err(InstallerError::Cancelled),
                    })
                    .left_future()
                } else {
                    ready(Ok(None)).right_future()
                };

                // Install the package if it was fetched.
                if let Some((cache_lock, record)) = package_to_install.await? {
                    let reporter = reporter
                        .as_deref()
                        .map(|r| (r, r.on_link_start(operation_idx, &record)));
                    link_package(
                        &record,
                        prefix,
                        cache_lock.path(),
                        base_install_options.clone(),
                        driver,
                    )
                    .await?;
                    if let Some((reporter, index)) = reporter {
                        reporter.on_link_complete(index);
                    }
                }
                if let Some(reporter) = &reporter {
                    if operation.record_to_install().is_some() {
                        reporter.on_transaction_operation_complete(operation_idx);
                    }
                }

                Ok::<_, InstallerError>(())
            };

            pending_link_futures.push(operation_future);
        }

        // Wait for all transaction operations to finish
        while let Some(result) = pending_unlink_futures.next().await {
            result?;
        }
        drop(pending_unlink_futures);

        driver
            .remove_empty_directories(&transaction.operations, remaining.as_slice(), &prefix)
            .unwrap();

        // Wait for all transaction operations to finish
        while let Some(result) = pending_link_futures.next().await {
            result?;
        }
        drop(pending_link_futures);

        // Post process the transaction
        let post_process_result = driver.post_process(&transaction, &prefix)?;

        if let Some(reporter) = &self.reporter {
            reporter.on_transaction_complete();
        }

        Ok(InstallationResult {
            transaction,
            pre_link_script_result: pre_process_result,
            post_link_script_result: post_process_result.post_link_result,
            clobbered_paths: post_process_result.clobbered_paths,
        })
    }
}

async fn link_package(
    record: &RepoDataRecord,
    target_prefix: &Prefix,
    cached_package_dir: &Path,
    install_options: InstallOptions,
    driver: &InstallDriver,
) -> Result<(), InstallerError> {
    let record = record.clone();
    let target_prefix = target_prefix.clone();
    let cached_package_dir = cached_package_dir.to_path_buf();
    let clobber_registry = driver.clobber_registry.clone();

    let (tx, rx) = tokio::sync::oneshot::channel();

    rayon::spawn_fifo(move || {
        let inner = move || {
            // Link the contents of the package into the prefix.
            let paths = crate::install::link_package_sync(
                &cached_package_dir,
                &target_prefix,
                clobber_registry,
                install_options,
            )
            .map_err(|e| InstallerError::LinkError(record.file_name.clone(), e))?;

            // Construct a PrefixRecord for the package
            let prefix_record = PrefixRecord {
                repodata_record: record.clone(),
                package_tarball_full_path: None,
                extracted_package_dir: Some(cached_package_dir.clone()),
                files: paths
                    .iter()
                    .map(|entry| entry.relative_path.clone())
                    .collect(),
                paths_data: paths.into(),
                // TODO: Retrieve the requested spec for this package from the request
                requested_spec: None,

                link: Some(Link {
                    source: cached_package_dir,
                    // TODO: compute the right value here based on the options and `can_hard_link`
                    // ...
                    link_type: Some(LinkType::HardLink),
                }),
                installed_system_menus: Vec::new(),
            };

            let conda_meta_path = target_prefix.path().join("conda-meta");
            std::fs::create_dir_all(&conda_meta_path).map_err(|e| {
                InstallerError::IoError("failed to create conda-meta directory".to_string(), e)
            })?;

            let pkg_meta_path = format!(
                "{}-{}-{}.json",
                prefix_record
                    .repodata_record
                    .package_record
                    .name
                    .as_normalized(),
                prefix_record.repodata_record.package_record.version,
                prefix_record.repodata_record.package_record.build
            );
            prefix_record
                .write_to_path(conda_meta_path.join(&pkg_meta_path), true)
                .map_err(|e| {
                    InstallerError::IoError(format!("failed to write {pkg_meta_path}"), e)
                })?;

            Ok(())
        };

        let _ = tx.send(inner());
    });

    rx.await.unwrap_or(Err(InstallerError::Cancelled))
}

/// Given a repodata record, fetch the package into the cache if its not already
/// there.
async fn populate_cache(
    record: &RepoDataRecord,
    downloader: reqwest_middleware::ClientWithMiddleware,
    cache: &PackageCache,
    reporter: Option<(Arc<dyn Reporter>, usize)>,
) -> Result<CacheLock, InstallerError> {
    struct CacheReporterBridge {
        reporter: Arc<dyn Reporter>,
        cache_index: usize,
    }

    impl CacheReporter for CacheReporterBridge {
        fn on_validate_start(&self) -> usize {
            self.reporter.on_validate_start(self.cache_index)
        }

        fn on_validate_complete(&self, index: usize) {
            self.reporter.on_validate_complete(index);
        }

        fn on_download_start(&self) -> usize {
            self.reporter.on_download_start(self.cache_index)
        }

        fn on_download_progress(&self, index: usize, progress: u64, total: Option<u64>) {
            self.reporter.on_download_progress(index, progress, total);
        }

        fn on_download_completed(&self, index: usize) {
            self.reporter.on_download_completed(index);
        }
    }

    cache
        .get_or_fetch_from_url_with_retry(
            &record.package_record,
            record.url.clone(),
            downloader,
            default_retry_policy(),
            reporter.map(|(reporter, cache_index)| {
                Arc::new(CacheReporterBridge {
                    reporter,
                    cache_index,
                }) as _
            }),
        )
        .await
        .map_err(|e| InstallerError::FailedToFetch(record.file_name.clone(), e))
}
