//! Conformance scenarios and the provider side they run against (plan,
//! chapter 8.1).
//!
//! A [`Scenario`] is a language-neutral JSON file in
//! `crates/musubi-testkit/conformance/`. The runner
//! (`tests/conformance.rs`) starts `musubi-syncd` as a process, plays the
//! [`Step`]s against one [`Target`] (a provider: the fake Google, the fake
//! DAV server, Radicale) and checks each [`Expect`] over the node's HTTP API.
//!
//! # File format
//!
//! Every object rejects unknown fields: a scenario written for a newer
//! runner fails to load instead of passing with a step ignored.
//!
//! ```json
//! {
//!   "name": "initial_read",
//!   "description": "Initial read: the window first, then everything.",
//!   "kinds": ["event"],
//!   "steps": [
//!     { "op": "put", "ref": "near", "object": { "kind": "event", "title": "near", "day": 1 } },
//!     { "op": "put", "ref": "far", "object": { "kind": "event", "title": "far", "day": 200 } },
//!     { "op": "connect" },
//!     { "op": "expect", "converged": true, "window_first": ["near"] }
//!   ]
//! }
//! ```
//!
//! - `kinds`: the entity kinds the scenario needs. A target that lacks one
//!   ([`Target::kinds`]) skips the whole scenario with that reason.
//! - `only`: optional list of [`Target::name`]s the scenario applies to (the
//!   "—" cells of the plan's table); other targets do not run it and do not
//!   report it as skipped.
//! - `ref`: a scenario-local name of a provider object. The first `put` of a
//!   ref creates the object; a later `put` of the same ref replaces it;
//!   `delete` removes it. The runner keeps the expected live set from these.
//! - Times are relative to the run: `day` is a whole-day offset from today
//!   (UTC), so `day: 1` lies inside the engine's default window (30 days
//!   back, 90 forward) and `day: 200` outside it.
//!
//! # Skips
//!
//! A step the target cannot do returns [`Skip`] with a reason; the runner
//! stops that scenario, reports it as skipped with the reason and prints the
//! list of skipped scenarios at the end. There is no silent skip.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::Path;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::harness::Api;

/// One scenario file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    /// Short identifier; by convention the file name without `.json`.
    pub name: String,
    /// What the scenario proves, one or two sentences.
    pub description: String,
    /// Entity kinds the scenario needs.
    pub kinds: Vec<Kind>,
    /// Targets the scenario applies to ([`Target::name`]); all when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub only: Option<Vec<String>>,
    /// The steps, in order.
    pub steps: Vec<Step>,
}

impl Scenario {
    /// Parses a scenario file.
    pub fn parse(json: &str) -> serde_json::Result<Self> {
        serde_json::from_str(json)
    }
}

/// An entity kind as the scenarios name it (the API's `kind`, with
/// `contact_group` shortened).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Calendar event (API `event`).
    Event,
    /// Task (API `task`).
    Task,
    /// Individual contact (API `contact`).
    Contact,
    /// Contact group (API `contact_group`).
    Group,
}

impl Kind {
    /// The API's `kind` string.
    pub fn api(self) -> &'static str {
        match self {
            Kind::Event => "event",
            Kind::Task => "task",
            Kind::Contact => "contact",
            Kind::Group => "contact_group",
        }
    }
}

/// A logical provider object. Each target renders it in its own format
/// (Google event JSON, iCalendar, vCard).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Object {
    /// A calendar event.
    Event {
        /// Title (`SUMMARY`, Google `summary`); also the identity the runner
        /// compares against the API's canonical `title`.
        title: String,
        /// Start day, offset from today (UTC).
        day: i64,
        /// Start hour, UTC (timed events).
        #[serde(default = "ten")]
        hour: u8,
        /// Duration in minutes (timed events).
        #[serde(default = "sixty")]
        minutes: u32,
        /// An all-day event on `day` (hour and minutes are ignored).
        #[serde(default)]
        all_day: bool,
        /// A value of a property the codecs do not know (an `X-` property in
        /// iCalendar, an extra member at Google). The raw layer must keep it
        /// byte for byte.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unknown: Option<String>,
    },
    /// A task (`VTODO`).
    Task {
        /// Title (`SUMMARY`), compared against the canonical `title`.
        title: String,
        /// Due day, offset from today (UTC); none = no due date.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        due_day: Option<i64>,
        /// Completed.
        #[serde(default)]
        done: bool,
        /// See [`Object::Event`]'s `unknown`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unknown: Option<String>,
    },
    /// An individual contact.
    Contact {
        /// Full name (`FN`), compared against the canonical `name.full`.
        name: String,
        /// One e-mail address.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        email: Option<String>,
        /// See [`Object::Event`]'s `unknown`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unknown: Option<String>,
    },
    /// A contact group. The target picks the group format it supports (vCard
    /// 4 `KIND:group`, or Apple's `X-ADDRESSBOOKSERVER-KIND:group` where
    /// only vCard 3 exists).
    Group {
        /// Group name (`FN`), compared against the canonical `name`.
        name: String,
        /// Refs of member contacts put earlier in the scenario.
        #[serde(default)]
        members: Vec<String>,
    },
}

