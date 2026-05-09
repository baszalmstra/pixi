use pixi_compute_reporters::OperationId;
use serde::{Deserialize, Serialize};

/// One event variant per reporter callback (pixi side + rattler install
/// side), plus a few cross-cutting variants (`Log`, `StreamHeader`,
/// `BackendOutput`).
///
/// Payloads use primitive types only so the JSONL schema stays
/// independent of pixi's internal `*Spec` types.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Event {
    // ---- Stream meta ----
    StreamHeader {
        schema_version: u32,
        started_at_unix_ns: u64,
        pixi_version: String,
    },

    // ---- Pixi solve (PixiSolveReporter) ----
    PixiSolveQueued {
        id: OperationId,
        parent: Option<OperationId>,
        environment: String,
        platform: String,
        has_direct_conda_dependency: bool,
    },
    PixiSolveStarted {
        id: OperationId,
    },
    PixiSolveFinished {
        id: OperationId,
    },

    // ---- Conda solve (CondaSolveReporter) ----
    CondaSolveQueued {
        id: OperationId,
        parent: Option<OperationId>,
        environment: Option<String>,
        platform: String,
    },
    CondaSolveStarted {
        id: OperationId,
    },
    CondaSolveFinished {
        id: OperationId,
    },

    // ---- Pixi install (PixiInstallReporter) ----
    PixiInstallQueued {
        id: OperationId,
        parent: Option<OperationId>,
        environment: String,
    },
    PixiInstallStarted {
        id: OperationId,
    },
    PixiInstallFinished {
        id: OperationId,
    },

    // ---- Git checkout (GitCheckoutReporter) ----
    GitCheckoutQueued {
        id: OperationId,
        parent: Option<OperationId>,
        url: String,
        reference: String,
    },
    GitCheckoutStarted {
        id: OperationId,
    },
    GitCheckoutFinished {
        id: OperationId,
    },

    // ---- URL checkout (UrlCheckoutReporter) ----
    UrlCheckoutQueued {
        id: OperationId,
        parent: Option<OperationId>,
        url: String,
    },
    UrlCheckoutStarted {
        id: OperationId,
    },
    UrlCheckoutFinished {
        id: OperationId,
    },

    // ---- Instantiate backend (InstantiateBackendReporter) ----
    InstantiateBackendQueued {
        id: OperationId,
        parent: Option<OperationId>,
        backend: String,
    },
    InstantiateBackendStarted {
        id: OperationId,
    },
    InstantiateBackendFinished {
        id: OperationId,
    },

    // ---- Build backend metadata (BuildBackendMetadataReporter) ----
    BuildBackendMetadataQueued {
        id: OperationId,
        parent: Option<OperationId>,
        manifest_source: String,
    },
    BuildBackendMetadataStarted {
        id: OperationId,
    },
    BuildBackendMetadataFinished {
        id: OperationId,
        failed: bool,
    },

    // ---- Source metadata (SourceMetadataReporter) ----
    SourceMetadataQueued {
        id: OperationId,
        parent: Option<OperationId>,
        package: String,
        backend: String,
    },
    SourceMetadataStarted {
        id: OperationId,
    },
    SourceMetadataFinished {
        id: OperationId,
    },

    // ---- Source record (SourceRecordReporter) ----
    SourceRecordQueued {
        id: OperationId,
        parent: Option<OperationId>,
        package: String,
        variant: Vec<(String, String)>,
    },
    SourceRecordStarted {
        id: OperationId,
    },
    SourceRecordFinished {
        id: OperationId,
    },

    // ---- Backend source build (BackendSourceBuildReporter) ----
    BackendSourceBuildQueued {
        id: OperationId,
        parent: Option<OperationId>,
        package: String,
        version: String,
        build: String,
        subdir: String,
    },
    BackendSourceBuildStarted {
        id: OperationId,
    },
    BackendSourceBuildFinished {
        id: OperationId,
        failed: bool,
    },

    // ---- Backend output (stdout/stderr forwarded line by line) ----
    BackendOutput {
        id: OperationId,
        stream: BackendStream,
        line: String,
    },

    // ---- Gateway repodata query lifecycle ----
    GatewayQueryQueued {
        id: OperationId,
        parent: Option<OperationId>,
        channels: Vec<String>,
        platforms: Vec<String>,
    },
    GatewayQueryStarted {
        id: OperationId,
    },
    GatewayQueryFinished {
        id: OperationId,
    },

    // ---- Per-file repodata download events from the gateway ----
    RepodataDownloadStarted {
        id: OperationId,
        parent: OperationId,
        url: String,
    },
    RepodataDownloadProgress {
        id: OperationId,
        bytes: u64,
        total: Option<u64>,
    },
    RepodataDownloadComplete {
        id: OperationId,
    },

    // ---- Rattler install: transaction lifecycle ----
    InstallTransactionStarted {
        id: OperationId,
        operations: Vec<TransactionOpSummary>,
    },
    InstallTransactionComplete {
        id: OperationId,
    },
    InstallOperationStarted {
        id: OperationId,
        parent: OperationId,
        op_idx: usize,
        op: TransactionOpSummary,
    },
    InstallOperationComplete {
        id: OperationId,
    },

    // ---- Rattler install: cache populate ----
    InstallCachePopulateStarted {
        id: OperationId,
        parent: OperationId,
        package: PackageRef,
    },
    InstallCachePopulateComplete {
        id: OperationId,
    },

    // ---- Rattler install: validate ----
    InstallValidateStarted {
        id: OperationId,
        parent: OperationId,
        package: PackageRef,
    },
    InstallValidateComplete {
        id: OperationId,
    },

    // ---- Rattler install: download ----
    InstallDownloadStarted {
        id: OperationId,
        parent: OperationId,
        package: PackageRef,
        size_bytes: Option<u64>,
    },
    InstallDownloadProgress {
        id: OperationId,
        bytes: u64,
        total: Option<u64>,
    },
    InstallDownloadComplete {
        id: OperationId,
    },

    // ---- Rattler install: link / unlink ----
    InstallLinkStarted {
        id: OperationId,
        parent: OperationId,
        package: PackageRef,
    },
    InstallLinkComplete {
        id: OperationId,
    },
    InstallUnlinkStarted {
        id: OperationId,
        parent: OperationId,
        package: PackageRef,
    },
    InstallUnlinkComplete {
        id: OperationId,
    },

    // ---- Rattler install: pre/post link scripts ----
    InstallPostLinkStarted {
        id: OperationId,
        parent: OperationId,
        package: String,
        script: String,
    },
    InstallPostLinkComplete {
        id: OperationId,
        success: bool,
    },
    InstallPreUnlinkStarted {
        id: OperationId,
        parent: OperationId,
        package: String,
        script: String,
    },
    InstallPreUnlinkComplete {
        id: OperationId,
        success: bool,
    },

    // ---- Tracing bridge ----
    Log(LogEntry),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageRef {
    pub name: String,
    pub version: Option<String>,
    pub build: Option<String>,
    pub subdir: Option<String>,
    /// Conda channel base url, when known.
    pub channel: Option<String>,
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionOpKind {
    Install,
    Change,
    Reinstall,
    Remove,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionOpSummary {
    pub kind: TransactionOpKind,
    pub name: String,
    pub from_version: Option<String>,
    pub to_version: Option<String>,
    pub size_bytes: Option<u64>,
    pub build: Option<String>,
    pub channel: Option<String>,
    pub subdir: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub id: Option<OperationId>,
    pub level: LogLevel,
    pub target: String,
    pub message: String,
    /// Additional structured fields captured from the tracing event.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub fields: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

pub const SCHEMA_VERSION: u32 = 1;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventEnvelope;

    fn op(n: u64) -> OperationId {
        OperationId(n)
    }

    #[test]
    fn serializes_lifecycle_envelope_round_trips() {
        let env = EventEnvelope {
            seq: 7,
            ts_unix_ns: 1_700_000_000_000_000_000,
            event: Event::PixiSolveQueued {
                id: op(1),
                parent: None,
                environment: "default".into(),
                platform: "osx-arm64".into(),
                has_direct_conda_dependency: false,
            },
        };
        let line = serde_json::to_string(&env).unwrap();
        let back: EventEnvelope = serde_json::from_str(&line).unwrap();
        assert_eq!(back.seq, 7);
        match back.event {
            Event::PixiSolveQueued { id, environment, .. } => {
                assert_eq!(id, op(1));
                assert_eq!(environment, "default");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn snapshot_representative_variants() {
        let cases: Vec<EventEnvelope> = vec![
            EventEnvelope {
                seq: 0,
                ts_unix_ns: 1,
                event: Event::StreamHeader {
                    schema_version: SCHEMA_VERSION,
                    started_at_unix_ns: 1,
                    pixi_version: "0.0.0-test".into(),
                },
            },
            EventEnvelope {
                seq: 1,
                ts_unix_ns: 2,
                event: Event::PixiSolveQueued {
                    id: op(1),
                    parent: None,
                    environment: "default".into(),
                    platform: "linux-64".into(),
                    has_direct_conda_dependency: true,
                },
            },
            EventEnvelope {
                seq: 2,
                ts_unix_ns: 3,
                event: Event::CondaSolveQueued {
                    id: op(2),
                    parent: Some(op(1)),
                    environment: Some("default".into()),
                    platform: "linux-64".into(),
                },
            },
            EventEnvelope {
                seq: 3,
                ts_unix_ns: 4,
                event: Event::InstallTransactionStarted {
                    id: op(3),
                    operations: vec![TransactionOpSummary {
                        kind: TransactionOpKind::Install,
                        name: "numpy".into(),
                        from_version: None,
                        to_version: Some("1.26.4".into()),
                        size_bytes: Some(7_500_000),
                        build: Some("py312h7d4b309_0".into()),
                        channel: Some("https://conda.anaconda.org/conda-forge/".into()),
                        subdir: Some("linux-64".into()),
                    }],
                },
            },
            EventEnvelope {
                seq: 4,
                ts_unix_ns: 5,
                event: Event::InstallDownloadProgress {
                    id: op(4),
                    bytes: 1024,
                    total: Some(2048),
                },
            },
            EventEnvelope {
                seq: 5,
                ts_unix_ns: 6,
                event: Event::Log(LogEntry {
                    id: Some(op(1)),
                    level: LogLevel::Info,
                    target: "pixi::solve".into(),
                    message: "starting solve".into(),
                    fields: serde_json::Map::new(),
                }),
            },
        ];

        let lines: Vec<String> = cases
            .iter()
            .map(|e| serde_json::to_string(e).unwrap())
            .collect();
        insta::assert_snapshot!(lines.join("\n"));
    }
}
