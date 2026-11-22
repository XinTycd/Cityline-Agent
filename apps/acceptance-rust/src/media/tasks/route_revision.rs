use std::collections::BTreeMap;

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

use super::{
    super::{
        Options, Snapshot, anonymize_food_case, period_months,
        plan::{
            Cost, Cover, Item, Selection, Source, bus_route_key, candidates, parse_required_routes,
        },
        read_snapshot_matching,
        schedule::Calendar,
        visuals::Strategy,
    },
    SnapshotPolicy, Task, get_task, start,
};

const UNAVAILABLE: WebError = (
    StatusCode::CONFLICT,
    "请从有必选线路要求且已锁定采购清单的待核任务发起。",
);
const MISSING_PERIODS: &str = "需先补齐所有采购段的精确档期快照，才能确认适用于整案的替换线路。";
const INVALID: WebError = (
    StatusCode::UNPROCESSABLE_ENTITY,
    "请选择待核线路和已核对的备选线路。",
);

struct Gap {
    route: String,
    periods: Vec<String>,
}

struct Phase {
    starts: String,
    ends: String,
    free: u32,
    grade: String,
    format: String,
    quote: Cost,
    stock_source: Source,
    rate_source: Source,
    coverage: Vec<Cover>,
    matches_core: bool,
}

struct Candidate {
    route: String,
    phases: Vec<Phase>,
}

impl Candidate {
    fn high_total(&self) -> u64 {
        self.phases.iter().map(|phase| phase.quote.high).sum()
    }

    fn quoted_wan(&self) -> String {
        super::super::wan(&self.high_total())
    }
}

#[derive(Default)]
struct Choices {
    notice: Option<&'static str>,
    gaps: Vec<Gap>,
    candidates: Vec<Candidate>,
}

#[derive(Template)]
#[template(path = "media-route-revision.html")]
struct Page {
    heading: &'static str,
    section: &'static str,
    task: Task,
    choices: Choices,
}

pub(super) async fn page(
    State(store): State<Store>,
    Path(id): Path<Uuid>,
) -> Result<Response, WebError> {
    let task = get_task(&store, id).await?;
    let choices = load_choices(&store, &task).await?;
    Ok(render(Page {
        heading: "必选线路复核",
        section: "tasks",
        task,
        choices,
    }))
}

#[derive(Deserialize)]
pub(super) struct Replacement {
    original: String,
    alternative: String,
    reason: String,
    attested: Option<u8>,
}

fn route_key(input: &str) -> Result<String, WebError> {
    let parsed = parse_required_routes(input)
        .map_err(|message| (StatusCode::UNPROCESSABLE_ENTITY, message))?;
    let [name] = parsed.as_slice() else {
        return Err(INVALID);
    };
    Ok(bus_route_key(name))
}

pub(super) async fn replace(
    State(store): State<Store>,
    Extension(agents): Extension<Agents>,
    Extension(refresh): Extension<Refresh>,
    Extension(actor): Extension<User>,
    Path(id): Path<Uuid>,
    Form(input): Form<Replacement>,
) -> Result<Redirect, WebError> {
    let reason = anonymize_food_case(input.reason.trim());
    if input.attested != Some(1)
        || reason.is_empty()
        || reason.chars().count() > 150
        || reason.chars().any(char::is_control)
    {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "请确认客户同意，并填写不超过 150 字的脱敏调整依据。",
        ));
    }
    let original_key = route_key(&input.original)?;
    let alternative_key = route_key(&input.alternative)?;
    let task = get_task(&store, id).await?;
    let choices = load_choices(&store, &task).await?;
    if let Some(notice) = choices.notice {
        return Err((StatusCode::CONFLICT, notice));
    }
    let original = choices
        .gaps
        .iter()
        .find(|gap| bus_route_key(&gap.route) == original_key)
        .ok_or(INVALID)?;
    let alternative = choices
        .candidates
        .iter()
        .find(|candidate| bus_route_key(&candidate.route) == alternative_key)
        .ok_or(INVALID)?;

    let mut options = task.options;
    let route = options
        .required_routes
        .iter_mut()
        .find(|route| bus_route_key(route) == original_key)
        .ok_or(INVALID)?;
    route.clone_from(&alternative.route);
    options.compare = None;
    options.source = super::Source::Local;
    let note = format!(
        "{} 已核对客户同意。必选线路 {} → {}，{} 段使用锁定库存重新核算。调整依据：{reason}",
        actor.identifier,
        original.route,
        alternative.route,
        alternative.phases.len(),
    );
    start(
        store,
        agents,
        refresh,
        actor,
        options,
        Some(id),
        SnapshotPolicy::ReplaceRoute(note),
    )
    .await
}

