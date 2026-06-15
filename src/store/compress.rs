//! Deterministic, reversible structural JSON compaction.
//!
//! Three transforms, applied in order:
//!   1. drop `null` object fields (they carry no information);
//!   2. **columnar table transposition** — any array of >= 2 objects that all
//!      share the same key set is replaced with a single schema (the field
//!      names, once) plus value-only rows, instead of repeating every field
//!      name on every object. This is the deterministic core of headroom's
//!      SmartCrusher CSV-schema compaction (`crates/headroom-core/.../
//!      compaction/{compactor,formatter}.rs`): emit the schema once, then rows
//!      as positional tuples. Homogeneous record arrays (issue lists, graph
//!      node/edge sets) are exactly its best case; and
//!   3. dictionary-encode repeated string *values* into a shared table,
//!      replacing each occurrence with a compact `{"$": index}` marker — the
//!      "dictionary-encode repeated values, not just keys" technique.
//!
//! The output is `{"_d": [<dict>], "_v": <transformed>}`. A transposed table is
//! a one-key object `{"_tbl": {"c": [<cols>], "r": [[<row values>], ...]}}`.
//! `expand` is the exact inverse of steps 2 and 3, so
//! `expand(compact(v)) == drop_nulls(v)`. The result is always paired with a
//! store ref to the untouched original, so nothing is ever lost.
//!
//! What is deliberately NOT ported from SmartCrusher: its lossy row-sampling /
//! anomaly-preservation path (keep ~50 of 1000 rows). That is lossy and not
//! reversible; ctxforge keeps every row and recovers the original via the
//! store. We port only the deterministic, lossless structural mechanism.

use std::collections::HashMap;

use serde_json::{json, Map, Value};

/// Minimum string length worth dictionary-encoding (the `{"$":N}` marker has
/// overhead, so very short strings would not shrink).
const MIN_DICT_LEN: usize = 5;

/// Marker key for a transposed table (see module docs). A one-key object whose
/// single key is `_tbl` and whose value is `{"c": [..], "r": [..]}`.
const TABLE_KEY: &str = "_tbl";

/// Minimum rows for table transposition to be worthwhile (matches SmartCrusher's
/// `min_items`): below 2 there is no repeated schema to factor out.
const MIN_TABLE_ROWS: usize = 2;

/// Compact a JSON value. Deterministic: same input -> identical output.
pub fn compact_json(value: &Value) -> Value {
    let pruned = drop_nulls(value);

    // Only transpose when the input contains no `_tbl`-shaped object of its
    // own. Otherwise our table markers would be indistinguishable from the
    // user's data on the way back, so we skip transposition for that (rare)
    // input and record `"_t": false` so `expand` doesn't rebuild it. The store
    // still holds the untouched original regardless.
    let do_transpose = !contains_key(&pruned, TABLE_KEY);
    let transposed = if do_transpose {
        transpose(&pruned)
    } else {
        pruned.clone()
    };

    let mut counts: HashMap<String, usize> = HashMap::new();
    count_strings(&transposed, &mut counts);

    // Dictionary: strings that repeat and are long enough to pay for the marker.
    let mut dict: Vec<String> = counts
        .iter()
        .filter(|(s, c)| **c >= 2 && s.len() >= MIN_DICT_LEN)
        .map(|(s, _)| s.clone())
        .collect();
    // Order by frequency (desc) then lexically for stable, deterministic output.
    dict.sort_by(|a, b| counts[b].cmp(&counts[a]).then_with(|| a.cmp(b)));

    let index: HashMap<&str, usize> =
        dict.iter().enumerate().map(|(i, s)| (s.as_str(), i)).collect();
    let transformed = encode(&transposed, &index);

    json!({ "_d": dict, "_v": transformed, "_t": do_transpose })
}

