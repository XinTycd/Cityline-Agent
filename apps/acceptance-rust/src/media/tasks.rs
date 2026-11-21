use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

use askama::Template;
use axum::{
    Extension, Form, Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{StatusCode, header},
    response::{Redirect, Response},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{
    Dataset, Industry, Options, ResultView, change_set,
    collaboration::{
        Candidate, Checks, Gate, Reason, Recommendation, recommend, review_queue, summarize,
    },
    export_response, load_plan, normalize_options, optimize_snapshot, optimized_snapshot_variants,
    optimizer::{Policy, Portfolio},
    period_months,
    plan::{Priority, Selection},
    render_export, visuals,
};
use crate::{
    agents::{Agents, Collaboration},
    audit,
    auth::User,
    model::{CallError, Model},
    refresh::{BrowserRequest, BrowserWindow, Refresh},
    settings::Brand,
    store::Store,
    web::{WebError, database, render},
};

const NOT_FOUND: WebError = (StatusCode::NOT_FOUND, "找不到这项提案任务。");
const EVIDENCE_ONLY: &str = "已沿用原 A／B 采购清单和报价，本次复核补充的线路与触达证据。";
const BUSY: WebError = (
    StatusCode::TOO_MANY_REQUESTS,
    "提案任务较多，请等一项完成后再提交。",
);
const MAX_ACTIVE_TASKS: i64 = 4;
const INTAKE: usize = 0;
const DATA: usize = 1;
const STRATEGY: usize = 2;
const MIX: usize = 3;
const REPORT: usize = 4;
const LABELS: [&str; 5] = ["需求接收", "数据核验", "提案策略", "组合核算", "报告交付"];
const TASK_NAME_PREFIX: &str = "饮食食品";
const TASK_COLUMNS: &str = "payload, active, review, review_note,
    (SELECT datetime(min(created_at), '+8 hours') FROM audit_events
     WHERE task_id=media_tasks.id AND category='任务' AND action='创建') AS created_at";
const TASK_TABLE_COLUMNS: &str =
    "rowid AS sequence, datetime(updated_at, '+8 hours') AS updated_at";

mod evidence_review;

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum TaskState {
    Active,
    Done,
    Review,
    Failed,
}

impl TaskState {
    fn active(self) -> bool {
        matches!(self, Self::Active)
    }
    fn done(self) -> bool {
        matches!(self, Self::Done)
    }
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum StepState {
    Pending,
    Running,
    Done,
    Fallback,
    Review,
    Failed,
}

impl StepState {
    fn label(self) -> &'static str {
        match self {
            Self::Pending => "待开始",
            Self::Running => "执行中",
            Self::Done => "已完成",
            Self::Fallback => "规则兜底",
            Self::Review => "待核对",
            Self::Failed => "未完成",
        }
    }

    fn kind(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::Fallback => "fallback",
            Self::Review => "review",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct Step {
    state: StepState,
    notes: Vec<String>,
}

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Task {
    pub(super) id: Uuid,
    options: Options,
    #[serde(default)]
    previous_task: Option<Uuid>,
    state: TaskState,
    steps: [Step; 5],
    agents: usize,
    #[serde(default)]
    agent_decisions: Vec<Value>,
    #[serde(default)]
    priority: Option<Priority>,
    #[serde(default)]
    portfolio: Option<Portfolio>,
    #[serde(default)]
    agent_error: bool,
    #[serde(default)]
    has_alternative: bool,
    #[serde(default)]
    login: PlatformLogin,
    #[serde(skip)]
    review: ReviewState,
    #[serde(skip)]
    review_note: String,
    #[serde(skip)]
    created_at: Option<String>,
}

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Source {
    #[default]
    Local,
    Platform,
}

impl Source {
    pub(super) fn platform(self) -> bool {
        matches!(self, Self::Platform)
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct PlatformLogin {
    newcloud: bool,
    redbean: bool,
    phase: String,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReviewState {
    #[default]
    Pending,
    Approved,
    Returned,
}

impl ReviewState {
    fn pending(self) -> bool {
        matches!(self, Self::Pending)
    }

    fn parse(value: &str) -> Self {
        match value {
            "approved" => Self::Approved,
            "returned" => Self::Returned,
            _ => Self::Pending,
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Returned => "returned",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Pending => "待人工确认",
            Self::Approved => "已确认",
            Self::Returned => "已退回",
        }
    }
}

impl Task {
    fn new(options: Options, previous_task: Option<Uuid>) -> Self {
        let mut task = Self {
            id: Uuid::new_v4(),
            options,
            previous_task,
            state: TaskState::Active,
            steps: std::array::from_fn(|_| Step {
                state: StepState::Pending,
                notes: Vec::new(),
            }),
            agents: 0,
            agent_decisions: Vec::new(),
            priority: None,
            portfolio: None,
            agent_error: false,
            has_alternative: false,
            login: PlatformLogin::default(),
            review: ReviewState::Pending,
            review_note: String::new(),
            created_at: None,
        };
        task.note(INTAKE, StepState::Done, "任务已创建，投放条件已确认。");
        if !task.options.target_centers.is_empty() {
            task.note(
                INTAKE,
                StepState::Done,
                format!(
                    "已确认查询中心 · {} · 半径 {} 米。",
                    task.options.target_centers.join("、"),
                    super::FOOD_COVERAGE_RADIUS
                ),
            );
        }
        if !task.options.required_routes.is_empty() {
            task.note(
                INTAKE,
                StepState::Done,
                format!("客户必选线路 · {}", task.options.required_routes.join("、")),
            );
        }
        if let Some(cap) = task.options.hard_cap {
            task.note(
                INTAKE,
                StepState::Done,
                format!("客户明确的预算硬上限 · ¥{cap}。"),
            );
        }
        if let Some(operations) = &task.options.operations {
            task.note(
                INTAKE,
                StepState::Done,
                format!("施工条件核对依据 · {}。", operations.source),
            );
        }
        task
    }

    fn note(&mut self, index: usize, state: StepState, message: impl Into<String>) {
        let step = &mut self.steps[index];
        step.state = state;
        step.notes.push(message.into());
    }

    fn current_step(&self) -> usize {
        self.steps
            .iter()
            .position(|step| matches!(step.state, StepState::Running))
            .or_else(|| {
                self.steps
                    .iter()
                    .rposition(|step| !matches!(step.state, StepState::Pending))
            })
            .unwrap_or(INTAKE)
    }

    fn fail(&mut self, message: &str) {
        let active = self
            .steps
            .iter()
            .position(|step| matches!(step.state, StepState::Running))
            .or_else(|| {
                self.steps
                    .iter()
                    .position(|step| matches!(step.state, StepState::Pending))
            })
            .unwrap_or(REPORT);
        self.note(active, StepState::Failed, message);
        self.state = TaskState::Failed;
    }

    pub(super) fn status(&self) -> &'static str {
        match self.state {
            TaskState::Active => "正在生成",
            TaskState::Done if self.agents == 3 => "已生成 · 三个 Agent 已协作",
            TaskState::Done if self.agents > 0 => "已生成 · Agent 与本地规则协作",
            TaskState::Done => "已生成 · 本地规则",
            TaskState::Review => "需要核对数据",
            TaskState::Failed => "生成未完成",
        }
    }

    pub(super) fn summary(&self) -> String {
        let time = self
            .options
            .starts
            .as_deref()
            .map(|starts| {
                format!(
                    "{starts} 至 {}",
                    self.options.ends.as_deref().unwrap_or("待确认")
                )
            })
            .unwrap_or_else(|| "使用最近核对档期".into());
        let source = if self.options.demo == Some(1) {
            "合成样例"
        } else if self.options.proof_only {
            "锁定库存 · 新云取证"
        } else if self.options.source.platform() {
            "平台取数"
        } else {
            "本地快照"
        };
        format!(
            "{} · {} 万元 · {} · {time} · {source}",
            self.options.industry.brand(),
            self.budget(),
            self.options.media.unwrap_or(Selection::Both).label(),
        )
    }

    pub(super) fn name(&self) -> String {
        let mut name = format!("{TASK_NAME_PREFIX} {}万", self.budget());
        if let Some(date) = self.created_at.as_deref().and_then(|time| time.get(2..10)) {
            name.push(' ');
            name.push_str(&date.replace('-', ""));
        }
        name
    }

    fn list_status(&self) -> &'static str {
        match self.state {
            TaskState::Active => "任务中",
            TaskState::Done if matches!(self.review, ReviewState::Approved) => "已完成",
            TaskState::Done if matches!(self.review, ReviewState::Returned) => "已退回",
            TaskState::Done => "待审核",
            TaskState::Review => "待核对",
            TaskState::Failed => "失败",
        }
    }

    fn budget(&self) -> u32 {
        self.options.budget.unwrap_or(super::SHOWCASE_BUDGET)
    }
    fn fixed_showcase_budget(&self) -> bool {
        self.options.fixed_showcase
    }
    fn needs_platform_login(&self) -> bool {
        self.options.proof_only
            || (self.options.demo != Some(1)
                && self.options.source.platform()
                && self.options.batch.is_none()
                && !self.options.lock_snapshot
                && self
                    .options
                    .rolling
                    .as_ref()
                    .is_none_or(|rolling| rolling.batches.is_empty()))
    }
    fn has_pinned_data(&self) -> bool {
        self.options.batch.is_some()
            || self
                .options
                .rolling
                .as_ref()
                .is_some_and(|rolling| !rolling.batches.is_empty())
    }

    fn can_evaluate_budget(&self) -> bool {
        self.options.demo != Some(1) && !self.state.active() && self.has_pinned_data()
    }

    fn can_use_local_snapshot(&self) -> bool {
        self.options.demo != Some(1)
            && !self.state.active()
            && (matches!(self.state, TaskState::Review | TaskState::Failed)
                || matches!(self.review, ReviewState::Returned))
            && self
                .options
                .starts
                .as_deref()
                .zip(self.options.ends.as_deref())
                .is_some_and(|(starts, ends)| period_months(starts, ends).is_some())
    }

    fn can_reuse_portfolio(&self) -> bool {
        self.can_use_local_snapshot() && self.portfolio.is_some() && self.has_pinned_data()
    }

    fn can_refresh_evidence(&self) -> bool {
        self.can_evaluate_budget() && self.portfolio.is_some()
    }

    fn can_review_operations(&self) -> bool {
        self.can_evaluate_budget() && self.options.rolling.is_some() && self.portfolio.is_some()
    }

    fn recovery_snapshots(&self) -> Vec<RecoverySnapshot> {
        if !self.can_use_local_snapshot() {
            return Vec::new();
        }
        let Some((starts, ends)) = self.options.starts.as_ref().zip(self.options.ends.as_ref())
        else {
            return Vec::new();
        };
        if let Some(rolling) = &self.options.rolling {
            return rolling
                .windows(starts, ends)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|window| {
                    Some(RecoverySnapshot {
                        id: *rolling.batches.get(&window.starts)?,
                        starts: window.starts,
                        ends: window.ends,
                    })
                })
                .collect();
        }
        self.options
            .batch
            .into_iter()
            .map(|id| RecoverySnapshot {
                id,
                starts: starts.clone(),
                ends: ends.clone(),
            })
            .collect()
    }
}

#[derive(Serialize)]
struct RecoverySnapshot {
    id: Uuid,
    starts: String,
    ends: String,
}

#[derive(Template)]
#[template(path = "media-task.html")]
struct Detail {
    heading: String,
    section: &'static str,
    task: Task,
    labels: [&'static str; 5],
    budgets: [u32; 3],
    recovery_snapshots: Vec<RecoverySnapshot>,
    can_read_audit: bool,
    operations_open: bool,
}

#[derive(Deserialize)]
struct DetailOptions {
    review: Option<String>,
}

fn decode(row: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    let json: String = row.get(0)?;
    let active: bool = row.get(1)?;
    let mut task: Task = serde_json::from_str(&json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })?;
    for note in &mut task.steps[DATA].notes {
        if note.contains("比赛平台") {
            *note = note.replace("比赛平台", "数据平台");
        }
    }
    task.review = ReviewState::parse(&row.get::<_, String>(2)?);
    task.review_note = row.get(3)?;
    task.created_at = row.get(4)?;
    if !active && task.state.active() {
        task.fail("工作台曾重启，任务已中断，可以重新发起。");
    }
    Ok(task)
}

fn encode(task: &Task) -> rusqlite::Result<String> {
    serde_json::to_string(task)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
}

async fn get_task(store: &Store, id: Uuid) -> Result<Task, WebError> {
    let store = store.clone();
    database(move || {
        store
            .connect()?
            .query_row(
                &format!("SELECT {TASK_COLUMNS} FROM media_tasks WHERE id=?1 AND deleted=0 AND json_extract(payload, '$.options.industry')='food'"),
                [id.to_string()],
                decode,
            )
            .optional()
    })
    .await?
    .ok_or(NOT_FOUND)
}

async fn save(store: &Store, task: &Task, report: Option<String>) -> Result<(), WebError> {
    let store = store.clone();
    let task = task.clone();
    database(move || {
        let mut db = store.connect()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous = tx.query_row(
            &format!(
                "SELECT {TASK_COLUMNS} FROM media_tasks WHERE id=?1 AND active=1 AND deleted=0"
            ),
            [task.id.to_string()],
            decode,
        )?;
        tx.execute(
            "UPDATE media_tasks SET active=?1, payload=?2, report=?3 WHERE id=?4",
            params![
                task.state.active(),
                encode(&task)?,
                report,
                task.id.to_string()
            ],
        )?;
        for (index, (step, old)) in task.steps.iter().zip(&previous.steps).enumerate() {
            for note in step.notes.iter().skip(old.notes.len()) {
                let category = if index >= STRATEGY && !note.contains("HTML 已渲染") {
                    "Agent"
                } else {
                    "任务"
                };
                let action = if note.contains("已建立 Agent 通信") {
                    "模型连接成功"
                } else {
                    LABELS[index]
                };
                audit::write(&tx, category, action, Some(task.id), note)?;
            }
        }
        for decision in task
            .agent_decisions
            .iter()
            .skip(previous.agent_decisions.len())
        {
            audit::write(
                &tx,
                "Agent",
                "结构化决策",
                Some(task.id),
                &decision.to_string(),
            )?;
        }
        if !task.state.active() {
            audit::write(&tx, "任务", "状态变更", Some(task.id), task.status())?;
        }
        tx.commit()
    })
    .await
}

enum SnapshotPolicy {
    Fresh,
    Locked(Option<Portfolio>),
    RefreshEvidence(Portfolio),
    ReviewOperations(Portfolio),
}

async fn start(
    store: Store,
    agents: Agents,
    refresh: Refresh,
    actor: User,
    options: Options,
    previous_task: Option<Uuid>,
    snapshot_policy: SnapshotPolicy,
) -> Result<Redirect, WebError> {
    let mut options = normalize_options(options)?;
    options.lock_snapshot = !matches!(snapshot_policy, SnapshotPolicy::Fresh);
    options.proof_only = matches!(snapshot_policy, SnapshotPolicy::RefreshEvidence(_));
    options.operations_required = options.requires_operations();
    if options.demo != Some(1) {
        if !options
            .starts
            .as_deref()
            .zip(options.ends.as_deref())
            .is_some_and(|(starts, ends)| period_months(starts, ends).is_some())
        {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                "平台取数需要指定 2027 年内的有效起止日期。",
            ));
        }
        if !options.lock_snapshot {
            options.source = Source::Platform;
            options.batch = None;
            if let Some(rolling) = &mut options.rolling {
                rolling.batches.clear();
            }
        }
    }
    if let Some(rolling) = &options.rolling {
        rolling
            .windows(
                options.starts.as_deref().unwrap_or_default(),
                options.ends.as_deref().unwrap_or_default(),
            )
            .map_err(|notice| (StatusCode::UNPROCESSABLE_ENTITY, notice))?;
    }
    let mut task = Task::new(options, previous_task);
    let revision = match snapshot_policy {
        SnapshotPolicy::Locked(Some(portfolio)) => Some((portfolio, "补证复核", EVIDENCE_ONLY)),
        SnapshotPolicy::RefreshEvidence(portfolio) => Some((
            portfolio,
            "按采购清单重新取证",
            "已锁定现有 A／B 清单、红豆库存和报价。本次只查询新云证据。",
        )),
        SnapshotPolicy::ReviewOperations(portfolio) => Some((
            portfolio,
            "施工复核",
            "已沿用原 A／B 采购清单和报价，本次复核施工窗口与产能。",
        )),
        _ => None,
    }
    .map(|(portfolio, action, note)| {
        task.portfolio = Some(portfolio);
        task.note(INTAKE, StepState::Done, note);
        (action, note)
    });
    let id = task.id;
    let record = task.clone();
    let db_store = store.clone();
    let created = database(move || {
        let mut db = db_store.connect()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let count: i64 = tx.query_row(
            "SELECT count(*) FROM media_tasks WHERE active=1",
            [],
            |row| row.get(0),
        )?;
        if count >= MAX_ACTIVE_TASKS {
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO media_tasks (id, active, payload, updated_at) VALUES (?1, 1, ?2, CURRENT_TIMESTAMP)",
            params![id.to_string(), encode(&record)?],
        )?;
        audit::write(&tx, "任务", "创建", Some(id), &record.summary())?;
        if let Some(previous) = record.previous_task {
            audit::write(
                &tx,
                "任务",
                "重新生成",
                Some(id),
                &format!("原任务 {previous}"),
            )?;
        }
        if let Some((action, note)) = revision {
            audit::write(&tx, "任务", action, Some(id), note)?;
        }
        tx.commit()?;
        Ok(true)
    })
    .await?;
    if !created {
        return Err(BUSY);
    }

    let (model, collaboration) = agents.current();
    tokio::spawn(async move {
        if execute(
            &store,
            model.as_deref(),
            collaboration,
            refresh,
            &actor.identifier,
            &mut task,
        )
        .await
        .is_err()
        {
            task.fail("生成过程暂时不可用，请重试。");
            let _ = save(&store, &task, None).await;
        }
    });
    Ok(Redirect::to(&format!("/media-proposals/tasks/{id}")))
}

async fn run(
    State(store): State<Store>,
    Extension(agents): Extension<Agents>,
    Extension(refresh): Extension<Refresh>,
    Extension(actor): Extension<User>,
    Form(options): Form<Options>,
) -> Result<Redirect, WebError> {
    start(
        store,
        agents,
        refresh,
        actor,
        options,
        None,
        SnapshotPolicy::Fresh,
    )
    .await
}

#[derive(Deserialize)]
struct TaskInputs {
    industry: String,
    budget: String,
    month: Option<String>,
    starts: Option<String>,
    ends: Option<String>,
    source: Option<String>,
    bus: Option<u8>,
    metro: Option<u8>,
    brief_used: Option<u8>,
    brief_confirmed: Option<u8>,
    brief_customer_profile: Option<String>,
    brief_objective: Option<String>,
    brief_campaign_goal: Option<String>,
    brief_audience: Option<String>,
    brief_region: Option<String>,
    brief_monitoring: Option<String>,
    target_centers: Option<String>,
    required_routes: Option<String>,
    scheduling_factors: Option<String>,
    #[serde(flatten)]
    operations: OperationsForm,
    hard_cap_wan: Option<String>,
}

#[derive(Deserialize)]
struct OperationsForm {
    ops_source: Option<String>,
    ops_hours: Option<String>,
    ops_bus: Option<String>,
    ops_metro: Option<String>,
    ops_parallel: Option<String>,
    ops_buffer: Option<String>,
    ops_windows: Option<String>,
    ops_confirmed: Option<String>,
}

impl OperationsForm {
    fn parse(
        &self,
        rolling: &super::schedule::Rolling,
        starts: &str,
        ends: &str,
    ) -> Result<Option<super::operations::Operations>, WebError> {
        let invalid = |message| (StatusCode::UNPROCESSABLE_ENTITY, message);
        let periods = rolling.windows(starts, ends).map_err(invalid)?;
        super::operations::Operations::parse(
            super::operations::Draft {
                source: self.ops_source.as_deref().unwrap_or_default(),
                hours: self.ops_hours.as_deref().unwrap_or_default(),
                buses_per_night: self.ops_bus.as_deref().unwrap_or_default(),
                exits_per_night: self.ops_metro.as_deref().unwrap_or_default(),
                parallel: self.ops_parallel.as_deref() == Some("1"),
                buffer: self.ops_buffer.as_deref().unwrap_or_default(),
                windows: self.ops_windows.as_deref().unwrap_or_default(),
                attested: self.ops_confirmed.as_deref() == Some("1"),
            },
            &periods,
        )
        .map_err(invalid)
    }
}

async fn create(
    State(store): State<Store>,
    Extension(agents): Extension<Agents>,
    Extension(refresh): Extension<Refresh>,
    Extension(actor): Extension<User>,
    Form(input): Form<TaskInputs>,
) -> Result<Redirect, WebError> {
    let invalid = |notice| (StatusCode::UNPROCESSABLE_ENTITY, notice);
    let industry = Industry::from_text(&input.industry)
        .filter(|industry| *industry == Industry::Food)
        .ok_or(super::FOOD_ONLY)?;
    let budget: u32 = input
        .budget
        .trim()
        .trim_end_matches("万元")
        .trim_end_matches('万')
        .trim()
        .parse()
        .map_err(|_| invalid("展示提案固定按约 200 万元核算。"))?;
    if budget != super::SHOWCASE_BUDGET {
        return Err(invalid("展示提案固定按约 200 万元核算。"));
    }
    if input.bus != Some(1) || input.metro != Some(1) {
        return Err(invalid("正式提案同时使用公交车身和地铁出入口。"));
    }
    if input
        .source
        .is_some_and(|source| source.trim() != "平台数据")
    {
        return Err(invalid("新提案统一从平台取数。"));
    }
    let required_routes =
        super::plan::parse_required_routes(input.required_routes.as_deref().unwrap_or_default())
            .map_err(invalid)?;
    let hard_cap = input
        .hard_cap_wan
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse::<u64>()
                .ok()
                .and_then(|amount| amount.checked_mul(10_000))
                .filter(|amount| (1..=super::MAX_EXPLICIT_CAP).contains(amount))
                .ok_or(invalid("客户最高承受金额须为正整数万元。"))
        })
        .transpose()?;
    let dates = match (
        input.starts.as_deref(),
        input.ends.as_deref(),
        input.month.as_deref(),
    ) {
        (Some(starts), Some(ends), None) if period_months(starts, ends).is_some() => {
            Some((starts.to_owned(), ends.to_owned()))
        }
        (None, None, Some(month)) => super::proposal_dates(month),
        (None, None, None) => Some((super::SHOWCASE_START.into(), super::SHOWCASE_END.into())),
        _ => None,
    };
    let (starts, ends) = dates.ok_or(invalid(
        "请选择 2027 年内的起止日期，结束日期不得早于开始日期。",
    ))?;
    let rolling = super::schedule::Rolling::showcase(
        input.scheduling_factors.as_deref().unwrap_or_default(),
        &starts,
        &ends,
    )
    .map_err(invalid)?;
    let operations = input.operations.parse(&rolling, &starts, &ends)?;
    let mut brief = super::brief::BriefContext {
        customer_profile: input.brief_customer_profile.unwrap_or_default(),
        objective: input.brief_objective.unwrap_or_default(),
        campaign_goal: input.brief_campaign_goal.unwrap_or_default(),
        audience: input.brief_audience.unwrap_or_default(),
        region: input.brief_region.unwrap_or_default(),
        monitoring: input.brief_monitoring.unwrap_or_default(),
    };
    if !brief.normalize() {
        return Err(invalid("Brief 补充说明每项最多 150 个字符。"));
    }
    if !brief.campaign_goal.is_empty()
        && super::brief::campaign_goal(&brief.campaign_goal).is_none()
    {
        return Err(invalid("请选择到店、认知、促销或拉新作为主目标。"));
    }
    let target_centers = parse_target_centers(input.target_centers.as_deref().unwrap_or_default())
        .map_err(invalid)?;
    if (input.brief_used == Some(1) || !brief.is_empty() || !target_centers.is_empty())
        && input.brief_confirmed != Some(1)
    {
        return Err(invalid("请确认客户假设书、目标圈与投放条件。"));
    }
    start(
        store,
        agents,
        refresh,
        actor,
        Options {
            industry,
            budget: Some(budget),
            fixed_showcase: true,
            starts: Some(starts),
            ends: Some(ends),
            media: Some(Selection::Both),
            required_routes,
            hard_cap,
            rolling: Some(rolling),
            brief: (!brief.is_empty()).then_some(brief),
            target_centers,
            operations,
            operations_required: true,
            ..Options::default()
        },
        None,
        SnapshotPolicy::Fresh,
    )
    .await
}