fn ten() -> u8 {
    10
}

fn sixty() -> u32 {
    60
}

impl Object {
    /// The object's kind.
    pub fn kind(&self) -> Kind {
        match self {
            Object::Event { .. } => Kind::Event,
            Object::Task { .. } => Kind::Task,
            Object::Contact { .. } => Kind::Contact,
            Object::Group { .. } => Kind::Group,
        }
    }

    /// The text the runner matches against the canonical form: an event's or
    /// task's title, a contact's full name, a group's name.
    pub fn label(&self) -> &str {
        match self {
            Object::Event { title, .. } | Object::Task { title, .. } => title,
            Object::Contact { name, .. } | Object::Group { name, .. } => name,
        }
    }
}

/// One step of a scenario.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    /// Create or replace an object at the provider ([`Target::put`]).
    Put {
        /// Scenario-local name of the object.
        #[serde(rename = "ref")]
        reference: String,
        /// Collection (by the name given to `add_collection`); the target's
        /// default collection for the object's kind when absent. Ignored when
        /// replacing.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        collection: Option<String>,
        /// The object's new content.
        object: Object,
    },
    /// Delete an object at the provider ([`Target::delete`]).
    Delete {
        /// Scenario-local name of the object.
        #[serde(rename = "ref")]
        reference: String,
    },
    /// Connect the account to the node ([`Target::connect`]); once per
    /// scenario, usually after the provider holds the initial data.
    Connect,
    /// Fix the account after `401` (a changed password, revoked tokens) and
    /// let the node resume ([`Target::reconnect`]).
    Reconnect,
    /// Invalidate the provider's sync cursors ([`Target::expire_cursor`]).
    ExpireCursor {
        /// Collection by name; all collections when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        collection: Option<String>,
    },
    /// Make the provider misbehave ([`Target::inject`]).
    Inject {
        /// The fault.
        fault: Fault,
        /// How many requests get it.
        #[serde(default = "one")]
        times: u32,
    },
    /// Add a collection the next discovery finds ([`Target::add_collection`]).
    AddCollection {
        /// Scenario-local name.
        name: String,
        /// Kinds it holds.
        kinds: Vec<Kind>,
    },
    /// Remove a collection with its objects ([`Target::remove_collection`]).
    RemoveCollection {
        /// Scenario-local name.
        name: String,
    },
    /// SIGKILL `musubi-syncd` and start it again over the same data.
    Kill,
    /// Sleep.
    Wait {
        /// Milliseconds.
        ms: u64,
    },
    /// Wait until every given condition holds (or fail at the timeout).
    Expect(Expect),
}

fn one() -> u32 {
    1
}

/// Conditions over the node's API. Each given field must hold; the runner
/// polls until all hold at once or `timeout_ms` passes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    /// The node shows exactly the live objects the steps left at the
    /// provider (by kind and [`Object::label`], from `/v1/changes` and
    /// `/v1/entities/{id}`), and every collection is fully read
    /// (`coverage.type == "full"`) without an error.
    #[serde(default)]
    pub converged: bool,
    /// These refs entered the journal before every other ref (the window
    /// is read before the rest).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub window_first: Vec<String>,
    /// The account's state (`GET /v1/accounts/{id}`), e.g. `active` or
    /// `reauth_required`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_state: Option<String>,
    /// No entity entry in the journal since the previous `expect` step (or
    /// since `connect`), checked after `quiet_ms`. Collection entries (sync
    /// state) do not count.
    #[serde(default)]
    pub unchanged: bool,
    /// Requests ([`Target::requests`] labels, e.g. `REPORT calendar-multiget`)
    /// the provider must not have received since the previous `expect`
    /// step (or since `connect`), checked after `quiet_ms`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub no_requests: Vec<String>,
    /// How long `unchanged` and `no_requests` must stay true, in
    /// milliseconds (default 2000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet_ms: Option<u64>,
    /// The raw provider bytes of a ref's item (`/v1/items/{id}?include=raw`)
    /// contain a text, typically an object's `unknown` value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_contains: Option<RawContains>,
    /// Give up after this many milliseconds (default 20000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// See [`Expect::raw_contains`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawContains {
    /// The object.
    #[serde(rename = "ref")]
    pub reference: String,
    /// The text.
    pub text: String,
}

