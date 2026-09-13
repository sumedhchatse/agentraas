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
    Team,
    Enterprise,
}

impl Tier {
    /// Maps `users.plan`'s stored string to a `Tier`. Unrecognized or
    /// absent values resolve to `Community` — never an error, since a
    /// stale or unexpected value must never accidentally unlock a paid
    /// feature. Covers today's stored default (`"free"`) as well as
    /// anything genuinely unrecognized. `"pro"` and `"agency"` are kept
    /// as aliases (the tier names collapsed from 4 to 3 — Pro renamed to
    /// Team, Agency folded up into Enterprise) so any row already
    /// carrying the old string keeps its entitlement instead of silently
    /// dropping to Community.
    pub fn from_plan_str(plan: &str) -> Tier {
        match plan {
            "team" | "pro" => Tier::Team,
            "agency" | "enterprise" => Tier::Enterprise,
            _ => Tier::Community,
        }
    }

    pub fn seat_limit(self) -> Option<u32> {
        match self {
            Tier::Community => Some(1),
            Tier::Team => Some(3),
            Tier::Enterprise => None, // unlimited
        }
    }

    /// Inverse of `from_plan_str` — used by license tokens (which encode
    /// a tier as this same string) and anywhere else a `Tier` needs to
    /// round-trip through storage/transport as text. Always the current
    /// canonical name, never an alias.
    pub fn as_plan_str(self) -> &'static str {
        match self {
            Tier::Community => "free",
            Tier::Team => "team",
            Tier::Enterprise => "enterprise",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_plan_str_maps_known_values() {
        assert_eq!(Tier::from_plan_str("team"), Tier::Team);
        assert_eq!(Tier::from_plan_str("enterprise"), Tier::Enterprise);
    }

    #[test]
    fn from_plan_str_keeps_old_names_as_aliases() {
        assert_eq!(Tier::from_plan_str("pro"), Tier::Team);
        assert_eq!(Tier::from_plan_str("agency"), Tier::Enterprise);
    }

    #[test]
    fn from_plan_str_defaults_unknown_to_community() {
        assert_eq!(Tier::from_plan_str("free"), Tier::Community);
        assert_eq!(Tier::from_plan_str(""), Tier::Community);
        assert_eq!(Tier::from_plan_str("something-unexpected"), Tier::Community);
    }

    #[test]
    fn as_plan_str_round_trips_through_from_plan_str() {
        for tier in [Tier::Community, Tier::Team, Tier::Enterprise] {
            assert_eq!(Tier::from_plan_str(tier.as_plan_str()), tier);
        }
    }

    #[test]
    fn tiers_order_correctly() {
        assert!(Tier::Community < Tier::Team);
        assert!(Tier::Team < Tier::Enterprise);
    }

    #[test]
    fn seat_limits_match_spec() {
        assert_eq!(Tier::Community.seat_limit(), Some(1));
        assert_eq!(Tier::Team.seat_limit(), Some(3));
        assert_eq!(Tier::Enterprise.seat_limit(), None);
    }
}
