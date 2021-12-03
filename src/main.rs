extern crate k8s_openapi;
extern crate kube;
extern crate policy_evaluator;

use anyhow::Result;
use opentelemetry::global::shutdown_tracer_provider;
use policy_evaluator::policy_metadata::Metadata;
use settings::VerificationSettings;
use std::{process, thread};
use tokio::{runtime::Runtime, sync::mpsc, sync::oneshot};
use tracing::{debug, error, info};

mod admission_review;
mod api;
mod cli;
mod kube_poller;
mod metrics;
mod server;
mod settings;
mod utils;
mod worker;

mod worker_pool;
use worker_pool::WorkerPool;

use std::path::PathBuf;

mod communication;
use communication::{EvalRequest, KubePollerBootRequest, WorkerPoolBootRequest};

fn main() -> Result<()> {
    let matches = cli::build_cli().get_matches();

    // init some variables based on the cli parameters
    let addr = cli::api_bind_address(&matches)?;
    let (cert_file, key_file) = cli::tls_files(&matches)?;
    let mut policies = cli::policies(&matches)?;
    let (sources, docker_config) = cli::remote_server_options(&matches)?;
    let pool_size = matches.value_of("workers").map_or_else(num_cpus::get, |v| {
        v.parse::<usize>()
            .expect("error parsing the number of workers")
    });

    // As of clap version 2, we have to do this check in separate
    // steps.
    //
    // `--enable-metrics` does not take a value. However, using
    // `env` from clap to link the `KUBEWARDEN_ENABLE_METRICS`
    // envvar to this flag forcefully sets `takes_value` on the
    // flag, making the usage of `--enable-metrics` argument weird
    // (this should not take a value).
    //
    // The answer is therefore, to just set up the
    // `--enable-metrics` flag in clap, and check manually whether
    // the environment variable is set.
    //
    // The same is true for `--verify-policies` flag, and
    // KUBEWARDEN_ENABLE_VERIFICATION env var.
    //
    // Check https://github.com/clap-rs/clap/issues/1476 for
    // further details.
    let metrics_enabled = matches.is_present("enable-metrics")
        || std::env::var_os("KUBEWARDEN_ENABLE_METRICS").is_some();
    let verify_enabled = matches.is_present("enable-verification")
        || std::env::var_os("KUBEWARDEN_ENABLE_VERIFICATION").is_some();

    let mut verification_settings: Option<VerificationSettings> = None;
    if verify_enabled {
        verification_settings = Some(cli::verification_settings(&matches)?);
    }

    ////////////////////////////////////////////////////////////////////////////
    //                                                                        //
    // Phase 1: setup the Wasm worker pool, this "lives" inside of a          //
    // dedicated system thread.                                               //
    //                                                                        //
    // The communication between the "synchronous world" (aka the Wasm worker //
    // pool) and the "asynchronous world" (aka the http server) happens via   //
    // tokio channels.                                                        //
    //                                                                        //
    ////////////////////////////////////////////////////////////////////////////

    // This is the channel used by the http server to communicate with the
    // Wasm workers
    let (api_tx, api_rx) = mpsc::channel::<EvalRequest>(32);

    // This is the channel used to have the asynchronous code trigger the
    // bootstrap of the worker pool. The bootstrap must be triggered
    // from within the asynchronous code because some of the tracing collectors
    // (e.g. OpenTelemetry) require a tokio::Runtime to be available.
    let (worker_pool_bootstrap_req_tx, worker_pool_bootstrap_req_rx) =
        oneshot::channel::<WorkerPoolBootRequest>();

    // Spawn the system thread that runs the main loop of the worker pool manager
    let wasm_thread = thread::spawn(move || {
        let worker_pool = WorkerPool::new(worker_pool_bootstrap_req_rx, api_rx);
        worker_pool.run();
    });

    ////////////////////////////////////////////////////////////////////////////
    //                                                                        //
    // Phase 2: setup a dedicated thread that runs the Kubernetes poller      //
    //                                                                        //
    ////////////////////////////////////////////////////////////////////////////

    // This is the channel used to have the asynchronous code trigger the
    // bootstrap of the kubernetes poller. The bootstrap must be triggered
    // from within the asynchronous code because some of the tracing collectors
    // (e.g. OpenTelemetry) require a tokio::Runtime to be available.
    let (kube_poller_bootstrap_req_tx, kube_poller_bootstrap_req_rx) =
        oneshot::channel::<KubePollerBootRequest>();

    // Spawn the system thread that runs the main loop of the worker pool manager
    let kube_poller_thread = thread::spawn(move || {
        let poller = match kube_poller::Poller::new(kube_poller_bootstrap_req_rx) {
            Ok(p) => p,
            Err(e) => {
                fatal_error(format!(
                    "Cannot init dedicated tokio runtime for the Kubernetes poller: {:?}",
                    e
                ));
                unreachable!()
            }
        };
        poller.run();
    });

    ////////////////////////////////////////////////////////////////////////////
    //                                                                        //
    // Phase 3: setup the asynchronous world.                                 //
    //                                                                        //
    // We setup the tokio Runtime manually, instead of relying on the the     //
    // `tokio::main` macro, because we don't want the "synchronous" world to  //
    // be spawned inside of one of the threads owned by the runtime.          //
    //                                                                        //
    ////////////////////////////////////////////////////////////////////////////

    let rt = match Runtime::new() {
        Ok(r) => r,
        Err(error) => {
            fatal_error(format!("error initializing tokio runtime: {}", error));
            unreachable!();
        }
    };
    rt.block_on(async {
        // Setup the tracing system. This MUST be done inside of a tokio Runtime
        // because some collectors rely on it and would panic otherwise.
        if let Err(err) = cli::setup_tracing(&matches) {
            fatal_error(err.to_string());
        }

        // The unused variable is required so the meter is not dropped early and
        // lives for the whole block lifetime, exporting metrics
        let _meter = if metrics_enabled {
            Some(metrics::init_meter())
        } else {
            None
        };

        // Download policies
        let policies_download_dir = matches.value_of("policies-download-dir").unwrap();
        let policies_total = policies.len();
        info!(
            download_dir = policies_download_dir,
            policies_count = policies_total,
            status = "init",
            "policies download",
        );
        for (name, policy) in policies.iter_mut() {
            debug!(policy = name.as_str(), "download");

            let mut verifier = policy_fetcher::verify::Verifier::new(sources.clone());
            let mut verified_manifest_digest: Option<String> = None;

            if verify_enabled {
                // verify policy prior to pulling for all keys, and keep the
                // verified manifest digest of last iteration, even if all are
                // the same:
                for key_value in verification_settings
                    .as_ref()
                    .unwrap()
                    .verification_keys
                    .values()
                {
                    verified_manifest_digest = Some(
                        verifier
                            .verify(
                                &policy.url,
                                docker_config.clone(),
                                verification_settings
                                    .as_ref()
                                    .unwrap()
                                    .verification_annotations
                                    .clone(),
                                key_value,
                            )
                            .await
                            .map_err(|e| fatal_error(e.to_string()))
                            .unwrap(),
                    );
                }
                info!(
                    name = name.as_str(),
                    sha256sum = verified_manifest_digest
                        .as_ref()
                        .unwrap_or(&"unknown".to_string())
                        .as_str(),
                    status = "verified-signatures",
                    "policy download",
                );
            }

            match policy_fetcher::fetch_policy(
                &policy.url,
                policy_fetcher::PullDestination::Store(PathBuf::from(policies_download_dir)),
                docker_config.clone(),
                sources.as_ref(),
            )
            .await
            {
                Ok(fetched_policy) => {
                    if verify_enabled {
                        if verified_manifest_digest.is_none() {
                            // when deserializing keys we check that have keys to
                            // verify. We will always have a digest manifest
                            fatal_error("Trying to verify but no keys were passed".to_string());
                            unreachable!();
                        }
                        verifier
                            .verify_local_file_checksum(
                                &fetched_policy.uri,
                                docker_config.clone(),
                                verified_manifest_digest.as_ref().unwrap(),
                            )
                            .await
                            .map_err(|e| fatal_error(e.to_string()))
                            .unwrap();
                        info!(
                            name = name.as_str(),
                            sha256sum = verified_manifest_digest
                                .as_ref()
                                .unwrap_or(&"unknown".to_string())
                                .as_str(),
                            status = "verified-local-checksum",
                            "policy download",
                        );
                    }

                    if let Ok(Some(policy_metadata)) =
                        Metadata::from_path(&fetched_policy.local_path)
                    {
                        info!(
                            name = name.as_str(),
                            path = fetched_policy.local_path.clone().into_os_string().to_str(),
                            sha256sum = fetched_policy
                                .digest()
                                .unwrap_or_else(|_| "unknown".to_string())
                                .as_str(),
                            mutating = policy_metadata.mutating,
                            "policy download",
                        );
                    } else {
                        info!(
                            name = name.as_str(),
                            path = fetched_policy.local_path.clone().into_os_string().to_str(),
                            sha256sum = fetched_policy
                                .digest()
                                .unwrap_or_else(|_| "unknown".to_string())
                                .as_str(),
                            "policy download",
                        );
                    }
                    policy.wasm_module_path = fetched_policy.local_path;
                }
                Err(e) => {
                    return fatal_error(format!(
                        "error while fetching policy {} from {}: {}",
                        name, policy.url, e
                    ));
                }
            };
        }
        info!(status = "done", "policies download");

        // Start the kubernetes poller
        info!(status = "init", "kubernetes poller bootstrap");
        let (kube_poller_bootstrap_res_tx, mut kube_poller_bootstrap_res_rx) =
            oneshot::channel::<Result<()>>();
        let kube_poller_bootstrap_data = KubePollerBootRequest {
            resp_chan: kube_poller_bootstrap_res_tx,
        };
        if kube_poller_bootstrap_req_tx
            .send(kube_poller_bootstrap_data)
            .is_err()
        {
            fatal_error("Cannot send bootstrap data to kubernetes poller".to_string());
        }

        // Wait for the kubernetes poller to be fully bootstraped before moving on.
        //
        // The poller must be stated before policies can be evaluated, otherwise
        // context-aware policies could not have the right data at their disposal.
        loop {
            match kube_poller_bootstrap_res_rx.try_recv() {
                Ok(res) => match res {
                    Ok(_) => break,
                    Err(e) => fatal_error(e.to_string()),
                },
                Err(oneshot::error::TryRecvError::Empty) => {
                    // the channel is empty, keep waiting
                }
                _ => {
                    fatal_error("Cannot receive kubernetes poller bootstrap result".to_string());
                    return;
                }
            }
        }
        info!(status = "done", "kubernetes poller bootstrap");

        // Bootstrap the worker pool
        info!(status = "init", "worker pool bootstrap");
        let (worker_pool_bootstrap_res_tx, mut worker_pool_bootstrap_res_rx) =
            oneshot::channel::<Result<()>>();
        let bootstrap_data = WorkerPoolBootRequest {
            policies,
            pool_size,
            resp_chan: worker_pool_bootstrap_res_tx,
        };
        if worker_pool_bootstrap_req_tx.send(bootstrap_data).is_err() {
            fatal_error("Cannot send bootstrap data to worker pool".to_string());
        }

        // Wait for the worker pool to be fully bootstraped before moving on.
        //
        // It's really important to NOT start the web server before the workers are ready.
        // Our Kubernetes deployment exposes a readiness probe that relies on the web server
        // to be listening. The API server will start hitting the policy server as soon as the
        // readiness probe marks the instance as ready.
        // We don't want Kubernetes API server to send admission reviews before ALL the workers
        // are ready.
        loop {
            match worker_pool_bootstrap_res_rx.try_recv() {
                Ok(res) => match res {
                    Ok(_) => break,
                    Err(e) => fatal_error(e.to_string()),
                },
                Err(oneshot::error::TryRecvError::Empty) => {
                    // the channel is empty, keep waiting
                }
                _ => {
                    fatal_error("Cannot receive worker pool bootstrap result".to_string());
                    return;
                }
            }
        }
        info!(status = "done", "worker pool bootstrap");

        // All is good, we can start listening for incoming requests through the
        // web server
        let tls_acceptor = if cert_file.is_empty() {
            None
        } else {
            match server::new_tls_acceptor(&cert_file, &key_file) {
                Ok(t) => Some(t),
                Err(e) => {
                    fatal_error(format!("error while creating tls acceptor: {:?}", e));
                    unreachable!();
                }
            }
        };
        server::run_server(&addr, tls_acceptor, api_tx).await;
    });

    if let Err(e) = wasm_thread.join() {
        fatal_error(format!("error while waiting for worker threads: {:?}", e));
    };

    if let Err(e) = kube_poller_thread.join() {
        fatal_error(format!("error while waiting for worker threads: {:?}", e));
    };

    Ok(())
}

fn fatal_error(msg: String) {
    error!("{}", msg);
    shutdown_tracer_provider();

    process::exit(1);
}
