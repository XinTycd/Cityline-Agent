use askama::Template;
use axum::{
    Extension, Router,
    extract::{Query, State},
    http::StatusCode,
    response::Response,
    routing::get,
};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use uuid::Uuid;

use super::{Selection, date_month, source_label, tasks::check_supplement_context};
use crate::{
    auth::User,
    refresh::Refresh,
    store::Store,
    web::{LIST_LIMIT, ListPages, WebError, database, render},
};

const SNAPSHOT_COLUMNS: &str =
    "id, starts, ends, observed_at, media_mask, source, evidence_root IS NOT NULL";
const MISSING_BATCH: WebError = (StatusCode::NOT_FOUND, "未找到该档期快照。");

#[derive(Deserialize)]
struct Filter {
    starts: Option<String>,
    ends: Option<String>,
    source: Option<String>,
    page: Option<u32>,
    batch: Option<Uuid>,
    task: Option<Uuid>,
}

pub(super) struct StockSnapshot {
    id: String,
    pub(super) starts: String,
    pub(super) ends: String,
    observed: String,
    pub(super) selection: Selection,
    source: String,
    supplemented: bool,
}

impl StockSnapshot {
    fn read(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let selection = match row.get::<_, u8>(4)? {
            1 => Selection::Bus,
            2 => Selection::Metro,
            3 => Selection::Both,
            _ => return Err(rusqlite::Error::InvalidQuery),
        };
        Ok(Self {
            id: row.get(0)?,
            starts: row.get(1)?,
            ends: row.get(2)?,
            observed: row.get(3)?,
            selection,
            source: row.get(5)?,
            supplemented: row.get(6)?,
        })
    }

    fn source_label(&self) -> &'static str {
        source_label(&self.source)
    }

    fn catalog(&self) -> &'static str {
        if self.source == "platform" {
            "platform"
        } else {
            "local"
        }
    }
}

pub(super) async fn snapshot(store: &Store, id: Uuid) -> Result<StockSnapshot, WebError> {
    let store = store.clone();
    database(move || {
        let db = store.connect()?;
        db.query_row(
            &format!("SELECT {SNAPSHOT_COLUMNS} FROM media_snapshots WHERE id=?1 AND industry='food' AND media_mask IN (1,2,3)"),
            [id.to_string()],
            StockSnapshot::read,
        ).optional()
    })
    .await?
    .ok_or(MISSING_BATCH)
}

#[derive(Template)]
#[template(path = "media-data.html")]
struct DataPage {
    heading: &'static str,
    section: &'static str,
    starts: String,
    ends: String,
    source: &'static str,
    batch: Option<Uuid>,
    source_task: Option<Uuid>,
    review_budget: Option<u32>,
    snapshots: Vec<StockSnapshot>,
    pages: ListPages,
    is_admin: bool,
    platform_issue: Option<&'static str>,
}

async fn page(
    State(store): State<Store>,
    Extension(user): Extension<User>,
    Query(filter): Query<Filter>,
) -> Result<Response, WebError> {
    let starts = filter.starts.unwrap_or_default();
    let ends = filter.ends.unwrap_or_default();
    let mut source = match filter.source.as_deref() {
        Some("local") => "local",
        _ => "platform",
    };
    if [&starts, &ends]
        .iter()
        .any(|day| !day.is_empty() && date_month(day).is_none())
        || (!ends.is_empty() && starts > ends)
    {
        return Err((StatusCode::BAD_REQUEST, "请选择 2027 年内有效的档期范围。"));
    }
    let batch = filter.batch;
    let source_task = filter.task.filter(|_| batch.is_some());
    let review_budget = if let Some((task, batch)) = source_task.zip(batch) {
        check_supplement_context(&store, task, batch).await?
    } else {
        None
    };
    let page = ListPages::number(filter.page);
    let mut snapshots = if let Some(batch) = batch {
        let entry = snapshot(&store, batch).await?;
        source = entry.catalog();
        vec![entry]
    } else {
        let from = starts.clone();
        let through = ends.clone();
        database(move || {
            let db = store.connect()?;
            db.prepare(&format!(
                "SELECT {SNAPSHOT_COLUMNS} FROM media_snapshots
                 WHERE industry='food' AND media_mask IN (1,2,3)
                   AND (?1='' OR starts>=?1) AND (?2='' OR ends<=?2)
                   AND CASE ?3 WHEN 'platform' THEN source='platform'
                               ELSE source IN ('manual','legacy') END
                 ORDER BY observed_at DESC, rowid DESC LIMIT ?4 OFFSET ?5"
            ))?
            .query_map(
                params![
                    from,
                    through,
                    source,
                    LIST_LIMIT as i64 + 1,
                    ListPages::offset(page, LIST_LIMIT as u32)
                ],
                StockSnapshot::read,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()
        })
        .await?
    };
    let pages = ListPages::new(page, &mut snapshots);
    let platform_issue = if batch.is_none() && source == "platform" {
        Refresh::preflight(false, Selection::Both.id())
            .err()
            .map(|(_, reason)| reason)
    } else {
        None
    };
    Ok(render(DataPage {
        heading: "数据中心",
        section: "media-data",
        starts,
        ends,
        source,
        batch,
        source_task,
        review_budget,
        snapshots,
        pages,
        is_admin: user.admin,
        platform_issue,
    }))
}

pub(super) fn router() -> Router<Store> {
    Router::new().route("/media-data", get(page))
}
