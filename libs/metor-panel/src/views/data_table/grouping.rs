use std::collections::{BTreeMap, BTreeSet};

use gpui::SharedString;
use metor_db::DB;
use metor_proto::types::ComponentId;

#[derive(Clone, Debug)]
pub struct Group {
    pub name: SharedString,
    pub fields: Vec<SharedString>,
    pub instances: Vec<GroupInstance>,
}

#[derive(Clone, Debug)]
pub struct GroupInstance {
    pub name: SharedString,
    pub short_name: SharedString,
    pub field_ids: Vec<Option<ComponentId>>,
}

#[derive(Clone, Debug)]
pub struct MetadataEntry {
    pub id: ComponentId,
    pub name: String,
    pub group_name: Option<String>,
    pub has_component: bool,
}

pub fn detect_groups(db: &DB) -> Vec<Group> {
    let entries: Vec<MetadataEntry> = db.with_state(|state| {
        state
            .component_metadata_iter()
            .filter(|(_, meta)| !meta.is_hidden())
            .map(|(id, meta)| MetadataEntry {
                id: *id,
                name: meta.name.clone(),
                group_name: meta.group_name().map(|s| s.to_string()),
                has_component: state.get_component(*id).is_some(),
            })
            .collect()
    });
    detect_groups_from_entries(entries)
}

