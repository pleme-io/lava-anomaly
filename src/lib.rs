//! lava-anomaly — typed AnomalyController surface for the lava-suite.
//!
//! Operationalizes theory/CONTINUOUS-SOLUTION-MACHINE.md anomaly
//! routing for IaC drift / synth failures / apply failures / policy
//! violations.
//!
//! ## Shape
//!
//! ```text
//! controller emits LavaAnomaly
//!   ─▶ AnomalyRouter::route(anomaly, policy) -> RoutingDecision
//!         { action: RemediationAction, requeue_after, escalation }
//!     ├─ AutoCorrect → controller drives reconverge tick
//!     ├─ RequireApproval → controller halts; CR awaits a typed
//!     │                    LavaApproval CR
//!     ├─ Escalate → router walks EscalationLadder, picking the
//!     │             current tier's NotifyTarget
//!     └─ Alert / NoOp → log + persist anomaly to chain only
//! ```
//!
//! ## Abstractions
//!
//! - [`LavaAnomaly`] — typed anomaly value emitted by ANY controller
//!   in the lava-suite (drift detector, synth pipeline, apply engine,
//!   dependency controller).
//! - [`RemediationPolicy`] — operator-authored policy attached to a
//!   `LavaArchitecture` (or applied cluster-wide).
//! - [`AnomalyRouter`] — typed routing decision. Production impl is
//!   [`PolicyRouter`]; tests use [`MockRouter`].
//! - [`EscalationLadder`] — tiered notification recipients with
//!   typed `NotifyTarget` variants.
//! - [`AnomalyPayload`] — implements `lava_outcome_chain::Payload` so
//!   anomaly chains share the OutcomeChain machinery from L1.

#![allow(clippy::module_name_repetitions)]

use chrono::{DateTime, Duration, Utc};
use indexmap::IndexMap;
use lava_drift::Severity;
use lava_outcome_chain::{ContentHash, Payload, ResourceAddress};
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ── LavaAnomaly ────────────────────────────────────────────────────

/// Stable kind tag every anomaly carries — routing keys + dashboards
/// match on this.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum AnomalyKind {
    /// Drift detected against the last Applied plan.
    DriftDetected,
    /// .tlisp source failed to synthesize to terraform.json.
    SynthesisFailure,
    /// magma apply returned an error.
    ApplyFailure,
    /// A LavaArchitectureDependency was not in its expected phase.
    DependencyBlocked,
    /// Policy violation surfaced by a gate (PolicyController, OPA).
    PolicyViolation,
    /// Anomaly emitted explicitly by a custom controller — kind name
    /// in `extension_kind`.
    Custom,
}

impl AnomalyKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DriftDetected => "DriftDetected",
            Self::SynthesisFailure => "SynthesisFailure",
            Self::ApplyFailure => "ApplyFailure",
            Self::DependencyBlocked => "DependencyBlocked",
            Self::PolicyViolation => "PolicyViolation",
            Self::Custom => "Custom",
        }
    }
}

/// One typed anomaly emission.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LavaAnomaly {
    pub kind: AnomalyKind,
    /// When `kind == Custom`, holds the controller-defined kind name.
    #[serde(default)]
    pub extension_kind: Option<String>,
    pub severity: Severity,
    pub source: ResourceAddress,
    pub message: String,
    pub emitted_at: DateTime<Utc>,
    /// Per-anomaly metadata (e.g. `drift.fields_count = 3`,
    /// `synth.error_code = E101`). Keeps the typed surface clean
    /// while still letting controllers attach structured context.
    #[serde(default)]
    pub metadata: IndexMap<String, String>,
}

impl LavaAnomaly {
    /// Convenience constructor for the common case.
    #[must_use]
    pub fn new(
        kind: AnomalyKind,
        severity: Severity,
        source: ResourceAddress,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            extension_kind: None,
            severity,
            source,
            message: message.into(),
            emitted_at: Utc::now(),
            metadata: IndexMap::new(),
        }
    }

    /// Set a metadata key/value.
    #[must_use]
    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// Set the extension kind (only meaningful when `kind == Custom`).
    #[must_use]
    pub fn with_extension_kind(mut self, name: impl Into<String>) -> Self {
        self.extension_kind = Some(name.into());
        self
    }
}

