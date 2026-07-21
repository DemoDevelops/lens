//! Per-model price table (Anthropic defaults), in US$ per million tokens (per-Mtok).
//!
//! The single source of truth for token pricing on both the dashboard's web
//! frontend and the `--tui`/CLI `--model` path, so the two can't drift. Prices
//! are the current public Anthropic sticker rates (see the `claude-api` skill's
//! model catalog); cache-read is priced at 0.1× the input rate, matching the
//! documented "cache reads cost ~0.1× base input price".
//!
//! Generic for any host: non-Claude ids (grok, gpt, gemini, ...) resolve
//! through the generated models.dev catalog (`pricing_catalog`, covering every
//! model in opencode's provider directory); anything still unknown falls back
//! to the Sonnet rate.
//!
//! Not a live feed — curated/generated const tables. Update the numbers here
//! when Anthropic publishes new pricing; regenerate `pricing_catalog.rs` for
//! the rest.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Serialize;

use super::pricing_catalog::CATALOG;

/// One model's token prices, in US$ per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ModelPrice {
    /// Input (prompt) tokens, $/Mtok.
    pub input: f64,
    /// Output (completion) tokens, $/Mtok.
    pub output: f64,
    /// Cache-read tokens, $/Mtok. Priced at `input * 0.1` (cache reads cost
    /// ~0.1× the base input rate).
    pub cache_read: f64,
}

// Canonical model keys. Every raw id / display-name variant normalizes to one of
// these, so a price lookup and a mix-share join agree on the same key.
const OPUS: &str = "claude-opus-4-8";
const SONNET: &str = "claude-sonnet-5";
const HAIKU: &str = "claude-haiku-4-5";
const FABLE: &str = "claude-fable-5";
/// Fallback key for an unrecognized model. Priced at the Sonnet rate (see
/// [`price_for`]) — a middle-tier default that never panics and never over- or
/// under-prices as badly as picking an extreme.
pub const UNKNOWN: &str = "unknown";

const OPUS_PRICE: ModelPrice = ModelPrice { input: 5.0, output: 25.0, cache_read: 0.5 };
// Sonnet 5 sticker price. (An introductory $2/$10 per-Mtok rate runs through
// 2026-08-31; this table uses the durable standard rate so it doesn't silently
// go stale when the promo ends.)
const SONNET_PRICE: ModelPrice = ModelPrice { input: 3.0, output: 15.0, cache_read: 0.3 };
const HAIKU_PRICE: ModelPrice = ModelPrice { input: 1.0, output: 5.0, cache_read: 0.1 };
const FABLE_PRICE: ModelPrice = ModelPrice { input: 10.0, output: 50.0, cache_read: 1.0 };

/// The canonical model keys, in display order (priciest first).
pub const MODELS: &[&str] = &[FABLE, OPUS, SONNET, HAIKU];

/// Map any raw model id or display name to one canonical key.
///
/// Liberal substring match on a lowercased copy, so every variant lands right:
/// `claude-opus-4-8`, `claude-opus-4-8[1m]`, and `Opus 4.8` all → [`OPUS`]; the
/// same for sonnet / haiku / fable. `claude-mythos-*` shares Fable's pricing, so
/// it folds into [`FABLE`]. Anything else (grok*, opencode, raw ids) → [`UNKNOWN`]
/// (falls back to Sonnet pricing; no claude- assumption).
pub fn normalize_model(raw: &str) -> &'static str {
    let s = raw.to_ascii_lowercase();
    if s.contains("opus") {
        OPUS
    } else if s.contains("sonnet") {
        SONNET
    } else if s.contains("haiku") {
        HAIKU
    } else if s.contains("fable") || s.contains("mythos") {
        FABLE
    } else {
        UNKNOWN
    }
}

/// The price for a model, by raw id or display name; never panics. Claude-family
/// ids use this module's curated table (it wins over the generated catalog's
/// promo rates); other ids resolve through the models.dev catalog; anything
/// still unknown falls back to the Sonnet rate.
pub fn price_for(model: &str) -> ModelPrice {
    match normalize_model(model) {
        OPUS => OPUS_PRICE,
        SONNET => SONNET_PRICE,
        HAIKU => HAIKU_PRICE,
        FABLE => FABLE_PRICE,
        _ => catalog_price(model).unwrap_or(SONNET_PRICE),
    }
}

