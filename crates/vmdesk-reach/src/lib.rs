//! Reachability: which interface routes to each VM, whether SSH works over its path,
//! and a monitor that rechecks on a schedule and whenever the laptop's network changes.

mod check;
mod monitor;
mod route;

pub use check::{Probe, check};
pub use monitor::{Checked, Monitor};
pub use route::{
    RouteInspector, SystemRoutes, local_interfaces, parse_ip_route_dev, parse_route_get,
};
