//! Routing-scheme validation against OBP-API's registry.
//!
//! The node caches the ACTIVE rows of OBP-API's routing-scheme registry
//! (`GET /obp/v7.0.0/routing-schemes`) and validates the beneficiary routing
//! of every A1.1 request against it *before* anything is persisted — a caller
//! using an unknown scheme or a malformed address is told immediately, not
//! after the dispatcher has already accepted the payment.
//!
//! Fail-open by design: until the registry has loaded once (OBP-API down,
//! mock-mode dev without OBP-API), validation is skipped with a warning — a
//! registry outage must not block payments. Refresh keeps running in the
//! background, so validation switches on as soon as OBP-API is reachable.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tracing::{info, warn};

use crate::obp_client::{ObpClient, ObpRoutingScheme};

/// One cached registry row, ready for validation.
struct SchemeRule {
    /// Compiled `address_pattern`; `None` when the registry row has no pattern
    /// (or a pattern this regex engine cannot compile — logged, not fatal).
    pattern: Option<regex::Regex>,
    example_address: String,
}

/// Shared, refreshable view of the registry. `None` = never loaded.
#[derive(Clone, Default)]
pub struct RoutingRegistry {
    inner: Arc<RwLock<Option<HashMap<String, SchemeRule>>>>,
}

/// What the validation rejected — maps onto the south-side error codes
/// `OBP-BANK-NODE-ROUTING-002` (scheme) / `-003` (address).
#[derive(Debug, PartialEq, Eq)]
pub enum RoutingViolation {
    UnknownScheme {
        field: &'static str,
        scheme: String,
    },
    AddressMismatch {
        field: &'static str,
        scheme: String,
        example_address: String,
    },
}

impl RoutingRegistry {
    /// Replace the cached registry with freshly fetched rows.
    pub fn load(&self, schemes: Vec<ObpRoutingScheme>) {
        let mut map = HashMap::new();
        for s in schemes {
            let pattern = if s.address_pattern.is_empty() {
                None
            } else {
                match regex::Regex::new(&s.address_pattern) {
                    Ok(re) => Some(re),
                    Err(e) => {
                        warn!(scheme = %s.scheme, pattern = %s.address_pattern, error = %e,
                            "routing scheme address_pattern does not compile — scheme accepted without address validation");
                        None
                    }
                }
            };
            map.insert(
                s.scheme,
                SchemeRule {
                    pattern,
                    example_address: s.example_address,
                },
            );
        }
        info!(schemes = map.len(), "routing-scheme registry loaded");
        *self.inner.write().unwrap() = Some(map);
    }

    pub fn is_loaded(&self) -> bool {
        self.inner.read().unwrap().is_some()
    }

    /// Validate one scheme/address pair. `Ok` when the registry has never
    /// loaded (fail-open), when the scheme is known and has no pattern, or
    /// when the address matches the scheme's pattern.
    pub fn check(
        &self,
        field: &'static str,
        scheme: &str,
        address: &str,
    ) -> Result<(), RoutingViolation> {
        let guard = self.inner.read().unwrap();
        let Some(map) = guard.as_ref() else {
            return Ok(());
        };
        let Some(rule) = map.get(scheme) else {
            return Err(RoutingViolation::UnknownScheme {
                field,
                scheme: scheme.to_string(),
            });
        };
        if let Some(re) = &rule.pattern {
            if !re.is_match(address) {
                return Err(RoutingViolation::AddressMismatch {
                    field,
                    scheme: scheme.to_string(),
                    example_address: rule.example_address.clone(),
                });
            }
        }
        Ok(())
    }
}

/// Background refresher: fetch the registry now, then every `refresh_secs`.
/// Failures only log — the registry keeps its last good state (or stays
/// unloaded, leaving validation off).
pub fn spawn_refresher(registry: RoutingRegistry, obp: Arc<ObpClient>, refresh_secs: u64) {
    tokio::spawn(async move {
        let interval = std::time::Duration::from_secs(refresh_secs.max(10));
        loop {
            match obp.get_routing_schemes().await {
                Ok(schemes) => registry.load(schemes),
                Err(e) => {
                    warn!(error = %e, loaded = registry.is_loaded(),
                        "routing-scheme registry refresh failed; validation runs on the last loaded state (or is off if never loaded)");
                }
            }
            tokio::time::sleep(interval).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scheme(name: &str, pattern: &str, example: &str) -> ObpRoutingScheme {
        ObpRoutingScheme {
            scheme: name.into(),
            status: "ACTIVE".into(),
            address_pattern: pattern.into(),
            example_address: example.into(),
        }
    }

    #[test]
    fn unloaded_registry_accepts_everything() {
        let reg = RoutingRegistry::default();
        assert!(!reg.is_loaded());
        assert!(reg
            .check("to.other_bank_routing_scheme", "ANYTHING", "x")
            .is_ok());
    }

    #[test]
    fn unknown_scheme_is_rejected_once_loaded() {
        let reg = RoutingRegistry::default();
        reg.load(vec![scheme("OBP", "^[a-z.]+$", "rt.bank.b")]);
        let err = reg.check("f", "SWIFT", "x").unwrap_err();
        assert_eq!(
            err,
            RoutingViolation::UnknownScheme {
                field: "f",
                scheme: "SWIFT".into()
            }
        );
    }

    #[test]
    fn address_must_match_the_scheme_pattern() {
        let reg = RoutingRegistry::default();
        reg.load(vec![scheme("TZ.MSISDN", "^255[0-9]{9}$", "255778300336")]);
        assert!(reg.check("f", "TZ.MSISDN", "255778300336").is_ok());
        let err = reg.check("f", "TZ.MSISDN", "0778300336").unwrap_err();
        assert_eq!(
            err,
            RoutingViolation::AddressMismatch {
                field: "f",
                scheme: "TZ.MSISDN".into(),
                example_address: "255778300336".into(),
            }
        );
    }

    #[test]
    fn empty_or_broken_pattern_accepts_any_address() {
        let reg = RoutingRegistry::default();
        reg.load(vec![
            scheme("NOPATTERN", "", "x"),
            scheme("BROKEN", "([", "x"),
        ]);
        assert!(reg.check("f", "NOPATTERN", "whatever").is_ok());
        assert!(reg.check("f", "BROKEN", "whatever").is_ok());
    }

    #[test]
    fn reload_replaces_the_previous_registry() {
        let reg = RoutingRegistry::default();
        reg.load(vec![scheme("OLD", "", "x")]);
        reg.load(vec![scheme("NEW", "", "x")]);
        assert!(reg.check("f", "NEW", "x").is_ok());
        assert!(matches!(
            reg.check("f", "OLD", "x"),
            Err(RoutingViolation::UnknownScheme { .. })
        ));
    }
}