/// Look a raw model id up in the generated models.dev catalog (lowercased
/// exact match, then the segment after the last `/` for `provider/model` ids).
/// `None` when the catalog doesn't price it.
pub fn catalog_price(model: &str) -> Option<ModelPrice> {
    static INDEX: OnceLock<HashMap<&'static str, ModelPrice>> = OnceLock::new();
    let index = INDEX.get_or_init(|| {
        CATALOG
            .iter()
            .map(|&(id, input, output, cache_read)| {
                (id, ModelPrice { input, output, cache_read })
            })
            .collect()
    });
    let s = model.trim().to_ascii_lowercase();
    index
        .get(s.as_str())
        .or_else(|| s.rsplit('/').next().and_then(|tail| index.get(tail)))
        .copied()
}

/// One row of the serialized price table: the canonical key plus its prices.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct PriceEntry {
    pub model: &'static str,
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
}

/// The full price table, one entry per canonical model in [`MODELS`] order, for
/// embedding in the dashboard `/api/stats` payload (T3).
pub fn price_table() -> Vec<PriceEntry> {
    MODELS
        .iter()
        .map(|&m| {
            let p = price_for(m);
            PriceEntry { model: m, input: p.input, output: p.output, cache_read: p.cache_read }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A raw id and its display name normalize to the same nonzero price.
    #[test]
    fn id_and_display_name_agree() {
        let by_id = price_for("claude-opus-4-8");
        let by_name = price_for("Opus 4.8");
        assert_eq!(by_id, by_name);
        assert!(by_id.input > 0.0 && by_id.output > 0.0);
        // The `[1m]` context-window suffix also lands on Opus.
        assert_eq!(price_for("claude-opus-4-8[1m]"), by_id);
    }

    /// Each tier is priced distinctly and matches the `claude-api` reference.
    #[test]
    fn tiers_are_distinct_and_correct() {
        let opus = price_for("claude-opus-4-8");
        let sonnet = price_for("claude-sonnet-5");
        let haiku = price_for("claude-haiku-4-5");
        let fable = price_for("claude-fable-5");

        // Distinct from one another.
        assert_ne!(opus, sonnet);
        assert_ne!(opus, haiku);
        assert_ne!(opus, fable);
        assert_ne!(sonnet, haiku);
        assert_ne!(sonnet, fable);
        assert_ne!(haiku, fable);

        // Exact reference values ($/Mtok), cache_read = input * 0.1.
        assert_eq!(opus, ModelPrice { input: 5.0, output: 25.0, cache_read: 0.5 });
        assert_eq!(sonnet, ModelPrice { input: 3.0, output: 15.0, cache_read: 0.3 });
        assert_eq!(haiku, ModelPrice { input: 1.0, output: 5.0, cache_read: 0.1 });
        assert_eq!(fable, ModelPrice { input: 10.0, output: 50.0, cache_read: 1.0 });

        // Haiku's date-suffixed id still resolves.
        assert_eq!(price_for("claude-haiku-4-5-20251001"), haiku);
    }

    /// A model absent from both the Claude table and the catalog returns the
    /// Sonnet-rate fallback without panicking.
    #[test]
    fn unknown_model_falls_back_to_sonnet() {
        assert_eq!(normalize_model("totally-made-up-model"), UNKNOWN);
        assert_eq!(price_for("totally-made-up-model"), SONNET_PRICE);
        assert_eq!(price_for(""), SONNET_PRICE);
    }

    /// Non-Claude ids resolve through the generated models.dev catalog:
    /// exact id, provider-prefixed id, and case-insensitivity all land.
    #[test]
    fn catalog_prices_opencode_directory_models() {
        let gpt5 = price_for("gpt-5");
        assert_eq!(gpt5, ModelPrice { input: 1.25, output: 10.0, cache_read: 0.125 });
        assert_eq!(price_for("openai/gpt-5"), gpt5);
        assert_eq!(price_for("GPT-5"), gpt5);
        let grok = price_for("grok-4.5");
        assert_eq!(grok, ModelPrice { input: 2.0, output: 6.0, cache_read: 0.5 });
        assert!(catalog_price("grok-build-0.1").is_some());
        assert!(catalog_price("totally-made-up-model").is_none());
    }

    /// The curated Claude table wins over the catalog (which may carry promo
    /// rates for the same ids).
    #[test]
    fn claude_table_wins_over_catalog() {
        assert_eq!(price_for("claude-sonnet-5"), SONNET_PRICE);
        assert_eq!(price_for("anthropic/claude-sonnet-5"), SONNET_PRICE);
    }

    /// The serialized table has one entry per canonical model.
    #[test]
    fn price_table_covers_every_model() {
        let table = price_table();
        assert_eq!(table.len(), MODELS.len());
        for (entry, &model) in table.iter().zip(MODELS) {
            assert_eq!(entry.model, model);
            let p = price_for(model);
            assert_eq!(entry.input, p.input);
            assert_eq!(entry.cache_read, p.cache_read);
        }
    }
}