fn parse_target_centers(raw: &str) -> Result<Vec<String>, &'static str> {
    let centers = raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if centers.len() > 8
        || centers
            .iter()
            .map(|name| name.chars().count())
            .sum::<usize>()
            + centers.len().saturating_sub(1)
            > 260
    {
        return Err("最多填写 8 个目标圈，合计不超过 260 个字符。");
    }
    let mut keys = std::collections::BTreeSet::new();
    if centers.iter().any(|name| {
        !name.starts_with("上海市")
            || !(4..=80).contains(&name.chars().count())
            || name
                .chars()
                .any(|ch| ch.is_control() || matches!(ch, '|' | '<' | '>'))
            || !keys.insert(super::plan::key(name))
    }) {
        return Err("请每行填写一个以「上海市」开头的完整地标或地址，目标圈不能重复。");
    }
    Ok(centers.into_iter().map(str::to_owned).collect())
}

async fn review_operations(
    State(store): State<Store>,
    Extension(agents): Extension<Agents>,
    Extension(refresh): Extension<Refresh>,
    Extension(actor): Extension<User>,
    Path(id): Path<Uuid>,
    Form(input): Form<OperationsForm>,
) -> Result<Redirect, WebError> {
    const UNAVAILABLE: WebError = (StatusCode::CONFLICT, "请先生成已锁定采购清单的提案。");
    let task = get_task(&store, id).await?;
    if !task.can_review_operations() {
        return Err(UNAVAILABLE);
    }
    let portfolio = task.portfolio.ok_or(UNAVAILABLE)?;
    let mut options = task.options;
    let rolling = options.rolling.as_ref().ok_or(UNAVAILABLE)?;
    let starts = options.starts.as_deref().ok_or(UNAVAILABLE)?;
    let ends = options.ends.as_deref().ok_or(UNAVAILABLE)?;
    options.operations = Some(
        input
            .parse(rolling, starts, ends)?
            .ok_or((StatusCode::UNPROCESSABLE_ENTITY, "请填写已确认的施工条件。"))?,
    );
    options.operations_required = true;
    options.source = Source::Local;
    options.compare_priority = task.priority;
    start(
        store,
        agents,
        refresh,
        actor,
        options,
        Some(id),
        SnapshotPolicy::ReviewOperations(portfolio),
    )
    .await
}

