//! Global + entity execution policy merge and a single evaluation path for capabilities.

use consistent_world_common::capabilities::{CapabilityId, ExecutionLevel};

use crate::config::GlobalPolicyLayer;
use crate::state::ExecutionPolicies;

/// Effective policy after merging the global layer with one entity's overrides.
#[derive(Debug, Clone)]
pub struct MergedExecutionPolicies {
    pub allowed_capabilities: Vec<CapabilityId>,
    pub allowed_execution_levels: Vec<ExecutionLevel>,
    pub approval_required_capabilities: Vec<CapabilityId>,
}

/// Result of checking one capability against a [`MergedExecutionPolicies`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityPolicyDecision {
    Allow,
    Deny(String),
    RequireApproval,
}

// The full set of capabilities policy can reason about; an empty allow-list
// at a layer is treated as "everything", so this is what that expands to.
fn all_capabilities() -> Vec<CapabilityId> {
    vec![
        CapabilityId::FsRead,
        CapabilityId::FsWrite,
        CapabilityId::ShellExecSafe,
        CapabilityId::ShellExecPrivileged,
        CapabilityId::HttpFetch,
        CapabilityId::MessageInternal,
        CapabilityId::MessageExternal,
        CapabilityId::PublishPublic,
    ]
}

/// Intersect non-empty lists; empty means "no restriction" at that layer.
fn merge_allowed(global: &[CapabilityId], entity: &[CapabilityId]) -> Vec<CapabilityId> {
    let universe = all_capabilities();
    let g = if global.is_empty() {
        universe.clone()
    } else {
        global.to_vec()
    };
    if entity.is_empty() {
        return g;
    }
    let e_set: std::collections::HashSet<_> = entity.iter().copied().collect();
    g.into_iter().filter(|c| e_set.contains(c)).collect()
}

// Entity-level execution levels replace the global list wholesale when set,
// rather than intersecting with it — unlike `merge_allowed`.
fn merge_levels(global: &[ExecutionLevel], entity: &[ExecutionLevel]) -> Vec<ExecutionLevel> {
    if !entity.is_empty() {
        return entity.to_vec();
    }
    global.to_vec()
}

// Union of both layers: anything either layer flags as approval-required
// stays approval-required, so an entity can only add gates, not remove them.
fn merge_approval(global: &[CapabilityId], entity: &[CapabilityId]) -> Vec<CapabilityId> {
    let mut out: Vec<CapabilityId> = global.to_vec();
    for c in entity {
        if !out.contains(c) {
            out.push(*c);
        }
    }
    out
}

/// Combine the global policy layer with one entity's overrides into a single
/// effective policy, ready for [`evaluate_capability`].
pub fn merge_global_and_entity(
    global: &GlobalPolicyLayer,
    entity: Option<&ExecutionPolicies>,
) -> MergedExecutionPolicies {
    let e_allowed = entity
        .map(|p| p.allowed_capabilities.as_slice())
        .unwrap_or(&[]);
    let e_levels = entity
        .map(|p| p.allowed_execution_levels.as_slice())
        .unwrap_or(&[]);
    let e_approval = entity
        .map(|p| p.approval_required_capabilities.as_slice())
        .unwrap_or(&[]);

    MergedExecutionPolicies {
        allowed_capabilities: merge_allowed(&global.allowed_capabilities, e_allowed),
        allowed_execution_levels: merge_levels(&global.allowed_execution_levels, e_levels),
        approval_required_capabilities: merge_approval(
            &global.approval_required_capabilities,
            e_approval,
        ),
    }
}

/// Decide the outcome for one capability: denied if it's outside the
/// effective allow-list, else gated if it requires approval, else allowed.
pub fn evaluate_capability(
    merged: &MergedExecutionPolicies,
    capability: CapabilityId,
) -> CapabilityPolicyDecision {
    if !merged.allowed_capabilities.is_empty() && !merged.allowed_capabilities.contains(&capability)
    {
        return CapabilityPolicyDecision::Deny(format!(
            "capability '{}' not in effective allow-list",
            capability
        ));
    }
    if merged.approval_required_capabilities.contains(&capability) {
        return CapabilityPolicyDecision::RequireApproval;
    }
    CapabilityPolicyDecision::Allow
}