// ── RemediationPolicy ──────────────────────────────────────────────

/// Per-severity policy for one anomaly kind. Routes the controller's
/// next move. Defaults: Cosmetic→Alert, Functional→AutoCorrect,
/// Critical→RequireApproval. Operator-authored CR can override.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemediationPolicy {
    /// Action to take when severity is Cosmetic.
    pub cosmetic: RemediationAction,
    /// Action to take when severity is Functional.
    pub functional: RemediationAction,
    /// Action to take when severity is Critical.
    pub critical: RemediationAction,
    /// Optional escalation ladder applied when the action is
    /// `Escalate`.
    #[serde(default)]
    pub escalation: Option<EscalationLadder>,
}

impl Default for RemediationPolicy {
    fn default() -> Self {
        Self {
            cosmetic: RemediationAction::Alert,
            functional: RemediationAction::AutoCorrect,
            critical: RemediationAction::RequireApproval,
            escalation: None,
        }
    }
}

impl RemediationPolicy {
    #[must_use]
    pub fn action_for(&self, severity: Severity) -> RemediationAction {
        match severity {
            Severity::Cosmetic => self.cosmetic,
            Severity::Functional => self.functional,
            Severity::Critical => self.critical,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum RemediationAction {
    /// Suppress the anomaly entirely; controller continues normally.
    NoOp,
    /// Log + persist anomaly + emit operator notification.
    Alert,
    /// Drive a reconverge tick automatically.
    AutoCorrect,
    /// Halt convergence; wait for a typed `LavaApproval` CR.
    RequireApproval,
    /// Walk the escalation ladder.
    Escalate,
}

// ── EscalationLadder ──────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EscalationLadder {
    /// Tier 0 fires first; tier N fires after tier N-1's
    /// `wait_before_next` elapses without resolution.
    pub tiers: Vec<EscalationTier>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EscalationTier {
    pub target: NotifyTarget,
    /// Duration before escalating to the next tier.
    #[serde(default = "default_wait")]
    pub wait_before_next: Duration,
}

fn default_wait() -> Duration {
    Duration::minutes(15)
}

/// Typed notification target. Concrete dispatch (Slack webhook, ntfy,
/// PagerDuty, …) is the consumer's responsibility — the operator
/// owns the wiring. This crate only types the address.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum NotifyTarget {
    Slack { webhook_secret_ref: String },
    Ntfy { topic: String },
    Pagerduty { service_key_secret_ref: String },
    Email { address: String },
    Webhook { url: String, secret_ref: Option<String> },
    /// Generic typed target — kind + body fields, lets operators
    /// wire anything without needing a code change here.
    Custom { name: String, params: IndexMap<String, String> },
}

// ── AnomalyRouter trait + impls ────────────────────────────────────

/// Typed routing decision a router returns.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutingDecision {
    pub action: RemediationAction,
    /// Suggested requeue delay before the controller re-evaluates.
    pub requeue_after: Duration,
    /// Selected escalation tier (when action == Escalate).
    pub escalation: Option<EscalationTier>,
}

/// Routing seam. Production impl: [`PolicyRouter`]; tests: [`MockRouter`].
pub trait AnomalyRouter {
    /// Decide what to do with `anomaly` given `policy`.
    ///
    /// # Errors
    /// Routing-specific failures surface as [`RouterError`].
    fn route(
        &self,
        anomaly: &LavaAnomaly,
        policy: &RemediationPolicy,
    ) -> Result<RoutingDecision, RouterError>;
}

#[derive(Debug, Error)]
pub enum RouterError {
    #[error("escalation: {0}")]
    Escalation(String),
}

/// The canonical policy-driven router.
#[derive(Clone, Copy, Debug, Default)]
pub struct PolicyRouter;

impl AnomalyRouter for PolicyRouter {
    fn route(
        &self,
        anomaly: &LavaAnomaly,
        policy: &RemediationPolicy,
    ) -> Result<RoutingDecision, RouterError> {
        let action = policy.action_for(anomaly.severity);
        let requeue_after = match action {
            RemediationAction::NoOp => Duration::minutes(10),
            RemediationAction::Alert => Duration::minutes(5),
            RemediationAction::AutoCorrect => Duration::seconds(30),
            RemediationAction::RequireApproval => Duration::minutes(2),
            RemediationAction::Escalate => Duration::minutes(1),
        };
        let escalation = if action == RemediationAction::Escalate {
            policy
                .escalation
                .as_ref()
                .and_then(|l| l.tiers.first().cloned())
                .ok_or_else(|| RouterError::Escalation("Escalate set but no ladder".into()))
                .map(Some)?
        } else {
            None
        };
        Ok(RoutingDecision {
            action,
            requeue_after,
            escalation,
        })
    }
}

/// Always-returns-the-same-decision router. Tests use this to drive
/// the controller path without policy logic in scope.
pub struct MockRouter {
    pub decision: RoutingDecision,
}

impl MockRouter {
    #[must_use]
    pub fn returning(decision: RoutingDecision) -> Self {
        Self { decision }
    }
}

impl AnomalyRouter for MockRouter {
    fn route(
        &self,
        _anomaly: &LavaAnomaly,
        _policy: &RemediationPolicy,
    ) -> Result<RoutingDecision, RouterError> {
        Ok(self.decision.clone())
    }
}

// ── AnomalyPayload (OutcomeChain integration) ─────────────────────

/// Receipt payload for anomaly chains. Same OutcomeChain machinery,
/// different P — proves the chain abstraction generalizes (third use
/// after OutcomePayload from lava-outcome-chain + DriftReport hashing
/// from lava-drift).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnomalyPayload {
    pub anomaly: LavaAnomaly,
    pub decision: RoutingDecision,
    pub anomaly_hash: ContentHash,
}