async fn refresh_evidence(
    State(store): State<Store>,
    Extension(agents): Extension<Agents>,
    Extension(refresh): Extension<Refresh>,
    Extension(actor): Extension<User>,
    Path(id): Path<Uuid>,
) -> Result<Redirect, WebError> {
    const UNAVAILABLE: WebError = (StatusCode::CONFLICT, "请先生成已锁定采购清单的提案。");
    let task = get_task(&store, id).await?;
    if !task.can_refresh_evidence() {
        return Err(UNAVAILABLE);
    }
    let portfolio = task.portfolio.ok_or(UNAVAILABLE)?;
    let mut options = task.options;
    options.source = Source::Local;
    options.compare_priority = task.priority;
    start(
        store,
        agents,
        refresh,
        actor,
        options,
        Some(id),
        SnapshotPolicy::RefreshEvidence(portfolio),
    )
    .await
}

#[derive(Deserialize)]
struct Rerun {
    budget: u32,
    local_snapshot: Option<u8>,
    same_inventory: Option<u8>,
    evaluation: Option<u8>,
}

async fn rerun(
    State(store): State<Store>,
    Extension(agents): Extension<Agents>,
    Extension(refresh): Extension<Refresh>,
    Extension(actor): Extension<User>,
    Path(id): Path<Uuid>,
    Form(form): Form<Rerun>,
) -> Result<Redirect, WebError> {
    let task = get_task(&store, id).await?;
    if task.state.active() {
        return Err((StatusCode::CONFLICT, "请等待当前任务结束。"));
    }
    let evaluation = form.evaluation == Some(1);
    let same_inventory = form.same_inventory == Some(1);
    let local_snapshot = same_inventory || form.local_snapshot == Some(1);
    if evaluation && (!task.can_evaluate_budget() || !super::allowed_budget(form.budget)) {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "评审重算需要已完成取数的原提案和赛题预算档。",
        ));
    }
    if task.fixed_showcase_budget() && form.budget != super::SHOWCASE_BUDGET && !evaluation {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "展示提案固定按约 200 万元核算。",
        ));
    }
    let old_budget = task.budget();
    if local_snapshot && (evaluation || !task.can_use_local_snapshot() || form.budget != old_budget)
    {
        return Err((
            StatusCode::CONFLICT,
            "请沿用待核、失败或退回任务的预算和档期使用备用快照。",
        ));
    }
    if same_inventory && !task.can_reuse_portfolio() {
        return Err((
            StatusCode::CONFLICT,
            "当前任务没有可沿用的采购清单，请按同档期快照生成。",
        ));
    }
    let mut options = task.options;
    if evaluation {
        options.fixed_showcase = false;
    }
    let snapshot_policy = if local_snapshot {
        let previous = snapshot_pins(&options);
        if same_inventory {
            select_evidence_snapshots(&store, &mut options, &previous).await?;
        } else {
            select_recovery_snapshots(&store, &mut options).await?;
        }
        options.source = Source::Local;
        let preserved = if let Some(portfolio) = task.portfolio {
            evidence_replacements(&store, previous, snapshot_pins(&options))
                .await?
                .and_then(|replacements| portfolio.with_evidence(&replacements))
        } else {
            None
        };
        if preserved.is_some() {
            options.compare_priority = task.priority;
        } else if same_inventory {
            return Err((
                StatusCode::CONFLICT,
                "补证版本的库存和原采购清单不匹配，请核对快照。",
            ));
        }
        SnapshotPolicy::Locked(preserved)
    } else if evaluation {
        SnapshotPolicy::Locked(None)
    } else {
        SnapshotPolicy::Fresh
    };
    if form.budget != old_budget || evaluation {
        options.compare = (form.budget != old_budget).then_some(old_budget);
        options.compare_priority = task.priority;
    }
    options.adjusted_budget = true;
    options.budget = Some(form.budget);
    start(
        store,
        agents,
        refresh,
        actor,
        options,
        Some(id),
        snapshot_policy,
    )
    .await
}

fn snapshot_pins(options: &Options) -> BTreeMap<String, Uuid> {
    if let Some(rolling) = &options.rolling {
        return rolling.batches.clone();
    }
    options
        .starts
        .clone()
        .zip(options.batch)
        .into_iter()
        .collect()
}

async fn select_evidence_snapshots(
    store: &Store,
    options: &mut Options,
    previous: &BTreeMap<String, Uuid>,
) -> Result<(), WebError> {
    let store = store.clone();
    let pins = previous.clone();
    let selected = database(move || {
        let db = store.connect()?;
        let mut select = db.prepare(
            "SELECT coalesce((SELECT latest.id FROM media_snapshots AS latest
                              WHERE latest.evidence_root=coalesce(pinned.evidence_root,pinned.id)
                              ORDER BY latest.rowid DESC LIMIT 1),pinned.id)
             FROM media_snapshots AS pinned WHERE pinned.id=?1 AND pinned.industry='food'",
        )?;
        let mut selected = BTreeMap::new();
        for (starts, id) in pins {
            let latest: Option<String> = select
                .query_row([id.to_string()], |row| row.get(0))
                .optional()?;
            if let Some(latest) = latest.and_then(|id| id.parse().ok()) {
                selected.insert(starts, latest);
            }
        }
        Ok(selected)
    })
    .await?;
    if !selected.keys().eq(previous.keys()) || selected == *previous {
        return Err((
            StatusCode::CONFLICT,
            "原库存未找到新的补证版本，请先补充证据。",
        ));
    }
    if let Some(rolling) = &mut options.rolling {
        rolling.batches = selected;
        options.batch = None;
    } else {
        options.batch = selected.into_values().next();
    }
    Ok(())
}

