//! Milestone detection. Achievements are derived purely from the numbers we
//! already fetched — no extra API calls, no stored history.

pub struct Achievement {
    pub title: &'static str,
    pub detail: String,
}

/// Everything the overview needs to judge a period.
pub struct Snapshot {
    pub users: f64,
    pub prev_users: f64,
    pub sessions: f64,
    pub views: f64,
    pub conversions: f64,
    pub prev_conversions: f64,
    pub bounce_rate: f64,
    pub prev_bounce_rate: f64,
    pub avg_duration: f64,
    /// Daily user counts, oldest first.
    pub daily_users: Vec<f64>,
}

impl Snapshot {
    /// Longest run of consecutive day-over-day increases, counted from the end.
    fn growth_streak(&self) -> usize {
        let mut streak = 0;
        for pair in self.daily_users.windows(2).rev() {
            if pair[1] > pair[0] {
                streak += 1;
            } else {
                break;
            }
        }
        streak
    }
}

pub fn unlocked(snap: &Snapshot) -> Vec<Achievement> {
    let mut out = Vec::new();

    let streak = snap.growth_streak();
    if streak >= 3 {
        out.push(Achievement {
            title: "Diamond Streak",
            detail: format!("{streak} days of back-to-back growth"),
        });
    }

    if snap.prev_users > 0.0 {
        let growth = (snap.users - snap.prev_users) / snap.prev_users;
        if growth >= 0.5 {
            out.push(Achievement {
                title: "Beacon Lit",
                detail: format!("villagers up {:.0}% on the previous period", growth * 100.0),
            });
        }
    }

    if snap.prev_conversions > 0.0 {
        let growth = (snap.conversions - snap.prev_conversions) / snap.prev_conversions;
        if growth >= 0.2 {
            out.push(Achievement {
                title: "Deep Vein",
                detail: format!("diamonds up {:.0}% — something is working", growth * 100.0),
            });
        }
    }

    if snap.sessions > 0.0 && snap.views / snap.sessions >= 3.0 {
        out.push(Achievement {
            title: "Strip Miner",
            detail: format!(
                "{:.1} blocks mined per expedition",
                snap.views / snap.sessions
            ),
        });
    }

    // Bounce rate falling is a win, so the comparison runs the other way.
    if snap.prev_bounce_rate > 0.0 && snap.bounce_rate < snap.prev_bounce_rate * 0.9 {
        out.push(Achievement {
            title: "Creeper Repelled",
            detail: format!(
                "creeper rate down to {:.1}% from {:.1}%",
                snap.bounce_rate * 100.0,
                snap.prev_bounce_rate * 100.0
            ),
        });
    }

    if snap.avg_duration >= 180.0 {
        out.push(Achievement {
            title: "Long Haul",
            detail: format!(
                "{}m average time survived",
                (snap.avg_duration / 60.0).round() as u64
            ),
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Snapshot {
        Snapshot {
            users: 1000.0,
            prev_users: 1000.0,
            sessions: 1000.0,
            views: 1000.0,
            conversions: 100.0,
            prev_conversions: 100.0,
            bounce_rate: 0.5,
            prev_bounce_rate: 0.5,
            avg_duration: 60.0,
            daily_users: vec![100.0, 100.0, 100.0],
        }
    }

    #[test]
    fn a_flat_period_unlocks_nothing() {
        assert!(unlocked(&base()).is_empty());
    }

    #[test]
    fn streak_counts_only_the_trailing_run() {
        let mut snap = base();
        // Rises, then a dip, then three straight rises.
        snap.daily_users = vec![10.0, 20.0, 5.0, 6.0, 7.0, 8.0];
        let found = unlocked(&snap);
        assert!(found.iter().any(|a| a.title == "Diamond Streak"));
        assert!(found[0].detail.contains('3'), "got {:?}", found[0].detail);
    }

    #[test]
    fn a_dip_at_the_end_breaks_the_streak() {
        let mut snap = base();
        snap.daily_users = vec![1.0, 2.0, 3.0, 4.0, 2.0];
        assert!(!unlocked(&snap).iter().any(|a| a.title == "Diamond Streak"));
    }

    #[test]
    fn falling_bounce_rate_is_a_win() {
        let mut snap = base();
        snap.bounce_rate = 0.30;
        snap.prev_bounce_rate = 0.50;
        assert!(unlocked(&snap)
            .iter()
            .any(|a| a.title == "Creeper Repelled"));
    }

    #[test]
    fn zero_baselines_do_not_panic_or_unlock() {
        let mut snap = base();
        snap.prev_users = 0.0;
        snap.prev_conversions = 0.0;
        snap.prev_bounce_rate = 0.0;
        snap.sessions = 0.0;
        let found = unlocked(&snap);
        assert!(!found.iter().any(|a| a.title == "Beacon Lit"));
        assert!(!found.iter().any(|a| a.title == "Strip Miner"));
    }
}
