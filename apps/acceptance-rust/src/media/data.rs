use askama::Template;
use axum::{
    Router,
    extract::{Query, State},
    http::StatusCode,
    response::Response,
    routing::get,
};
use rusqlite::params;
use serde::Deserialize;
use uuid::Uuid;

use super::{date_month, source_label};
use crate::{
    store::Store,
    web::{LIST_LIMIT, ListPages, WebError, database, render},
};

#[derive(Deserialize)]
struct Filter {
    starts: Option<String>,
    ends: Option<String>,
    source: Option<String>,
    page: Option<u32>,
    batch: Option<Uuid>,
    task: Option<Uuid>,
}

struct StockSnapshot {
    id: String,
    starts: String,
    ends: String,
    observed: String,
    selection: &'static str,
    media: &'static str,
    source: &'static str,
}

#[derive(Template)]
#[template(path = "media-data.html")]
struct DataPage {
    heading: &'static str,
    section: &'static str,
    starts: String,
    ends: String,
    source: String,
    batch: Option<Uuid>,
    source_task: Option<Uuid>,
    snapshots: Vec<StockSnapshot>,
    pages: ListPages,
}

async fn page(
    State(store): State<Store>,
    Query(filter): Query<Filter>,
) -> Result<Response, WebError> {
    let starts = filter.starts.unwrap_or_default();
    let ends = filter.ends.unwrap_or_default();
    let source = filter
        .source
        .filter(|value| matches!(value.as_str(), "platform" | "local"))
        .unwrap_or_default();
    if [&starts, &ends]
        .iter()
        .any(|day| !day.is_empty() && date_month(day).is_none())
        || (!ends.is_empty() && starts > ends)
    {
        return Err((StatusCode::BAD_REQUEST, "请选择 2027 年内有效的档期范围。"));
    }
    let batch = filter.batch;
    let page = if batch.is_some() {
        1
    } else {
        ListPages::number(filter.page)
    };
    let query = (starts.clone(), ends.clone(), source.clone());
    let mut snapshots = database(move || {
        let db = store.connect()?;
        db.prepare(
            "SELECT id, starts, ends, observed_at, media_mask, source
             FROM media_snapshots
             WHERE industry='food' AND media_mask IN (1,2,3)
               AND (?1='' OR starts>=?1) AND (?2='' OR ends<=?2)
               AND CASE ?3 WHEN 'platform' THEN source='platform'
                           WHEN 'local' THEN source IN ('manual','legacy') ELSE 1 END
               AND (?6 IS NULL OR id=?6)
             ORDER BY observed_at DESC, rowid DESC LIMIT ?4 OFFSET ?5",
        )?
        .query_map(
            params![
                query.0,
                query.1,
                query.2,
                LIST_LIMIT as i64 + 1,
                ListPages::offset(page, LIST_LIMIT as u32),
                batch.map(|id| id.to_string()),
            ],
            |row| {
                let (selection, media) = match row.get::<_, u8>(4)? {
                    1 => ("bus", "公交车身"),
                    2 => ("metro", "地铁出入口"),
                    3 => ("both", "公交＋地铁"),
                    _ => return Err(rusqlite::Error::InvalidQuery),
                };
                Ok(StockSnapshot {
                    id: row.get(0)?,
                    starts: row.get(1)?,
                    ends: row.get(2)?,
                    observed: row.get(3)?,
                    selection,
                    media,
                    source: source_label(&row.get::<_, String>(5)?),
                })
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()
    })
    .await?;
    if batch.is_some() && snapshots.is_empty() {
        return Err((StatusCode::NOT_FOUND, "未找到该档期快照。"));
    }
    let pages = ListPages::new(page, &mut snapshots);
    Ok(render(DataPage {
        heading: "数据中心",
        section: "media-data",
        starts,
        ends,
        source,
        batch,
        source_task: filter.task.filter(|_| batch.is_some()),
        snapshots,
        pages,
    }))
}

pub(super) fn router() -> Router<Store> {
    Router::new().route("/media-data", get(page))
}