pub fn detect_groups_from_entries(entries: Vec<MetadataEntry>) -> Vec<Group> {
    // Longest-prefix-wins so nested groups claim their own fields
    // rather than bleeding into an ancestor's field list.
    let mut instance_prefixes: Vec<(String, String)> = entries
        .iter()
        .filter_map(|e| e.group_name.clone().map(|g| (g, e.name.clone())))
        .collect();
    instance_prefixes.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

    let mut by_group: BTreeMap<String, BTreeMap<String, BTreeMap<String, ComponentId>>> =
        BTreeMap::new();
    // Seed so instances with zero detected fields still render as a row.
    for (g, iname) in &instance_prefixes {
        by_group
            .entry(g.clone())
            .or_default()
            .entry(iname.clone())
            .or_default();
    }

    for entry in &entries {
        if !entry.has_component {
            continue;
        }
        let mut matched: Option<(&String, &String)> = None;
        for (g, iname) in &instance_prefixes {
            if entry.name.len() > iname.len()
                && entry.name.as_bytes()[iname.len()] == b'.'
                && entry.name.starts_with(iname.as_str())
            {
                matched = Some((g, iname));
                break;
            }
        }
        let Some((g, iname)) = matched else { continue };
        let rel_path = entry.name[iname.len() + 1..].to_string();
        by_group
            .entry(g.clone())
            .or_default()
            .entry(iname.clone())
            .or_default()
            .insert(rel_path, entry.id);
    }

    let mut groups: Vec<Group> = by_group
        .into_iter()
        .map(|(gname, inst_map)| {
            let mut field_set: BTreeSet<String> = BTreeSet::new();
            for fields in inst_map.values() {
                for p in fields.keys() {
                    field_set.insert(p.clone());
                }
            }
            let fields: Vec<SharedString> = field_set.into_iter().map(SharedString::from).collect();

            let instances: Vec<GroupInstance> = inst_map
                .into_iter()
                .map(|(iname, field_map)| {
                    let field_ids: Vec<Option<ComponentId>> = fields
                        .iter()
                        .map(|f| field_map.get(f.as_ref()).copied())
                        .collect();
                    let short = iname
                        .rsplit('.')
                        .next()
                        .unwrap_or(iname.as_str())
                        .to_string();
                    GroupInstance {
                        name: SharedString::from(iname),
                        short_name: SharedString::from(short),
                        field_ids,
                    }
                })
                .collect();

            Group {
                name: SharedString::from(gname),
                fields,
                instances,
            }
        })
        .collect();

    groups.sort_by(|a, b| a.name.cmp(&b.name));
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cid(n: u64) -> ComponentId {
        ComponentId(n)
    }

    fn parent(n: u64, name: &str, group: &str) -> MetadataEntry {
        MetadataEntry {
            id: cid(n),
            name: name.to_string(),
            group_name: Some(group.to_string()),
            has_component: false,
        }
    }

    fn leaf(n: u64, name: &str) -> MetadataEntry {
        MetadataEntry {
            id: cid(n),
            name: name.to_string(),
            group_name: None,
            has_component: true,
        }
    }

    #[test]
    fn two_instances_one_group() {
        let groups = detect_groups_from_entries(vec![
            parent(1, "gps0", "GPS"),
            parent(2, "gps1", "GPS"),
            leaf(10, "gps0.altitude"),
            leaf(11, "gps0.pos_ecef"),
            leaf(12, "gps0.lat_long"),
            leaf(13, "gps1.altitude"),
            leaf(14, "gps1.pos_ecef"),
            leaf(15, "gps1.lat_long"),
        ]);
        assert_eq!(groups.len(), 1);
        let g = &groups[0];
        assert_eq!(g.name.as_ref(), "GPS");
        assert_eq!(
            g.fields.iter().map(|s| s.as_ref()).collect::<Vec<_>>(),
            vec!["altitude", "lat_long", "pos_ecef"]
        );
        assert_eq!(g.instances.len(), 2);
        let gps0 = &g.instances[0];
        assert_eq!(gps0.short_name.as_ref(), "gps0");
        assert_eq!(gps0.field_ids.iter().filter(|x| x.is_some()).count(), 3);
    }

    #[test]
    fn ragged_instance_keeps_field_with_none_cell() {
        let groups = detect_groups_from_entries(vec![
            parent(1, "gps0", "GPS"),
            parent(2, "gps1", "GPS"),
            leaf(10, "gps0.altitude"),
            leaf(11, "gps0.extra"),
            leaf(13, "gps1.altitude"),
        ]);
        assert_eq!(groups.len(), 1);
        let g = &groups[0];
        assert_eq!(
            g.fields.iter().map(|s| s.as_ref()).collect::<Vec<_>>(),
            vec!["altitude", "extra"]
        );
        // gps1 is missing "extra"
        let gps1 = g
            .instances
            .iter()
            .find(|i| i.short_name.as_ref() == "gps1")
            .unwrap();
        let extra_ix = g.fields.iter().position(|f| f.as_ref() == "extra").unwrap();
        assert!(gps1.field_ids[extra_ix].is_none());
    }

    #[test]
    fn distinct_group_names_are_independent() {
        let groups = detect_groups_from_entries(vec![
            parent(1, "gps0", "GPS"),
            parent(2, "imu0", "IMU"),
            leaf(10, "gps0.altitude"),
            leaf(11, "imu0.temp"),
        ]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].name.as_ref(), "GPS");
        assert_eq!(groups[1].name.as_ref(), "IMU");
    }

    #[test]
    fn metadata_without_group_name_is_ignored() {
        let groups = detect_groups_from_entries(vec![leaf(10, "orphan.value"), leaf(11, "other")]);
        assert!(groups.is_empty());
    }

    #[test]
    fn instance_with_no_leaves_still_forms_a_group() {
        let groups =
            detect_groups_from_entries(vec![parent(1, "gps0", "GPS"), parent(2, "gps1", "GPS")]);
        assert_eq!(groups.len(), 1);
        let g = &groups[0];
        assert!(g.fields.is_empty());
        assert_eq!(g.instances.len(), 2);
    }

    #[test]
    fn nested_instance_prefix_wins_longest_match() {
        // A hypothetical: "cube_sat.gps0" nested under a group, and
        // "cube_sat" itself declared as another group. Fields must go
        // to the closest ancestor instance, not the outer one.
        let groups = detect_groups_from_entries(vec![
            parent(1, "cube_sat", "Satellite"),
            parent(2, "cube_sat.gps0", "GPS"),
            parent(3, "cube_sat.gps1", "GPS"),
            leaf(10, "cube_sat.gps0.altitude"),
            leaf(11, "cube_sat.gps1.altitude"),
            leaf(12, "cube_sat.status"),
        ]);
        let gps = groups.iter().find(|g| g.name.as_ref() == "GPS").unwrap();
        assert_eq!(gps.fields.len(), 1);
        assert_eq!(gps.fields[0].as_ref(), "altitude");

        let sat = groups
            .iter()
            .find(|g| g.name.as_ref() == "Satellite")
            .unwrap();
        assert_eq!(sat.fields.len(), 1);
        assert_eq!(sat.fields[0].as_ref(), "status");
    }
}