async fn evidence_replacements(
    store: &Store,
    before: BTreeMap<String, Uuid>,
    after: BTreeMap<String, Uuid>,
) -> Result<Option<BTreeMap<String, (Uuid, Uuid)>>, WebError> {
    if before.is_empty() || !before.keys().eq(after.keys()) {
        return Ok(None);
    }
    let store = store.clone();
    database(move || {
        let db = store.connect()?;
        let mut related = db.prepare(
            "SELECT json_remove(a.payload, '$.evidence'), json_remove(b.payload, '$.evidence')
             FROM media_snapshots AS a JOIN media_snapshots AS b ON b.id=?2
             WHERE a.id=?1 AND a.industry=b.industry AND a.starts=b.starts AND a.ends=b.ends
               AND a.media_mask=b.media_mask AND b.evidence_root=coalesce(a.evidence_root,a.id)",
        )?;
        let dataset = |payload: &str| {
            serde_json::from_str::<Dataset>(payload)
                .and_then(serde_json::to_value)
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
        };
        let mut replacements = BTreeMap::new();
        for ((starts, before), after) in before.into_iter().zip(after.into_values()) {
            if before != after {
                let pair = related
                    .query_row(params![before.to_string(), after.to_string()], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })
                    .optional()?;
                let Some((old, new)) = pair else {
                    return Ok(None);
                };
                // Allocation quantities are indexed by candidate order, which depends on the input arrays.
                if old != new && dataset(&old)? != dataset(&new)? {
                    return Ok(None);
                }
            }
            replacements.insert(starts, (before, after));
        }
        Ok(Some(replacements))
    })
    .await
}

async fn comparison_portfolio(store: &Store, task: &Task) -> Result<Option<Portfolio>, WebError> {
    let Some(budget) = task.options.compare.filter(|_| task.options.lock_snapshot) else {
        return Ok(None);
    };
    let mut parent = task.previous_task;
    while let Some(id) = parent {
        let prior = match get_task(store, id).await {
            Ok(prior) => prior,
            Err(error) if error == NOT_FOUND => return Ok(None),
            Err(error) => return Err(error),
        };
        if prior.budget() == budget {
            let Some(portfolio) = prior.portfolio else {
                return Ok(None);
            };
            let replacements = evidence_replacements(
                store,
                snapshot_pins(&prior.options),
                snapshot_pins(&task.options),
            )
            .await?;
            return Ok(replacements.and_then(|replacements| portfolio.with_evidence(&replacements)));
        }
        if prior.budget() != task.budget() || prior.options.compare != Some(budget) {
            break;
        }
        parent = prior.previous_task;
    }
    Ok(None)
}

async fn select_recovery_snapshots(store: &Store, options: &mut Options) -> Result<(), WebError> {
    const MISSING: WebError = (
        StatusCode::CONFLICT,
        "缺少与所选媒体、实际档期和目标圈匹配的快照，请先在数据中心补录。",
    );
    const UNCHANGED: WebError = (
        StatusCode::CONFLICT,
        "已使用最新的匹配快照，请补证或导入同档期数据后重算。",
    );
    let (starts, ends) = options
        .starts
        .as_deref()
        .zip(options.ends.as_deref())
        .ok_or(MISSING)?;
    let mut query = options.clone();
    query.batch = None;
    if let Some(rolling) = &mut options.rolling {
        let windows = rolling
            .windows(starts, ends)
            .map_err(|message| (StatusCode::UNPROCESSABLE_ENTITY, message))?;
        let mut selected = BTreeMap::new();
        for window in windows.into_iter().filter(|window| window.needs_stock()) {
            query.starts = Some(window.starts.clone());
            query.ends = Some(window.ends);
            if let Some(batch) = recovery_batch(store, &query).await? {
                selected.insert(window.starts, batch);
            }
        }
        if selected.is_empty() {
            return Err(MISSING);
        }
        if selected == rolling.batches {
            return Err(UNCHANGED);
        }
        rolling.batches = selected;
        options.batch = None;
    } else {
        let selected = recovery_batch(store, &query).await?.ok_or(MISSING)?;
        if options.batch == Some(selected) {
            return Err(UNCHANGED);
        }
        options.batch = Some(selected);
    }
    Ok(())
}

async fn recovery_batch(store: &Store, query: &Options) -> Result<Option<Uuid>, WebError> {
    Ok(super::read_snapshot_matching(store.clone(), query, true)
        .await?
        .and_then(|snapshot| snapshot.id)
        .and_then(|id| id.parse().ok()))
}

async fn detail(
    State(store): State<Store>,
    Extension(user): Extension<User>,
    Path(id): Path<Uuid>,
    Query(options): Query<DetailOptions>,
) -> Result<Response, WebError> {
    let task = get_task(&store, id).await?;
    let recovery_snapshots = task.recovery_snapshots();
    let operations_open =
        options.review.as_deref() == Some("operations") && task.can_review_operations();
    Ok(render(Detail {
        heading: task.name(),
        section: "tasks",
        task,
        labels: LABELS,
        budgets: [100, 200, 500],
        recovery_snapshots,
        can_read_audit: user.admin,
        operations_open,
    }))
}

#[derive(Deserialize)]
struct ReviewForm {
    decision: ReviewState,
    note: String,
}

async fn review_task(
    State(store): State<Store>,
    Extension(user): Extension<User>,
    Path(id): Path<Uuid>,
    Form(form): Form<ReviewForm>,
) -> Result<Redirect, WebError> {
    let note = form.note.trim().to_owned();
    if matches!(form.decision, ReviewState::Pending)
        || note.chars().count() > 300
        || (matches!(form.decision, ReviewState::Returned) && note.is_empty())
    {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "退回需要填写原因。审核说明最多 300 个字符。",
        ));
    }
    let changed = database(move || {
        let mut db = store.connect()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE media_tasks SET review=?1, review_note=?2
             WHERE id=?3 AND active=0 AND deleted=0 AND report IS NOT NULL
               AND review='pending' AND json_extract(payload, '$.state')='done'",
            params![form.decision.key(), note, id.to_string()],
        )?;
        if changed == 1 {
            audit::write(
                &tx,
                "人工审核",
                form.decision.label(),
                Some(id),
                &format!("{} · {note}", user.identifier),
            )?;
        }
        tx.commit()?;
        Ok(changed == 1)
    })
    .await?;
    if !changed {
        return Err((StatusCode::CONFLICT, "这项任务已审核或没有可审核的报告。"));
    }
    Ok(Redirect::to(&format!("/media-proposals/tasks/{id}")))
}

async fn delete_task(
    State(store): State<Store>,
    Extension(user): Extension<User>,
    Path(id): Path<Uuid>,
) -> Result<Redirect, WebError> {
    let changed = database(move || {
        let mut db = store.connect()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE media_tasks SET deleted=1, report=NULL WHERE id=?1 AND active=0 AND deleted=0",
            [id.to_string()],
        )?;
        if changed == 1 {
            audit::write(
                &tx,
                "任务",
                "删除",
                Some(id),
                &format!("{} 删除任务并清除报告正文。", user.identifier),
            )?;
        }
        tx.commit()?;
        Ok(changed == 1)
    })
    .await?;
    if !changed {
        return Err((StatusCode::CONFLICT, "任务仍在执行或已删除。"));
    }
    Ok(Redirect::to("/task-log"))
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TaskFilter {
    Exceptions,
    #[default]
    All,
    Active,
    Pending,
    Approved,
    Failed,
}

impl TaskFilter {
    fn key(&self) -> &'static str {
        match self {
            Self::Exceptions => "exceptions",
            Self::All => "all",
            Self::Active => "active",
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Failed => "failed",
        }
    }
}

#[derive(Default, Deserialize)]
struct TaskQuery {
    #[serde(default)]
    filter: TaskFilter,
    q: Option<String>,
    page: Option<u32>,
    page_size: Option<u32>,
}

#[derive(Template)]
#[template(path = "task-log.html")]
struct TaskLog {
    heading: &'static str,
    section: &'static str,
    filter: &'static str,
    q: String,
    tasks: Vec<TaskTableRow>,
    pages: crate::web::ListPages,
}

struct TaskTableRow {
    sequence: i64,
    task: Task,
    updated_at: Option<String>,
}

fn decode_table_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskTableRow> {
    Ok(TaskTableRow {
        sequence: row.get("sequence")?,
        task: decode(row)?,
        updated_at: row.get("updated_at")?,
    })
}

async fn listing(
    State(store): State<Store>,
    Query(query): Query<TaskQuery>,
) -> Result<Response, WebError> {
    let filter = query.filter.key();
    let q: String = query
        .q
        .unwrap_or_default()
        .trim()
        .chars()
        .take(100)
        .collect();
    let keyword = q.clone();
    let sequence = q.parse::<i64>().ok();
    let page = query.page;
    let size = crate::web::ListPages::size(query.page_size, 10);
    let (tasks, pages) = database(move || {
        let mut db = store.connect()?;
        let tx = db.transaction()?;
        let matched = format!(
            "WITH tasks AS (
                 SELECT {TASK_COLUMNS}, {TASK_TABLE_COLUMNS},
                        json_extract(payload, '$.state') AS state,
                        report IS NULL AS missing_report,
                        coalesce(json_extract(payload, '$.options.budget'), ?4) AS budget
                 FROM media_tasks
                 WHERE deleted=0 AND json_extract(payload, '$.options.industry')='food'
             ), matched AS (
             SELECT payload, active, review, review_note, created_at, sequence, updated_at FROM tasks
             WHERE CASE ?1
                 WHEN 'all' THEN 1
                 WHEN 'active' THEN active=1
                 WHEN 'pending' THEN active=0 AND state='done' AND review='pending' AND NOT missing_report
                 WHEN 'approved' THEN state='done' AND review='approved'
                 WHEN 'failed' THEN active=0 AND state IN ('active','failed')
                 ELSE active=0 AND (review='returned' OR state IN ('review','failed') OR missing_report OR json_extract(payload, '$.agent_error')=1)
               END
               AND CASE WHEN EXISTS (
                   SELECT 1 FROM media_tasks WHERE rowid=?6 AND deleted=0
                       AND json_extract(payload, '$.options.industry')='food'
               ) THEN sequence=?6
               ELSE (?2='' OR instr(payload,?2)>0 OR instr(review_note,?2)>0 OR instr(?3,?2)>0
                     OR instr(created_at,?2)>0 OR instr(updated_at,?2)>0
                     OR instr(?5 || ' ' || budget || '万' || coalesce(' ' || substr(strftime('%Y%m%d', created_at),3),''),replace(?2,'+',' '))>0)
               END)",
        );
        let total = tx.query_row(
            &format!("{matched} SELECT count(*) FROM matched"),
            params![filter, keyword, Industry::Food.brand(), super::SHOWCASE_BUDGET, TASK_NAME_PREFIX, sequence],
            |row| row.get(0),
        )?;
        let pages = crate::web::ListPages::counted(page, size, total);
        let tasks = tx.prepare(&format!(
            "{matched} SELECT * FROM matched ORDER BY sequence DESC LIMIT ?7 OFFSET ?8",
        ))?.query_map(
            params![filter, keyword, Industry::Food.brand(), super::SHOWCASE_BUDGET, TASK_NAME_PREFIX, sequence, size, crate::web::ListPages::offset(pages.page, size)],
            decode_table_row,
        )?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((tasks, pages))
    }).await?;
    Ok(render(TaskLog {
        heading: "任务明细",
        section: "task-log",
        filter,
        q,
        tasks,
        pages,
    }))
}

