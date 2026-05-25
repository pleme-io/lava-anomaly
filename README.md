# lava-anomaly

Typed AnomalyController surface for the lava-suite. L4 foundation.
Operationalizes [theory/CONTINUOUS-SOLUTION-MACHINE.md](https://github.com/pleme-io/theory)
anomaly routing for IaC drift / synth failures / apply failures /
policy violations.

## Shape

```text
controller emits LavaAnomaly
  ─▶ AnomalyRouter::route(anomaly, policy) -> RoutingDecision
        { action, requeue_after, escalation }
    ├─ AutoCorrect → controller reconverges
    ├─ RequireApproval → halts; waits for LavaApproval CR
    ├─ Escalate → walks EscalationLadder
    └─ Alert / NoOp → log + chain only
```

## Abstractions

| Type | Purpose |
|---|---|
| `LavaAnomaly { kind, severity, source, message, metadata }` | Typed event |
| `AnomalyKind` | Stable tag: Drift / Synth / Apply / Dependency / Policy / Custom |
| `RemediationPolicy { cosmetic, functional, critical, escalation }` | Per-severity routing |
| `RemediationAction` | NoOp / Alert / AutoCorrect / RequireApproval / Escalate |
| `EscalationLadder` + `EscalationTier` + `NotifyTarget` | Typed notifications |
| `AnomalyRouter` trait + `PolicyRouter` + `MockRouter` | Routing seam |
| `RoutingDecision` | Typed action + requeue + escalation |
| `AnomalyPayload : Payload` | Chains via lava-outcome-chain machinery |

## Default policy

| Severity | Default action |
|---|---|
| Cosmetic | Alert |
| Functional | AutoCorrect |
| Critical | RequireApproval |

Override per-LavaArchitecture via a typed CR.

## Use

```rust
let policy = RemediationPolicy::default();
let anomaly = LavaAnomaly::new(
    AnomalyKind::DriftDetected,
    Severity::Critical,
    ResourceAddress::new("rio", "infra", "prod-vpc"),
    "drift detected: 3 critical fields",
);
let decision = PolicyRouter.route(&anomaly, &policy)?;
match decision.action {
    RemediationAction::RequireApproval => { /* halt + wait */ }
    RemediationAction::AutoCorrect => { /* reconverge */ }
    RemediationAction::Escalate => { /* notify decision.escalation */ }
    _ => { /* alert + chain */ }
}
```

## Tests

10 unit tests cover default policy, PolicyRouter for all severities,
escalation ladder selection, escalation-without-ladder error, anomaly
serde, AnomalyPayload through OutcomeChain machinery (third use of
the chain abstraction), MockRouter, NotifyTarget variants, Custom
anomaly extension kind, requeue urgency ordering.
