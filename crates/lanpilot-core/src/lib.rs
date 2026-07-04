pub const PRODUCT_NAME: &str = "LanPilot";
pub const TAGLINE: &str = "LAN-first remote desktop control";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeIdentity {
    pub machine_name: String,
    pub ipv4: String,
}

impl NodeIdentity {
    pub fn new(machine_name: impl Into<String>, ipv4: impl Into<String>) -> Self {
        Self {
            machine_name: machine_name.into(),
            ipv4: ipv4.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_constants_are_stable() {
        assert_eq!(PRODUCT_NAME, "LanPilot");
        assert_eq!(TAGLINE, "LAN-first remote desktop control");
    }

    #[test]
    fn node_identity_builder_sets_fields() {
        let node = NodeIdentity::new("pc-host", "192.168.1.33");
        assert_eq!(node.machine_name, "pc-host");
        assert_eq!(node.ipv4, "192.168.1.33");
    }
}