#[derive(Template)]
#[template(path = "overview.html")]
struct Overview {
    heading: &'static str,
    section: &'static str,
    total: u32,
    active_count: u32,
    ready_count: u32,
    approved_count: u32,
    active: Vec<TaskTableRow>,
    ready: Vec<TaskTableRow>,
}

async fn overview(State(store): State<Store>) -> Result<Response, WebError> {
    let data = database(move || {
        let db = store.connect()?;
        let (total, active_count, ready_count, approved_count) = db.query_row(
            "SELECT count(*), count(*) FILTER (WHERE active=1),
                count(*) FILTER (WHERE report IS NOT NULL AND review='pending' AND json_extract(payload, '$.state')='done'),
                count(*) FILTER (WHERE review='approved')
             FROM media_tasks WHERE deleted=0 AND json_extract(payload, '$.options.industry')='food'",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        let active = db.prepare(
            &format!("SELECT {TASK_COLUMNS}, {TASK_TABLE_COLUMNS} FROM media_tasks WHERE deleted=0 AND json_extract(payload, '$.options.industry')='food' AND active=1 ORDER BY rowid DESC LIMIT 12"),
        )?.query_map([], decode_table_row)?.collect::<rusqlite::Result<_>>()?;
        let ready = db.prepare(
            &format!("SELECT {TASK_COLUMNS}, {TASK_TABLE_COLUMNS} FROM media_tasks WHERE deleted=0 AND json_extract(payload, '$.options.industry')='food' AND report IS NOT NULL AND review='pending' AND json_extract(payload, '$.state')='done' ORDER BY rowid DESC LIMIT 12"),
        )?.query_map([], decode_table_row)?.collect::<rusqlite::Result<_>>()?;
        Ok(Overview { heading: "数据总览", section: "overview", total, active_count, ready_count, approved_count, active, ready })
    }).await?;
    Ok(render(data))
}

async fn progress(
    State(store): State<Store>,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, WebError> {
    let task = get_task(&store, id).await?;
    let steps = task
        .steps
        .iter()
        .map(|step| {
            json!({
                "state": step.state,
                "label": step.state.label(),
                "notes": step.notes,
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "state": task.state,
        "status": task.status(),
        "summary": task.summary(),
        "current_step": task.current_step(),
        "login": task.login,
        "offer_evaluation": task.can_evaluate_budget(),
        "offer_local_snapshot": task.can_use_local_snapshot(),
        "offer_same_inventory": task.can_reuse_portfolio(),
        "offer_proof_refresh": task.can_refresh_evidence(),
        "offer_operations": task.can_review_operations(),
        "recovery_snapshots": task.recovery_snapshots(),
        "has_alternative": task.has_alternative,
        "steps": steps,
    })))
}

#[derive(Deserialize)]
struct ReportOptions {
    preview: Option<u8>,
}

#[derive(Template)]
#[template(path = "media-campaign.html")]
struct CampaignSheet<'a> {
    campaign: super::brief::CampaignView<'a>,
    css: &'static str,
    brand: Brand,
}

async fn campaign_sheet(
    State(store): State<Store>,
    Path(id): Path<Uuid>,
    Query(options): Query<ReportOptions>,
) -> Result<Response, WebError> {
    let task = get_task(&store, id).await?;
    let html = CampaignSheet {
        campaign: super::brief::CampaignView::new(&task.options),
        css: crate::web::CSS,
        brand: Brand::read(&store).await?,
    }
    .render()
    .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "无法生成假设书。"))?;
    let mut response = crate::web::report_response(html);
    if options.preview != Some(1) {
        response.headers_mut().insert(
            header::CONTENT_DISPOSITION,
            header::HeaderValue::from_static("attachment; filename=cityline-campaign.html"),
        );
    }
    Ok(response)
}

async fn report(
    State(store): State<Store>,
    Path(id): Path<Uuid>,
    Query(options): Query<ReportOptions>,
) -> Result<Response, WebError> {
    let task = get_task(&store, id).await?;
    let budget = task.budget();
    let html: Option<String> = database(move || {
        store.connect()?.query_row(
            "SELECT report FROM media_tasks WHERE id=?1",
            [id.to_string()],
            |row| row.get(0),
        )
    })
    .await?;
    let html = html
        .ok_or((StatusCode::CONFLICT, "当前任务尚无完整报告。"))?
        .replacen(
            "<html lang=\"zh-CN\">",
            "<html lang=\"zh-CN\" data-theme=\"light\">",
            1,
        )
        .replacen(
            "<html lang=\"zh-CN\" data-theme=\"dark\">",
            "<html lang=\"zh-CN\" data-theme=\"light\">",
            1,
        );
    export_response(
        html,
        options.preview == Some(1),
        task.options.industry,
        task.options.media.unwrap_or(Selection::Both),
        budget,
        task.options.starts.as_deref().unwrap_or("2027"),
    )
}

#[derive(Clone, Copy, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Angle {
    #[default]
    Visit,
    Commute,
    Awareness,
}

impl Angle {
    fn from_client_goal(goal: &str) -> Option<Self> {
        [
            (
                Self::Visit,
                &["到访", "引导到店", "门店体验", "到店", "访客", "试饮"][..],
            ),
            (Self::Commute, &["通勤", "早晚高峰", "上下班"][..]),
            (
                Self::Awareness,
                &["品牌知名度", "品牌认知", "品牌形象", "曝光"][..],
            ),
        ]
        .into_iter()
        .find_map(|(angle, terms)| {
            terms
                .iter()
                .any(|term| goal.contains(term))
                .then_some(angle)
        })
    }

