use serde::{Deserialize, Serialize};

/// Explicit ownership boundary between the existing Browser Runtime and the
/// Computer broker. The broker does not inspect DOM, tabs, network traffic,
/// downloads, or browser sessions.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputerSurfaceKind {
    NativeDesktop,
    BrowserDom,
    BrowserCustomRendered,
    BrowserNativeDialog,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputerBoundaryRoute {
    ComputerBroker,
    BrowserRuntime,
    Denied,
}

pub fn route_surface(surface: ComputerSurfaceKind) -> ComputerBoundaryRoute {
    match surface {
        ComputerSurfaceKind::NativeDesktop | ComputerSurfaceKind::BrowserNativeDialog => {
            ComputerBoundaryRoute::ComputerBroker
        }
        ComputerSurfaceKind::BrowserDom => ComputerBoundaryRoute::BrowserRuntime,
        ComputerSurfaceKind::BrowserCustomRendered => ComputerBoundaryRoute::ComputerBroker,
    }
}
