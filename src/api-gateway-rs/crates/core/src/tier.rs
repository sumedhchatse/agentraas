//! Plan tier — the single source of truth for what an org is entitled to.
//! Every feature-gating check (HITL, inbound webhooks, team seats, ...)
//! should compare against a `Tier` rather than re-deriving one from
//! `state.enterprise_mode` or a raw plan string. This module is the pure
//! type only (`crates/core` has no `sqlx` dependency by design); the
//! actual DB-backed resolution — `effective_tier()` — lives in
//! `crates/api/src/agent/db.rs` alongside `check_org_write_permission`
//! and friends, which follow the same `pg: &PgPool` pattern.

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    Community,
    Pro,
    Agency,
    Enterprise,
}

impl Tier {
    /// Maps `users.plan`'s stored string to a `Tier`. Unrecognized or
    /// absent values resolve to `Community` — never an error, since a
    /// stale or unexpected value must never accidentally unlock a paid
    /// feature. Covers today's stored default (`"free"`) as well as
    /// anything genuinely unrecognized.
    pub fn from_plan_str(plan: &str) -> Tier {
        match plan {
            "pro" => Tier::Pro,
            "agency" => Tier::Agency,
            "enterprise" => Tier::Enterprise,
            _ => Tier::Community,
        }
    }

    pub fn seat_limit(self) -> Option<u32> {
        match self {
            Tier::Community => Some(1),
            Tier::Pro => Some(3),
            Tier::Agency => Some(10),
            Tier::Enterprise => None, // unlimited
        }
    }

    /// Inverse of `from_plan_str` — used by license tokens (which encode
    /// a tier as this same string) and anywhere else a `Tier` needs to
    /// round-trip through storage/transport as text.
    pub fn as_plan_str(self) -> &'static str {
        match self {
            Tier::Community => "free",
            Tier::Pro => "pro",
            Tier::Agency => "agency",
            Tier::Enterprise => "enterprise",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_plan_str_maps_known_values() {
        assert_eq!(Tier::from_plan_str("pro"), Tier::Pro);
        assert_eq!(Tier::from_plan_str("agency"), Tier::Agency);
        assert_eq!(Tier::from_plan_str("enterprise"), Tier::Enterprise);
    }

    #[test]
    fn from_plan_str_defaults_unknown_to_community() {
        assert_eq!(Tier::from_plan_str("free"), Tier::Community);
        assert_eq!(Tier::from_plan_str(""), Tier::Community);
        assert_eq!(Tier::from_plan_str("something-unexpected"), Tier::Community);
    }

    #[test]
    fn as_plan_str_round_trips_through_from_plan_str() {
        for tier in [Tier::Community, Tier::Pro, Tier::Agency, Tier::Enterprise] {
            assert_eq!(Tier::from_plan_str(tier.as_plan_str()), tier);
        }
    }

    #[test]
    fn tiers_order_correctly() {
        assert!(Tier::Community < Tier::Pro);
        assert!(Tier::Pro < Tier::Agency);
        assert!(Tier::Agency < Tier::Enterprise);
    }

    #[test]
    fn seat_limits_match_spec() {
        assert_eq!(Tier::Community.seat_limit(), Some(1));
        assert_eq!(Tier::Pro.seat_limit(), Some(3));
        assert_eq!(Tier::Agency.seat_limit(), Some(10));
        assert_eq!(Tier::Enterprise.seat_limit(), None);
    }
}
