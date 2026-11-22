use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use serde::Deserialize;
use uuid::Uuid;

const MAX_AGE: u64 = 30 * 60;

#[derive(Deserialize)]
pub(super) struct Update {
    pub(super) session: Option<String>,
    pub(super) max_age: Option<u64>,
}

struct Session {
    value: String,
    expires: Instant,
}

#[derive(Default)]
pub(super) struct Sessions(HashMap<Uuid, Session>);

impl Sessions {
    pub(super) fn get(&mut self, scope: Uuid) -> Option<String> {
        self.0.retain(|_, session| session.expires > Instant::now());
        self.0.get(&scope).map(|session| session.value.clone())
    }

    pub(super) fn remember(&mut self, scope: Uuid, session: String, max_age: Option<u64>) -> bool {
        let ttl = max_age.unwrap_or(MAX_AGE).min(MAX_AGE);
        if ttl == 0
            || session.is_empty()
            || session.len() > 8192
            || !session
                .bytes()
                .all(|ch| ch.is_ascii_graphic() || ch == b' ')
            || !session.split(';').all(|pair| {
                pair.trim()
                    .split_once('=')
                    .is_some_and(|(name, _)| !name.is_empty())
            })
        {
            return false;
        }
        let now = Instant::now();
        self.0.retain(|_, value| value.expires > now);
        self.0.insert(
            scope,
            Session {
                value: session,
                expires: now + Duration::from_secs(ttl),
            },
        );
        true
    }

    pub(super) fn clear(&mut self, scope: Uuid) {
        self.0.remove(&scope);
    }
}
