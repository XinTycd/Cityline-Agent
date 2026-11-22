use askama::Template;
use axum::{extract::State, response::Response};

use crate::{
    store::Store,
    web::{WebError, database, render},
};

use super::{TaskTableRow, decode_table_row, task_queues_sql};

#[derive(Template)]
#[template(path = "overview.html")]
struct Overview {
    heading: &'static str,
    section: &'static str,
    queues: Queues,
}

#[derive(Template)]
#[template(path = "overview-queues.html")]
struct Queues {
    total: u32,
    active_count: u32,
    ready_count: u32,
    approved_count: u32,
    exception_count: u32,
    snapshot_count: u32,
    active: Vec<TaskTableRow>,
    ready: Vec<TaskTableRow>,
    exceptions: Vec<TaskTableRow>,
}

async fn read(store: Store) -> Result<Queues, WebError> {
    database(move || {
        let mut db = store.connect()?;
        let tx = db.transaction()?;
        let sql = task_queues_sql();
        let (total, active_count, ready_count, approved_count, exception_count) = tx.query_row(
            &format!(
                "{sql} SELECT count(*), count(*) FILTER (WHERE active=1),
                 count(*) FILTER (WHERE awaiting_confirmation),
                 count(*) FILTER (WHERE state='done' AND NOT missing_report AND review='approved'),
                 count(*) FILTER (WHERE has_exception)
                 FROM tasks"
            ),
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )?;
        let snapshot_count = tx.query_row(
            "SELECT count(*) FROM media_snapshots WHERE industry='food' AND media_mask IN (1,2,3)",
            [],
            |row| row.get(0),
        )?;
        let list = |condition| {
            tx.prepare(&format!(
                "{sql} SELECT * FROM tasks WHERE {condition} ORDER BY updated_at DESC, sequence DESC LIMIT 12"
            ))?
            .query_map([], decode_table_row)?
            .collect::<rusqlite::Result<_>>()
        };
        Ok(Queues {
            total,
            active_count,
            ready_count,
            approved_count,
            exception_count,
            snapshot_count,
            active: list("active=1")?,
            ready: list("awaiting_confirmation")?,
            exceptions: list("has_exception")?,
        })
    })
    .await
}

pub(super) async fn page(State(store): State<Store>) -> Result<Response, WebError> {
    Ok(render(Overview {
        heading: "数据总览",
        section: "overview",
        queues: read(store).await?,
    }))
}

pub(super) async fn queues(State(store): State<Store>) -> Result<Response, WebError> {
    Ok(render(read(store).await?))
}
