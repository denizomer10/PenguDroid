//! The binder service registry and (fallback) userspace broker.
//!
//! On a machine with the kernel binder driver, services would be real binder
//! nodes. Here we run a **userspace broker**: named services get a small
//! integer handle, and transactions are routed through the daemon's control
//! socket as [`pengudroid_proto::Message::BinderTransact`]. This preserves the
//! *shape* of a binder transaction (`handle`, `code`, `flags`, `data`) while
//! avoiding any dependency on a kernel binder device. The mapping to real
//! binder nodes is a drop-in replacement behind [`ServiceTable::transact`].

use std::collections::BTreeMap;

use crate::stubs;

/// Result of a transaction: an Android-style `status_t` (-errno on failure)
/// plus the reply parcel bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub status: i32,
    pub data: Vec<u8>,
}

impl Reply {
    pub fn ok(data: Vec<u8>) -> Reply {
        Reply { status: 0, data }
    }

    /// `-ENOSYS`: the service exists but does not implement this transaction.
    pub fn unsupported() -> Reply {
        Reply { status: -38, data: Vec::new() }
    }

    /// `-EINVAL`: malformed request.
    pub fn invalid() -> Reply {
        Reply { status: -22, data: Vec::new() }
    }
}

/// Which stub implementation backs a service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceKind {
    Telephony,
    Camera,
    ActivityManager,
    PackageManager,
    SurfaceComposer,
    AudioFlinger,
    WindowManager,
    InputFlinger,
    /// Registered by a guest; transactions are echoed back with `-ENOSYS`
    /// until a real implementation lands.
    Guest,
}

pub struct Service {
    pub name: String,
    pub iface: String,
    pub handle: u32,
    pub kind: ServiceKind,
}

/// Handle allocator + dispatch table.
#[derive(Default)]
pub struct ServiceTable {
    next: u32,
    by_name: BTreeMap<String, u32>,
    by_handle: BTreeMap<u32, Service>,
}

impl ServiceTable {
    pub fn new() -> ServiceTable {
        ServiceTable { next: 1, ..Default::default() }
    }

    /// Pre-register the platform services that apps expect to find. These are
    /// stubs (see [`crate::stubs`]) exposing the correct names and interfaces.
    pub fn with_system_stubs() -> ServiceTable {
        let mut t = ServiceTable::new();
        for (name, iface, kind) in stubs::system_services() {
            t.register(name, iface, kind);
        }
        t
    }

    pub fn register(&mut self, name: &str, iface: &str, kind: ServiceKind) -> u32 {
        if let Some(&h) = self.by_name.get(name) {
            return h;
        }
        let handle = self.next;
        self.next = self.next.wrapping_add(1).max(1);
        self.by_name.insert(name.to_string(), handle);
        self.by_handle.insert(handle, Service {
            name: name.to_string(),
            iface: iface.to_string(),
            handle,
            kind,
        });
        handle
    }

    pub fn lookup(&self, name: &str) -> Option<u32> {
        self.by_name.get(name).copied()
    }

    pub fn count(&self) -> usize {
        self.by_handle.len()
    }

    pub fn list(&self) -> Vec<(&str, &str, u32)> {
        let mut v: Vec<_> =
            self.by_handle.values().map(|s| (s.name.as_str(), s.iface.as_str(), s.handle)).collect();
        v.sort_by_key(|(_, _, h)| *h);
        v
    }

    /// Route a transaction to the backing implementation.
    pub fn transact(&self, handle: u32, code: u32, _flags: u32, data: &[u8]) -> Reply {
        let Some(svc) = self.by_handle.get(&handle) else {
            return Reply { status: -22, data: Vec::new() };
        };
        match svc.kind {
            ServiceKind::Telephony => stubs::telephony_transact(code, data),
            ServiceKind::Camera => stubs::camera_transact(code, data),
            ServiceKind::ActivityManager => stubs::activity_manager_transact(code, data),
            ServiceKind::PackageManager => stubs::package_manager_transact(code, data),
            ServiceKind::SurfaceComposer => stubs::surface_composer_transact(code, data),
            ServiceKind::AudioFlinger | ServiceKind::WindowManager | ServiceKind::InputFlinger => {
                Reply::ok(Vec::new())
            }
            ServiceKind::Guest => Reply::unsupported(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_services_are_registered() {
        let t = ServiceTable::with_system_stubs();
        assert!(t.count() >= 6);
        assert!(t.lookup("phone").is_some());
        assert!(t.lookup("media.camera").is_some());
    }

    #[test]
    fn unknown_handle_is_invalid() {
        let t = ServiceTable::new();
        assert_eq!(t.transact(999, 0, 0, &[]).status, -22);
    }

    #[test]
    fn register_is_idempotent() {
        let mut t = ServiceTable::new();
        let a = t.register("x", "IX", ServiceKind::Guest);
        let b = t.register("x", "IX", ServiceKind::Guest);
        assert_eq!(a, b);
        assert_eq!(t.count(), 1);
    }

    #[test]
    fn camera_reports_zero_cameras() {
        let t = ServiceTable::with_system_stubs();
        let h = t.lookup("media.camera").unwrap();
        // getNumberOfCameras (illustrative code 1) returns 0.
        let r = t.transact(h, 1, 0, &[]);
        assert_eq!(r.status, 0);
        assert_eq!(r.data, 0i32.to_le_bytes().to_vec());
    }
}