/// Inverse of the encoding + transposition steps in [`compact_json`].
pub fn expand_json(value: &Value) -> Value {
    let dict: Vec<String> = value
        .get("_d")
        .and_then(|d| d.as_array())
        .map(|a| {
            a.iter()
                .map(|v| v.as_str().unwrap_or("").to_string())
                .collect()
        })
        .unwrap_or_default();
    let body = value.get("_v").cloned().unwrap_or(Value::Null);
    let decoded = decode(&body, &dict);
    // `_t` is absent on legacy payloads (pre-transposition); those never
    // contain our markers, so untransposing is a safe no-op either way, but we
    // honor the flag when present.
    let transposed = value.get("_t").and_then(|v| v.as_bool()).unwrap_or(true);
    if transposed {
        untranspose(&decoded)
    } else {
        decoded
    }
}

/// Recursively drop `null` object fields. Exposed so the reversibility audit
/// (`ctxforge verify`) can check decompaction against the same canonical form
/// `compact_json` operates on.
pub fn drop_nulls(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                if v.is_null() {
                    continue;
                }
                out.insert(k.clone(), drop_nulls(v));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(drop_nulls).collect()),
        other => other.clone(),
    }
}

/// True if any object anywhere in `value` has a key equal to `key`.
fn contains_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(map) => {
            map.contains_key(key) || map.values().any(|v| contains_key(v, key))
        }
        Value::Array(arr) => arr.iter().any(|v| contains_key(v, key)),
        _ => false,
    }
}

