use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use crate::aws_cli::run_aws_cli;
use crate::error::Result;
use crate::models::{AuthStatus, AwsContext, Instance, Inventory, Mode, TagMapping};
use crate::sim;
use crate::util::normalize_field;

static CACHE: OnceLock<Mutex<HashMap<String, CacheEntry>>> = OnceLock::new();

#[derive(Clone)]
struct CacheEntry {
    inventory: Inventory,
    cached_at: Instant,
}

pub fn load_inventory(
    context: &AwsContext,
    mapping: &TagMapping,
    force_refresh: bool,
) -> Result<Inventory> {
    if context.mode == Mode::Live && context.auth_status != AuthStatus::Ok {
        return Ok(Inventory {
            instances: Vec::new(),
            fetched_at: SystemTime::now(),
        });
    }

    let account = context
        .account_id
        .clone()
        .unwrap_or_else(|| "unknown-account".to_string());
    let cache_key = format!("{}:{}:{}", context.mode.as_str(), account, context.region);

    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    if !force_refresh {
        if let Ok(guard) = cache.lock() {
            if let Some(entry) = guard.get(&cache_key) {
                if entry.cached_at.elapsed() <= Duration::from_secs(45) {
                    return Ok(entry.inventory.clone());
                }
            }
        }
    }

    let inventory = if context.mode == Mode::Sim {
        sim::generate_inventory(&context.region, mapping)
    } else {
        load_live_inventory(context, mapping)?
    };

    if let Ok(mut guard) = cache.lock() {
        guard.insert(
            cache_key,
            CacheEntry {
                inventory: inventory.clone(),
                cached_at: Instant::now(),
            },
        );
    }

    Ok(inventory)
}

fn load_live_inventory(context: &AwsContext, mapping: &TagMapping) -> Result<Inventory> {
    let profile = context.profile.to_string();
    let region = context.region.to_string();

    let p1 = profile.clone();
    let r1 = region.clone();
    let basics_handle = std::thread::spawn(move || {
        run_aws_cli(
            Some(&p1),
            Some(&r1),
            &[
                "ec2",
                "describe-instances",
                "--output",
                "text",
                "--query",
                "Reservations[].Instances[].[InstanceId,State.Name,PrivateIpAddress,Placement.AvailabilityZone,InstanceType,LaunchTime]",
            ],
        )
    });

    let p2 = profile.clone();
    let r2 = region.clone();
    let tags_handle = std::thread::spawn(move || {
        run_aws_cli(
            Some(&p2),
            Some(&r2),
            &[
                "ec2",
                "describe-tags",
                "--output",
                "text",
                "--query",
                "Tags[].[ResourceId,Key,Value]",
            ],
        )
    });

    let p3 = profile.clone();
    let r3 = region.clone();
    let ssm_handle = std::thread::spawn(move || {
        run_aws_cli(
            Some(&p3),
            Some(&r3),
            &[
                "ssm",
                "describe-instance-information",
                "--output",
                "text",
                "--query",
                "InstanceInformationList[].[InstanceId,PingStatus,LastPingDateTime]",
            ],
        )
    });

    let basics_output = basics_handle.join().expect("basics thread panicked")?;
    let tags_output = tags_handle.join().expect("tags thread panicked")?;
    let ssm_output = ssm_handle.join().expect("ssm thread panicked")?;

    let mut instances = parse_instance_basics(&basics_output)?;
    apply_tags(&mut instances, &tags_output);
    apply_ssm_status(&mut instances, &ssm_output);
    derive_fields(&mut instances, mapping);

    let mut out: Vec<Instance> = instances.into_values().collect();
    out.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));

    Ok(Inventory {
        instances: out,
        fetched_at: SystemTime::now(),
    })
}

fn parse_instance_basics(raw: &str) -> Result<BTreeMap<String, Instance>> {
    let mut map = BTreeMap::new();
    let raw_trimmed = raw.trim();

    if raw_trimmed.is_empty()
        || raw_trimmed.eq_ignore_ascii_case("none")
        || raw_trimmed.eq_ignore_ascii_case("null")
    {
        return Ok(map);
    }

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 2 {
            continue;
        }

        let instance_id = fields[0].trim();
        let state = fields[1].trim();

        if instance_id.is_empty() {
            continue;
        }

        let mut instance = Instance::new(instance_id.to_string(), state.to_string());
        instance.private_ip = fields.get(2).and_then(|v| normalize_field(v));
        instance.az = fields.get(3).and_then(|v| normalize_field(v));
        instance.instance_type = fields.get(4).and_then(|v| normalize_field(v));
        instance.launch_time = fields.get(5).and_then(|v| normalize_field(v));

        map.insert(instance_id.to_string(), instance);
    }

    Ok(map)
}

fn apply_tags(instances: &mut BTreeMap<String, Instance>, raw: &str) {
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 3 {
            continue;
        }

        let offset = if fields[0] == "TAGS" { 1 } else { 0 };
        if fields.len() < offset + 3 {
            continue;
        }

        let resource_id = fields[offset].trim();
        let key = fields[offset + 1].trim();
        let value = fields[offset + 2..].join("\t").trim().to_string();

        if let Some(instance) = instances.get_mut(resource_id) {
            instance.tags.insert(key.to_string(), value);
        }
    }
}

fn apply_ssm_status(instances: &mut BTreeMap<String, Instance>, raw: &str) {
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 2 {
            continue;
        }

        let instance_id = fields[0].trim();
        if let Some(instance) = instances.get_mut(instance_id) {
            instance.ssm_managed = true;
            instance.ssm_ping = fields.get(1).and_then(|v| normalize_field(v));
            instance.ssm_last_ping = fields.get(2).and_then(|v| normalize_field(v));
        }
    }
}

fn derive_fields(instances: &mut BTreeMap<String, Instance>, mapping: &TagMapping) {
    for instance in instances.values_mut() {
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
}

fn first_tag(tags: &BTreeMap<String, String>, keys: &[String]) -> Option<String> {
    for key in keys {
        if let Some(value) = tags.get(key) {
            if !value.trim().is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basics_lines() {
        let raw = "i-1\trunning\t10.0.0.1\tus-east-1a\tt3.micro\t2026-01-01T00:00:00Z\n";
        let got = parse_instance_basics(raw).expect("expected one instance row");
        assert_eq!(got.len(), 1);
        let one = got.get("i-1").expect("instance must exist");
        assert_eq!(one.private_ip.as_deref(), Some("10.0.0.1"));
        assert_eq!(one.state, "running");
    }

    #[test]
    fn parse_basics_empty_output_is_empty_inventory() {
        let got = parse_instance_basics("").expect("empty output should parse");
        assert!(got.is_empty());
    }

    #[test]
    fn parse_basics_none_output_is_empty_inventory() {
        let got = parse_instance_basics("None").expect("None output should parse");
        assert!(got.is_empty());
    }

    #[test]
    fn tag_apply_with_prefix() {
        let mut map = BTreeMap::new();
        map.insert(
            "i-1".to_string(),
            Instance::new("i-1".to_string(), "running".to_string()),
        );
        apply_tags(&mut map, "TAGS\ti-1\tName\tapi\n");
        let one = map.get("i-1").expect("instance must exist");
        assert_eq!(one.tags.get("Name").map(String::as_str), Some("api"));
    }
}
