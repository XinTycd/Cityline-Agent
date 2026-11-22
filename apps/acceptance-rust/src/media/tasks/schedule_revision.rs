use askama::Template;
use axum::{
    Extension, Form,
    extract::{Path, State},
    http::StatusCode,
    response::{Redirect, Response},
};
use serde::Deserialize;
use uuid::Uuid;

use crate::{
    agents::Agents,
    auth::User,
    refresh::Refresh,
    store::Store,
    web::{WebError, render},
};

use super::super::schedule::{Rolling, ScheduleRevision, Window};
use super::{SnapshotPolicy, Source, Task, get_task, select_recovery_snapshots, start};

const UNAVAILABLE: WebError = (
    StatusCode::CONFLICT,
    "请在有有效档期的正式任务结束后调整排期。",
);

fn paid_windows(
    rolling: Option<&Rolling>,
    starts: &str,
    ends: &str,
) -> Result<Vec<Window>, WebError> {
    let snapshot = Rolling::for_snapshot(starts, Uuid::nil());
    Ok(rolling
        .unwrap_or(&snapshot)
        .windows(starts, ends)
        .map_err(|notice| (StatusCode::UNPROCESSABLE_ENTITY, notice))?
        .into_iter()
        .filter(Window::needs_stock)
        .collect())
}

fn labels(windows: &[Window]) -> Vec<String> {
    windows.iter().map(Window::booking_label).collect()
}

#[derive(Template)]
#[template(path = "media-schedule-revision.html")]
struct Page<'a> {
    heading: &'static str,
    section: &'static str,
    task: &'a Task,
    starts: &'a str,
    ends: &'a str,
    paid: Vec<String>,
    trial_window: Option<&'a str>,
}

pub(super) async fn page(
    State(store): State<Store>,
    Path(id): Path<Uuid>,
) -> Result<Response, WebError> {
    let task = get_task(&store, id).await?;
    let (starts, ends) = task.schedule_dates().ok_or(UNAVAILABLE)?;
    let paid = paid_windows(task.options.rolling.as_ref(), starts, ends)?;
    Ok(render(Page {
        heading: "调整投放档期",
        section: "tasks",
        task: &task,
        starts,
        ends,
        paid: labels(&paid),
        trial_window: task
            .options
            .brief
            .as_ref()
            .and_then(|brief| brief.trial_measurement.as_ref())
            .map(|trial| trial.window.as_str()),
    }))
}

#[derive(Deserialize)]
pub(super) struct RevisionForm {
    starts: String,
    ends: String,
    factors: Option<String>,
    reason: String,
    trial_window: Option<String>,
    confirmed: Option<u8>,
    local_snapshot: Option<u8>,
}

pub(super) async fn revise(
    State(store): State<Store>,
    Extension(agents): Extension<Agents>,
    Extension(refresh): Extension<Refresh>,
    Extension(actor): Extension<User>,
    Path(id): Path<Uuid>,
    Form(form): Form<RevisionForm>,
) -> Result<Redirect, WebError> {
    let invalid = |notice| (StatusCode::UNPROCESSABLE_ENTITY, notice);
    let reason = super::super::anonymize_food_case(form.reason.trim()).into_owned();
    let factors = form.factors.as_deref().unwrap_or_default();
    if form.confirmed != Some(1)
        || reason.is_empty()
        || reason.chars().count() > 150
        || reason.chars().any(char::is_control)
        || factors.chars().count() > 600
    {
        return Err(invalid(
            "请填写调整依据并确认新档期。调整依据限 150 字，月度说明限 600 字。",
        ));
    }
    let task = get_task(&store, id).await?;
    let (starts, ends) = task.schedule_dates().ok_or(UNAVAILABLE)?;
    let previous = task.options.rolling.as_ref();
    let before = paid_windows(previous, starts, ends)?;
    let rolling = Rolling::revised(previous, factors, &form.starts, &form.ends).map_err(invalid)?;
    let after = paid_windows(Some(&rolling), &form.starts, &form.ends)?;
    if starts == form.starts && ends == form.ends && (factors.trim().is_empty() || before == after)
    {
        return Err((StatusCode::CONFLICT, "档期与调配方式没有变化。"));
    }
    let dates_of = |windows: &[Window]| {
        windows
            .iter()
            .map(|window| (window.starts.clone(), window.ends.clone()))
            .collect::<Vec<_>>()
    };
    let review_operations = dates_of(&before) != dates_of(&after);
    let revision = ScheduleRevision {
        previous_start: starts.to_owned(),
        previous_end: ends.to_owned(),
        before: labels(&before),
        after: labels(&after),
        reason,
        review_operations,
    };
    let mut options = task.options;
    options.starts = Some(form.starts);
    options.ends = Some(form.ends);
    options.batch = None;
    options.rolling = Some(rolling);
    options.compare = None;
    options.compare_priority = None;
    options.schedule_revision = Some(revision);
    if review_operations {
        options.operations = None;
    }
    if let Some(trial) = options
        .brief
        .as_mut()
        .and_then(|brief| brief.trial_measurement.as_mut())
    {
        trial
            .update_window(form.trial_window.as_deref().unwrap_or_default())
            .map_err(invalid)?;
    }
    let local = form.local_snapshot == Some(1);
    if local {
        select_recovery_snapshots(&store, &mut options).await?;
        options.source = Source::Local;
    }
    start(
        store,
        agents,
        refresh,
        actor,
        options,
        Some(id),
        SnapshotPolicy::ReviseSchedule { local },
    )
    .await
}
