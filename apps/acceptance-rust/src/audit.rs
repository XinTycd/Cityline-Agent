use askama::Template;
use axum::{
    Router,
    extract::{Query, State},
    response::Response,
    routing::get,
};
use rusqlite::{Connection, params};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use crate::{
    store::Store,
    web::{LIST_LIMIT, ListPages, WebError, database, render},
};

pub(crate) const CATEGORIES: [&str; 7] = [
    "任务",
    "人工审核",
    "Agent",
    "平台",
    "API 服务",
    "登录",
    "设置",
];

pub(crate) fn write(
    db: &Connection,
    category: &str,
    action: &str,
    task: Option<Uuid>,
    detail: &str,
) -> rusqlite::Result<()> {
    db.execute(
        "INSERT INTO audit_events (category, action, task_id, detail) VALUES (?1,?2,?3,?4)",
        params![category, action, task.map(|id| id.to_string()), detail],
    )?;
    Ok(())
}

pub(crate) async fn record(
    store: &Store,
    category: &'static str,
    action: &'static str,
    task: Option<Uuid>,
    detail: String,
) -> Result<(), WebError> {
    let store = store.clone();
    database(move || write(&store.connect()?, category, action, task, &detail)).await
}

#[derive(Default, Deserialize)]
struct Filters {
    category: Option<String>,
    q: Option<String>,
    task: Option<Uuid>,
    page: Option<u32>,
    page_size: Option<u32>,
}

struct Event {
    at: String,
    category: String,
    action: String,
    task: Option<String>,
    task_open: bool,
    detail: Detail,
}

struct DecisionSummary {
    role: &'static str,
    validation: &'static str,
    source: &'static str,
    attempts: u64,
    duration_ms: u64,
    reason: Option<String>,
}

impl DecisionSummary {
    fn from_value(value: &Value) -> Option<Self> {
        let role = match value.get("role")?.as_str()? {
            "strategy" => "提案策略 Agent",
            "combination" => "媒介组合 Agent",
            "verification" => "报告核验 Agent",
            _ => return None,
        };
        let attempts = value.get("attempts")?.as_u64()?;
        let validation = match value.get("validation")?.as_str()? {
            "accepted" => "决策已采纳",
            "overridden_or_partial" => "部分采纳或校正",
            "fallback" if attempts == 0 => "按本地规则执行",
            "fallback" => "规则兜底",
            _ => return None,
        };
        let source = match value.get("source")?.as_str()? {
            "model" => "模型",
            "customer" => "客户确认",
            "rules" => "本地规则",
            _ => return None,
        };
        Some(Self {
            role,
            validation,
            source,
            attempts,
            duration_ms: value.get("duration_ms")?.as_u64()?,
            reason: value
                .get("fallback_reason")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }
}

struct Detail {
    text: String,
    structured: bool,
    decision: Option<DecisionSummary>,
}

impl Detail {
    fn from_record(text: String, category: &str, action: &str) -> Self {
        if let Ok(value) = serde_json::from_str::<Value>(&text)
            && (value.is_object() || value.is_array())
            && let Ok(text) = serde_json::to_string_pretty(&value)
        {
            let decision = if category == "Agent" && action == "结构化决策" {
                DecisionSummary::from_value(&value)
            } else {
                None
            };
            Self {
                text,
                structured: true,
                decision,
            }
        } else {
            Self {
                text,
                structured: false,
                decision: None,
            }
        }
    }
}

#[derive(Template)]
#[template(path = "audit.html")]
struct AuditPage {
    heading: &'static str,
    section: &'static str,
    category: String,
    q: String,
    task: Option<Uuid>,
    categories: [&'static str; 7],
    events: Vec<Event>,
    pages: ListPages,
}

async fn page(
    State(store): State<Store>,
    Query(filters): Query<Filters>,
) -> Result<Response, WebError> {
    let category = filters
        .category
        .filter(|value| CATEGORIES.contains(&value.as_str()))
        .unwrap_or_default();
    let q: String = filters
        .q
        .unwrap_or_default()
        .trim()
        .chars()
        .take(100)
        .collect();
    let page = filters.page;
    let size = ListPages::size(filters.page_size, LIST_LIMIT as u32);
    let task = filters.task;
    let filters = (category.clone(), q.clone(), task.map(|id| id.to_string()));
    let (events, pages) = database(move || {
        // Display the current wording for historical entries and their search results.
        const MATCHED: &str = "FROM (
            SELECT id, created_at, category, task_id,
                   replace(replace(action,'取数','获取数据'),'获获取数据','获取数') AS action,
                   replace(replace(detail,'取数','获取数据'),'获获取数据','获取数') AS detail
            FROM audit_events
        ) AS audit_events WHERE (?1='' OR category=?1)
            AND (?2='' OR instr(action,?2)>0 OR instr(detail,?2)>0 OR instr(coalesce(task_id,''),?2)>0)
            AND (?3 IS NULL OR task_id=?3)";
        let mut db = store.connect()?;
        let tx = db.transaction()?;
        let total = tx.query_row(&format!("SELECT count(*) {MATCHED}"), params![filters.0, filters.1, filters.2], |row| row.get(0))?;
        let pages = ListPages::counted(page, size, total);
        let events = tx.prepare(&format!(
            "SELECT datetime(created_at,'+8 hours'), category, action, task_id, detail,
                    EXISTS (SELECT 1 FROM media_tasks WHERE id=audit_events.task_id AND deleted=0
                            AND json_extract(payload, '$.options.industry')='food')
             {MATCHED} ORDER BY id DESC LIMIT ?4 OFFSET ?5",
        ))?.query_map(params![filters.0, filters.1, filters.2, size, ListPages::offset(pages.page, size)], |row| {
            let category: String = row.get(1)?;
            let action: String = row.get(2)?;
            let detail = Detail::from_record(row.get(4)?, &category, &action);
            Ok(Event {
                at: row.get(0)?,
                category,
                action,
                task: row.get(3)?,
                task_open: row.get(5)?,
                detail,
            })
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((events, pages))
    }).await?;
    Ok(render(AuditPage {
        heading: "日志审计",
        section: "audit",
        category,
        q,
        task,
        categories: CATEGORIES,
        events,
        pages,
    }))
}

pub(crate) fn router(store: Store) -> Router {
    Router::new().route("/audit", get(page)).with_state(store)
}
