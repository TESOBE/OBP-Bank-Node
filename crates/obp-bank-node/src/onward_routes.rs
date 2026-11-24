// Copyright (C) 2025-2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later
// Licensed under the GNU Affero General Public License v3 or later: see the LICENSE and NOTICE files.

//! A hub's onward routes: on each OBP host the node is on, it states which
//! banks on its other OBP hosts a payment can reach through it
//! (DOCS/MULTI_HOST_ROUTING.md, section 5).
//!
//! The node confirms its routes on a timer. OBP-API drops a route that is not
//! confirmed again within the period it sets, so a hub whose node stops
//! confirming drops out of routing by itself. A node on one OBP host has
//! nothing to state and does not run this.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::Duration;

use serde_json::{json, Value};
use tracing::{info, warn};

use crate::interface_c::router::{ForwardTarget, ForwardTargets};

/// The bank attribute that marks an Open Corridor participant, as the setup page sets it.
const PARTICIPANT_ATTRIBUTE: &str = "OPEN_CORRIDOR_PARTICIPANT";

/// The bank attribute that gives a bank's ISO 3166 country, when its operator has set one.
const COUNTRY_ATTRIBUTE: &str = "COUNTRY";

/// What the node knows about one of its other OBP hosts, to build routes to the banks there.
pub struct OtherHost<'a> {
    /// The other OBP host's name, for example `localhost:8081`.
    pub obp_host: &'a str,
    /// The node's own bank there, which is never a route.
    pub own_bank_id: &'a str,
    /// The currencies the node has a corridor account for there (upper case).
    pub currencies: &'a HashSet<String>,
    /// The country of the banks there, for a bank that has no COUNTRY attribute.
    pub default_country: Option<&'a str>,
    /// That OBP host's getBanks answer, as OBP-API sent it.
    pub banks: &'a Value,
}