    fn copy(self) -> &'static str {
        match self {
            Self::Visit => {
                "以到访引导为重点，通过公交线路或地铁出入口帮助受众识别目的地和到达路径。"
            }
            Self::Commute => {
                "以通勤覆盖为重点，沿日常出行路径安排媒体，帮助同一受众在不同出行环节记住投放信息。"
            }
            Self::Awareness => "以目标圈认知为重点，用可核对的覆盖圈解释点位组合及各圈的投放作用。",
        }
    }
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum GoalReason {
    ConfirmedGoal,
    ExampleGoal,
    NeedsConfirmation,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Strategy {
    angle: Angle,
    #[serde(default)]
    reason: Option<GoalReason>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Mix {
    priority: MixPriority,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum MixPriority {
    Balanced,
    KeyCircles,
    Nearby,
}

impl From<MixPriority> for Priority {
    fn from(priority: MixPriority) -> Self {
        match priority {
            MixPriority::Balanced => Self::Balanced,
            MixPriority::KeyCircles => Self::KeyCircles,
            MixPriority::Nearby => Self::Nearby,
        }
    }
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Check {
    Booking,
    Access,
    Materials,
}

impl Check {
    fn copy(&self) -> &'static str {
        match self {
            Self::Booking => "签单前复核档期占用和剩余可售数量，并向运营方确认最终价格。",
            Self::Access => "发布前与客户核对门店、地标和到达路线，确认指引与所选点位匹配。",
            Self::Materials => "制作前核对媒体版式、规格和上刊窗口，完成投放内容的人工审批。",
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Review {
    check: Check,
    #[serde(default)]
    review_order: Vec<Gate>,
    #[serde(default)]
    preferred_variant: Option<Candidate>,
    #[serde(default)]
    reason: Option<Reason>,
}

#[derive(Clone, Copy)]
struct Agent {
    id: &'static str,
    name: &'static str,
    instructions: &'static str,
}

const STRATEGIST: Agent = Agent {
    id: "strategy",
    name: "需求策略 Agent",
    instructions: "根据 4A Brief 的脱敏结构化目标选择到访引导 visit、通勤覆盖 commute、品牌认知 awareness。confirmed_goal_category 有值时原样采用，reason 为 confirmed_goal。assumption_source 为 example 时 reason 为 example_goal，unknown 时 reason 为 needs_confirmation。需求字段、必选线路和档期已由工作台锁定。只返回 angle、reason，例如 {\"angle\":\"visit\",\"reason\":\"example_goal\"}。",
};
const PLANNER: Agent = Agent {
    id: "combination",
    name: "媒介组合 Agent",
    instructions: "为尚未计算的 A、B 两案选择一个共同的次要优化目标。A 始终优先广覆盖和更多不同公交线路，B 始终优先核心圈 500 米内的近场资源与公交频次。根据 strategy_from_previous_step，到访引导优先 nearby；通勤覆盖或品牌认知优先 key_circles；目标待确认时用 balanced，提高与目标金额的贴合。若 locked_priority 有值则沿用它。只返回一个字段 priority，取 balanced、key_circles、nearby，例如 {\"priority\":\"nearby\"}。",
};
const REVIEWER: Agent = Agent {
    id: "verification",
    name: "报告核验 Agent",
    instructions: "核验 Rust 已算出的 A、B 候选。首要人工复核事项 check 取 booking、access 或 materials，采购条件未齐选 booking。review_order 只排列 missing_checks 中的实际缺项，每项唯一。preferred_variant 取 A、B 或 none，只推荐同时满足 placement_feasible 和 evidence_complete 的候选。零个合格用 none/pending_evidence；仅一案合格用 only_feasible；两案合格时可按覆盖优势 wider_coverage 或近场优势 core_frequency 推荐。无可辨差别用 none/equal_tradeoff。报价高位超过目标时须协商，客户明确的硬上限由 Rust 核对。只返回 JSON，例如 {\"check\":\"booking\",\"review_order\":[\"inventory\",\"reach\"],\"preferred_variant\":\"none\",\"reason\":\"pending_evidence\"}。",
};

struct Consultation<T> {
    output: Option<T>,
    attempt_trace: Vec<ModelAttempt>,
    duration_ms: u128,
    cause: Option<String>,
    input: Value,
}

#[derive(Serialize)]
struct ModelAttempt {
    prompt_sha256: String,
    duration_ms: u128,
    error: Option<String>,
}

async fn run_attempt<T: DeserializeOwned>(
    model: &Model,
    role: &str,
    instructions: &str,
    input: &Value,
) -> (Result<T, CallError>, ModelAttempt) {
    let prompt = Model::system_prompt(role, instructions);
    let started = Instant::now();
    let result = model.complete_structured(&prompt, input).await;
    let attempt = ModelAttempt {
        duration_ms: started.elapsed().as_millis(),
        prompt_sha256: STANDARD.encode(Sha256::digest(prompt.as_bytes())),
        error: result.as_ref().err().map(CallError::description),
    };
    (result, attempt)
}

impl<T: Serialize> Consultation<T> {
    fn record(&self, task: &mut Task, agent: Agent, applied: Value, source: &str, adopted: bool) {
        let validation = match (self.output.is_some(), adopted) {
            (true, true) => "accepted",
            (true, false) => "overridden_or_partial",
            (false, _) => "fallback",
        };
        let fallback_reason = self.cause.as_deref().or_else(|| {
            (validation == "overridden_or_partial")
                .then_some("部分建议未满足本地证据或客户约束，已按实际条件校正。")
        });
        task.agent_decisions.push(json!({
            "role": agent.id,
            "contract": 4,
            "input_summary": self.input,
            "model_output": self.output,
            "adopted": applied,
            "source": source,
            "validation": validation,
            "attempts": self.attempt_trace.len(),
            "attempt_trace": self.attempt_trace,
            "duration_ms": self.duration_ms,
            "fallback_reason": fallback_reason,
        }));
    }
}

async fn consult<T: DeserializeOwned>(
    store: &Store,
    model: Option<&Model>,
    task: &mut Task,
    index: usize,
    agent: Agent,
    input: Value,
) -> Result<Consultation<T>, WebError> {
    let started = Instant::now();
    let Some(model) = model else {
        task.note(
            index,
            StepState::Fallback,
            format!("按当前配置，由本地规则执行{}的步骤。", agent.name),
        );
        save(store, task, None).await?;
        return Ok(Consultation {
            output: None,
            attempt_trace: Vec::new(),
            duration_ms: 0,
            cause: Some("按配置使用本地规则".into()),
            input,
        });
    };
    task.note(
        index,
        StepState::Running,
        format!("{} 已进入调用队列。", agent.name),
    );
    save(store, task, None).await?;
    let _slot = model
        .wait_for_slot()
        .await
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "Agent 服务暂时不可用。"))?;
    task.note(
        index,
        StepState::Running,
        format!("正在请求{}。", agent.name),
    );
    save(store, task, None).await?;
    let (mut output, first_attempt) =
        run_attempt(model, agent.name, agent.instructions, &input).await;
    let mut attempt_trace = vec![first_attempt];
    if let Err(error) = &output
        && error.can_retry()
    {
        task.note(
            index,
            StepState::Running,
            format!("{}，正在重试一次。", error.description()),
        );
        save(store, task, None).await?;
        let instructions = match error {
            CallError::InvalidContract | CallError::Incomplete => format!(
                "{} 上次输出未通过校验。只输出示例所示字段，逐字使用约定的英文枚举，不要添加字段、说明或代码围栏。",
                agent.instructions
            ),
            _ => agent.instructions.to_owned(),
        };
        let (retry, retry_attempt) = run_attempt(model, agent.name, &instructions, &input).await;
        output = retry;
        attempt_trace.push(retry_attempt);
    }
    let cause = output.as_ref().err().map(|error| error.description());
    if output.is_ok() {
        task.agents += 1;
        task.note(
            index,
            StepState::Running,
            format!("已建立 Agent 通信，{}已返回并通过格式校验。", agent.name),
        );
    } else if let Err(error) = &output {
        task.agent_error = true;
        task.note(
            index,
            StepState::Fallback,
            format!("{}，本步骤已切换到本地规则。", error.description()),
        );
    }
    save(store, task, None).await?;
    Ok(Consultation {
        output: output.ok(),
        attempt_trace,
        duration_ms: started.elapsed().as_millis(),
        cause,
        input,
    })
}

async fn platform_review(store: &Store, task: &mut Task, reason: &str) -> Result<bool, WebError> {
    task.login.phase = "failed".into();
    task.note(DATA, StepState::Review, reason);
    task.state = TaskState::Review;
    audit::record(store, "平台", "取数未完成", Some(task.id), reason.into()).await?;
    save(store, task, None).await?;
    Ok(false)
}

async fn collect_platform(
    store: &Store,
    refresh: &Refresh,
    actor: &str,
    task: &mut Task,
) -> Result<bool, WebError> {
    let options = &task.options;
    let Some((starts, ends)) = options
        .starts
        .as_ref()
        .zip(options.ends.as_ref())
        .filter(|(a, b)| period_months(a, b).is_some())
    else {
        return platform_review(store, task, "平台取数需要指定 2027 年内的有效起止日期。").await;
    };
    let mut periods = BrowserWindow::for_schedule(starts, ends, options.rolling.as_ref())?;
    let proof = if options.proof_only {
        let pins = snapshot_pins(options);
        periods.retain(|window| pins.contains_key(&window.starts));
        Some(Arc::new(super::platform::EvidenceRefresh::new(pins)))
    } else {
        None
    };
    let count = periods.len();
    let request = BrowserRequest {
        industry: options.industry.id(),
        center: if options.target_centers.is_empty() {
            options.industry.coverage_centers().into()
        } else {
            options.target_centers.join("|")
        },
        radius: options.industry.coverage_radius(),
        supplemental_center: options.industry.supplemental_metro_landmark(),
        required_routes: options.required_routes.clone(),
        starts: starts.clone(),
        ends: ends.clone(),
        media: options.media.unwrap_or(Selection::Both).id(),
        periods,
        budget: task.budget(),
        rolling: options.rolling.clone(),
        target_centers: options.target_centers.clone(),
        policy: Policy {
            emphasis: task.priority.unwrap_or_default(),
            hard_cap: options.hard_cap,
        },
        portfolio: Arc::new(
            if let Some(frozen) = task.portfolio.clone().filter(|_| proof.is_some()) {
                frozen.into()
            } else {
                Default::default()
            },
        ),
        evidence_plan: Arc::default(),
        proof: proof.clone(),
    };
    let portfolio = request.portfolio.clone();
    let proof_only = proof.is_some();
    task.login.phase = "opening".into();
    task.note(
        DATA,
        StepState::Running,
        if proof_only {
            format!("共 {count} 段已锁定采购档期，正在打开新云平台的官方登录窗口。")
        } else {
            format!("共 {count} 段采购档期，正在打开数据平台的官方登录窗口。")
        },
    );
    save(store, task, None).await?;
    let id = match refresh.launch(request, actor) {
        Ok(id) => id,
        Err((_, message)) if proof_only => {
            record_evidence_refresh(store, task, "error_launch", message, 0).await?;
            return Ok(true);
        }
        Err((_, message)) => return platform_review(store, task, message).await,
    };
    audit::record(
        store,
        "平台",
        "开始认证",
        Some(task.id),
        if proof_only {
            format!("只验证新云授权，按已锁定的 {count} 段采购清单取证。")
        } else {
            format!("核验新云会话和红豆库存访问权限，计划查询 {count} 段采购档期。")
        },
    )
    .await?;
    let deadline = tokio::time::Instant::now()
        + Duration::from_secs(635 + 150 * count.saturating_sub(1) as u64);
    let mut last_period = None;
    let mut last_evidence = None;
    let mut redbean_fallback = false;
    let status = loop {
        let status = refresh.status(id)?;
        let state = status.state.as_str();
        let newcloud = status.newcloud && !task.login.newcloud;
        let redbean = status.redbean && !task.login.redbean;
        if newcloud {
            task.login.newcloud = true;
            task.note(DATA, StepState::Running, "新云平台登录已确认。");
            audit::record(
                store,
                "平台",
                "新云连接成功",
                Some(task.id),
                "已确认官方平台登录。".into(),
            )
            .await?;
        }
        if redbean {
            task.login.redbean = true;
            task.note(DATA, StepState::Running, "红豆平台登录已确认。");
            audit::record(
                store,
                "平台",
                "红豆连接成功",
                Some(task.id),
                "已确认官方平台登录。".into(),
            )
            .await?;
        }
        let evidence = status
            .evidence
            .filter(|evidence| Some(*evidence) != last_evidence);
        let opened_redbean = status.redbean_fallback && !redbean_fallback;
        if newcloud
            || redbean
            || task.login.phase != state
            || last_period != status.period
            || evidence.is_some()
            || opened_redbean
        {
            if let Some(message) = match state {
                "login" if task.login.phase == "opening" && proof_only => {
                    Some("请在 Edge 打开的新云平台完成认证。")
                }
                "login" if task.login.phase == "opening" => {
                    Some("正在核验两平台授权。若 Edge 打开登录页，请在对应页面完成认证。")
                }
                "coverage" => Some("正在查询新云覆盖。"),
                "required_inventory" => Some("正在按档期读取红豆库存，核对客户指定线路。"),
                "inventory" => Some("覆盖已导出，正在通过红豆库存 API 分页读取该档期。"),
                "importing" => Some("执行脱敏原表校验并导入快照。"),
                "proof" => Some("按实际入选组合绘制新云线路，并查询历史触达。"),
                _ => None,
            } {
                let text = status
                    .period
                    .as_deref()
                    .map(|period| format!("{period} · {message}"))
                    .unwrap_or_else(|| message.into());
                task.note(DATA, StepState::Running, text);
            }
            last_period = status.period.clone();
            task.login.phase = state.to_owned();
            if opened_redbean {
                redbean_fallback = true;
                let message = "红豆临时令牌未通过库存权限检查，已打开红豆平台登录页。";
                task.note(DATA, StepState::Running, message);
                audit::record(store, "平台", "红豆登录回退", Some(task.id), message.into()).await?;
            }
            if let Some(evidence) = evidence {
                task.note(
                    DATA,
                    StepState::Running,
                    format!(
                        "新云已取得 · 公交线路原图 {}/{} 张 · 有历史数据的资源 {}/{} 项 · 有可核验单位触达的资源 {} 项。",
                        evidence.maps, evidence.expected_maps, evidence.observed_reach,
                        evidence.expected_reach, evidence.reach,
                    ),
                );
                last_evidence = Some(evidence);
            }
            save(store, task, None).await?;
        }
        if status.terminal() {
            break status;
        }
        if tokio::time::Instant::now() >= deadline {
            let mut status = status;
            status.state = "error_timeout".into();
            break status;
        }
        tokio::time::sleep(Duration::from_millis(850)).await;
    };
    let reason = match status.state.as_str() {
        "done" if proof_only => "已完成本次新云取证。",
        "needs_review" if proof_only => "新云未返回全部证据，请核对各段缺项或从数据中心补证。",
        "done" | "needs_review" => "未取得有效的平台数据快照。",
        "error_launch" => "无法打开数据平台。请检查本机 Edge。",
        "error_connection" => "官方登录页面暂时无法打开，请检查网络后重新取数。",
        "error_login" if proof_only => "新云平台认证超时，可按采购清单再次取证。",
        "error_login" => match (status.newcloud, status.redbean) {
            (true, false) => "新云平台已认证。红豆平台认证超时，可重新取数完成认证。",
            (false, true) => "红豆平台已认证。新云平台认证超时，可重新取数完成认证。",
            _ => "新云平台和红豆平台均未完成认证，可重新取数完成认证。",
        },
        "error_redbean_auth" => "红豆平台登录未取得库存查询权限，请核对账号能否查询所选媒体。",
        "error_coverage" => "新云没有返回所选媒体的可用覆盖结果。请核对定位与媒体类型。",
        "error_inventory" => "红豆库存或规格下载失败。请核对档期与导出权限。",
        "error_import" => "平台导出未通过校验，请核对平台中的查询条件后重试。",
        "error_interrupted" => "本机取数进程意外结束。",
        "error_timeout" => "平台取数超时。",
        _ => "平台取数未完成。",
    };

    if let Some(proof) = proof {
        let previous = snapshot_pins(&task.options);
        let updated = proof.publish(store, id, task.id).await?;
        let changed = updated
            .iter()
            .filter(|(starts, id)| previous.get(*starts) != Some(*id))
            .count();
        let replacements = evidence_replacements(store, previous, updated.clone()).await?;
        let frozen = replacements
            .as_ref()
            .and_then(|replacements| task.portfolio.clone()?.with_evidence(replacements))
            .ok_or((StatusCode::CONFLICT, "新云补证版本与原采购清单不相符。"))?;
        if let Some(rolling) = &mut task.options.rolling {
            rolling.batches = updated;
            task.options.batch = None;
        } else {
            task.options.batch = updated.into_values().next();
        }
        task.portfolio = Some(frozen);
        record_evidence_refresh(store, task, &status.state, reason, changed).await?;
        return Ok(true);
    }

    // An import carries this job ID only after both official sessions and the requested dates
    // are checked. Recover its committed batches even if the worker could not finish writing status.
    let batch_store = store.clone();
    let committed = database(move || {
        let db = batch_store.connect()?;
        let mut query = db.prepare(
            "SELECT starts, id FROM media_snapshots WHERE refresh_id=?1 AND source='platform' ORDER BY starts",
        )?;
        let rows = query
            .query_map([id.to_string()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(date, id)| Some((date, id.parse::<Uuid>().ok()?)))
            .collect::<std::collections::BTreeMap<_, _>>())
    })
    .await?;
    let imported = committed.len();
    if imported == 0 {
        return platform_review(store, task, reason).await;
    }

    if imported < count {
        task.login.phase = "partial".into();
        task.note(
            DATA,
            StepState::Review,
            format!("已保留 {imported} / {count} 段平台快照，其余采购档期仍待核对。"),
        );
    } else {
        task.options.batch = committed.values().next().copied();
        task.note(
            DATA,
            StepState::Running,
            format!("官方导出已导入本任务的 {imported} 段采购档期快照。"),
        );
    }
    if let Some(rolling) = &mut task.options.rolling {
        rolling.batches = committed;
    }
    task.portfolio = portfolio.get().cloned();
    if status.state.starts_with("error_") {
        if imported == count {
            task.login.phase = "needs_review".into();
        }
        task.note(DATA, StepState::Review, reason);
        audit::record(
            store,
            "平台",
            "取数中断",
            Some(task.id),
            format!("{reason} 已保留 {imported} / {count} 段平台快照。"),
        )
        .await?;
    }
    if status.inventory.api > 0 || status.inventory.exports > 0 {
        let message = format!(
            "红豆库存读取 · 官方分页 API {} 组 · 官方 Excel 导出备用 {} 组。",
            status.inventory.api, status.inventory.exports,
        );
        task.note(DATA, StepState::Running, &message);
        audit::record(store, "平台", "红豆库存读取", Some(task.id), message).await?;
    }
    save(store, task, None).await?;
    Ok(true)
}

async fn record_evidence_refresh(
    store: &Store,
    task: &mut Task,
    state: &str,
    reason: &str,
    changed: usize,
) -> Result<(), WebError> {
    task.login.phase = state.into();
    let message = format!("{reason} 已保存 {changed} 段新证据，沿用锁定的库存、采购清单和报价。");
    let success = state == "done";
    task.note(
        DATA,
        if success {
            StepState::Running
        } else {
            StepState::Review
        },
        &message,
    );
    audit::record(
        store,
        "平台",
        if success {
            "新云重新取证完成"
        } else {
            "新云重新取证待核"
        },
        Some(task.id),
        message,
    )
    .await?;
    save(store, task, None).await
}

struct PlanningContext {
    scene: Value,
    angle: Angle,
    client_confirmed: bool,
    strategy_applied: bool,
    mix_adopted: bool,
}

async fn prepare_policy(
    store: &Store,
    model: Option<&Model>,
    downstream: Option<&Model>,
    task: &mut Task,
) -> Result<PlanningContext, WebError> {
    let brief_objective = task
        .options
        .brief
        .as_ref()
        .map(|brief| brief.objective.as_str());
    let campaign_goal = task
        .options
        .brief
        .as_ref()
        .and_then(|brief| super::brief::campaign_goal(&brief.campaign_goal));
    let client_goal = match campaign_goal {
        Some("到店") => Some(Angle::Visit),
        Some("认知") => Some(Angle::Awareness),
        _ => brief_objective.and_then(Angle::from_client_goal),
    };
    let (goal_source, goal_reason) = if client_goal.is_some() {
        ("confirmed", GoalReason::ConfirmedGoal)
    } else if campaign_goal.is_some() || brief_objective.is_some_and(|value| !value.is_empty()) {
        ("unknown", GoalReason::NeedsConfirmation)
    } else {
        ("example", GoalReason::ExampleGoal)
    };
    let scene = json!({
        "industry": task.options.industry.label(),
        "public_brand": task.options.industry.brand(),
        "example_objective": task.options.industry.campaign(),
        "requested_media": task.options.media.unwrap_or(Selection::Both).label(),
        "rolling_schedule": task.options.rolling.is_some(),
        "explicit_budget_ceiling": task.options.hard_cap.is_some(),
        "confirmed_goal_category": client_goal,
        "campaign_goal": campaign_goal,
        "custom_target_circle_count": task.options.target_centers.len(),
        "assumption_source": goal_source,
        "required_bus_route_count": task.options.required_routes.len(),
    });
    let strategy: Consultation<Strategy> =
        consult(store, model, task, STRATEGY, STRATEGIST, scene.clone()).await?;
    let angle = client_goal
        .or_else(|| strategy.output.as_ref().map(|plan| plan.angle))
        .unwrap_or_default();
    strategy.record(
        task,
        STRATEGIST,
        json!({"angle": angle, "reason": goal_reason}),
        if client_goal.is_some() {
            "customer"
        } else if strategy.output.is_some() {
            "model"
        } else {
            "rules"
        },
        strategy.output.as_ref().is_some_and(|suggestion| {
            suggestion.angle == angle && suggestion.reason == Some(goal_reason)
        }),
    );
    task.note(
        STRATEGY,
        if strategy.output.is_some() {
            StepState::Done
        } else {
            StepState::Fallback
        },
        if client_goal.is_some() {
            format!("采用已确认的客户目标。{}", angle.copy())
        } else {
            angle.copy().to_owned()
        },
    );
    save(store, task, None).await?;

    let locked = task.options.compare_priority;
    let mix: Consultation<Mix> = consult(
        store,
        downstream,
        task,
        MIX,
        PLANNER,
        json!({
            "scene": scene,
            "strategy_from_previous_step": angle,
            "locked_priority": locked,
        }),
    )
    .await?;
    let proposed = mix
        .output
        .as_ref()
        .map(|plan| Priority::from(plan.priority));
    let priority = locked.or(proposed).unwrap_or_else(|| {
        if goal_source == "unknown" {
            Priority::Balanced
        } else if angle == Angle::Visit {
            Priority::Nearby
        } else {
            Priority::KeyCircles
        }
    });
    let mix_adopted = locked.is_none() && proposed == Some(priority);
    mix.record(
        task,
        PLANNER,
        json!({ "priority": priority }),
        if mix_adopted { "model" } else { "rules" },
        proposed == Some(priority),
    );
    task.priority = Some(priority);
    task.note(
        MIX,
        StepState::Pending,
        format!("{}。等库存核验后生成两案。", priority.label()),
    );
    save(store, task, None).await?;
    Ok(PlanningContext {
        scene,
        angle,
        client_confirmed: client_goal.is_some(),
        strategy_applied: strategy.output.is_some(),
        mix_adopted,
    })
}
async fn execute(
    store: &Store,
    model: Option<&Model>,
    collaboration: Collaboration,
    refresh: Refresh,
    actor: &str,
    task: &mut Task,
) -> Result<(), WebError> {
    let downstream = if matches!(collaboration, Collaboration::Coordinated) {
        model
    } else {
        None
    };
    let PlanningContext {
        scene,
        angle,
        client_confirmed,
        strategy_applied,
        mix_adopted,
    } = if task.options.proof_only {
        prepare_policy(store, None, None, task).await?
    } else {
        prepare_policy(store, model, downstream, task).await?
    };
    let priority = task.priority.unwrap_or_default();
    if task.needs_platform_login() && !collect_platform(store, &refresh, actor, task).await? {
        return Ok(());
    }
    task.note(
        DATA,
        StepState::Running,
        "正在匹配行业、档期与已导入的赛用快照。",
    );
    save(store, task, None).await?;
    let mut calendar = None;
    let mut single = None;
    if let Some(mut rolling) = task.options.rolling.take() {
        let loaded = super::schedule::Calendar::load(store, &task.options, &rolling).await?;
        let records = loaded.record(&mut rolling);
        let missing = records.iter().any(|(_, found)| !found);
        for (message, _) in records {
            task.note(DATA, StepState::Running, message);
        }
        task.note(
            DATA,
            if missing {
                StepState::Review
            } else {
                StepState::Done
            },
            if missing {
                "有投放段缺少实际档期快照，缺失段保留待确认。"
            } else {
                "已锁定各段独立档期快照。"
            },
        );
        task.options.rolling = Some(rolling);
        calendar = Some(loaded);
    } else {
        let (page, snapshot) = load_plan(store.clone(), task.options.clone()).await?;
        let Some(snapshot) = snapshot else {
            task.note(
                DATA,
                StepState::Review,
                page.notice.unwrap_or("请核对档期后重新从平台取数。"),
            );
            task.state = TaskState::Review;
            return save(store, task, None).await;
        };
        task.options.starts = Some(snapshot.starts.clone());
        task.options.ends = Some(snapshot.ends.clone());
        task.options.batch = snapshot
            .id
            .as_deref()
            .and_then(|id| Uuid::parse_str(id).ok());
        task.note(
            DATA,
            StepState::Done,
            format!(
                "已锁定{}。来源为{}。载入 {} 条库存、{} 条覆盖、{} 条刊例规则。",
                if snapshot.data.demo {
                    "合成样例"
                } else {
                    "赛用数据快照"
                },
                snapshot.source,
                snapshot.data.stock.len(),
                snapshot.data.coverage.len(),
                snapshot.data.rates.len(),
            ),
        );
        for medium in snapshot.data.empty_inventory.keys() {
            task.note(
                DATA,
                StepState::Done,
                format!("{}库存 API 返回 0 条。", medium.label()),
            );
        }
        single = Some((page, snapshot));
    };
    save(store, task, None).await?;

    task.note(
        MIX,
        StepState::Running,
        if task.portfolio.is_some() {
            "正在核验已锁定的 A／B 采购清单、刊例及证据。"
        } else {
            "正在按已锁定的库存、刊例及配置分别求解方案 A、B。"
        },
    );
    save(store, task, None).await?;
    let options = task.options.clone();
    let budget = task.budget();
    let existing = task.portfolio.take();
    let previous = comparison_portfolio(store, task).await?;
    let policy = Policy {
        emphasis: priority,
        hard_cap: options.hard_cap,
    };
    let (calendar, single, portfolio, previous, variants) =
        tokio::task::spawn_blocking(move || {
            let optimize = |amount, policy| {
                if let Some(calendar) = &calendar {
                    calendar.optimize(&options, amount, policy)
                } else if let Some((_, snapshot)) = &single {
                    optimize_snapshot(&options, snapshot, amount, policy)
                } else {
                    Portfolio::default()
                }
            };
            let portfolio = existing.unwrap_or_else(|| optimize(budget, policy));
            let previous =
                previous.or_else(|| options.compare.map(|amount| optimize(amount, policy)));
            let variants = if let Some(calendar) = &calendar {
                calendar.variants(&options, &portfolio)
            } else if let Some((_, snapshot)) = &single {
                optimized_snapshot_variants(&options, snapshot, &portfolio)
            } else {
                Vec::new()
            };
            (calendar, single, portfolio, previous, variants)
        })
        .await
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "媒介核算未能完成。"))?;
    if variants.len() != 2 {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, "未找到任务的数据快照。"));
    }
    let [optimal, feasible, fallback, unavailable] = portfolio.engine_counts();
    task.note(MIX, StepState::Running, format!(
        "分段核算结果 · 最优 {optimal} · 限时可行 {feasible} · 规则回退 {fallback} · 无可核价资源 {unavailable}。"
    ));
    task.portfolio = Some(portfolio.clone());
    let portfolio = &portfolio;
    let operations_required = task.options.requires_operations();
    let summaries = summarize(&variants, operations_required);
    let checks = Checks::new(
        &variants,
        !task.options.required_routes.is_empty(),
        operations_required,
    );
    let missing_checks = checks.missing();
    let result = if let Some((page, snapshot)) = single {
        let strategy = visuals::Strategy::CommerceCoverage;
        let months = period_months(&snapshot.starts, &snapshot.ends).unwrap_or(1);
        let build = |portfolio: &Portfolio| {
            portfolio
                .proposal(
                    strategy,
                    &snapshot,
                    page.media,
                    months,
                    &page.required_routes,
                )
                .map(|(_, proposal)| proposal)
        };
        let proposal =
            build(portfolio).ok_or((StatusCode::CONFLICT, "锁定的组合与快照不匹配。"))?;
        let changes = previous
            .as_ref()
            .and_then(build)
            .zip(task.options.compare)
            .map(|(before, amount)| change_set(&before, &proposal, amount));
        Some((
            page,
            ResultView {
                snapshot,
                proposal,
                changes,
                months,
            },
        ))
    } else {
        None
    };
    let rolling = calendar.as_ref().map(|calendar| {
        calendar.plan(
            &task.options,
            budget,
            portfolio,
            visuals::Strategy::CommerceCoverage,
        )
    });
    let annual_conditions_incomplete =
        rolling.is_some() && variants.iter().any(|variant| !variant.available);
    let single_conditions_incomplete = result.as_ref().is_some_and(|(_, result)| {
        !result.proposal.complete_required()
            || !result.proposal.complete_media
            || variants.iter().any(|variant| !variant.available)
    });
    let mut report_available = true;
    if let Some((_, result)) = &result {
        for check in &result.proposal.required_routes {
            task.note(
                MIX,
                StepState::Running,
                format!("必选线路 {} · {}", check.name, check.reason),
            );
        }
        if single_conditions_incomplete && !result.proposal.required_routes.is_empty() {
            task.note(
                MIX,
                StepState::Review,
                "当前方案未满足全部必选线路或媒体组合，需与客户确认替换线路、档期或预算。",
            );
            report_available = !result.proposal.items.is_empty()
                || !result.snapshot.data.empty_inventory.is_empty();
        } else if !result.proposal.complete_media || result.proposal.items.is_empty() {
            task.note(
                MIX,
                StepState::Review,
                "当前档期或预算下没有完整、可核价的媒体组合。请核对库存与刊例，或调整预算重新生成。",
            );
            report_available = !result.snapshot.data.empty_inventory.is_empty();
        } else {
            task.note(
                MIX,
                if mix_adopted {
                    StepState::Done
                } else {
                    StepState::Fallback
                },
                format!(
                    "核算完成，{} 项媒体通过校验，报价上限 ¥{}。",
                    result.proposal.items.len(),
                    result.proposal.total.high
                ),
            );
        }
    } else if let Some(plan) = &rolling {
        if annual_conditions_incomplete {
            task.note(
                MIX,
                StepState::Review,
                "至少一案有投放段缺少库存、有效报价或必选资源，请核对报告明细。",
            );
        } else {
            task.note(
                MIX,
                if mix_adopted {
                    StepState::Done
                } else {
                    StepState::Fallback
                },
                format!(
                    "已按实际档期、可售上限及阶段预算权重完成两案，A 案高位报价 ¥{}。",
                    plan.total.high
                ),
            );
        }
    }
    save(store, task, None).await?;
    let conditions_incomplete = annual_conditions_incomplete
        || single_conditions_incomplete
        || missing_checks.contains(&Gate::Operations);
    let prioritize_booking =
        conditions_incomplete || missing_checks.iter().any(|gate| gate.allows_booking());
    let review: Consultation<Review> = consult(store, downstream, task, REPORT, REVIEWER, json!({
        "scene": scene, "strategy_from_previous_step": angle, "combination_rule": priority.label(),
        "placement_conditions_complete": !prioritize_booking,
        "candidates": summaries,
        "checks": checks,
        "missing_checks": missing_checks,
    })).await?;
    let proposed = review.output.as_ref().and_then(|plan| {
        Some(Recommendation {
            candidate: plan.preferred_variant?,
            reason: plan.reason?,
        })
    });
    let (recommendation, recommendation_adopted) =
        recommend(&summaries, proposed, angle == Angle::Visit);
    task.note(REPORT, StepState::Running, recommendation.guidance());
    let suggested_check = review.output.as_ref().map(|output| output.check);
    let check = if prioritize_booking {
        Check::Booking
    } else {
        suggested_check.unwrap_or(Check::Booking)
    };
    let proposed_order = review
        .output
        .as_ref()
        .map_or(&[][..], |output| output.review_order.as_slice());
    let (ordered_checks, review_order_adopted) = review_queue(&missing_checks, proposed_order);
    review.record(
        task,
        REVIEWER,
        json!({
            "check": check, "review_order": ordered_checks,
            "review_order_source": if review_order_adopted { "model" } else { "rules" },
            "recommendation": recommendation,
            "recommendation_source": if recommendation_adopted { "model" } else { "rules" },
            "solver": {"optimal": optimal, "feasible": feasible, "fallback": fallback, "unavailable": unavailable},
        }),
        if suggested_check == Some(check) {
            "model"
        } else {
            "rules"
        },
        suggested_check == Some(check) && recommendation_adopted
            && (missing_checks.is_empty() || review_order_adopted),
    );
    let mut guidance = vec![
        (
            if client_confirmed {
                "客户确认 · 投放目标"
            } else if strategy_applied {
                "策略 Agent · 品牌与目标"
            } else {
                "默认策略 · 品牌与目标"
            },
            angle.copy(),
        ),
        (
            if mix_adopted {
                "组合 Agent · 核算优先级"
            } else {
                "默认核算优先级"
            },
            priority.label(),
        ),
        ("A／B 推荐结论", recommendation.guidance()),
        (
            if prioritize_booking {
                "本地校验 · 待补档期与采购条件"
            } else if review.output.is_some() {
                "核验 Agent · 交付前复核"
            } else {
                "交付前复核"
            },
            check.copy(),
        ),
    ];
    for gate in ordered_checks {
        let (label, detail) = gate.guidance();
        task.note(
            REPORT,
            StepState::Running,
            format!("待核 · {label}。{detail}"),
        );
        guidance.push((label, detail));
    }
    task.note(REPORT, StepState::Running, check.copy());
    if !report_available {
        task.note(
            REPORT,
            StepState::Review,
            "当前资料不足以生成可用组合，请按待核事项补齐后重算。",
        );
        task.state = TaskState::Review;
        return save(store, task, None).await;
    }
    save(store, task, None).await?;
    let brand = Brand::read(store).await?;
    let (html, full_evidence) = if let Some((page, result)) = &result {
        render_export(page, result, variants, &guidance, &brand)?
    } else if let Some(calendar) = &calendar {
        calendar.render(
            &task.options,
            variants,
            portfolio,
            previous.as_ref(),
            &brand,
            &guidance,
        )?
    } else {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, "未找到任务的投放方案。"));
    };
    if conditions_incomplete || !full_evidence {
        task.note(
            REPORT,
            StepState::Review,
            "已保存 A／B 方案草案。请按报告核对线路截图、出口实点、单位触达和标出的投放条件。",
        );
        task.state = TaskState::Review;
        task.has_alternative = true;
        return save(store, task, Some(html)).await;
    }
    task.note(
        REPORT,
        if review.output.is_some() {
            StepState::Done
        } else {
            StepState::Fallback
        },
        "可交互 HTML 已渲染并保存，可直接预览或离线下载。",
    );
    task.state = TaskState::Done;
    save(store, task, Some(html)).await
}

