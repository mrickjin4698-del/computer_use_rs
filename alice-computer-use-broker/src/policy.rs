use alice_computer_use_core::{
    ComputerAccessBoundary, ComputerAction, ComputerExecutionIntent, ComputerExecutionRequest,
    SemanticAction, Window, WindowId,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::{BrokerError, ComputerRequestOwner};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputerRiskLevel {
    ReadOnly,
    BenignInteraction,
    ContentMutation,
    ExternalEffect,
    Sensitive,
    Blocked,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputerApprovalProfile {
    #[default]
    Conservative,
    Balanced,
    Autonomous,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundComputerAccess {
    #[default]
    ObserveOnly,
    InteractiveWithApproval,
    InteractiveGranted,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum HostProtectedSurface {
    None,
    Window { window_id: WindowId },
    Desktop { reason: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "decision")]
pub enum ComputerPolicyDecision {
    Allowed {
        risk: ComputerRiskLevel,
    },
    ApprovalRequired {
        approval_id: String,
        risk: ComputerRiskLevel,
    },
    Denied {
        risk: ComputerRiskLevel,
        reason: String,
    },
}

#[derive(Clone, Debug)]
pub struct ComputerPolicy {
    profile: ComputerApprovalProfile,
    background_access: BackgroundComputerAccess,
    protected_windows: HashSet<WindowId>,
    protected_desktop: Option<String>,
    granted_approvals: HashSet<String>,
}

impl Default for ComputerPolicy {
    fn default() -> Self {
        Self {
            profile: ComputerApprovalProfile::Conservative,
            background_access: BackgroundComputerAccess::ObserveOnly,
            protected_windows: HashSet::new(),
            protected_desktop: None,
            granted_approvals: HashSet::new(),
        }
    }
}

impl ComputerPolicy {
    pub fn profile(&self) -> ComputerApprovalProfile {
        self.profile
    }

    pub fn background_access(&self) -> BackgroundComputerAccess {
        self.background_access
    }

    pub fn set_profile(&mut self, profile: ComputerApprovalProfile) {
        self.profile = profile;
    }

    pub fn set_background_access(&mut self, access: BackgroundComputerAccess) {
        self.background_access = access;
    }

    pub fn protect_window(&mut self, window_id: WindowId) {
        self.protected_windows.insert(window_id);
    }

    pub fn unprotect_window(&mut self, window_id: &WindowId) {
        self.protected_windows.remove(window_id);
    }

    pub fn protect_desktop(&mut self, reason: impl Into<String>) {
        self.protected_desktop = Some(reason.into());
    }

    pub fn clear_protected_desktop(&mut self) {
        self.protected_desktop = None;
    }

    /// The host records approval only after its authoritative approval flow has
    /// resolved. A request can reference that opaque id, but cannot grant it.
    pub fn record_approval(&mut self, approval_id: impl Into<String>) {
        self.granted_approvals.insert(approval_id.into());
    }

    pub fn revoke_approval(&mut self, approval_id: &str) {
        self.granted_approvals.remove(approval_id);
    }

    pub fn protected_surface(&self, target: Option<&Window>) -> HostProtectedSurface {
        if let Some(reason) = &self.protected_desktop {
            return HostProtectedSurface::Desktop {
                reason: reason.clone(),
            };
        }
        if let Some(window) = target {
            if self.protected_windows.contains(&window.id)
                || window.process_id == Some(std::process::id())
            {
                return HostProtectedSurface::Window {
                    window_id: window.id.clone(),
                };
            }
        }
        HostProtectedSurface::None
    }

    pub fn admit(
        &self,
        request_id: &str,
        owner: &ComputerRequestOwner,
        request: &ComputerExecutionRequest,
        target: Option<&Window>,
    ) -> Result<ComputerPolicyDecision, BrokerError> {
        if let Some(window) = target {
            if let Some(security) = &window.security {
                if security.boundary != ComputerAccessBoundary::Allowed {
                    return Err(BrokerError::SecurityBlocked(
                        security.reason.clone().unwrap_or_else(|| {
                            format!("target boundary is {:?}", security.boundary)
                        }),
                    ));
                }
            }
        }
        match self.protected_surface(target) {
            HostProtectedSurface::None => {}
            HostProtectedSurface::Window { window_id } => {
                return Err(BrokerError::ProtectedSurface {
                    target: Some(window_id),
                })
            }
            HostProtectedSurface::Desktop { reason } => {
                return Err(BrokerError::ProtectedSurfaceReason {
                    target: None,
                    reason,
                })
            }
        }

        if owner.background_task_id.is_some()
            && self.background_access == BackgroundComputerAccess::ObserveOnly
        {
            return Err(BrokerError::BackgroundAccessDenied);
        }

        let risk = risk_for(request);
        if risk == ComputerRiskLevel::Blocked {
            return Ok(ComputerPolicyDecision::Denied {
                risk,
                reason: "computer action is blocked by policy".into(),
            });
        }
        let needs_approval = match self.profile {
            ComputerApprovalProfile::Conservative => true,
            ComputerApprovalProfile::Balanced => {
                risk >= ComputerRiskLevel::ContentMutation || owner.background_task_id.is_some()
            }
            ComputerApprovalProfile::Autonomous => false,
        } || (owner.background_task_id.is_some()
            && self.background_access == BackgroundComputerAccess::InteractiveWithApproval);
        if needs_approval && !self.granted_approvals.contains(request_id) {
            return Err(BrokerError::ApprovalRequired {
                approval_id: request_id.to_owned(),
                risk,
            });
        }
        Ok(ComputerPolicyDecision::Allowed { risk })
    }
}

pub(crate) fn risk_for(request: &ComputerExecutionRequest) -> ComputerRiskLevel {
    match &request.intent {
        ComputerExecutionIntent::Semantic(action) => match action {
            SemanticAction::Focus { .. }
            | SemanticAction::Invoke { .. }
            | SemanticAction::Toggle { .. }
            | SemanticAction::Select { .. }
            | SemanticAction::Expand { .. }
            | SemanticAction::Collapse { .. }
            | SemanticAction::ScrollIntoView { .. } => ComputerRiskLevel::BenignInteraction,
            SemanticAction::SetValue { .. } | SemanticAction::SetRangeValue { .. } => {
                ComputerRiskLevel::ContentMutation
            }
        },
        ComputerExecutionIntent::Pixel { action, .. } => match action {
            ComputerAction::Click { .. }
            | ComputerAction::DoubleClick { .. }
            | ComputerAction::RightClick { .. }
            | ComputerAction::MovePointer { .. }
            | ComputerAction::Drag { .. }
            | ComputerAction::Scroll { .. }
            | ComputerAction::FocusWindow { .. }
            | ComputerAction::ModifiedPointer { .. } => ComputerRiskLevel::BenignInteraction,
            ComputerAction::TypeText { .. }
            | ComputerAction::KeyPress { .. }
            | ComputerAction::Hotkey { .. }
            | ComputerAction::MouseDown { .. }
            | ComputerAction::MouseUp { .. }
            | ComputerAction::MiddleClick { .. }
            | ComputerAction::TripleClick { .. }
            | ComputerAction::ModifierClick { .. }
            | ComputerAction::KeyDown { .. }
            | ComputerAction::KeyUp { .. }
            | ComputerAction::HoldKey { .. } => ComputerRiskLevel::ContentMutation,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alice_computer_use_core::{
        ComputerExecutionRequest, Coordinate, CoordinateSpace, DpiScale, ElementId, Point,
        SemanticAction, Size, Window, WindowId,
    };

    fn owner(background_task_id: Option<&str>) -> ComputerRequestOwner {
        ComputerRequestOwner::new(
            "application-session",
            "agent-thread",
            "turn",
            "tool-call",
            background_task_id.map(str::to_owned),
        )
    }

    fn request() -> ComputerExecutionRequest {
        ComputerExecutionRequest::semantic(SemanticAction::Focus {
            element_id: ElementId::new("element"),
        })
    }

    #[test]
    fn conservative_policy_requires_opaque_per_request_approval() {
        let policy = ComputerPolicy::default();
        let result = policy.admit("approval-id", &owner(None), &request(), None);
        assert!(matches!(
            result,
            Err(BrokerError::ApprovalRequired { approval_id, .. })
                if approval_id == "approval-id"
        ));
    }

    #[test]
    fn recorded_approval_allows_only_that_request() {
        let mut policy = ComputerPolicy::default();
        policy.record_approval("approval-id");
        assert!(matches!(
            policy.admit("approval-id", &owner(None), &request(), None),
            Ok(ComputerPolicyDecision::Allowed { .. })
        ));
        assert!(matches!(
            policy.admit("different-id", &owner(None), &request(), None),
            Err(BrokerError::ApprovalRequired { .. })
        ));
    }

    #[test]
    fn autonomous_profile_does_not_create_a_per_action_prompt() {
        let mut policy = ComputerPolicy::default();
        policy.set_profile(ComputerApprovalProfile::Autonomous);
        assert!(matches!(
            policy.admit("request-id", &owner(None), &request(), None),
            Ok(ComputerPolicyDecision::Allowed { .. })
        ));
    }

    #[test]
    fn host_process_windows_are_protected_without_renderer_registration() {
        let policy = ComputerPolicy::default();
        let mut window = Window {
            id: WindowId::new("host-window"),
            title: "Alice".into(),
            class_name: None,
            process_id: Some(std::process::id()),
            bounds: Coordinate {
                space: CoordinateSpace::DesktopPhysical,
                point: Point { x: 0.0, y: 0.0 },
                extent: Size {
                    width: 100.0,
                    height: 100.0,
                },
                dpi: DpiScale::ONE,
                display_id: None,
                frame_id: None,
            },
            screen_id: None,
            active: true,
            security: None,
        };
        assert!(matches!(
            policy.protected_surface(Some(&window)),
            HostProtectedSurface::Window { .. }
        ));
        window.process_id = None;
        assert_eq!(
            policy.protected_surface(Some(&window)),
            HostProtectedSurface::None
        );
    }

    #[test]
    fn background_observe_only_fails_closed_before_approval() {
        let policy = ComputerPolicy::default();
        assert!(matches!(
            policy.admit(
                "approval-id",
                &owner(Some("background-task")),
                &request(),
                None,
            ),
            Err(BrokerError::BackgroundAccessDenied)
        ));
    }
}