impl AnomalyPayload {
    /// Build a payload from anomaly + decision; precomputes the
    /// anomaly's BLAKE3 content-address for cross-chain lookups.
    ///
    /// # Errors
    /// Surfaces serde encoding failures.
    pub fn from(anomaly: LavaAnomaly, decision: RoutingDecision) -> Result<Self, serde_json::Error> {
        let anomaly_hash = ContentHash::of_value(&anomaly)?;
        Ok(Self {
            anomaly,
            decision,
            anomaly_hash,
        })
    }
}

impl Payload for AnomalyPayload {
    const KIND: &'static str = "lava.anomaly";
}

#[cfg(test)]
mod tests {
    use super::*;
    use lava_outcome_chain::{verify_chain, InMemorySink, NoOpVerifier, NoSigning, OutcomeChain};

    fn sample_anomaly(severity: Severity) -> LavaAnomaly {
        LavaAnomaly::new(
            AnomalyKind::DriftDetected,
            severity,
            ResourceAddress::new("rio", "infra", "prod-vpc"),
            "drift detected",
        )
        .with_metadata("fields_count", "3")
    }

    #[test]
    fn default_policy_assigns_typed_actions() {
        let p = RemediationPolicy::default();
        assert_eq!(p.action_for(Severity::Cosmetic), RemediationAction::Alert);
        assert_eq!(
            p.action_for(Severity::Functional),
            RemediationAction::AutoCorrect,
        );
        assert_eq!(
            p.action_for(Severity::Critical),
            RemediationAction::RequireApproval,
        );
    }

    #[test]
    fn policy_router_picks_action_from_policy() {
        let policy = RemediationPolicy::default();
        let anomaly = sample_anomaly(Severity::Functional);
        let decision = PolicyRouter.route(&anomaly, &policy).unwrap();
        assert_eq!(decision.action, RemediationAction::AutoCorrect);
    }

    #[test]
    fn policy_router_picks_first_tier_when_action_is_escalate() {
        let mut policy = RemediationPolicy::default();
        policy.critical = RemediationAction::Escalate;
        policy.escalation = Some(EscalationLadder {
            tiers: vec![
                EscalationTier {
                    target: NotifyTarget::Ntfy {
                        topic: "tier-0".into(),
                    },
                    wait_before_next: Duration::minutes(15),
                },
                EscalationTier {
                    target: NotifyTarget::Pagerduty {
                        service_key_secret_ref: "/cofre/pd".into(),
                    },
                    wait_before_next: Duration::minutes(30),
                },
            ],
        });
        let decision = PolicyRouter
            .route(&sample_anomaly(Severity::Critical), &policy)
            .unwrap();
        assert_eq!(decision.action, RemediationAction::Escalate);
        match decision.escalation.unwrap().target {
            NotifyTarget::Ntfy { topic } => assert_eq!(topic, "tier-0"),
            _ => panic!("unexpected tier"),
        }
    }