/// The routes the node states on one OBP host, from what it knows about its other OBP hosts.
///
/// A route is each Open Corridor participant on another OBP host, other than the
/// node's own bank there, that the node can pay in a currency it also receives
/// on this OBP host. The node does not convert currencies yet, so a route lists
/// the currencies the node has a corridor account for on both OBP hosts, and no
/// rates. A bank with no country is left out, because a sending bank must be
/// able to rule countries out. A bank reached through two other OBP hosts is
/// listed once, through the first.
pub fn build_routes(this_host_currencies: &HashSet<String>, others: &[OtherHost<'_>]) -> Vec<Value> {
    // The routes, in the order they are found.
    let mut routes = Vec::new();
    // The routing pairs already listed, so no bank is listed twice.
    let mut listed_pairs: HashSet<(String, String)> = HashSet::new();
    // Each of the node's other OBP hosts in turn.
    for other in others {
        // The currencies the node can take here and pay out there, in a stable order.
        let currencies: BTreeSet<String> = this_host_currencies.intersection(other.currencies).cloned().collect();
        // With no currency in common, nothing can be carried to that OBP host.
        if currencies.is_empty() {
            // Say why that OBP host gives no routes.
            warn!(obp_host = other.obp_host, "no currency with a corridor account on both OBP hosts: no onward routes to it");
            // Go on to the next OBP host.
            continue;
        }
        // Each bank that OBP host lists.
        for bank in other.banks["banks"].as_array().cloned().unwrap_or_default() {
            // The bank's id on that OBP host.
            let bank_id = bank["bank_id"].as_str().unwrap_or_default();
            // The node's own bank there is where the payment goes on from, not a route.
            if bank_id == other.own_bank_id {
                // Skip it.
                continue;
            }
            // Only Open Corridor participants can be paid there.
            if attribute(&bank, PARTICIPANT_ATTRIBUTE).is_none_or(|value| !value.eq_ignore_ascii_case("true")) {
                // Skip a bank that is not a participant.
                continue;
            }
            // The bank's routings, without the OBP scheme: a bank_id only means something on its own OBP host.
            let routings: Vec<(String, String)> = bank["bank_routings"]
                // The routings OBP-API listed.
                .as_array()
                // None listed is an empty list.
                .cloned()
                .unwrap_or_default()
                // Each routing.
                .iter()
                // As an upper-case scheme and a trimmed address.
                .map(|routing| {
                    // The pair.
                    (
                        // The scheme, in capitals.
                        routing["scheme"].as_str().unwrap_or_default().trim().to_uppercase(),
                        // The address as given.
                        routing["address"].as_str().unwrap_or_default().trim().to_string(),
                    )
                })
                // Leave out empty ones and the OBP scheme.
                .filter(|(scheme, address)| !scheme.is_empty() && !address.is_empty() && scheme != "OBP")
                // As a list.
                .collect();
            // A bank with no other routing cannot be named from another OBP host.
            if routings.is_empty() {
                // Say which bank, so its operator can add a BIC.
                warn!(obp_host = other.obp_host, bank_id, "bank has no routing other than OBP: no onward route to it");
                // Skip it.
                continue;
            }
            // A bank already listed through an earlier OBP host is listed once.
            if routings.iter().any(|pair| listed_pairs.contains(&(pair.0.clone(), pair.1.to_uppercase()))) {
                // Skip the repeat.
                continue;
            }
            // The bank's country: its own COUNTRY attribute, or else the country set for that OBP host.
            let country = attribute(&bank, COUNTRY_ATTRIBUTE)
                // As an owned string.
                .map(|value| value.trim().to_uppercase())
                // Or the OBP host's.
                .or_else(|| other.default_country.map(|value| value.trim().to_uppercase()));
            // Without a country, a sending bank could not rule the route out, so it is not stated.
            let Some(country) = country else {
                // Say what is missing and where to set it.
                warn!(obp_host = other.obp_host, bank_id, "bank has no COUNTRY attribute and the OBP host has no country in the node's config: no onward route to it");
                // Skip it.
                continue;
            };
            // Remember the bank's pairs, so it is not listed again.
            for (scheme, address) in &routings {
                // Addresses are compared in capitals, as OBP-API does.
                listed_pairs.insert((scheme.clone(), address.to_uppercase()));
            }
            // The route, as OBP-API takes it.
            routes.push(json!({
                // The bank's full name, or its id when it has none.
                "full_name": bank["full_name"].as_str().filter(|name| !name.trim().is_empty()).unwrap_or(bank_id),
                // How the bank is named from another OBP host.
                "bank_routings": routings.iter().map(|(scheme, address)| json!({ "scheme": scheme, "address": address })).collect::<Vec<_>>(),
                // The OBP host the bank is on.
                "obp_host": other.obp_host,
                // Its country.
                "country": country,
                // The currencies a payment can be sent in.
                "currencies": currencies.iter().collect::<Vec<_>>(),
                // No rates: the node does not convert currencies yet.
                "rates": [],
            }));
        }
    }
    // Every route found.
    routes
}

/// The value of a bank's attribute, as getBanks lists it.
fn attribute<'a>(bank: &'a Value, name: &str) -> Option<&'a str> {
    // The bank's attributes.
    bank["attributes"]
        // As a list.
        .as_array()?
        // Each one.
        .iter()
        // The one with that name.
        .find(|entry| entry["name"].as_str() == Some(name))?
        // Its value.
        ["value"]
        // As text.
        .as_str()
}

/// The currencies a node has a corridor account for on one OBP host (upper case).
fn currencies_of(target: &ForwardTarget) -> HashSet<String> {
    // The keys of its corridor accounts, which are already upper case.
    target.corridor_accounts.keys().cloned().collect()
}

/// Confirms the node's onward routes on each of its OBP hosts, on a timer.
pub struct OnwardRouteConfirmer {
    /// Every OBP host the node is on, with its OBP-API client and the node's bank there.
    pub targets: ForwardTargets,
    /// The country of each OBP host's banks, by OBP host, for banks with no COUNTRY attribute.
    pub countries: HashMap<String, String>,
    /// How often the routes are confirmed. Well within the validity OBP-API sets.
    pub every: Duration,
}

impl OnwardRouteConfirmer {
    /// Confirm the routes now, then again every `every`, for as long as the node runs.
    pub async fn run(self) {
        // Say how often, once.
        info!(every_secs = self.every.as_secs(), "confirming the node's onward routes on each of its OBP hosts");
        // For as long as the node runs.
        loop {
            // One round on every OBP host.
            self.confirm_once().await;
            // Then wait for the next round.
            tokio::time::sleep(self.every).await;
        }
    }