async fn load_choices(store: &Store, task: &Task) -> Result<Choices, WebError> {
    if !task.can_replace_route() {
        return Err(UNAVAILABLE);
    }
    let options = Options {
        lock_snapshot: true,
        ..task.options.clone()
    };
    let task = task.clone();
    if let Some(rolling) = &options.rolling {
        let calendar = Calendar::load(store, &options, rolling).await?;
        return tokio::task::spawn_blocking(move || {
            build_choices(&task, calendar.purchase_snapshots().unwrap_or_default())
        })
        .await
        .map_err(|_| UNAVAILABLE)?;
    }
    let snapshot = read_snapshot_matching(store.clone(), &options, true).await?;
    tokio::task::spawn_blocking(move || build_choices(&task, snapshot.iter().collect()))
        .await
        .map_err(|_| UNAVAILABLE)?
}

fn build_choices(task: &Task, snapshots: Vec<&Snapshot>) -> Result<Choices, WebError> {
    if snapshots.is_empty() {
        return Ok(Choices {
            notice: Some(MISSING_PERIODS),
            ..Choices::default()
        });
    }
    let portfolio = task.portfolio.as_ref().ok_or(UNAVAILABLE)?;
    let media = task.options.media.unwrap_or(Selection::Both);
    let required = &task.options.required_routes;
    let mut gaps = BTreeMap::<String, Gap>::new();
    for snapshot in &snapshots {
        let months = period_months(&snapshot.starts, &snapshot.ends).ok_or(UNAVAILABLE)?;
        let period = format!("{} 至 {}", snapshot.starts, snapshot.ends);
        for strategy in Strategy::ALL {
            let (_, proposal) = portfolio
                .proposal(strategy, snapshot, media, months, required)
                .ok_or(UNAVAILABLE)?;
            for missing in proposal
                .required_routes
                .into_iter()
                .filter(|route| !route.selected)
            {
                let gap = gaps
                    .entry(bus_route_key(&missing.name))
                    .or_insert_with(|| Gap {
                        route: missing.name,
                        periods: Vec::new(),
                    });
                if gap.periods.last() != Some(&period) {
                    gap.periods.push(period.clone());
                }
            }
        }
    }
    if gaps.is_empty() {
        return Ok(Choices::default());
    }

    let contains_required = |key: &str| required.iter().any(|name| bus_route_key(name) == key);
    let mut options = BTreeMap::<String, Candidate>::new();
    for (index, snapshot) in snapshots.iter().enumerate() {
        let months = period_months(&snapshot.starts, &snapshot.ends).ok_or(UNAVAILABLE)?;
        let mut by_route = BTreeMap::<String, Item>::new();
        for item in candidates(&snapshot.data, Selection::Bus, months, &[]).items {
            let key = bus_route_key(&item.stock.name);
            if contains_required(&key) || route_key(&item.stock.name).ok().as_ref() != Some(&key) {
                continue;
            }
            let preference = |item: &Item| {
                let matching_core = item
                    .coverage
                    .iter()
                    .any(|cover| Some(cover.circle.as_str()) == snapshot.data.core_circle());
                (!matching_core, item.total.high)
            };
            if by_route
                .get(&key)
                .is_none_or(|old| preference(&item) < preference(old))
            {
                by_route.insert(key, item);
            }
        }
        if index > 0 {
            options.retain(|key, _| by_route.contains_key(key));
        }
        for (key, item) in by_route {
            if index == 0 {
                options.insert(
                    key.clone(),
                    Candidate {
                        route: item.stock.name.clone(),
                        phases: Vec::new(),
                    },
                );
            }
            if let Some(candidate) = options.get_mut(&key) {
                candidate.phases.push(Phase {
                    starts: snapshot.starts.clone(),
                    ends: snapshot.ends.clone(),
                    free: item.stock.free,
                    grade: item.stock.grade,
                    format: item.stock.format,
                    quote: item.total,
                    stock_source: item.stock.source,
                    rate_source: item.rate.source,
                    matches_core: item
                        .coverage
                        .iter()
                        .any(|cover| Some(cover.circle.as_str()) == snapshot.data.core_circle()),
                    coverage: item.coverage,
                });
            }
        }
    }
    let mut candidates = options.into_values().collect::<Vec<_>>();
    candidates.sort_by_key(|candidate| {
        let core = candidate
            .phases
            .iter()
            .filter(|phase| phase.matches_core)
            .count();
        (std::cmp::Reverse(core), candidate.high_total())
    });
    Ok(Choices {
        notice: None,
        gaps: gaps.into_values().collect(),
        candidates,
    })
}
