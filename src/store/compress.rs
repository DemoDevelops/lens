//! Deterministic, reversible structural JSON compaction.
//!
//! Three transforms, applied in order:
//!   1. drop `null` object fields (they carry no information);
//!   2. **columnar table transposition** -- any array of >= 2 objects that all
//!      share the same key set is replaced with a single schema (the field
//!      names, once) plus value-only rows, instead of repeating every field
//!      name on every object. This is the deterministic core of headroom's
//!      SmartCrusher CSV-schema compaction (`crates/headroom-core/.../
//!      compaction/{compactor,formatter}.rs`): emit the schema once, then rows
//!      as positional tuples. Homogeneous record arrays (issue lists, graph
//!      node/edge sets) are exactly its best case; and
//!   3. dictionary-encode repeated string *values* into a shared table,
//!      replacing each occurrence with a compact `$N` token (a JSON string).
//!      Strings that themselves start with `$` are escaped by doubling the
//!      leading `$`, so decoding is unambiguous: `$\d+` is a dict ref,
//!      `$$...` is a literal that started with `$`, anything else is literal.
//!   4. run-length encode integer arrays that have at least one run of 2+
//!      identical consecutive values, replacing them with
//!      `{"_rle": [[value, count], ...]}`.
//!
//! The output is `{"_d": [<dict>], "_v": <transformed>}`. A transposed table is
//! a one-key object `{"_tbl": {"c": [<cols>], "r": [[<row values>], ...]}}`.
//! `expand` is the exact inverse of steps 2, 3, and 4, so
//! `expand(compact(v)) == drop_nulls(v)`. The result is always paired with a
//! store ref to the untouched original, so nothing is ever lost.
//!
//! What is deliberately NOT ported from SmartCrusher: its lossy row-sampling /
//! anomaly-preservation path (keep ~50 of 1000 rows). That is lossy and not
//! reversible; lens keeps every row and recovers the original via the
//! store. We port only the deterministic, lossless structural mechanism.

use std::collections::HashMap;

use serde_json::{json, Map, Value};

/// Minimum string length worth dictionary-encoding. The `$N` ref is 2+ bytes,
/// so strings shorter than this would not shrink even when repeated.
const MIN_DICT_LEN: usize = 5;

/// Marker key for run-length encoded integer arrays. A one-key object whose
/// single key is `_rle` and whose value is `[[value, count], ...]`.
const RLE_KEY: &str = "_rle";

/// Marker key for a transposed table (see module docs). A one-key object whose
/// single key is `_tbl` and whose value is `{"c": [..], "r": [..]}`.
const TABLE_KEY: &str = "_tbl";