    /// One round: on each OBP host, state the routes to the banks on the node's other OBP hosts.
    pub async fn confirm_once(&self) {
        // The OBP hosts, once main has set them.
        let Some(targets) = self.targets.get() else {
            // Not set yet: nothing to do this round.
            return;
        };
        // Each OBP host's getBanks answer, fetched once per round. A host that cannot be asked is left out.
        let mut banks_by_host: HashMap<&str, Value> = HashMap::new();
        // Each OBP host in turn.
        for target in targets {
            // Ask its OBP-API for its banks.
            match target.obp.get_banks().await {
                // Keep the answer.
                Ok(banks) => {
                    // By OBP host.
                    banks_by_host.insert(target.obp_host.as_str(), banks);
                }
                // Routes to it are left out this round, and so expire if this goes on.
                Err(e) => warn!(obp_host = %target.obp_host, error = %e, "could not list the banks on this OBP host: no onward routes to it this round"),
            }
        }
        // The currencies on each OBP host, worked out once.
        let currencies: Vec<HashSet<String>> = targets.iter().map(currencies_of).collect();
        // Each OBP host, to state its routes on.
        for (index, target) in targets.iter().enumerate() {
            // The node's other OBP hosts whose banks are known this round.
            let others: Vec<OtherHost<'_>> = targets
                // Every OBP host.
                .iter()
                // With its currencies.
                .zip(currencies.iter())
                // Numbered.
                .enumerate()
                // Not this one.
                .filter(|(other_index, _)| *other_index != index)
                // Only those whose banks are known.
                .filter_map(|(_, (other, other_currencies))| {
                    // Its banks, if they were fetched.
                    banks_by_host.get(other.obp_host.as_str()).map(|banks| OtherHost {
                        // Its name.
                        obp_host: &other.obp_host,
                        // The node's bank there.
                        own_bank_id: &other.bank_id,
                        // Its currencies.
                        currencies: other_currencies,
                        // Its banks' country, if the config gives one.
                        default_country: self.countries.get(&other.obp_host).map(String::as_str),
                        // Its banks.
                        banks,
                    })
                })
                // As a list.
                .collect();
            // The routes to state on this OBP host.
            let routes = build_routes(&currencies[index], &others);
            // Send them: the list replaces the bank's earlier one there.
            match target.obp.put_onward_routes(&target.bank_id, &routes).await {
                // Confirmed.
                Ok(()) => info!(obp_host = %target.obp_host, bank_id = %target.bank_id, routes = routes.len(), "confirmed the bank's onward routes"),
                // Refused for a missing Scope: say what to grant.
                Err(crate::obp_client::ObpClientError::Rejected { status: 403, .. }) => warn!(obp_host = %target.obp_host, bank_id = %target.bank_id, "OBP-API refused the onward routes: grant the node's Consumer the Scope CanUpdateOnwardRoutes at this bank"),
                // Any other failure: the routes stay as they were until they expire, and the next round tries again.
                Err(e) => warn!(obp_host = %target.obp_host, bank_id = %target.bank_id, error = %e, "could not confirm the bank's onward routes"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A currency set from a list.
    fn currencies(list: &[&str]) -> HashSet<String> {
        // Each currency as an owned string.
        list.iter().map(|currency| currency.to_string()).collect()
    }

    /// A getBanks answer for OBP host B: the hub's own bank, Bank C (a participant), and Bank D (not one).
    fn banks_on_b() -> Value {
        // As OBP-API v6.0.0 lists them.
        json!({"banks": [
            // The hub's own bank there.
            {"bank_id": "rt.bank.b4", "full_name": "Bank B",
             "bank_routings": [{"scheme": "OBP", "address": "rt.bank.b4"}, {"scheme": "BIC", "address": "DEMBZZ22"}],
             "attributes": [{"name": "OPEN_CORRIDOR_PARTICIPANT", "value": "true"}]},
            // A participant, with a country.
            {"bank_id": "rt.bank.c4", "full_name": "Bank C",
             "bank_routings": [{"scheme": "OBP", "address": "rt.bank.c4"}, {"scheme": "BIC", "address": "DEMCZZ22"}],
             "attributes": [{"name": "OPEN_CORRIDOR_PARTICIPANT", "value": "true"}, {"name": "COUNTRY", "value": "ng"}]},
            // Not a participant.
            {"bank_id": "rt.bank.d4", "full_name": "Bank D",
             "bank_routings": [{"scheme": "BIC", "address": "DEMDZZ22"}],
             "attributes": []}
        ]})
    }

    // A participant on the other OBP host becomes a route, without its OBP routing and in the shared currencies.
    #[test]
    fn a_participant_on_another_host_is_a_route() {
        // OBP host B's banks.
        let banks = banks_on_b();
        // The node's currencies on B.
        let currencies_on_b = currencies(&["KES", "NGN", "USD"]);
        // OBP host B as an other host.
        let others = [OtherHost { obp_host: "localhost:8081", own_bank_id: "rt.bank.b4", currencies: &currencies_on_b, default_country: None, banks: &banks }];
        // The routes the node states on A, where it has KES and USD.
        let routes = build_routes(&currencies(&["KES", "USD", "EUR"]), &others);
        // Only Bank C: not the hub's own bank, not Bank D.
        assert_eq!(routes.len(), 1);
        // Named by its BIC only.
        assert_eq!(routes[0]["bank_routings"], json!([{"scheme": "BIC", "address": "DEMCZZ22"}]));
        // Its country from its attribute, in capitals.
        assert_eq!(routes[0]["country"], "NG");
        // The currencies on both OBP hosts, in order.
        assert_eq!(routes[0]["currencies"], json!(["KES", "USD"]));
        // On OBP host B.
        assert_eq!(routes[0]["obp_host"], "localhost:8081");
        // With no rates.
        assert_eq!(routes[0]["rates"], json!([]));
    }

    // A bank with no country attribute takes the OBP host's country from the config, and is left out without one.
    #[test]
    fn a_bank_without_a_country_takes_the_hosts_or_is_left_out() {
        // Bank C with its COUNTRY attribute removed.
        let mut banks = banks_on_b();
        // Just the participant mark.
        banks["banks"][1]["attributes"] = json!([{"name": "OPEN_CORRIDOR_PARTICIPANT", "value": "true"}]);
        // The node's currencies on B.
        let currencies_on_b = currencies(&["KES"]);
        // With a country for OBP host B.
        let with_country = [OtherHost { obp_host: "localhost:8081", own_bank_id: "rt.bank.b4", currencies: &currencies_on_b, default_country: Some("ke"), banks: &banks }];
        // Bank C is a route, in Kenya.
        assert_eq!(build_routes(&currencies(&["KES"]), &with_country)[0]["country"], "KE");
        // Without one.
        let without_country = [OtherHost { obp_host: "localhost:8081", own_bank_id: "rt.bank.b4", currencies: &currencies_on_b, default_country: None, banks: &banks }];
        // There is no route.
        assert!(build_routes(&currencies(&["KES"]), &without_country).is_empty());
    }

    // With no currency in common between the two OBP hosts, nothing can be carried, so there are no routes.
    #[test]
    fn no_shared_currency_means_no_routes() {
        // OBP host B's banks.
        let banks = banks_on_b();
        // Only NGN on B.
        let currencies_on_b = currencies(&["NGN"]);
        // OBP host B as an other host.
        let others = [OtherHost { obp_host: "localhost:8081", own_bank_id: "rt.bank.b4", currencies: &currencies_on_b, default_country: None, banks: &banks }];
        // Only KES on A: nothing in common.
        assert!(build_routes(&currencies(&["KES"]), &others).is_empty());
    }

    /// A stub OBP-API that answers getBanks with `banks` and keeps every onward-routes body it is sent.
    async fn stub_obp_api(banks: Value) -> (String, std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
        // The bodies the stub receives.
        let received = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        // A copy for the handler.
        let received_by_handler = std::sync::Arc::clone(&received);
        // getBanks and the onward-routes PUT.
        let router = axum::Router::new()
            // getBanks at v6.0.0, as the node calls it.
            .route("/obp/v6.0.0/banks", axum::routing::get(move || {
                // Each request gets its own copy.
                let banks = banks.clone();
                // The banks.
                async move { axum::Json(banks) }
            }))
            // The onward routes of any bank.
            .route("/obp/v7.0.0/banks/:bank_id/open-corridor/onward-routes", axum::routing::put(move |axum::Json(body): axum::Json<Value>| {
                // Where to keep the body.
                let received = std::sync::Arc::clone(&received_by_handler);
                // Keep it, and answer as OBP-API does.
                async move {
                    // Kept.
                    received.lock().unwrap().push(body);
                    // An empty list is enough: the node does not read the answer.
                    axum::Json(json!({"onward_routes": []}))
                }
            }));
        // A free port.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        // Its address.
        let address = listener.local_addr().unwrap();
        // Serve in the background.
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        // The base URL and the bodies.
        (format!("http://{address}"), received)
    }

    /// One OBP host the node is on, for the confirmer.
    fn target(obp_host: &str, bank_id: &str, base_url: &str, currencies: &[&str]) -> ForwardTarget {
        // As main builds it.
        ForwardTarget {
            // The OBP host.
            obp_host: obp_host.into(),
            // The node's bank there.
            bank_id: bank_id.into(),
            // Unused here.
            account_id: "settlement".into(),
            // One corridor account per currency.
            corridor_accounts: currencies.iter().map(|currency| (currency.to_string(), format!("settlement-{currency}"))).collect(),
            // Its OBP-API, without credentials.
            obp: std::sync::Arc::new(crate::obp_client::ObpClient::new(base_url, crate::obp_client::ObpAuth::None).unwrap()),
        }
    }

    // One round states, on each OBP host, the banks on the other one.
    #[tokio::test]
    async fn a_round_states_each_hosts_routes_to_the_other() {
        // OBP host A: Bank A (a participant) and the hub's own bank.
        let (url_a, received_a) = stub_obp_api(json!({"banks": [
            // The hub's own bank on A.
            {"bank_id": "rt.bank.b4", "bank_routings": [{"scheme": "BIC", "address": "DEMBZZ22"}], "attributes": [{"name": "OPEN_CORRIDOR_PARTICIPANT", "value": "true"}]},
            // Bank A.
            {"bank_id": "rt.bank.a4", "full_name": "Bank A", "bank_routings": [{"scheme": "BIC", "address": "DEMAZZ22"}], "attributes": [{"name": "OPEN_CORRIDOR_PARTICIPANT", "value": "true"}]}
        ]})).await;
        // OBP host B: the hub's own bank, Bank C and Bank D.
        let (url_b, received_b) = stub_obp_api(banks_on_b()).await;
        // The hub on both, with KES on each.
        let targets: ForwardTargets = std::sync::Arc::new(std::sync::OnceLock::new());
        // Set as main sets it.
        let _ = targets.set(vec![target("localhost:8080", "rt.bank.b4", &url_a, &["KES"]), target("localhost:8081", "rt.bank.b4", &url_b, &["KES", "NGN"])]);
        // OBP host A's banks are in Kenya, for Bank A, which has no COUNTRY attribute.
        let confirmer = OnwardRouteConfirmer { targets, countries: [("localhost:8080".to_string(), "KE".to_string())].into_iter().collect(), every: Duration::from_secs(600) };
        // One round.
        confirmer.confirm_once().await;
        // On A, the hub states Bank C on B.
        let on_a = received_a.lock().unwrap().clone();
        // One PUT.
        assert_eq!(on_a.len(), 1);
        // Naming Bank C, in KES.
        assert_eq!(on_a[0]["onward_routes"][0]["bank_routings"][0]["address"], "DEMCZZ22");
        // The currency both hosts have.
        assert_eq!(on_a[0]["onward_routes"][0]["currencies"], json!(["KES"]));
        // On B, the hub states Bank A on A.
        let on_b = received_b.lock().unwrap().clone();
        // One PUT.
        assert_eq!(on_b.len(), 1);
        // Naming Bank A.
        assert_eq!(on_b[0]["onward_routes"][0]["bank_routings"][0]["address"], "DEMAZZ22");
        // In Kenya, from the config.
        assert_eq!(on_b[0]["onward_routes"][0]["country"], "KE");
        // On host A.
        assert_eq!(on_b[0]["onward_routes"][0]["obp_host"], "localhost:8080");
    }

    // A bank reached through two other OBP hosts is listed once.
    #[test]
    fn a_bank_on_two_other_hosts_is_listed_once() {
        // The same banks on both other OBP hosts.
        let banks = banks_on_b();
        // KES on both.
        let currencies_there = currencies(&["KES"]);
        // Two other OBP hosts listing Bank C.
        let others = [
            // The first.
            OtherHost { obp_host: "localhost:8081", own_bank_id: "rt.bank.b4", currencies: &currencies_there, default_country: None, banks: &banks },
            // The second.
            OtherHost { obp_host: "localhost:8082", own_bank_id: "rt.bank.b4", currencies: &currencies_there, default_country: None, banks: &banks },
        ];
        // Routes on A.
        let routes = build_routes(&currencies(&["KES"]), &others);
        // Bank C once, through the first.
        assert_eq!(routes.len(), 1);
        // The first OBP host.
        assert_eq!(routes[0]["obp_host"], "localhost:8081");
    }
}
