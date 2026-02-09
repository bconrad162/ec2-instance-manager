use std::time::SystemTime;

use crate::models::{Instance, Inventory, TagMapping};

pub fn generate_inventory(_region: &str, mapping: &TagMapping) -> Inventory {
    let mut instances = vec![
        make_instance(
            "i-sim0001",
            "running",
            "10.0.10.11",
            "us-east-1a",
            "api-a",
            "prod",
            "billing",
            "web",
            "team-payments",
            true,
            "Online",
        ),
        make_instance(
            "i-sim0002",
            "running",
            "10.0.10.12",
            "us-east-1a",
            "api-b",
            "prod",
            "billing",
            "web",
            "team-payments",
            true,
            "Online",
        ),
        make_instance(
            "i-sim0003",
            "stopped",
            "10.0.20.10",
            "us-east-1b",
            "worker-a",
            "stage",
            "jobs",
            "worker",
            "team-platform",
            false,
            "Offline",
        ),
        make_instance(
            "i-sim0004",
            "running",
            "10.0.20.11",
            "us-east-1b",
            "worker-b",
            "stage",
            "jobs",
            "worker",
            "team-platform",
            true,
            "Offline",
        ),
        make_instance(
            "i-sim0005",
            "running",
            "10.0.30.21",
            "us-east-1c",
            "db-a",
            "prod",
            "storage",
            "db",
            "team-data",
            true,
            "Online",
        ),
        make_instance(
            "i-sim0006",
            "terminated",
            "",
            "us-east-1c",
            "legacy-a",
            "dev",
            "legacy",
            "misc",
            "team-legacy",
            false,
            "Offline",
        ),
    ];

    for instance in &mut instances {
        derive_fields(instance, mapping);
    }

    Inventory {
        instances,
        fetched_at: SystemTime::now(),
    }
}

fn make_instance(
    instance_id: &str,
    state: &str,
    private_ip: &str,
    az: &str,
    name: &str,
    env: &str,
    app: &str,
    role: &str,
    team: &str,
    ssm_managed: bool,
    ping: &str,
) -> Instance {
    let mut instance = Instance::new(instance_id.to_string(), state.to_string());
    instance.private_ip = if private_ip.is_empty() {
        None
    } else {
        Some(private_ip.to_string())
    };
    instance.az = Some(az.to_string());
    instance.ssm_managed = ssm_managed;
    instance.ssm_ping = Some(ping.to_string());
    instance.ssm_last_ping = Some("2026-02-08T13:00:00Z".to_string());
    instance.tags.insert("Name".to_string(), name.to_string());
    instance.tags.insert("Env".to_string(), env.to_string());
    instance.tags.insert("App".to_string(), app.to_string());
    instance.tags.insert("Role".to_string(), role.to_string());
    instance.tags.insert("Team".to_string(), team.to_string());

    if role == "web" {
        instance.tags.insert(
            "aws:autoscaling:groupName".to_string(),
            "asg-web-prod".to_string(),
        );
    }

    instance
}

fn derive_fields(instance: &mut Instance, mapping: &TagMapping) {
    instance.name = instance.tags.get("Name").cloned();
    instance.env = first_tag(&instance.tags, &mapping.env_keys);
    instance.app_service = first_tag(&instance.tags, &mapping.app_keys);
    instance.role = first_tag(&instance.tags, &mapping.role_keys);
    instance.team_owner = first_tag(&instance.tags, &mapping.team_keys);
    instance.asg = instance
        .tags
        .get("aws:autoscaling:groupName")
        .cloned()
        .or_else(|| instance.tags.get("AutoScalingGroupName").cloned());
}

fn first_tag(tags: &std::collections::BTreeMap<String, String>, keys: &[String]) -> Option<String> {
    for key in keys {
        if let Some(v) = tags.get(key) {
            if !v.trim().is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::TagMapping;

    #[test]
    fn sim_inventory_has_instances() {
        let inv = generate_inventory("us-east-1", &TagMapping::default());
        assert_eq!(inv.instances.len(), 6);
        assert!(inv.instances.iter().any(|i| i.ssm_managed));
        assert!(inv.instances.iter().any(|i| i.state == "stopped"));
    }
}