/// Marker key for the TOON-style tabular form. A one-key object whose single key
/// is `__toon__` and whose value is `{"keys": [..], "rows": [[..], ...]}`.
/// This is a deliberate narrow subset of the TOON (Token-Oriented Object
/// Notation) format, not the full spec: only a uniform array of flat objects
/// with all-scalar values, no nesting and no YAML-style indentation.
const TOON_KEY: &str = "__toon__";

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
    let do_transpose = !contains_key(&pruned, TABLE_KEY) && !contains_key(&pruned, TOON_KEY);
    let transposed = if do_transpose {
        transpose(&pruned)
    } else {
        pruned.clone()
    };

    // Count string occurrences using &str keys to avoid cloning every string.
    let mut counts: HashMap<&str, usize> = HashMap::new();
    count_strings(&transposed, &mut counts);

    // Dictionary: strings that repeat and are long enough to pay for the marker.
    let mut dict: Vec<&str> = counts
        .iter()
        .filter(|(s, c)| **c >= 2 && s.len() >= MIN_DICT_LEN)
        .map(|(s, _)| *s)
        .collect();
    // Order by frequency (desc) then lexically for stable, deterministic output.
    dict.sort_by(|a, b| counts[b].cmp(&counts[a]).then_with(|| a.cmp(b)));

    let index: HashMap<&str, usize> = dict
        .iter()
        .enumerate()
        .map(|(i, s)| (*s, i))
        .collect();
    let transformed = encode(&transposed, &index);

    let dict_owned: Vec<String> = dict.iter().map(|s| (*s).to_string()).collect();

    json!({ "_d": dict_owned, "_v": transformed, "_t": do_transpose })
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
/// (`lens verify`) can check decompaction against the same canonical form
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
        Value::Object(map) => map.contains_key(key) || map.values().any(|v| contains_key(v, key)),
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
            // Prefer the TOON form for the strict uniform-flat-scalar shape; fall
            // back to the columnar `_tbl` form for homogeneous objects that still
            // carry nested values.
            if let Some(cols) = flat_scalar_columns(arr) {
                encode_toon(arr, &cols)
            } else if let Some(cols) = homogeneous_object_columns(arr) {
                let c: Vec<Value> = cols.iter().map(|k| Value::String(k.clone())).collect();
                let rows: Vec<Value> = arr
                    .iter()
                    .map(|item| {
                        let obj = item.as_object().expect("checked all-objects");
                        let cells: Vec<Value> = cols.iter().map(|k| transpose(&obj[k])).collect();
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

/// If `arr` is >= [`MIN_TABLE_ROWS`] objects that all share exactly the same
/// non-empty key set AND whose every value is a scalar (string, number, bool,
/// or null), return that key set (sorted, as `Map` iteration yields). Otherwise
/// `None`. This is the strict shape the TOON form encodes; any nested object or
/// array in any cell rejects it back to the `_tbl` / element-wise paths.
///
/// Fused single pass: checks key uniformity and scalar-only constraint together,
/// avoiding a second iteration over `arr` that the two-call alternative would need.
fn flat_scalar_columns(arr: &[Value]) -> Option<Vec<String>> {
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
        if !map.values().all(is_scalar) {
            return None;
        }
    }
    cols
}

/// A JSON scalar: anything that is not an object or array.
fn is_scalar(value: &Value) -> bool {
    !matches!(value, Value::Object(_) | Value::Array(_))
}

/// Encode a uniform array of flat-scalar objects as the TOON form: the keys once
/// in `keys`, then one row of scalar values per element in `rows`. Lossless and
/// inverted by [`as_toon`].
fn encode_toon(arr: &[Value], cols: &[String]) -> Value {
    let keys: Vec<Value> = cols.iter().map(|k| Value::String(k.clone())).collect();
    let rows: Vec<Value> = arr
        .iter()
        .map(|item| {
            let obj = item.as_object().expect("checked all-objects");
            let cells: Vec<Value> = cols.iter().map(|k| obj[k].clone()).collect();
            Value::Array(cells)
        })
        .collect();
    let mut inner = Map::new();
    inner.insert("keys".to_string(), Value::Array(keys));
    inner.insert("rows".to_string(), Value::Array(rows));
    let mut outer = Map::new();
    outer.insert(TOON_KEY.to_string(), Value::Object(inner));
    Value::Object(outer)
}

/// Exact inverse of [`transpose`]: rebuild each `{"_tbl": {"c", "r"}}` table
/// into its array of objects, recursing into cell values. Non-table objects and
/// arrays recurse element-wise.
fn untranspose(value: &Value) -> Value {
    if let Some(toon) = as_toon(value) {
        return toon;
    }
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

/// Recognize a TOON-form table and rebuild it, or `None` if `value` is not
/// exactly a one-key `__toon__` object wrapping `{"keys": [..], "rows": [..]}`.
/// Exact inverse of [`encode_toon`].
fn as_toon(value: &Value) -> Option<Value> {
    let outer = value.as_object()?;
    if outer.len() != 1 {
        return None;
    }
    let inner = outer.get(TOON_KEY)?.as_object()?;
    let keys = inner.get("keys")?.as_array()?;
    let rows = inner.get("rows")?.as_array()?;
    let key_names: Vec<&str> = keys.iter().map(|k| k.as_str()).collect::<Option<_>>()?;
    let mut out: Vec<Value> = Vec::with_capacity(rows.len());
    for row in rows {
        let cells = row.as_array()?;
        if cells.len() != key_names.len() {
            return None;
        }
        let mut obj = Map::new();
        for (name, cell) in key_names.iter().zip(cells) {
            obj.insert((*name).to_string(), cell.clone());
        }
        out.push(Value::Object(obj));
    }
    Some(Value::Array(out))
}

fn count_strings<'a>(value: &'a Value, counts: &mut HashMap<&'a str, usize>) {
    match value {
        Value::String(s) => {
            *counts.entry(s.as_str()).or_insert(0) += 1;
        }
        Value::Array(arr) => arr.iter().for_each(|v| count_strings(v, counts)),
        Value::Object(map) => map.values().for_each(|v| count_strings(v, counts)),
        _ => {}
    }
}

fn encode(value: &Value, index: &HashMap<&str, usize>) -> Value {
    match value {
        Value::String(s) => match index.get(s.as_str()) {
            // Dict ref: emit "$N" string. Unambiguous because the dict only
            // contains strings of length >= MIN_DICT_LEN, so they cannot
            // themselves be mistaken for refs on decode.
            Some(i) => Value::String(format!("${i}")),
            // Literal that starts with "$": escape by doubling the leading "$"
            // so the decoder can distinguish it from a dict ref.
            None if s.starts_with('$') => Value::String(format!("${s}")),
            // Ordinary literal: emit as-is.
            None => Value::String(s.clone()),
        },
        Value::Array(arr) => rle_encode_array(arr, index),
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

/// Encode an array: if all elements are integers and there is at least one run
/// of 2+ identical consecutive values, emit `{"_rle": [[v, c], ...]}`.
/// Otherwise recurse element-wise as a plain array.
fn rle_encode_array(arr: &[Value], index: &HashMap<&str, usize>) -> Value {
    if arr.len() >= 2 {
        let ints: Option<Vec<i64>> = arr.iter().map(|v| v.as_i64()).collect();
        if let Some(ints) = ints {
            let pairs = rle_pairs(&ints);
            // Only emit RLE when there is at least one run (saves space).
            let has_run = pairs.iter().any(|(_, c)| *c >= 2);
            if has_run {
                let rle_arr: Vec<Value> = pairs
                    .into_iter()
                    .map(|(v, c)| Value::Array(vec![Value::Number(v.into()), Value::Number(c.into())]))
                    .collect();
                let mut out = Map::new();
                out.insert(RLE_KEY.to_string(), Value::Array(rle_arr));
                return Value::Object(out);
            }
        }
    }
    Value::Array(arr.iter().map(|v| encode(v, index)).collect())
}

/// Run-length encode a slice of integers into `(value, count)` pairs.
fn rle_pairs(ints: &[i64]) -> Vec<(i64, u64)> {
    let mut out: Vec<(i64, u64)> = Vec::new();
    for &x in ints {
        match out.last_mut() {
            Some((v, c)) if *v == x => *c += 1,
            _ => out.push((x, 1)),
        }
    }
    out
}

fn decode(value: &Value, dict: &[String]) -> Value {
    match value {
        Value::String(s) => decode_str(s, dict),
        Value::Object(map) => {
            // RLE marker: exactly {"_rle": [[v, c], ...]}.
            if map.len() == 1 {
                if let Some(pairs) = map.get(RLE_KEY).and_then(|v| v.as_array()) {
                    if let Some(expanded) = rle_decode(pairs) {
                        return Value::Array(expanded);
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

/// Decode a string token:
/// - `$\d+`  -> dict lookup by index
/// - `$$...` -> literal string with the leading `$` stripped (unescape)
/// - anything else -> literal as-is
fn decode_str(s: &str, dict: &[String]) -> Value {
    if let Some(rest) = s.strip_prefix('$') {
        if rest.starts_with('$') {
            // Escaped literal: "$$..." -> "$..."
            return Value::String(rest.to_string());
        }
        // Try dict ref: "$N" where N is all digits.
        if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(idx) = rest.parse::<usize>() {
                if let Some(entry) = dict.get(idx) {
                    return Value::String(entry.clone());
                }
            }
        }
    }
    Value::String(s.to_string())
}

/// Expand `[[value, count], ...]` pairs back to a flat integer array.
/// Returns `None` if any pair is malformed (wrong shape or non-integer).
fn rle_decode(pairs: &[Value]) -> Option<Vec<Value>> {
    let mut out: Vec<Value> = Vec::new();
    for pair in pairs {
        let cells = pair.as_array()?;
        if cells.len() != 2 {
            return None;
        }
        let v = cells[0].as_i64()?;
        let c = cells[1].as_u64()?;
        for _ in 0..c {
            out.push(Value::Number(v.into()));
        }
    }
    Some(out)
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
        // The schema is emitted once. All-scalar rows take the TOON path.
        assert!(compact.contains("__toon__"), "expected a toon marker");
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

    #[test]
    fn toon_roundtrip_preserves_value() {
        // A uniform array of >= 20 flat objects with mixed scalar types. No
        // null fields: `compact_json` prunes nulls first, so an all-null field
        // would not survive to compare equal to the untouched original.
        let items: Vec<Value> = (0..25)
            .map(|i| {
                json!({
                    "id": i,
                    "name": format!("name_{i}"),
                    "active": i % 2 == 0,
                    "score": (i as f64) * 1.5,
                })
            })
            .collect();
        let v = Value::Array(items);
        let compact_v = compact_json(&v);
        assert!(
            serde_json::to_string(&compact_v)
                .unwrap()
                .contains("__toon__"),
            "expected a toon marker"
        );
        assert_eq!(expand_json(&compact_v), v);
    }

    #[test]
    fn toon_is_at_least_30_percent_smaller() {
        let items: Vec<Value> = (0..50)
            .map(|i| {
                json!({
                    "identifier": i,
                    "display_name": format!("item number {i}"),
                    "is_active": i % 3 == 0,
                })
            })
            .collect();
        let v = Value::Array(items);
        let original = serde_json::to_vec(&v).unwrap();
        let compact = serde_json::to_vec(&compact_json(&v)).unwrap();
        assert!(
            (compact.len() as f64) <= original.len() as f64 * 0.70,
            "toon should be >= 30% smaller: {} vs {}",
            compact.len(),
            original.len()
        );
    }

    #[test]
    fn toon_falls_through_for_nonuniform_and_nested() {
        // Different key sets per object: not the uniform shape, returned as-is.
        let nonuniform = json!([
            {"a": 1, "b": 2},
            {"a": 3, "c": 4},
        ]);
        let compact_nonuniform = compact_json(&nonuniform);
        assert!(!serde_json::to_string(&compact_nonuniform)
            .unwrap()
            .contains("__toon__"));
        assert_eq!(expand_json(&compact_nonuniform), nonuniform);

        // Same keys but a nested object value: not flat-scalar, no toon.
        let nested = json!([
            {"id": 1, "meta": {"k": "v"}},
            {"id": 2, "meta": {"k": "w"}},
        ]);
        let compact_nested = compact_json(&nested);
        assert!(!serde_json::to_string(&compact_nested)
            .unwrap()
            .contains("__toon__"));
        assert_eq!(expand_json(&compact_nested), nested);
    }

    // --- $N reversibility tests (adversarial dollar-string inputs) ---

    #[test]
    fn dollar_ref_roundtrip_adversarial_strings() {
        // Build a value where some strings are in the dict (repeated, long enough)
        // AND the input also contains literal strings starting with "$".
        let v = json!({
            "nodes": [
                {"kind": "function", "file": "src/server/handler.rs", "name": "$5"},
                {"kind": "function", "file": "src/server/handler.rs", "name": "$"},
                {"kind": "function", "file": "src/server/handler.rs", "name": "$$"},
                {"kind": "function", "file": "src/server/handler.rs", "name": "$abc"},
                {"kind": "function", "file": "src/server/handler.rs", "name": "normal"},
            ]
        });
        // "src/server/handler.rs" and "function" repeat 5x: they go into the dict.
        // The dollar-prefixed names are singletons: they get the $$ escape.
        let compact = compact_json(&v);
        let expanded = expand_json(&compact);
        assert_eq!(expanded, v, "adversarial dollar strings must round-trip exactly");
    }

    #[test]
    fn dollar_string_exactly_like_ref_roundtrips() {
        // A string value that looks exactly like a dict ref pattern: "$0", "$1", "$99".
        // These must be escaped and recovered without touching the dict.
        let v = json!(["$0", "$1", "$99", "$100", "$", "$$", "$abc"]);
        let compact = compact_json(&v);
        let expanded = expand_json(&compact);
        assert_eq!(expanded, v);
    }

    #[test]
    fn dict_substitution_fires_and_roundtrips() {
        // Force dict substitution: same long string repeated many times.
        let repeated = "repeated_value_string";
        let v: Value = Value::Array((0..10).map(|i| json!({"key": repeated, "n": i})).collect());
        let compact = compact_json(&v);
        // Verify substitution actually occurred (output contains $0 or similar ref).
        let compact_str = serde_json::to_string(&compact).unwrap();
        assert!(
            compact_str.contains("$0"),
            "expected a dict ref in compact output: {compact_str}"
        );
        let expanded = expand_json(&compact);
        assert_eq!(expanded, v, "dict-substituted value must round-trip exactly");
    }

    #[test]
    fn deeply_nested_dollar_strings_roundtrip() {
        let v = json!({
            "a": {
                "b": {
                    "c": ["$42", "$$escaped", "$xyz", "plain"]
                }
            }
        });
        let compact = compact_json(&v);
        let expanded = expand_json(&compact);
        assert_eq!(expanded, v);
    }

    // --- RLE reversibility tests ---

    #[test]
    fn rle_roundtrip_long_run() {
        // Integer array with long runs: classic RLE win case.
        let v = json!([0, 0, 0, 0, 0, 1, 1, 1, 2, 2, 2, 2]);
        let compact = compact_json(&v);
        let compact_str = serde_json::to_string(&compact).unwrap();
        // Should have used RLE.
        assert!(
            compact_str.contains("_rle"),
            "expected _rle marker in compact output: {compact_str}"
        );
        let expanded = expand_json(&compact);
        assert_eq!(expanded, v, "RLE long-run must round-trip");
    }

    #[test]
    fn rle_roundtrip_no_runs_stays_plain() {
        // All unique integers: RLE would expand, so plain array expected.
        let v = json!([0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let compact = compact_json(&v);
        let compact_str = serde_json::to_string(&compact).unwrap();
        assert!(
            !compact_str.contains("_rle"),
            "unique ints must NOT be RLE-encoded: {compact_str}"
        );
        let expanded = expand_json(&compact);
        assert_eq!(expanded, v);
    }

    #[test]
    fn rle_roundtrip_single_element() {
        let v = json!([42]);
        let compact = compact_json(&v);
        let expanded = expand_json(&compact);
        assert_eq!(expanded, v);
    }

    #[test]
    fn rle_roundtrip_empty_array() {
        let v = json!([]);
        let compact = compact_json(&v);
        let expanded = expand_json(&compact);
        assert_eq!(expanded, v);
    }

    #[test]
    fn rle_roundtrip_mixed_runs() {
        // Some runs, some singletons.
        let v = json!([0, 0, 0, 1, 2, 3, 3, 4]);
        let compact = compact_json(&v);
        let compact_str = serde_json::to_string(&compact).unwrap();
        assert!(
            compact_str.contains("_rle"),
            "mixed runs should trigger RLE: {compact_str}"
        );
        let expanded = expand_json(&compact);
        assert_eq!(expanded, v, "RLE mixed-run must round-trip");
    }

    #[test]
    fn rle_key_in_input_is_preserved() {
        // If the user's own data has a key "_rle", it must survive losslessly.
        // (It is not a one-key object so the decoder does not mistake it for RLE.)
        let v = json!({"_rle": [[0, 3], [1, 2]], "other": "value"});
        let compact = compact_json(&v);
        let expanded = expand_json(&compact);
        assert_eq!(expanded, v);
    }
}
