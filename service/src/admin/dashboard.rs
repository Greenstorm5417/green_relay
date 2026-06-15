//! Dashboard data: health/signal/recent-activity aggregation and the
//! activity-window selection logic. SQL lives in [`crate::db`]; this layer
//! formats rows into [`ActivityEntry`] values and applies the recency window.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::db::{Db, DbError};
use crate::health::{ServiceHealth, derive_health};

use super::AdminState;

/// The limit of recent activity entries to return.
pub const RECENT_ACTIVITY_LIMIT: usize = 10;

/// An activity entry recording a recent event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityEntry {
    /// The timestamp of the activity.
    pub timestamp: DateTime<Utc>,
    /// Description of the activity.
    pub description: String,
}

/// Retrieves the recent activities within the timeframe limit.
pub fn recent_activity(entries: &[ActivityEntry], now: DateTime<Utc>) -> Vec<ActivityEntry> {
    let window = chrono::Duration::hours(24);

    let mut recent: Vec<ActivityEntry> = entries
        .iter()
        .filter(|entry| {
            let age = now.signed_duration_since(entry.timestamp);
            age >= chrono::Duration::zero() && age <= window
        })
        .cloned()
        .collect();

    recent.sort_by_key(|e| std::cmp::Reverse(e.timestamp));
    recent.truncate(RECENT_ACTIVITY_LIMIT);
    recent
}

/// Admin dashboard statistics and recent activity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardData {
    /// Service health status.
    pub health: ServiceHealth,
    /// Signal strength percentage.
    pub signal_percent: Option<u8>,
    /// Recent activities.
    pub recent: Vec<ActivityEntry>,
}

/// Retrieves data required for rendering the admin dashboard.
pub async fn dashboard_data(
    state: &AdminState,
    now_utc: DateTime<Utc>,
) -> Result<DashboardData, DbError> {
    let snapshot = state.modem.current();
    let health = derive_health(&snapshot);
    let signal_percent = snapshot.signal_percent;
    let recent = recent_message_activity(&state.db, now_utc).await?;
    Ok(DashboardData {
        health,
        signal_percent,
        recent: recent_activity(&recent, now_utc),
    })
}

pub(crate) async fn recent_message_activity(
    db: &Db,
    now_utc: DateTime<Utc>,
) -> Result<Vec<ActivityEntry>, DbError> {
    let cutoff = now_utc
        .checked_sub_signed(chrono::Duration::hours(24))
        .unwrap_or(now_utc)
        .to_rfc3339();
    let mut entries = Vec::new();

    for row in db.recent_outbound_activity(&cutoff).await? {
        entries.push(ActivityEntry {
            timestamp: row.created_at,
            description: format!("Outbound to {} ({})", row.to_number, row.status),
        });
    }

    for row in db.recent_inbound_activity(&cutoff).await? {
        entries.push(ActivityEntry {
            timestamp: row.received_at,
            description: format!("Inbound from {}", row.from_number),
        });
    }

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::testutil::test_state;
    use crate::models::MessageStatus;
    use chrono::TimeZone;

    fn at(hours_ago: i64, now: DateTime<Utc>) -> DateTime<Utc> {
        now - chrono::Duration::hours(hours_ago)
    }

    #[test]
    fn recent_activity_filters_and_orders() {
        let now = Utc.with_ymd_and_hms(2024, 6, 1, 12, 0, 0).unwrap();
        let entries = vec![
            ActivityEntry {
                timestamp: at(1, now),
                description: "recent".into(),
            },
            ActivityEntry {
                timestamp: at(25, now),
                description: "too old".into(),
            },
            ActivityEntry {
                timestamp: at(3, now),
                description: "older recent".into(),
            },
        ];
        let selected = recent_activity(&entries, now);
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].description, "recent");
        assert_eq!(selected[1].description, "older recent");
    }

    #[test]
    fn recent_activity_caps_at_ten() {
        let now = Utc.with_ymd_and_hms(2024, 6, 1, 12, 0, 0).unwrap();
        let entries: Vec<ActivityEntry> = (0..20)
            .map(|i| ActivityEntry {
                timestamp: now - chrono::Duration::minutes(i),
                description: format!("entry {i}"),
            })
            .collect();
        let selected = recent_activity(&entries, now);
        assert_eq!(selected.len(), RECENT_ACTIVITY_LIMIT);
        assert_eq!(selected[0].description, "entry 0");
    }

    #[test]
    fn recent_activity_excludes_future_entries() {
        let now = Utc.with_ymd_and_hms(2024, 6, 1, 12, 0, 0).unwrap();
        let entries = vec![ActivityEntry {
            timestamp: now + chrono::Duration::hours(1),
            description: "future".into(),
        }];
        assert!(recent_activity(&entries, now).is_empty());
    }

    #[tokio::test]
    async fn dashboard_reports_health_signal_and_activity() {
        let state = test_state().await;

        state
            .db
            .create_outbound_message("+14155552671", "hi", MessageStatus::Queued, 1)
            .await
            .unwrap();

        let data = dashboard_data(&state, Utc::now()).await.unwrap();
        assert_eq!(data.health, ServiceHealth::Healthy);
        assert_eq!(data.signal_percent, Some(75));
        assert_eq!(data.recent.len(), 1);
        assert!(data.recent[0].description.contains("Outbound"));
    }

    #[tokio::test]
    async fn dashboard_caps_recent_activity_at_ten() {
        let state = test_state().await;
        for _ in 0..15 {
            state
                .db
                .create_outbound_message("+14155552671", "hi", MessageStatus::Queued, 1)
                .await
                .unwrap();
        }
        let data = dashboard_data(&state, Utc::now()).await.unwrap();
        assert_eq!(data.recent.len(), RECENT_ACTIVITY_LIMIT);
    }
}
