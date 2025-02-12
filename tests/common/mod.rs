use axum::Router;
use policy_server::{
    config::{Config, PolicyGroupMember, PolicyMode, PolicyOrPolicyGroup},
    PolicyServer,
};
use std::{
    collections::{BTreeSet, HashMap},
    net::SocketAddr,
};
use tempfile::tempdir;

use std::sync::Once;
static START: Once = Once::new();

/// Common setup for tests. This function should be called at the beginning of each test.
pub(crate) fn setup() {
    START.call_once(|| {
        // Starting from rustls 0.22, each application must set its default crypto provider.
        // This setup is done inside of the `main` function of the policy server,
        // which is not called in this test.
        // Hence we have to setup the crypto provider here.
        let crypto_provider = rustls::crypto::ring::default_provider();
        crypto_provider
            .install_default()
            .expect("Failed to install crypto provider");
    });
}

pub(crate) fn default_test_config() -> Config {
    let policies = HashMap::from([
        (
            "pod-privileged".to_owned(),
            PolicyOrPolicyGroup::Policy {
                module: "ghcr.io/kubewarden/tests/pod-privileged:v0.2.1".to_owned(),
                policy_mode: PolicyMode::Protect,
                allowed_to_mutate: None,
                settings: None,
                context_aware_resources: BTreeSet::new(),
            },
        ),
        (
            "raw-mutation".to_owned(),
            PolicyOrPolicyGroup::Policy {
                module: "ghcr.io/kubewarden/tests/raw-mutation-policy:v0.1.0".to_owned(),
                policy_mode: PolicyMode::Protect,
                allowed_to_mutate: Some(true),
                settings: Some(HashMap::from([
                    (
                        "forbiddenResources".to_owned(),
                        vec!["banana", "carrot"].into(),
                    ),
                    ("defaultResource".to_owned(), "hay".into()),
                ])),
                context_aware_resources: BTreeSet::new(),
            },
        ),
        (
            "sleep".to_owned(),
            PolicyOrPolicyGroup::Policy {
                module: "ghcr.io/kubewarden/tests/sleeping-policy:v0.1.0".to_owned(),
                policy_mode: PolicyMode::Protect,
                allowed_to_mutate: None,
                settings: Some(HashMap::from([("sleepMilliseconds".to_owned(), 2.into())])),
                context_aware_resources: BTreeSet::new(),
            },
        ),
        (
            "group-policy-just-pod-privileged".to_owned(),
            PolicyOrPolicyGroup::PolicyGroup {
                expression: "pod_privileged() && true".to_string(),
                message: "The group policy rejected your request".to_string(),
                policy_mode: PolicyMode::Protect,
                policies: HashMap::from([(
                    "pod_privileged".to_string(),
                    PolicyGroupMember {
                        module: "ghcr.io/kubewarden/tests/pod-privileged:v0.2.1".to_owned(),
                        settings: None,
                        context_aware_resources: BTreeSet::new(),
                    },
                )]),
            },
        ),
        (
            "group-policy-just-raw-mutation".to_owned(),
            PolicyOrPolicyGroup::PolicyGroup {
                expression: "raw_mutation() && true".to_string(),
                message: "The group policy rejected your request".to_string(),
                policy_mode: PolicyMode::Protect,
                policies: HashMap::from([(
                    "raw_mutation".to_string(),
                    PolicyGroupMember {
                        module: "ghcr.io/kubewarden/tests/raw-mutation-policy:v0.1.0".to_owned(),
                        settings: Some(HashMap::from([
                            (
                                "forbiddenResources".to_owned(),
                                vec!["banana", "carrot"].into(),
                            ),
                            ("defaultResource".to_owned(), "hay".into()),
                        ])),
                        context_aware_resources: BTreeSet::new(),
                    },
                )]),
            },
        ),
    ]);

    Config {
        addr: SocketAddr::from(([127, 0, 0, 1], 3001)),
        readiness_probe_addr: SocketAddr::from(([127, 0, 0, 1], 3002)),
        sources: None,
        policies,
        policies_download_dir: tempdir().unwrap().into_path(),
        ignore_kubernetes_connection_failure: true,
        always_accept_admission_reviews_on_namespace: None,
        policy_evaluation_limit_seconds: Some(2),
        tls_config: None,
        pool_size: 2,
        metrics_enabled: false,
        sigstore_cache_dir: tempdir().unwrap().into_path(),
        verification_config: None,
        log_level: "info".to_owned(),
        log_fmt: "json".to_owned(),
        log_no_color: false,
        daemon: false,
        daemon_pid_file: "policy_server.pid".to_owned(),
        daemon_stdout_file: None,
        daemon_stderr_file: None,
        enable_pprof: false,
        continue_on_errors: false,
    }
}

pub(crate) async fn app(config: Config) -> Router {
    let server = PolicyServer::new_from_config(config).await.unwrap();

    server.router()
}