/// A provider fault a scenario injects. Targets map it to their own error
/// shape (Google's JSON errors, plain DAV statuses).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Fault {
    /// `429` or `503`, optionally with `Retry-After` in seconds.
    RateLimited {
        /// HTTP status (`429` or `503`).
        status: u16,
        /// `Retry-After` in seconds.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after: Option<u64>,
    },
    /// The credentials stop working (`401`) until [`Step::Reconnect`].
    Unauthorized,
    /// Every response is delayed.
    Slow {
        /// Delay in milliseconds.
        ms: u64,
    },
}

/// Why a target cannot run a step; the scenario is reported as skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skip(pub String);

impl Skip {
    /// A skip with this reason.
    pub fn because(reason: impl Into<String>) -> Self {
        Skip(reason.into())
    }
}

/// A provider the scenarios run against. One value serves one scenario run:
/// the runner creates a fresh target per scenario.
///
/// Handles are the target's own ids of objects and collections (an event id,
/// an object path); the runner only stores and passes them back.
#[async_trait]
pub trait Target: Send {
    /// Short name used in `only` and in the report (`google`, `dav-generic`,
    /// `dav-icloud`, `radicale`).
    fn name(&self) -> &str;

    /// Entity kinds this target serves.
    fn kinds(&self) -> &[Kind];

    /// The `musubi-syncd` configuration for a node listening on `listen` with
    /// its data in `data` (provider settings included).
    fn config(&self, listen: SocketAddr, data: &Path) -> Value;

    /// Connects the account through the node's API with the tenant's admin
    /// key; returns the account id. Called once, at [`Step::Connect`].
    async fn connect(&mut self, api: &Api, admin: &str) -> Result<String, Skip>;

    /// Creates (`handle` none) or replaces (`handle` from an earlier `put`)
    /// an object. `collection` is a handle from [`Target::add_collection`],
    /// or none for the default collection of the object's kind. `refs` maps
    /// the scenario's refs to handles (group members). Returns the handle.
    async fn put(
        &mut self,
        handle: Option<&str>,
        collection: Option<&str>,
        object: &Object,
        refs: &BTreeMap<String, String>,
    ) -> Result<String, Skip>;

    /// Deletes an object.
    async fn delete(&mut self, handle: &str) -> Result<(), Skip>;

    /// Invalidates the sync cursors of one collection (handle) or all.
    async fn expire_cursor(&mut self, collection: Option<&str>) -> Result<(), Skip>;

    /// Injects a fault.
    async fn inject(&mut self, fault: &Fault, times: u32) -> Result<(), Skip>;

    /// Undoes [`Fault::Unauthorized`] and lets the node use the account again.
    async fn reconnect(&mut self, _api: &Api, _admin: &str) -> Result<(), Skip> {
        Err(Skip::because(format!("{} cannot reconnect", self.name())))
    }

    /// Adds a collection holding `kinds`; returns its handle.
    async fn add_collection(&mut self, name: &str, _kinds: &[Kind]) -> Result<String, Skip> {
        Err(Skip::because(format!(
            "{} cannot add collection {name}",
            self.name()
        )))
    }

    /// Removes a collection by handle.
    async fn remove_collection(&mut self, _handle: &str) -> Result<(), Skip> {
        Err(Skip::because(format!(
            "{} cannot remove collections",
            self.name()
        )))
    }

    /// How many requests with this label the provider has received (for
    /// [`Expect::no_requests`]).
    fn requests(&self, label: &str) -> Result<usize, Skip> {
        Err(Skip::because(format!(
            "{} does not count {label} requests",
            self.name()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenarios_parse_and_reject_unknown_fields() {
        let s = Scenario::parse(
            r#"{"name":"n","description":"d","kinds":["event","group"],"steps":[
                {"op":"put","ref":"a","object":{"kind":"event","title":"t","day":-3}},
                {"op":"inject","fault":{"type":"rate_limited","status":429,"retry_after":2}},
                {"op":"expect","converged":true,"window_first":["a"]}]}"#,
        )
        .expect("parse");
        assert_eq!(
            s.steps[0],
            Step::Put {
                reference: "a".into(),
                collection: None,
                object: Object::Event {
                    title: "t".into(),
                    day: -3,
                    hour: 10,
                    minutes: 60,
                    all_day: false,
                    unknown: None,
                },
            }
        );
        assert!(matches!(s.steps[1], Step::Inject { times: 1, .. }));
        let Step::Expect(e) = &s.steps[2] else {
            panic!("expect")
        };
        assert!(e.converged && e.window_first == ["a"] && e.timeout_ms.is_none());
        for bad in [
            r#"{"name":"n","description":"d","kinds":[],"steps":[],"extra":1}"#,
            r#"{"name":"n","description":"d","kinds":[],"steps":[{"op":"expect","eventually":true}]}"#,
            r#"{"name":"n","description":"d","kinds":[],"steps":[{"op":"teleport"}]}"#,
        ] {
            assert!(Scenario::parse(bad).is_err(), "{bad}");
        }
    }
}
