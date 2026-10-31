//! Folds a common layer under its overlay.

use crate::node::{Entry, Node, NodeKind};

/// Merges `overlay` over `base`: maps merge key by key, the overlay winning; a null
/// deletes the key; an empty map or list at a key the base holds opts out of it,
/// deleting the key at any depth; any other value replaces the base's. Keys keep
/// the base's order, and keys only the overlay has follow.
pub fn deep_merge(base: &Node, overlay: &Node) -> Node {
    let (Some(base_entries), Some(overlay_entries)) = (base.entries(), overlay.entries()) else {
        return overlay.clone();
    };
    let mut merged: Vec<Entry> = base_entries.to_vec();
    for overlay_entry in overlay_entries {
        let existing = merged
            .iter()
            .position(|entry| entry.key == overlay_entry.key);
        match (&overlay_entry.value.kind, existing) {
            (NodeKind::Null, Some(index)) => {
                merged.remove(index);
            }
            (NodeKind::Null, None) => {}
            (NodeKind::Map(entries), Some(index)) if entries.is_empty() => {
                merged.remove(index);
            }
            (NodeKind::List(items), Some(index)) if items.is_empty() => {
                merged.remove(index);
            }
            (NodeKind::Map(_), Some(index)) if merged[index].value.is_map() => {
                merged[index].value = deep_merge(&merged[index].value, &overlay_entry.value);
            }
            (_, Some(index)) => merged[index] = overlay_entry.clone(),
            (_, None) => merged.push(overlay_entry.clone()),
        }
    }
    Node::new(NodeKind::Map(merged), overlay.position.clone())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::yaml_reader::parse;

    fn node(text: &str) -> Node {
        parse(&Arc::from("mock.yml"), text, &[]).unwrap()
    }

    #[test]
    fn the_overlay_wins_key_by_key() {
        let merged = deep_merge(
            &node("queue:\n  orders:\n    retention: 1\n    visibility: 30\nlocals:\n  a: 1\n"),
            &node("queue:\n  orders:\n    retention: 2\n  payments:\n    retention: 3\n"),
        );
        assert_eq!(
            merged.to_json(),
            serde_json::json!({
                "queue": {
                    "orders": {"retention": 2, "visibility": 30},
                    "payments": {"retention": 3}
                },
                "locals": {"a": 1}
            })
        );
    }

    #[test]
    fn a_null_deletes_the_key() {
        let merged = deep_merge(&node("a:\n  x: 1\nb: 2\n"), &node("a: null\n"));
        assert_eq!(merged.to_json(), serde_json::json!({"b": 2}));
    }

    #[test]
    fn an_empty_map_or_list_opts_out_of_the_common_layer_at_any_depth() {
        let merged = deep_merge(
            &node("a:\n  x: 1\nb: [1]\nc:\n  d:\n    e:\n      f: 1\n    g: 2\n"),
            &node("a: {}\nb: []\nc:\n  d:\n    e: {}\n"),
        );
        assert_eq!(merged.to_json(), serde_json::json!({"c": {"d": {"g": 2}}}));
    }

    #[test]
    fn an_empty_map_or_list_the_common_layer_lacks_is_content() {
        let merged = deep_merge(&node("a: 1\n"), &node("b: {}\nc: []\n"));
        assert_eq!(
            merged.to_json(),
            serde_json::json!({"a": 1, "b": {}, "c": []})
        );
    }

    #[test]
    fn lists_are_replaced_whole() {
        let merged = deep_merge(&node("a: [1, 2, 3]\n"), &node("a: [4]\n"));
        assert_eq!(merged.to_json(), serde_json::json!({"a": [4]}));
    }
}