pub(super) fn router() -> Router<Store> {
    Router::new()
        .route("/overview", get(overview))
        .route("/task-log", get(listing))
        .route(
            "/media-proposals/create",
            post(create).layer(DefaultBodyLimit::max(32 * 1024)),
        )
        .route(
            "/media-proposals/run",
            post(run).layer(DefaultBodyLimit::max(4096)),
        )
        .route("/media-proposals/tasks/{id}", get(detail))
        .route(
            "/media-proposals/tasks/{id}/rerun",
            post(rerun).layer(DefaultBodyLimit::max(128)),
        )
        .route("/media-proposals/tasks/{id}/report", get(report))
        .route("/media-proposals/tasks/{id}/campaign", get(campaign_sheet))
        .route(
            "/media-proposals/tasks/{id}/evidence",
            get(evidence_review::checklist),
        )
        .route(
            "/media-proposals/tasks/{id}/refresh-evidence",
            post(refresh_evidence),
        )
        .route(
            "/media-proposals/tasks/{id}/operations",
            post(review_operations).layer(DefaultBodyLimit::max(16 * 1024)),
        )
        .route(
            "/media-proposals/tasks/{id}/review",
            post(review_task).layer(DefaultBodyLimit::max(4096)),
        )
        .route("/media-proposals/tasks/{id}/delete", post(delete_task))
        .route("/api/media-proposals/tasks/{id}", get(progress))
}