    #[test]
    fn policy_router_errors_on_escalate_without_ladder() {
        let mut policy = RemediationPolicy::default();
        policy.critical = RemediationAction::Escalate;
        let err = PolicyRouter
            .route(&sample_anomaly(Severity::Critical), &policy)
            .unwrap_err();
        matches!(err, RouterError::Escalation(_));
    }

    #[test]
    fn anomaly_kind_token_round_trips_through_serde() {
        let a = sample_anomaly(Severity::Functional);
        let j = serde_json::to_string(&a).unwrap();
        assert!(j.contains(r#""kind":"DriftDetected""#));
    }

    #[test]
    fn anomaly_payload_chains_via_outcome_chain_machinery() {
        let mut chain = OutcomeChain::new(InMemorySink::<AnomalyPayload>::default(), NoSigning);
        let p1 = AnomalyPayload::from(
            sample_anomaly(Severity::Functional),
            PolicyRouter
                .route(&sample_anomaly(Severity::Functional), &RemediationPolicy::default())
                .unwrap(),
        )
        .unwrap();
        let p2 = AnomalyPayload::from(
            sample_anomaly(Severity::Critical),
            RoutingDecision {
                action: RemediationAction::RequireApproval,
                requeue_after: Duration::minutes(2),
                escalation: None,
            },
        )
        .unwrap();
        chain.append(p1).unwrap();
        chain.append(p2).unwrap();
        let receipts = chain.read_all().unwrap();
        assert_eq!(receipts.len(), 2);
        assert_eq!(receipts[1].prev_hash, receipts[0].content_hash);
        verify_chain(&receipts, &NoOpVerifier).unwrap();
    }

    #[test]
    fn mock_router_returns_supplied_decision() {
        let d = RoutingDecision {
            action: RemediationAction::NoOp,
            requeue_after: Duration::minutes(1),
            escalation: None,
        };
        let r = MockRouter::returning(d.clone());
        let got = r
            .route(
                &sample_anomaly(Severity::Cosmetic),
                &RemediationPolicy::default(),
            )
            .unwrap();
        assert_eq!(got, d);
    }

    #[test]
    fn notify_target_variants_round_trip_through_serde() {
        let targets = vec![
            NotifyTarget::Slack {
                webhook_secret_ref: "/cofre/slack".into(),
            },
            NotifyTarget::Ntfy {
                topic: "ops".into(),
            },
            NotifyTarget::Webhook {
                url: "https://example".into(),
                secret_ref: None,
            },
            NotifyTarget::Custom {
                name: "mqtt".into(),
                params: IndexMap::from_iter([("broker".into(), "tcp://x".into())]),
            },
        ];
        for t in targets {
            let j = serde_json::to_string(&t).unwrap();
            let parsed: NotifyTarget = serde_json::from_str(&j).unwrap();
            assert_eq!(t, parsed);
        }
    }

    #[test]
    fn custom_anomaly_carries_extension_kind() {
        let a = LavaAnomaly::new(
            AnomalyKind::Custom,
            Severity::Functional,
            ResourceAddress::new("c", "n", "x"),
            "custom signal",
        )
        .with_extension_kind("my-controller.RateLimitHit");
        assert_eq!(
            a.extension_kind.as_deref(),
            Some("my-controller.RateLimitHit")
        );
    }

    #[test]
    fn requeue_after_shortens_with_action_urgency() {
        let policy = RemediationPolicy::default();
        let auto = PolicyRouter
            .route(&sample_anomaly(Severity::Functional), &policy)
            .unwrap();
        let alert = PolicyRouter
            .route(&sample_anomaly(Severity::Cosmetic), &policy)
            .unwrap();
        assert!(auto.requeue_after < alert.requeue_after);
    }
}