/// Recursively replace every array of >= 2 objects that all share the same key
/// set with a transposed table `{"_tbl": {"c": [cols], "r": [[vals], ...]}}`.
/// Field names are emitted once in `c`; each row in `r` is the values in column
/// order. Non-homogeneous arrays and all other values recurse element-wise.
///
/// `serde_json::Map` is a `BTreeMap` (no `preserve_order` feature), so
/// `keys()` is sorted and the column order is deterministic — and identical to
/// the key order a rebuilt object will have, which makes [`untranspose`] an
/// exact inverse.
fn transpose(value: &Value) -> Value {
    match value {
        Value::Array(arr) => {
            if let Some(cols) = homogeneous_object_columns(arr) {
                let c: Vec<Value> = cols.iter().map(|k| Value::String(k.clone())).collect();
                let rows: Vec<Value> = arr
                    .iter()
                    .map(|item| {
                        let obj = item.as_object().expect("checked all-objects");
                        let cells: Vec<Value> =
                            cols.iter().map(|k| transpose(&obj[k])).collect();
                        Value::Array(cells)
                    })
                    .collect();
                let mut tbl = Map::new();
                tbl.insert("c".to_string(), Value::Array(c));
                tbl.insert("r".to_string(), Value::Array(rows));
                let mut outer = Map::new();
                outer.insert(TABLE_KEY.to_string(), Value::Object(tbl));
                Value::Object(outer)
            } else {
                Value::Array(arr.iter().map(transpose).collect())
            }
        }
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                out.insert(k.clone(), transpose(v));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// If `arr` is >= [`MIN_TABLE_ROWS`] objects all sharing exactly the same key
/// set, return that key set (sorted, as `Map` iteration yields). Otherwise
/// `None`. Empty key sets are rejected — there is nothing to factor out.
fn homogeneous_object_columns(arr: &[Value]) -> Option<Vec<String>> {
    if arr.len() < MIN_TABLE_ROWS {
        return None;
    }
    let mut cols: Option<Vec<String>> = None;
    for item in arr {
        let map = item.as_object()?;
        let keys: Vec<String> = map.keys().cloned().collect();
        if keys.is_empty() {
            return None;
        }
        match &cols {
            None => cols = Some(keys),
            Some(existing) if *existing != keys => return None,
            _ => {}
        }
    }
    cols
}

/// Exact inverse of [`transpose`]: rebuild each `{"_tbl": {"c", "r"}}` table
/// into its array of objects, recursing into cell values. Non-table objects and
/// arrays recurse element-wise.
fn untranspose(value: &Value) -> Value {
    if let Some(tbl) = as_table(value) {
        return tbl;
    }
    match value {
        Value::Array(arr) => Value::Array(arr.iter().map(untranspose).collect()),
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                out.insert(k.clone(), untranspose(v));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// Recognize a transposed table and rebuild it, or return `None` if `value` is
/// not exactly a one-key `_tbl` object wrapping `{"c": [..], "r": [..]}`.
fn as_table(value: &Value) -> Option<Value> {
    let outer = value.as_object()?;
    if outer.len() != 1 {
        return None;
    }
    let inner = outer.get(TABLE_KEY)?.as_object()?;
    let cols = inner.get("c")?.as_array()?;
    let rows = inner.get("r")?.as_array()?;
    let col_names: Vec<&str> = cols.iter().map(|c| c.as_str()).collect::<Option<_>>()?;
    let mut out: Vec<Value> = Vec::with_capacity(rows.len());
    for row in rows {
        let cells = row.as_array()?;
        if cells.len() != col_names.len() {
            return None;
        }
        let mut obj = Map::new();
        for (name, cell) in col_names.iter().zip(cells) {
            obj.insert((*name).to_string(), untranspose(cell));
        }
        out.push(Value::Object(obj));
    }
    Some(Value::Array(out))
}

fn count_strings(value: &Value, counts: &mut HashMap<String, usize>) {
    match value {
        Value::String(s) => {
            *counts.entry(s.clone()).or_insert(0) += 1;
        }
        Value::Array(arr) => arr.iter().for_each(|v| count_strings(v, counts)),
        Value::Object(map) => map.values().for_each(|v| count_strings(v, counts)),
        _ => {}
    }
}

fn encode(value: &Value, index: &HashMap<&str, usize>) -> Value {
    match value {
        Value::String(s) => match index.get(s.as_str()) {
            Some(i) => json!({ "$": i }),
            None => Value::String(s.clone()),
        },
        Value::Array(arr) => Value::Array(arr.iter().map(|v| encode(v, index)).collect()),
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                out.insert(k.clone(), encode(v, index));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

fn decode(value: &Value, dict: &[String]) -> Value {
    match value {
        Value::Object(map) => {
            // A marker is exactly {"$": <index>}.
            if map.len() == 1 {
                if let Some(idx) = map.get("$").and_then(|v| v.as_u64()) {
                    if let Some(s) = dict.get(idx as usize) {
                        return Value::String(s.clone());
                    }
                }
            }
            let mut out = Map::new();
            for (k, v) in map {
                out.insert(k.clone(), decode(v, dict));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(|v| decode(v, dict)).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_preserves_value() {
        let v = json!({
            "nodes": [
                {"kind": "function", "file": "src/server/handler.rs", "name": "alpha"},
                {"kind": "function", "file": "src/server/handler.rs", "name": "beta"},
                {"kind": "function", "file": "src/server/handler.rs", "name": "gamma"},
            ],
            "edges": [
                {"from": "alpha", "to": "beta", "kind": "calls"},
                {"from": "beta", "to": "gamma", "kind": "calls"},
            ]
        });
        let compact = compact_json(&v);
        let expanded = expand_json(&compact);
        assert_eq!(expanded, v);
    }

    #[test]
    fn compact_is_smaller_for_repetitive_data() {
        // Many nodes sharing long file/kind strings.
        let nodes: Vec<Value> = (0..50)
            .map(|i| {
                json!({
                    "kind": "function",
                    "file": "src/discovery/extract.rs",
                    "language": "rust",
                    "name": format!("fn_{i}")
                })
            })
            .collect();
        let v = json!({ "nodes": nodes });
        let original = serde_json::to_string(&v).unwrap();
        let compact = serde_json::to_string(&compact_json(&v)).unwrap();
        assert!(
            compact.len() < original.len(),
            "compact {} should be < original {}",
            compact.len(),
            original.len()
        );
    }

    #[test]
    fn nulls_are_dropped() {
        let v = json!({"a": 1, "b": null, "c": {"d": null, "e": 2}});
        let compact = compact_json(&v);
        let expanded = expand_json(&compact);
        assert_eq!(expanded, json!({"a": 1, "c": {"e": 2}}));
    }

    #[test]
    fn deterministic_output() {
        let v = json!({"x": ["repeated_string", "repeated_string", "other_value"]});
        let a = serde_json::to_string(&compact_json(&v)).unwrap();
        let b = serde_json::to_string(&compact_json(&v)).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn homogeneous_array_is_transposed_and_smaller() {
        // 20 records sharing the same 4-key schema. The naive form repeats
        // every key 20×; the columnar form emits the schema once.
        let items: Vec<Value> = (0..20)
            .map(|i| {
                json!({
                    "id": i,
                    "status": if i % 2 == 0 { "open" } else { "closed" },
                    "component": "auth-service",
                    "title": format!("issue number {i}"),
                })
            })
            .collect();
        let v = Value::Array(items);
        let original = serde_json::to_string(&v).unwrap();
        let compact_v = compact_json(&v);
        let compact = serde_json::to_string(&compact_v).unwrap();
        // The schema is emitted once.
        assert!(compact.contains("\"_tbl\""), "expected a table marker");
        // Even with all-unique titles (incompressible), dropping the repeated
        // schema alone clears a wide margin.
        assert!(
            (compact.len() as f64) < original.len() as f64 * 0.65,
            "columnar should beat naive JSON by a wide margin: {} vs {}",
            compact.len(),
            original.len()
        );
        // Exact round-trip.
        assert_eq!(expand_json(&compact_v), v);
    }

    #[test]
    fn nested_homogeneous_arrays_transpose() {
        // The graph payload shape: an object holding two homogeneous arrays.
        let v = json!({
            "nodes": [
                {"file": "a.rs", "kind": "function", "name": "alpha"},
                {"file": "a.rs", "kind": "function", "name": "beta"},
                {"file": "b.rs", "kind": "function", "name": "gamma"},
            ],
            "edges": [
                {"from": "alpha", "kind": "calls", "to": "beta"},
                {"from": "beta", "kind": "calls", "to": "gamma"},
            ]
        });
        let compact_v = compact_json(&v);
        assert_eq!(expand_json(&compact_v), v);
    }

    #[test]
    fn heterogeneous_array_not_transposed() {
        // Different key sets per object -> no table, but still reversible.
        let v = json!([
            {"a": 1, "b": 2},
            {"a": 3, "c": 4},
        ]);
        let compact_v = compact_json(&v);
        assert!(!serde_json::to_string(&compact_v).unwrap().contains("_tbl"));
        assert_eq!(expand_json(&compact_v), v);
    }

    #[test]
    fn table_marker_in_input_survives_roundtrip() {
        // Pathological: input already contains an object shaped like our table
        // marker. We must detect this, skip transposition, and round-trip the
        // literal exactly rather than mis-rebuilding it into an array.
        let v = json!({"_tbl": {"c": ["x"], "r": [[1]]}, "other": "value"});
        let compact_v = compact_json(&v);
        assert_eq!(compact_v.get("_t").and_then(|t| t.as_bool()), Some(false));
        assert_eq!(expand_json(&compact_v), v);
    }

    #[test]
    fn marker_collision_inside_homogeneous_array_is_safe() {
        // Even when the colliding marker is nested inside an otherwise
        // transposable array, transposition is skipped wholesale and the value
        // round-trips exactly.
        let v = json!([
            {"id": 1, "payload": {"_tbl": {"c": ["x"], "r": [[1]]}}},
            {"id": 2, "payload": {"_tbl": {"c": ["y"], "r": [[2]]}}},
        ]);
        let compact_v = compact_json(&v);
        assert_eq!(compact_v.get("_t").and_then(|t| t.as_bool()), Some(false));
        assert_eq!(expand_json(&compact_v), v);
    }
}
