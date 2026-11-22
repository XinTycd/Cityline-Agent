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
    Dataset, Industry, Options, ReportFile, ResultView, change_set,
    collaboration::{
        Candidate, Checks, ComparisonFocus, Gate, Reason, Recommendation, missing_primary_metric,
        recommend, review_queue, summarize,
    },
    export_response, load_plan, normalize_options, optimize_snapshot, optimized_snapshot_variants,
    optimizer::{Policy, Portfolio},
    period_months,
    plan::{Priority, Selection},
    platform::ReachCheck,
    render_export, visuals,
};
use crate::{
    agents::{AgentModels, Agents, Role},
    audit,
    auth::User,
    model::{CallError, Model},
    refresh::{BrowserRequest, BrowserWindow, InventoryFailure, InventoryIssue, Refresh},
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
const AGENT_CONTRACT: u8 = 9;
const LABELS: [&str; 5] = ["需求接收", "数据核验", "提案策略", "组合核算", "报告交付"];
const TASK_NAME_PREFIX: &str = "饮食食品";
const TASK_COLUMNS: &str = "payload, active, review, review_note,
    (SELECT datetime(min(created_at), '+8 hours') FROM audit_events
     WHERE task_id=media_tasks.id AND category='任务' AND action='创建') AS created_at,
    report IS NOT NULL AS report_saved";
const TASK_TABLE_COLUMNS: &str =
    "rowid AS sequence, datetime(updated_at, '+8 hours') AS updated_at";
const FORMAL_REPORT: &str = "report IS NOT NULL
    AND json_extract(payload, '$.state')='done'
    AND coalesce(json_extract(payload, '$.options.demo'), 0)<>1";
const APPROVAL_READY: &str = "json_extract(payload, '$.approval_readiness.a')=1
    AND json_extract(payload, '$.approval_readiness.b')=1
    AND json_type(payload, '$.approval_quotes')='object'
    AND json_extract(payload, '$.options.brief.campaign_goal') IN ('到店','认知','促销','拉新')";

mod agent_trace;
mod evidence_review;
mod overview;
mod report_delivery;
mod route_revision;
mod schedule_revision;

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum TaskState {
    Active,
    Done,
    Demo,
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
    strategy: Option<StrategyDecision>,
    #[serde(default)]
    portfolio: Option<Portfolio>,
    #[serde(default)]
    agent_error: bool,
    #[serde(skip)]
    denied_models: Vec<(Arc<Model>, StatusCode)>,
    #[serde(default)]
    has_alternative: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    route_replacement_needed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    approval_readiness: Option<ApprovalReadiness>,
    #[serde(default)]
    platform_report_data: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    approval_quotes: Option<ApprovalQuotes>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    approval_inventory_issue: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    approval_inventory_review: Option<ApprovalInventoryReview>,
    #[serde(default)]
    login: PlatformLogin,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    reach_checks: Vec<ReachCheck>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    route_overviews: Vec<visuals::OfficialMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    approved_variant: Option<ApprovedVariant>,
    #[serde(skip)]
    review: ReviewState,
    #[serde(skip)]
    review_note: String,
    #[serde(skip)]
    report_saved: bool,
    #[serde(skip)]
    created_at: Option<String>,
    #[serde(skip)]
    started: Option<Instant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    generation_ms: Option<u64>,
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

impl PlatformLogin {
    fn interrupted(&self) -> bool {
        self.phase.starts_with("error_") || matches!(self.phase.as_str(), "failed" | "partial")
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
struct ApprovalReadiness {
    a: bool,
    b: bool,
}

#[derive(Clone, Deserialize, Serialize)]
struct ApprovalInventoryReview {
    variant: ApprovedVariant,
    issues: Vec<InventoryIssue>,
}

impl ApprovalReadiness {
    fn complete(self) -> bool {
        self.a && self.b
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
struct ApprovalQuotes {
    a: u64,
    b: u64,
}

impl ApprovalQuotes {
    fn high(self, variant: ApprovedVariant) -> u64 {
        match variant {
            ApprovedVariant::A => self.a,
            ApprovedVariant::B => self.b,
        }
    }
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReviewState {
    #[default]
    Pending,
    Approved,
    Returned,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
enum ApprovedVariant {
    A,
    B,
}

impl ApprovedVariant {
    fn code(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::B => "B",
        }
    }

    fn report_id(self) -> &'static str {
        match self {
            Self::A => "report-view-1",
            Self::B => "report-view-2",
        }
    }

    fn strategy(self) -> visuals::Strategy {
        match self {
            Self::A => visuals::Strategy::CommerceCoverage,
            Self::B => visuals::Strategy::StoreParticipation,
        }
    }
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
            strategy: None,
            portfolio: None,
            agent_error: false,
            denied_models: Vec::new(),
            has_alternative: false,
            route_replacement_needed: None,
            approval_readiness: None,
            platform_report_data: false,
            approval_quotes: None,
            approval_inventory_issue: None,
            approval_inventory_review: None,
            login: PlatformLogin::default(),
            reach_checks: Vec::new(),
            route_overviews: Vec::new(),
            approved_variant: None,
            review: ReviewState::Pending,
            report_saved: false,
            review_note: String::new(),
            created_at: None,
            started: Some(Instant::now()),
            generation_ms: None,
        };
        let received = if task.options.goal_needs_confirmation() {
            "任务已创建，已记录投放范围。客户主目标待确认。"
        } else {
            "任务已创建，已记录投放范围和主目标。"
        };
        task.note(INTAKE, StepState::Done, received);
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
        if let Some(core) = &task.options.core_circle {
            task.note(
                INTAKE,
                StepState::Done,
                format!("已指定核心目标圈 · {core}。"),
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
                self.step_order()
                    .into_iter()
                    .rev()
                    .find(|&index| !matches!(self.steps[index].state, StepState::Pending))
            })
            .unwrap_or(INTAKE)
    }

    fn step_order(&self) -> [usize; 5] {
        if !self.options.lock_snapshot && self.options.source.platform() && !self.is_demo() {
            [INTAKE, STRATEGY, DATA, MIX, REPORT]
        } else {
            [INTAKE, DATA, STRATEGY, MIX, REPORT]
        }
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
        if self.report_saved && !self.is_demo() {
            return "已完成 · HTML 已生成";
        }
        match self.state {
            TaskState::Active => "正在生成",
            TaskState::Demo => "已生成 · 合成样例",
            TaskState::Done if self.legacy_pending_review() => "已生成 · 待复核证据",
            TaskState::Done if self.agents == 3 => "已生成 · 三个 Agent 已响应",
            TaskState::Done if self.agents > 0 => "已生成 · 含 Agent 响应",
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
            "获取平台数据"
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

    fn list_status(&self) -> (&'static str, &'static str) {
        if self.report_saved && !self.is_demo() {
            return ("已完成", "success");
        }
        match self.state {
            TaskState::Active => ("任务中", "info"),
            TaskState::Demo => ("演示样例", "info"),
            TaskState::Done if matches!(self.review, ReviewState::Approved) => {
                ("已完成", "success")
            }
            TaskState::Done if matches!(self.review, ReviewState::Returned) => ("已退回", "error"),
            TaskState::Done if self.approval_inventory_issue.is_some() => {
                ("库存复核待处理", "info")
            }
            TaskState::Done if self.legacy_pending_review() => ("待核对", "warning"),
            TaskState::Done => ("待审核", "warning"),
            TaskState::Review => ("待核对", "warning"),
            TaskState::Failed => ("失败", "error"),
        }
    }

    fn running_step(&self) -> Option<&'static str> {
        self.state.active().then(|| LABELS[self.current_step()])
    }

    fn budget(&self) -> u32 {
        self.options.budget.unwrap_or(super::SHOWCASE_BUDGET)
    }
    fn fixed_showcase_budget(&self) -> bool {
        self.options.fixed_showcase
    }
    fn planning_policy(&self) -> Policy {
        Policy {
            emphasis: self.priority.unwrap_or_default(),
            hard_cap: self.options.hard_cap,
            confirmed_focus: self.options.confirmed_comparison_focus().is_some(),
        }
    }
    fn is_demo(&self) -> bool {
        self.options.demo == Some(1)
    }

    fn legacy_pending_review(&self) -> bool {
        self.state.done()
            && self.review.pending()
            && (self.approval_readiness.is_none()
                || self.approval_quotes.is_none()
                || self.options.goal_needs_confirmation())
    }

    fn approval_evidence_complete(&self) -> bool {
        self.state.done()
            && !self.is_demo()
            && !self.options.goal_needs_confirmation()
            && self.approval_quotes.is_some()
            && self
                .approval_readiness
                .is_some_and(ApprovalReadiness::complete)
    }

    fn ready_for_review(&self) -> bool {
        self.review.pending() && self.approval_evidence_complete()
    }

    fn confirmed_delivery(&self) -> Option<ApprovedVariant> {
        self.approved_variant.filter(|_| {
            matches!(self.review, ReviewState::Approved) && self.approval_evidence_complete()
        })
    }

    fn can_publish_report(&self) -> bool {
        self.is_demo() || (self.platform_report_data && self.confirmed_delivery().is_some())
    }

    fn budget_confirmation_required(&self) -> bool {
        self.approval_quotes.is_some_and(|quotes| {
            [quotes.a, quotes.b]
                .into_iter()
                .any(|high| super::budget_gap_needs_confirmation(self.budget(), high))
        })
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

    fn can_retry_agents(&self) -> bool {
        self.agent_error && self.can_evaluate_budget() && self.portfolio.is_some()
    }

    fn can_use_local_snapshot(&self) -> bool {
        self.options.demo != Some(1)
            && !self.state.active()
            && (matches!(self.state, TaskState::Review | TaskState::Failed)
                || self.legacy_pending_review()
                || matches!(self.review, ReviewState::Returned))
            && self
                .options
                .starts
                .as_deref()
                .zip(self.options.ends.as_deref())
                .is_some_and(|(starts, ends)| period_months(starts, ends).is_some())
    }

    fn can_review_same_inventory(&self) -> bool {
        self.can_use_local_snapshot()
            && self.has_pinned_data()
            && (self.portfolio.is_some() || self.legacy_pending_review())
    }

    fn can_refresh_evidence(&self) -> bool {
        self.can_evaluate_budget() && self.portfolio.is_some()
    }

    fn can_review_operations(&self) -> bool {
        self.can_evaluate_budget() && self.options.rolling.is_some() && self.portfolio.is_some()
    }

    fn can_confirm_goal(&self) -> bool {
        self.options.demo != Some(1)
            && !self.state.active()
            && !matches!(self.review, ReviewState::Approved)
            && self.options.goal_needs_confirmation()
    }

    fn campaign(&self) -> super::brief::CampaignView<'_> {
        super::brief::CampaignView::new(&self.options)
    }

    fn can_revise_campaign(&self) -> bool {
        !self.is_demo() && !self.state.active()
    }

    fn schedule_dates(&self) -> Option<(&str, &str)> {
        if !self.can_revise_campaign() {
            return None;
        }
        let (starts, ends) = self
            .options
            .starts
            .as_deref()
            .zip(self.options.ends.as_deref())?;
        period_months(starts, ends).map(|_| (starts, ends))
    }

    fn can_replace_route(&self) -> bool {
        matches!(self.state, TaskState::Review)
            && !self.is_demo()
            && self.portfolio.is_some()
            && self.has_pinned_data()
            && !self.options.required_routes.is_empty()
            && self.route_replacement_needed.unwrap_or(true)
    }

    fn generation_time(&self) -> Option<String> {
        self.generation_ms
            .map(|ms| format!("{:.2} 秒", ms as f64 / 1000.0))
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
    is_admin: bool,
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
    for step in &mut task.steps {
        for note in &mut step.notes {
            if note.contains("取数") {
                *note = note
                    .replace("取数", "获取数据")
                    .replace("获获取数据", "获取数");
            }
        }
    }
    task.review = ReviewState::parse(&row.get::<_, String>(2)?);
    task.review_note = row.get(3)?;
    task.created_at = row.get(4)?;
    task.report_saved = row.get("report_saved")?;
    if task.report_saved && matches!(task.steps[REPORT].state, StepState::Review) {
        task.steps[REPORT].state = StepState::Done;
        if let Some(note) = task.steps[REPORT].notes.last_mut() {
            *note = "可交互 HTML 已生成，可以直接浏览。".into();
        }
    }
    if task.is_demo() && task.state.done() {
        task.state = TaskState::Demo;
        task.approved_variant = None;
    }
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
    let mut task = task.clone();
    task.report_saved = report.is_some();
    if !task.state.active() {
        task.generation_ms = task
            .started
            .and_then(|started| u64::try_from(started.elapsed().as_millis()).ok());
    }
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
        let report = report
            .map(|html| {
                Ok::<_, rusqlite::Error>(crate::web::stamp_report(
                    html,
                    &crate::web::report_clock(&tx)?,
                ))
            })
            .transpose()?;
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
    UseSnapshot(Uuid),
    Recheck(Option<Portfolio>),
    RefreshEvidence(Portfolio),
    ReviewOperations(Portfolio),
    ReviseCampaign(Option<Portfolio>),
    ReviseSchedule { local: bool },
    ReplaceRoute(String),
    RetryAgents,
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
    let models = agents.current();
    if matches!(snapshot_policy, SnapshotPolicy::RetryAgents) && !models.available() {
        return Err((
            StatusCode::CONFLICT,
            "请先配置可用的模型，再重新运行 Agent。",
        ));
    }
    let mut options = normalize_options(options)?;
    options.lock_snapshot = !matches!(
        snapshot_policy,
        SnapshotPolicy::Fresh | SnapshotPolicy::ReviseSchedule { local: false }
    );
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
                "获取平台数据需要指定 2027 年内的有效起止日期。",
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
    if task.options.lock_snapshot
        && let Some(id) = previous_task
    {
        let parent = get_task(&store, id).await?;
        if !matches!(snapshot_policy, SnapshotPolicy::RetryAgents)
            && task.options.campaign_goal() == parent.options.campaign_goal()
        {
            task.strategy = parent.strategy_for_revision();
            task.options.compare_priority = parent.priority.or(task.options.compare_priority);
        }
        task.route_overviews = parent.route_overviews;
    }
    let revision = match snapshot_policy {
        SnapshotPolicy::UseSnapshot(snapshot) => Some((
            None,
            "使用快照生成",
            format!("已选定库存快照 {snapshot}，按其完整档期核算。"),
        )),
        SnapshotPolicy::Locked(Some(portfolio)) => {
            Some((Some(portfolio), "补证复核", EVIDENCE_ONLY.to_owned()))
        }
        SnapshotPolicy::Recheck(portfolio) => {
            let note = if portfolio.is_some() {
                "沿用原快照和采购清单，按当前规则复核历史报告。"
            } else {
                "沿用原快照重新生成 A／B 方案，按当前规则核验证据。"
            };
            Some((portfolio, "历史证据复核", note.to_owned()))
        }
        SnapshotPolicy::RefreshEvidence(portfolio) => Some((
            Some(portfolio),
            "按采购清单重新取证",
            "已锁定现有 A／B 清单、红豆库存和报价。本次只查询新云证据。".to_owned(),
        )),
        SnapshotPolicy::ReviewOperations(portfolio) => Some((
            Some(portfolio),
            "施工复核",
            "已沿用原 A／B 采购清单和报价，本次复核施工窗口与产能。".to_owned(),
        )),
        SnapshotPolicy::ReviseCampaign(portfolio) => Some((
            portfolio,
            "修订客户假设书",
            "已锁定原档期和库存。按修订后的客户假设书完成提案复核。".to_owned(),
        )),
        SnapshotPolicy::ReviseSchedule { local } => {
            task.options.schedule_revision.as_ref().map(|revision| {
                let action = if local {
                    "核对同档期快照"
                } else {
                    "按新档期重新获取数据"
                };
                let operation_note = if revision.review_operations {
                    "。施工窗口待运营重新确认"
                } else {
                    ""
                };
                (
                    None,
                    "调整投放档期",
                    format!(
                        "{} 至 {} → {} 至 {} · {} · {}{operation_note}。",
                        revision.previous_start,
                        revision.previous_end,
                        task.options.starts.as_deref().unwrap_or_default(),
                        task.options.ends.as_deref().unwrap_or_default(),
                        revision.reason,
                        action,
                    ),
                )
            })
        }
        SnapshotPolicy::ReplaceRoute(note) => Some((None, "客户确认替换必选线路", note)),
        SnapshotPolicy::RetryAgents => Some((
            None,
            "重试 Agent",
            "沿用原需求、预算和库存快照，使用当前模型重新运行协作，生成新提案版本。".to_owned(),
        )),
        _ => None,
    }
    .map(|(portfolio, action, note)| {
        task.portfolio = portfolio;
        task.note(INTAKE, StepState::Done, &note);
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
            audit::write(&tx, "任务", action, Some(id), &note)?;
        }
        tx.commit()?;
        Ok(true)
    })
    .await?;
    if !created {
        return Err(BUSY);
    }

    tokio::spawn(async move {
        if execute(&store, &models, refresh, &actor, &mut task)
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
    Form(mut options): Form<Options>,
) -> Result<Redirect, WebError> {
    if options.demo == Some(1)
        && options.starts.as_deref() == Some(super::SHOWCASE_START)
        && options.ends.as_deref() == Some(super::SHOWCASE_END)
    {
        options.rolling = Some(
            super::schedule::Rolling::showcase(
                super::schedule::SHOWCASE_PHASES,
                super::SHOWCASE_START,
                super::SHOWCASE_END,
            )
            .expect("showcase phases are within the showcase year"),
        );
        options.brief = Some(super::brief::showcase_brief());
    }
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

async fn start_from_snapshot(
    State(store): State<Store>,
    Extension(agents): Extension<Agents>,
    Extension(refresh): Extension<Refresh>,
    Extension(actor): Extension<User>,
    Path(id): Path<Uuid>,
) -> Result<Redirect, WebError> {
    let snapshot = super::data::snapshot(&store, id).await?;
    let rolling = super::schedule::Rolling::for_snapshot(&snapshot.starts, id);
    start(
        store,
        agents,
        refresh,
        actor,
        Options {
            budget: Some(super::SHOWCASE_BUDGET),
            fixed_showcase: true,
            starts: Some(snapshot.starts),
            ends: Some(snapshot.ends),
            media: Some(snapshot.selection),
            source: Source::Local,
            rolling: Some(rolling),
            ..Options::default()
        },
        None,
        SnapshotPolicy::UseSnapshot(id),
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
    brief_comparison_focus: Option<String>,
    brief_audience: Option<String>,
    brief_region: Option<String>,
    brief_monitoring: Option<String>,
    #[serde(flatten)]
    trial: super::brief::TrialMeasurementInput,
    target_centers: Option<String>,
    core_circle: Option<String>,
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
        return Err(invalid("新提案统一从平台获取数据。"));
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
    let comparison_focus = input
        .brief_comparison_focus
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| ComparisonFocus::parse(value).ok_or(invalid("请选择有效的方案比选指标。")))
        .transpose()?;
    let goal = input.brief_campaign_goal.unwrap_or_default();
    let trial_measurement = input.trial.parse(&goal, None).map_err(invalid)?;
    let mut brief = super::brief::BriefContext {
        customer_profile: input.brief_customer_profile.unwrap_or_default(),
        objective: input.brief_objective.unwrap_or_default(),
        campaign_goal: goal,
        comparison_focus,
        audience: input.brief_audience.unwrap_or_default(),
        region: input.brief_region.unwrap_or_default(),
        monitoring: input.brief_monitoring.unwrap_or_default(),
        trial_measurement,
    };
    if !brief.normalize() {
        return Err(invalid("Brief 补充说明每项最多 150 个字符。"));
    }
    if (input.brief_used.is_some() || !brief.campaign_goal.is_empty())
        && super::brief::campaign_goal(&brief.campaign_goal).is_none()
    {
        return Err(invalid(
            "请先在客户假设书中选择到店、认知、促销或拉新作为主目标。",
        ));
    }
    let target_centers = parse_target_centers(input.target_centers.as_deref().unwrap_or_default())
        .map_err(invalid)?;
    if (input.brief_used == Some(1) || !brief.is_empty() || !target_centers.is_empty())
        && input.brief_confirmed != Some(1)
    {
        return Err(invalid("请确认客户假设书、目标圈与投放条件。"));
    }
    if input
        .core_circle
        .as_ref()
        .is_some_and(|core| !core.trim().is_empty())
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
            core_circle: input.core_circle,
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

#[derive(Deserialize)]
struct ConfirmGoal {
    goal: String,
}

#[derive(Deserialize)]
struct CampaignRevision {
    customer_profile: String,
    objective: String,
    goal: String,
    audience: String,
    region: String,
    monitoring: String,
    #[serde(flatten)]
    trial: super::brief::TrialMeasurementInput,
    attested: Option<u8>,
}

async fn revise_campaign(
    State(store): State<Store>,
    Extension(agents): Extension<Agents>,
    Extension(refresh): Extension<Refresh>,
    Extension(actor): Extension<User>,
    Path(id): Path<Uuid>,
    Form(input): Form<CampaignRevision>,
) -> Result<Redirect, WebError> {
    let task = get_task(&store, id).await?;
    if !task.can_revise_campaign() {
        return Err((StatusCode::CONFLICT, "请在正式任务完成当前步骤后修订。"));
    }
    if input.attested != Some(1) {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "请确认假设书已脱敏并核对。",
        ));
    }
    let same_goal = task.options.campaign_goal() == super::brief::campaign_goal(&input.goal);
    let compare = task.options.confirmed_comparison_focus();
    let trial_measurement = input
        .trial
        .parse(
            &input.goal,
            task.options
                .brief
                .as_ref()
                .and_then(|brief| brief.trial_measurement.as_ref()),
        )
        .map_err(|message| (StatusCode::UNPROCESSABLE_ENTITY, message))?;
    let mut options = task.options.clone();
    let mut brief = super::brief::BriefContext {
        customer_profile: input.customer_profile,
        objective: input.objective,
        campaign_goal: input.goal,
        audience: input.audience,
        region: input.region,
        monitoring: input.monitoring,
        comparison_focus: same_goal.then_some(compare).flatten(),
        trial_measurement,
    };
    if !brief.normalize() {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, "每项内容限 150 个字符。"));
    }
    options.brief = Some(brief);
    if !super::brief::CampaignView::new(&options)
        .missing()
        .is_empty()
    {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "请填写脱敏品牌规模、生意目标、主目标、目标人群与区域、成功衡量及监测。",
        ));
    }
    options.compare = None;
    options.compare_priority = None;
    let policy = if task.has_pinned_data() {
        options.source = Source::Local;
        SnapshotPolicy::ReviseCampaign(if same_goal { task.portfolio } else { None })
    } else {
        SnapshotPolicy::Fresh
    };
    start(store, agents, refresh, actor, options, Some(id), policy).await
}

async fn confirm_goal(
    State(store): State<Store>,
    Extension(agents): Extension<Agents>,
    Extension(refresh): Extension<Refresh>,
    Extension(actor): Extension<User>,
    Path(id): Path<Uuid>,
    Form(input): Form<ConfirmGoal>,
) -> Result<Redirect, WebError> {
    let task = get_task(&store, id).await?;
    if !task.can_confirm_goal() {
        return Err((StatusCode::CONFLICT, "这项任务当前没有待确认的投放目标。"));
    }
    let goal = super::brief::campaign_goal(&input.goal).ok_or((
        StatusCode::UNPROCESSABLE_ENTITY,
        "请选择到店、认知、促销或拉新。",
    ))?;
    let policy = if task.has_pinned_data() {
        SnapshotPolicy::Locked(None)
    } else {
        SnapshotPolicy::Fresh
    };
    let mut options = task.options;
    options
        .brief
        .get_or_insert_with(Default::default)
        .campaign_goal = goal.to_owned();
    options.compare = None;
    options.compare_priority = None;
    if !matches!(policy, SnapshotPolicy::Fresh) {
        options.source = Source::Local;
    }
    start(store, agents, refresh, actor, options, Some(id), policy).await
}

async fn retry_agents(
    State(store): State<Store>,
    Extension(agents): Extension<Agents>,
    Extension(refresh): Extension<Refresh>,
    Extension(actor): Extension<User>,
    Path(id): Path<Uuid>,
) -> Result<Redirect, WebError> {
    let task = get_task(&store, id).await?;
    if !task.can_retry_agents() {
        return Err((
            StatusCode::CONFLICT,
            "当前任务没有可复跑的 Agent 异常版本或已锁定的库存。",
        ));
    }
    let mut options = task.options;
    options.source = Source::Local;
    options.compare = None;
    options.compare_priority = None;
    start(
        store,
        agents,
        refresh,
        actor,
        options,
        Some(id),
        SnapshotPolicy::RetryAgents,
    )
    .await
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
            "预算重算需要已获取数据的原提案，请选择 100、200 或 500 万元。",
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
    if same_inventory && !task.can_review_same_inventory() {
        return Err((
            StatusCode::CONFLICT,
            "当前任务没有可沿用的采购清单，请按同档期快照生成。",
        ));
    }
    let recheck = same_inventory && task.legacy_pending_review();
    let mut options = task.options;
    if evaluation {
        options.fixed_showcase = false;
    }
    let snapshot_policy = if recheck {
        options.source = Source::Local;
        SnapshotPolicy::Recheck(task.portfolio)
    } else if local_snapshot {
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

pub(super) async fn check_supplement_context(
    store: &Store,
    task: Uuid,
    snapshot: Uuid,
) -> Result<Option<u32>, WebError> {
    let task = get_task(store, task).await?;
    let pins = snapshot_pins(&task.options);
    let store = store.clone();
    let newer = database(move || {
        let db = store.connect()?;
        let mut query = db.prepare(
            "SELECT EXISTS (SELECT 1 FROM media_snapshots AS latest
                            WHERE latest.evidence_root=coalesce(pinned.evidence_root,pinned.id)
                              AND latest.rowid>pinned.rowid)
             FROM media_snapshots AS requested
             JOIN media_snapshots AS pinned ON pinned.id=?2
             WHERE requested.id=?1 AND requested.industry='food'
               AND coalesce(requested.evidence_root,requested.id)=coalesce(pinned.evidence_root,pinned.id)",
        )?;
        for pin in pins.into_values() {
            let newer = query
                .query_row([snapshot.to_string(), pin.to_string()], |row| row.get::<_, bool>(0))
                .optional()?;
            if newer.is_some() {
                return Ok(newer);
            }
        }
        Ok(None)
    })
    .await?
    .ok_or((StatusCode::NOT_FOUND, "该快照不属于原提案的库存版本。"))?;
    Ok(
        (newer && task.can_review_same_inventory() && !task.legacy_pending_review())
            .then(|| task.budget()),
    )
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
        is_admin: user.admin,
        operations_open,
    }))
}

#[derive(Deserialize)]
struct ReviewForm {
    decision: ReviewState,
    note: String,
    variant: Option<String>,
    accept_budget_gap: Option<u8>,
}

async fn inventory_for_approval(
    store: &Store,
    task: &Task,
    choice: ApprovedVariant,
) -> Result<Vec<crate::refresh::InventoryCheck>, WebError> {
    const MISSING: WebError = (
        StatusCode::CONFLICT,
        "当前版本的采购清单与锁定库存无法对应。请重新生成提案后审核。",
    );
    let portfolio = task.portfolio.as_ref().ok_or(MISSING)?;
    let strategy = choice.strategy();
    let options = Options {
        lock_snapshot: true,
        ..task.options.clone()
    };
    let checks = if let Some(rolling) = &options.rolling {
        super::schedule::Calendar::load(store, &options, rolling)
            .await?
            .selected_inventory(&options, task.budget(), portfolio, strategy)
    } else if options.batch.is_some() {
        let snapshot = super::read_snapshot_matching(store.clone(), &options, true)
            .await?
            .ok_or(MISSING)?;
        let months = period_months(&snapshot.starts, &snapshot.ends).ok_or(MISSING)?;
        let (_, proposal) = portfolio
            .proposal(
                strategy,
                &snapshot,
                options.media.unwrap_or(Selection::Both),
                months,
                &options.required_routes,
            )
            .ok_or(MISSING)?;
        super::selected_inventory(&snapshot.starts, &snapshot.ends, &proposal)
            .map(|check| vec![check])
    } else {
        None
    };
    checks.filter(|items| !items.is_empty()).ok_or(MISSING)
}

async fn record_approval_inventory_issue(
    store: &Store,
    id: Uuid,
    choice: ApprovedVariant,
    failure: &InventoryFailure,
) -> Result<(), WebError> {
    let store = store.clone();
    let message = failure.error.1;
    let details = json!(ApprovalInventoryReview {
        variant: choice,
        issues: failure.issues.clone(),
    });
    let audit_detail = if failure.issues.is_empty() {
        format!("方案 {} · {message}", choice.code())
    } else {
        json!({"方案": choice.code(), "说明": message, "明细": details["issues"]}).to_string()
    };
    database(move || {
        let mut db = store.connect()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE media_tasks SET payload=json_set(payload, '$.approval_inventory_issue', ?2,
                    '$.approval_inventory_review', json(?3))
             WHERE id=?1 AND active=0 AND deleted=0 AND review='pending'",
            params![id.to_string(), message, details.to_string()],
        )?;
        if changed == 1 {
            audit::write(&tx, "平台", "审批库存复核未通过", Some(id), &audit_detail)?;
        }
        tx.commit()
    })
    .await
}

async fn review_task(
    State(store): State<Store>,
    Extension(user): Extension<User>,
    Extension(refresh): Extension<Refresh>,
    Path(id): Path<Uuid>,
    Form(form): Form<ReviewForm>,
) -> Result<Redirect, WebError> {
    const NOT_REVIEWABLE: WebError = (StatusCode::CONFLICT, "这项任务已审核或没有可审核的报告。");
    if !user.admin {
        return Err((StatusCode::FORBIDDEN, "只有工作台管理员可以审核提案。"));
    }
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
    let task = get_task(&store, id).await?;
    if !task.ready_for_review() {
        return Err(NOT_REVIEWABLE);
    }
    let selected = if matches!(form.decision, ReviewState::Approved) {
        match form.variant.as_deref() {
            Some("A") => Some(ApprovedVariant::A),
            Some("B") => Some(ApprovedVariant::B),
            _ => {
                return Err((
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "请明确选择采用方案 A 或方案 B。",
                ));
            }
        }
    } else {
        None
    };
    let budget_acknowledgement = if let Some((choice, quotes)) = selected.zip(task.approval_quotes)
    {
        let high = quotes.high(choice);
        let target = task.budget();
        if super::budget_gap_needs_confirmation(target, high) {
            if form.accept_budget_gap != Some(1) || note.is_empty() {
                return Err((
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "该方案的高位报价超出预算参考区间。请勾选确认，并记录客户同意的原因。",
                ));
            }
            format!(" · 已确认预算偏差（目标 {target} 万元，高位报价 {high} 元）")
        } else {
            String::new()
        }
    } else {
        String::new()
    };
    let checked_periods = if let Some(choice) = selected {
        let result = async {
            let checks = inventory_for_approval(&store, &task, choice).await?;
            refresh
                .verify_redbean_inventory(&user.identifier, &checks)
                .await?;
            Ok::<_, InventoryFailure>(checks.len())
        }
        .await;
        match result {
            Ok(count) => count,
            Err(failure) => {
                if failure.error.0 != StatusCode::TOO_MANY_REQUESTS {
                    record_approval_inventory_issue(&store, id, choice, &failure).await?;
                }
                return Err(failure.error);
            }
        }
    } else {
        0
    };
    let changed = database(move || {
        let mut db = store.connect()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            &format!(
                "UPDATE media_tasks SET review=?1, review_note=?2,
                payload=json_set(json_remove(payload, '$.approval_inventory_issue',
                    '$.approval_inventory_review'), '$.approved_variant', ?4)
             WHERE id=?3 AND active=0 AND deleted=0 AND review='pending'
               AND {FORMAL_REPORT} AND {APPROVAL_READY}"
            ),
            params![
                form.decision.key(),
                note,
                id.to_string(),
                selected.map(ApprovedVariant::code)
            ],
        )?;
        if changed == 1 {
            let detail = if let Some(choice) = selected {
                audit::write(
                    &tx,
                    "平台",
                    "审批库存复核通过",
                    Some(id),
                    &format!(
                        "方案 {} · 已查询红豆 {checked_periods} 段档期并核对全部所选资源余量。",
                        choice.code()
                    ),
                )?;
                format!(
                    "{} · 采用方案 {}{budget_acknowledgement} · {note}",
                    user.identifier,
                    choice.code()
                )
            } else {
                format!("{} · {note}", user.identifier)
            };
            audit::write(&tx, "人工审核", form.decision.label(), Some(id), &detail)?;
        }
        tx.commit()?;
        Ok(changed == 1)
    })
    .await?;
    if !changed {
        return Err(NOT_REVIEWABLE);
    }
    Ok(Redirect::to(&format!("/media-proposals/tasks/{id}")))
}

async fn delete_task(
    State(store): State<Store>,
    Extension(user): Extension<User>,
    Path(id): Path<Uuid>,
) -> Result<Redirect, WebError> {
    if !user.admin {
        return Err((StatusCode::FORBIDDEN, "只有工作台管理员可以删除提案任务。"));
    }
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

fn task_queues_sql() -> String {
    format!(
        "WITH task_state AS (
            SELECT {TASK_COLUMNS}, {TASK_TABLE_COLUMNS},
                   CASE WHEN json_extract(payload, '$.state')='done'
                        AND json_extract(payload, '$.options.demo')=1 THEN 'demo'
                        ELSE json_extract(payload, '$.state') END AS state,
                   coalesce(json_extract(payload, '$.options.demo'),0)=1 AS is_demo,
                   report IS NULL AS missing_report,
                   coalesce(({APPROVAL_READY}), 0) AS approval_ready,
                   json_type(payload, '$.approval_inventory_issue') IS 'text' AS inventory_blocked
            FROM media_tasks
            WHERE deleted=0 AND json_extract(payload, '$.options.industry')='food'
        ), tasks AS (
            SELECT *,
                (active=0 AND state='done' AND review='pending' AND NOT missing_report
                    AND approval_ready AND NOT inventory_blocked) AS awaiting_confirmation,
                (active=0 AND state<>'demo' AND NOT is_demo AND (
                    review='returned' OR state IN ('review','failed')
                    OR (state='done' AND review='pending' AND NOT approval_ready)
                    OR (review='pending' AND inventory_blocked)
                    OR missing_report OR json_extract(payload, '$.agent_error')=1)) AS has_exception
            FROM task_state
        )"
    )
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
            "{}, matched AS (
             SELECT payload, active, review, review_note, created_at, report_saved, sequence, updated_at FROM tasks
             WHERE CASE ?1
                 WHEN 'all' THEN 1
                 WHEN 'active' THEN active=1
                 WHEN 'pending' THEN awaiting_confirmation
                 WHEN 'approved' THEN state='done' AND NOT missing_report AND review='approved'
                 WHEN 'failed' THEN active=0 AND state IN ('active','failed')
                 ELSE has_exception
               END
               AND CASE WHEN EXISTS (
                   SELECT 1 FROM media_tasks WHERE rowid=?6 AND deleted=0
                       AND json_extract(payload, '$.options.industry')='food'
               ) THEN sequence=?6
               ELSE (?2='' OR instr(payload,?2)>0 OR instr(review_note,?2)>0 OR instr(?3,?2)>0
                     OR instr(created_at,?2)>0 OR instr(updated_at,?2)>0
                     OR instr(?5 || ' ' || coalesce(json_extract(payload, '$.options.budget'), ?4) || '万'
                           || coalesce(' ' || substr(strftime('%Y%m%d', created_at),3),''),replace(?2,'+',' '))>0)
               END)",
            task_queues_sql(),
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

async fn progress(
    State(store): State<Store>,
    Extension(user): Extension<User>,
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
        "state": if task.report_saved && !task.is_demo() { TaskState::Done } else { task.state },
        "status": task.status(),
        "summary": task.summary(),
        "approved_variant": task.approved_variant,
        "approval_readiness": task.approval_readiness,
        "offer_review": user.admin && task.ready_for_review(),
        "offer_report": task.report_saved,
        "offer_formal_delivery": task.can_publish_report() && !task.is_demo(),
        "budget_confirmation_required": task.budget_confirmation_required(),
        "approval_inventory_issue": task.approval_inventory_issue,
        "approval_inventory_review": task.approval_inventory_review,
        "generation_time": task.generation_time(),
        "has_agent_trace": !task.agent_decisions.is_empty(),
        "current_step": task.current_step(),
        "login": task.login,
        "platform_interrupted": task.login.interrupted(),
        "offer_evaluation": task.can_evaluate_budget(),
        "offer_goal_confirmation": task.can_confirm_goal(),
        "offer_schedule_revision": task.schedule_dates().is_some(),
        "offer_route_replacement": task.can_replace_route(),
        "offer_local_snapshot": task.can_use_local_snapshot(),
        "offer_same_inventory": task.can_review_same_inventory(),
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
    delivery: Option<u8>,
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
    Extension(user): Extension<User>,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    Path(id): Path<Uuid>,
    Query(options): Query<ReportOptions>,
) -> Result<Response, WebError> {
    let (task, html, legacy_generated_at) = database(move || {
        store.connect()?.query_row(
            &format!("SELECT {TASK_COLUMNS}, report,
                (SELECT strftime('%Y-%m-%dT%H:%M:%S+08:00', min(created_at), '+8 hours') FROM audit_events
                 WHERE task_id=media_tasks.id AND action='报告交付'
                 AND (detail LIKE '可交互 HTML 已渲染并保存%' OR detail LIKE '合成演示 HTML 已保存%'
                   OR detail LIKE '已保存 A／B 方案草案%')) AS legacy_generated_at
                FROM media_tasks WHERE id=?1 AND deleted=0 AND json_extract(payload, '$.options.industry')='food'"),
            [id.to_string()],
            |row| Ok((decode(row)?, row.get::<_, Option<String>>("report")?,
                row.get::<_, Option<String>>("legacy_generated_at")?)),
        ).optional()
    })
    .await?
    .ok_or(NOT_FOUND)?;
    let workpaper = uri.path().ends_with("/review-copy");
    if workpaper && !user.admin {
        return Err((StatusCode::FORBIDDEN, "内部核验副本仅供管理员查阅。"));
    }
    let approved = task.confirmed_delivery();
    if options.delivery == Some(1) && (approved.is_none() || !task.platform_report_data) {
        return Err((
            StatusCode::CONFLICT,
            "正式报告需两案均取得完整的平台数据，并通过库存复核和人工确认。请查看任务核验清单。",
        ));
    }
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
    let html = match legacy_generated_at {
        Some(timestamp) if !html.contains("class=\"report-generated\"") => {
            crate::web::stamp_report(html, &timestamp)
        }
        _ => html,
    };
    if options.delivery == Some(1) && report_has_open_items(&html) {
        return Err((
            StatusCode::CONFLICT,
            "报告仍有未完成项，请核对后生成新版本。",
        ));
    }
    const MAIN: &str = "<main id=\"main\" tabindex=\"-1\">";
    let html = if task.is_demo() && !html.contains("data-demo-report") {
        html.replacen(
            MAIN,
            &format!("{MAIN}<p class=\"media-notice report-draft\" data-demo-report><strong>合成样例 · 仅供演示</strong>。</p>"),
            1,
        )
    } else if let Some(choice) = approved {
        let selection = format!(
            "{MAIN}<p class=\"media-notice\" data-selected-variant=\"{}\">采用方案 {}</p>",
            choice.code(),
            choice.code(),
        );
        html.replacen(MAIN, &selection, 1)
            .replacen(
                "id=\"report-view-all\" checked",
                "id=\"report-view-all\"",
                1,
            )
            .replacen(
                &format!("id=\"{}\"", choice.report_id()),
                &format!("id=\"{}\" checked", choice.report_id()),
                1,
            )
    } else {
        html
    };
    let html = if workpaper {
        html.replace(
            "<strong>备选草案 · 待确认</strong>",
            "<strong>投放条件说明</strong>",
        )
        .replace(
            "<strong>备选草案 · 待客户确认</strong>",
            "<strong>媒介与档期条件</strong>",
        )
        .replacen(
            MAIN,
            &format!(
                "{MAIN}<p class=\"media-notice report-draft\"><strong>内部核验副本</strong></p>"
            ),
            1,
        )
    } else if !task.is_demo() && options.delivery != Some(1) {
        report_delivery::for_browsing(html)
    } else {
        html
    };
    export_response(
        html,
        workpaper || options.preview == Some(1),
        task.options.industry,
        task.options.media.unwrap_or(Selection::Both),
        task.budget(),
        task.options.starts.as_deref().unwrap_or("2027"),
        if approved.is_some() {
            ReportFile::Confirmed
        } else {
            ReportFile::Standard
        },
    )
}

fn report_has_open_items(html: &str) -> bool {
    let Some((_, body)) = html.split_once("<body") else {
        return true;
    };
    [
        "待核",
        "待补",
        "待确认",
        "待复核",
        "未确认",
        "未核验",
        "未审批",
        "待审批",
        "审批中",
        "待审核",
        "未审核",
    ]
    .iter()
    .any(|label| body.contains(label))
}

#[derive(Clone, Copy, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Angle {
    #[default]
    Visit,
    Commute,
    Awareness,
    Promotion,
    Acquisition,
    Unspecified,
}

impl Angle {
    fn from_confirmed_goal(goal: &str) -> Option<Self> {
        match goal {
            "到店" => Some(Self::Visit),
            "认知" => Some(Self::Awareness),
            "促销" => Some(Self::Promotion),
            "拉新" => Some(Self::Acquisition),
            _ => None,
        }
    }

    fn comparison_focus(self) -> ComparisonFocus {
        match self {
            Self::Visit | Self::Promotion | Self::Acquisition => ComparisonFocus::CoreBusDays,
            Self::Awareness => ComparisonFocus::EstimatedExposure,
            Self::Commute | Self::Unspecified => ComparisonFocus::Coverage,
        }
    }

    fn permitted_focuses(self, options: &Options) -> Vec<ComparisonFocus> {
        if let Some(focus) = options.confirmed_comparison_focus() {
            return vec![focus];
        }
        let has_bus = options.media.unwrap_or(Selection::Both) != Selection::Metro;
        match self.comparison_focus() {
            ComparisonFocus::EstimatedExposure => vec![
                ComparisonFocus::EstimatedExposure,
                ComparisonFocus::Coverage,
            ],
            ComparisonFocus::CoreBusDays if has_bus && options.core_circle.is_some() => {
                vec![ComparisonFocus::CoreBusDays, ComparisonFocus::Coverage]
            }
            _ => vec![ComparisonFocus::Coverage],
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Visit => "到店",
            Self::Commute => "通勤覆盖",
            Self::Awareness => "认知",
            Self::Promotion => "促销",
            Self::Acquisition => "拉新",
            Self::Unspecified => "待确认",
        }
    }

    fn copy(self) -> &'static str {
        match self {
            Self::Visit => {
                "以到访引导为重点，通过公交线路或地铁出入口帮助受众识别目的地和到达路径。"
            }
            Self::Commute => {
                "以通勤覆盖为重点，沿日常出行路径安排媒体，帮助同一受众在不同出行环节记住投放信息。"
            }
            Self::Awareness => {
                "以品牌认知为重点，结合目标圈覆盖与有出处的曝光估算，核对不同媒介组合的触达。"
            }
            Self::Promotion => {
                "以促销参与为重点，优先比较活动门店近场资源，按实际活动口径核对领取或核销。"
            }
            Self::Acquisition => {
                "以新客获取为重点，优先比较目标门店近场资源，按已确认的口径核对新增客户。"
            }
            Self::Unspecified => "客户主目标待确认。草案先按均衡优先级核算，确认目标后可重新生成。",
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

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
struct StrategyDecision {
    angle: Angle,
    reason: GoalReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    comparison_focus: Option<ComparisonFocus>,
    #[serde(default)]
    focus_from_model: bool,
}

impl StrategyDecision {
    fn focus(self, options: &Options) -> ComparisonFocus {
        options
            .confirmed_comparison_focus()
            .or(self.comparison_focus)
            .unwrap_or_else(|| self.angle.permitted_focuses(options)[0])
    }
}

impl Task {
    fn strategy_for_revision(&self) -> Option<StrategyDecision> {
        if let Some(previous) = self.strategy {
            return Some(previous);
        }
        if !matches!(
            self.steps[STRATEGY].state,
            StepState::Done | StepState::Fallback | StepState::Review
        ) {
            return None;
        }
        if let Some(angle) = self
            .options
            .campaign_goal()
            .and_then(Angle::from_confirmed_goal)
        {
            return Some(StrategyDecision {
                angle,
                reason: GoalReason::ConfirmedGoal,
                comparison_focus: None,
                focus_from_model: false,
            });
        }
        if self.options.goal_needs_confirmation() {
            return Some(StrategyDecision {
                angle: Angle::Unspecified,
                reason: GoalReason::NeedsConfirmation,
                comparison_focus: None,
                focus_from_model: false,
            });
        }
        self.agent_decisions
            .iter()
            .find(|decision| decision["role"] == STRATEGIST.id)
            .and_then(|decision| {
                serde_json::from_value::<StrategyDecision>(decision["adopted"].clone()).ok()
            })
            .filter(|decision| decision.reason == GoalReason::ExampleGoal)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Strategy {
    angle: Angle,
    #[serde(default)]
    reason: Option<GoalReason>,
    #[serde(default)]
    comparison_focus: Option<ComparisonFocus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    phases: Vec<super::schedule::PhaseChoice>,
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
    instructions: "confirmed_goal_category 有值时原样采用，reason 为 confirmed_goal。到店对应 visit、认知对应 awareness、促销对应 promotion、拉新对应 acquisition。示例用 visit、commute 或 awareness，reason 为 example_goal。正式需求待确认时返回 angle=unspecified、reason=needs_confirmation。comparison_focus 从 permitted_comparison_focuses 选择。客户指定指标时原样采用。到店、促销、拉新可选择核心圈公交车日 core_bus_days 或目标圈覆盖 coverage，无公交或核心圈未指明时优先 coverage。认知可选择可核验预估人次 estimated_exposure 或 coverage；目标圈较多时可优先 coverage。通勤覆盖使用 coverage，主目标待确认时 comparison_focus 为 null。schedule_candidate_months 非空且目标已确认时，按目标选择 1–3 个待查月份，返回 phases 数组，例如 [{\"month\":\"2027-08\",\"phase\":\"main\"}]。月份和阶段均不重复，按 warmup、main、followup 的先后顺序排列，必须包含 main。未收到可选月份或活动时间依据不足时可省略 phases，不假定任何活动、库存或价格。已明确的 confirmed_schedule 不得更改。只返回 angle、reason、comparison_focus 和可选 phases。按月规划客户可核验的效益。若输入已明确预热、主投或回访的档期，分别围绕小范围验证、核心目标集中覆盖、补足覆盖与复盘。日期和权重取自输入。客户目标与业务指标优先。没有真实活动时点、库存或触达依据时，只提出待核验的候选，不能推定季节需求、成交或转化表现。",
};
const PLANNER: Agent = Agent {
    id: "combination",
    name: "媒介组合 Agent",
    instructions: "根据已采用的 comparison_focus 为 A、B 选择共同的次要优化目标。只选择 permitted_priorities 中的一项，若 locked_priority 有值则沿用。客户选定比选指标时优先保障对应组合目标。预估人次须等取证后核验，不预测未取证资源的曝光。只返回 priority，例如 {\"priority\":\"nearby\"}。逐段查看 monthly_inventory 中的实际档期、库存是否取得、各媒体可售标志及刊例标志。预热优先可测覆盖和控制投入，主投优先客户主指标及核心点位，回访优先补充目标圈。按实际可执行月份选择约定的共同次要优先级，Rust 按每段库存和预算独立求解。缺项不推断有库存，不用别月数据替代。不得虚构或输出具体采购清单、触达值或 ROI。",
};
const REVIEWER: Agent = Agent {
    id: "verification",
    name: "报告核验 Agent",
    instructions: "核验 Rust 已算出的 A、B 候选。check 取 booking、access 或 materials，采购条件未齐选 booking。review_order 只排列 missing_checks 中的缺项，每项唯一。goal_pending 时返回 none/pending_goal。primary_metric_pending 时返回 none/pending_primary_metric。其余只推荐 placement_feasible 和 evidence_complete 均成立的候选。零案合格用 none/pending_evidence，仅一案合格用 only_feasible。两案合格时按 comparison_order 的顺序检查 core_bus_days、estimated_exposure、wider_coverage 或 budget_fit。wider_coverage 对应 coverage。选第一个有 higher 的指标及对应候选，跳过 equal 和 unknown。均无差异用 none/equal_tradeoff。只返回 JSON，例如 {\"check\":\"booking\",\"review_order\":[\"inventory\",\"reach\"],\"preferred_variant\":\"none\",\"reason\":\"pending_evidence\"}。结合 monthly_evidence 逐段核对库存、施工、覆盖和单位日触达。正式结论以已核证的档期结果为准。历史基准注明适用月份，不作为当月实测。客户设定的扫码提升是验收目标，不作成交、转化或效益承诺。存在缺证时按已列缺项输出，不用全年合计掩盖单段不可执行。",
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
            "contract": AGENT_CONTRACT,
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
    model: Option<&Arc<Model>>,
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
    if let Some((_, status)) = task
        .denied_models
        .iter()
        .find(|(denied, _)| Arc::ptr_eq(denied, model))
    {
        let reason = format!(
            "前序 Agent 已收到 HTTP {}，本步骤使用本地规则。",
            status.as_u16()
        );
        task.note(index, StepState::Fallback, &reason);
        save(store, task, None).await?;
        return Ok(Consultation {
            output: None,
            attempt_trace: Vec::new(),
            duration_ms: 0,
            cause: Some(reason),
            input,
        });
    }
    task.note(
        index,
        StepState::Running,
        format!("{} 已进入调用队列。", agent.name),
    );
    save(store, task, None).await?;
    let Some(_slot) = model.wait_for_slot().await else {
        let reason = "模型调用排队超时，本步骤已切换到本地规则。";
        task.agent_error = true;
        task.note(index, StepState::Fallback, reason);
        save(store, task, None).await?;
        return Ok(Consultation {
            output: None,
            attempt_trace: Vec::new(),
            duration_ms: started.elapsed().as_millis(),
            cause: Some(reason.into()),
            input,
        });
    };
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
    model.observe_connection(
        "提案任务",
        output.is_ok(),
        cause
            .clone()
            .unwrap_or_else(|| "模型已返回符合 Agent 契约的响应。".into()),
    );
    if output.is_ok() {
        task.agents += 1;
        task.note(
            index,
            StepState::Running,
            format!("已建立 Agent 通信，{}已返回并通过格式校验。", agent.name),
        );
    } else if let Err(error) = &output {
        task.agent_error = true;
        if let Some(status) = error.denied_status() {
            task.denied_models.push((model.clone(), status));
        }
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

async fn platform_review(
    store: &Store,
    task: &mut Task,
    phase: &str,
    reason: &str,
) -> Result<(), WebError> {
    task.login.phase = phase.into();
    task.note(DATA, StepState::Review, reason);
    task.state = TaskState::Review;
    audit::record(
        store,
        "平台",
        "获取数据未完成",
        Some(task.id),
        reason.into(),
    )
    .await?;
    save(store, task, None).await
}

enum PlatformCollection {
    Stopped,
    Continue(Option<PlanningContext>),
}

async fn imported_periods(
    store: &Store,
    id: Uuid,
) -> Result<std::collections::BTreeMap<String, Uuid>, WebError> {
    let batch_store = store.clone();
    database(move || {
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
    .await
}

async fn monthly_inventory_context(
    store: &Store,
    options: &Options,
) -> Result<Vec<Value>, WebError> {
    let Some(rolling) = &options.rolling else {
        return Ok(Vec::new());
    };
    Ok(super::schedule::Calendar::load(store, options, rolling)
        .await?
        .planning_context())
}

async fn collect_platform(
    store: &Store,
    refresh: &Refresh,
    actor: &User,
    task: &mut Task,
    mut strategy: Option<StrategyContext>,
    combination: Option<&Arc<Model>>,
) -> Result<PlatformCollection, WebError> {
    let options = &task.options;
    let Some((starts, ends)) = options
        .starts
        .as_ref()
        .zip(options.ends.as_ref())
        .filter(|(a, b)| period_months(a, b).is_some())
    else {
        platform_review(
            store,
            task,
            "error_dates",
            "获取平台数据需要指定 2027 年内的有效起止日期。",
        )
        .await?;
        return Ok(PlatformCollection::Stopped);
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
    let proof_only = proof.is_some();
    let (policy_ready, policy) =
        tokio::sync::watch::channel(proof_only.then(|| task.planning_policy()));
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
        core_circle: options.core_circle.clone(),
        policy,
        portfolio: Arc::new(
            if let Some(frozen) = task.portfolio.clone().filter(|_| proof.is_some()) {
                frozen.into()
            } else {
                Default::default()
            },
        ),
        evidence_plan: Arc::default(),
        saved_overview_routes: task
            .route_overviews
            .iter()
            .filter(|map| map.basemap_verified)
            .map(visuals::OfficialMap::route_keys)
            .collect(),
        proof: proof.clone(),
    };
    let portfolio = request.portfolio.clone();
    let manifest = request.evidence_plan.clone();
    task.login.phase = "opening".into();
    task.note(
        DATA,
        StepState::Running,
        if proof_only {
            format!("共 {count} 段已锁定采购档期，正在打开新云平台的官方登录窗口。")
        } else if refresh.redbean.uses_stored_account() {
            format!("共 {count} 段采购档期，工作台正在验证红豆预存账号和新云授权。")
        } else {
            format!("共 {count} 段采购档期，正在打开数据平台的官方登录窗口。")
        },
    );
    save(store, task, None).await?;
    let id = match refresh.launch(request, actor).await {
        Ok(id) => id,
        Err((_, message)) if proof_only => {
            record_evidence_refresh(store, task, "error_launch", message, 0).await?;
            return Ok(PlatformCollection::Continue(None));
        }
        Err((_, message)) if refresh.redbean.uses_stored_account() => {
            platform_review(store, task, "error_preflight", message).await?;
            return Ok(PlatformCollection::Stopped);
        }
        Err((_, message)) => {
            platform_review(store, task, "error_launch", message).await?;
            return Ok(PlatformCollection::Stopped);
        }
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
    let mut newcloud_fallback = false;
    let mut planning = None;
    let status = loop {
        let status = refresh.status(id)?;
        let state = status.state.as_str();
        let newcloud = status.newcloud && !task.login.newcloud;
        let redbean = status.redbean && !task.login.redbean;
        if newcloud {
            task.login.newcloud = true;
            let message = if status.newcloud_reused {
                "已复核并沿用当前工作台会话的新云授权。"
            } else {
                "新云平台登录已确认。"
            };
            task.note(DATA, StepState::Running, message);
            audit::record(store, "平台", "新云连接成功", Some(task.id), message.into()).await?;
        }
        if redbean {
            task.login.redbean = true;
            let message = if refresh.redbean.uses_stored_account() {
                "红豆预存账号已自动通过库存权限校验。"
            } else {
                "红豆平台登录已确认。"
            };
            task.note(DATA, StepState::Running, message);
            audit::record(store, "平台", "红豆连接成功", Some(task.id), message.into()).await?;
        }
        let evidence = status
            .evidence
            .filter(|evidence| Some(*evidence) != last_evidence);
        if state == "proof"
            && status.imported > 0
            && let Some(strategy) = strategy.take()
        {
            task.note(
                DATA,
                if status.imported == count {
                    StepState::Done
                } else {
                    StepState::Review
                },
                format!(
                    "两平台认证已通过，已导入 {} / {count} 段采购快照。",
                    status.imported
                ),
            );
            save(store, task, None).await?;
            if let Some(rolling) = &mut task.options.rolling {
                rolling.batches = imported_periods(store, id).await?;
            }
            let periods = monthly_inventory_context(store, &task.options).await?;
            planning =
                Some(prepare_combination(store, combination, task, strategy, &periods).await?);
            task.note(
                MIX,
                StepState::Running,
                "正在按已核验库存和 Agent 策略锁定 A／B 采购清单。",
            );
            save(store, task, None).await?;
            policy_ready.send_replace(Some(task.planning_policy()));
        }
        let opened_redbean = status.redbean_fallback && !redbean_fallback;
        let retry_newcloud = status.newcloud_fallback && !newcloud_fallback;
        if newcloud
            || redbean
            || task.login.phase != state
            || last_period != status.period
            || evidence.is_some()
            || opened_redbean
            || retry_newcloud
        {
            if let Some(message) = match state {
                "login" if task.login.phase == "opening" && proof_only => {
                    Some("正在验证新云授权。若出现 Edge 登录页，请在该页面完成认证。")
                }
                "login"
                    if task.login.phase == "opening" && refresh.redbean.uses_stored_account() =>
                {
                    Some("红豆使用服务端预存账号。若 Edge 打开新云登录页，请完成新云认证。")
                }
                "login" if task.login.phase == "opening" => {
                    Some("正在核验两平台授权。若出现 Edge 登录页，请在对应页面完成认证。")
                }
                "coverage" => Some("正在查询新云覆盖。"),
                "required_inventory" => Some("正在按档期读取红豆库存，核对客户指定线路。"),
                "inventory" => Some("覆盖已导出，正在通过红豆库存 API 分页读取该档期。"),
                "importing" => Some("执行脱敏原表校验并导入快照。"),
                "proof" => Some("核对入选线路图、单位触达和地铁出口实点，按缺项查询新云。"),
                _ => None,
            } {
                let text = status
                    .period
                    .as_deref()
                    .map(|period| format!("{period} · {message}"))
                    .unwrap_or_else(|| message.into());
                let step = if planning.is_some() && state == "proof" {
                    MIX
                } else {
                    DATA
                };
                task.note(step, StepState::Running, text);
            }
            last_period = status.period.clone();
            task.login.phase = state.to_owned();
            if opened_redbean {
                redbean_fallback = true;
                let message = "红豆临时令牌未通过库存权限检查，已打开红豆平台登录页。";
                task.note(DATA, StepState::Running, message);
                audit::record(store, "平台", "红豆登录回退", Some(task.id), message.into()).await?;
            }
            if retry_newcloud {
                newcloud_fallback = true;
                let message = "现有新云会话未通过授权检查，正在切换到官方页面认证。";
                task.note(DATA, StepState::Running, message);
                audit::record(store, "平台", "新云登录回退", Some(task.id), message.into()).await?;
            }
            if let Some(evidence) = evidence {
                let map_progress = if evidence.expected_overview_maps == 0 {
                    "无需新增线路总图".into()
                } else {
                    format!(
                        "线路总图 {}/{} 张",
                        evidence.overview_maps, evidence.expected_overview_maps
                    )
                };
                task.note(
                    if planning.is_some() { MIX } else { DATA },
                    StepState::Running,
                    format!(
                        "取证进度 · {map_progress} · 地铁出口实点 {}/{} 处 · 有可引用统计的资源月份 {}/{} · 可核验单位触达 {} 个资源月份（含 5 月基准 {} 个）。",
                        evidence.exits, evidence.expected_exits,
                        evidence.observed_reach, evidence.expected_reach, evidence.reach, evidence.baseline_reach,
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
    if let Some((manifest, maps)) = manifest.get().zip(
        refresh
            .route_overview_archive(id)
            .and_then(|bytes| visuals::read_maps(&bytes).ok()),
    ) {
        let mut saved = 0;
        for map in maps
            .into_iter()
            .filter(|map| map.basemap_verified && manifest.accepts_overview(map))
        {
            let routes = map.route_keys();
            task.route_overviews
                .retain(|prior| prior.route_keys() != routes);
            task.route_overviews.push(map);
            saved += 1;
        }
        if saved > 0 {
            let message =
                format!("按完整入选线路核对并保存 {saved} 张新云总图。各采购段复用同案总图。");
            task.note(DATA, task.steps[DATA].state, &message);
            audit::record(store, "平台", "保存线路总图", Some(task.id), message).await?;
        }
    }
    task.reach_checks = status.reach_checks;
    if !task.reach_checks.is_empty() {
        let count = task.reach_checks.len();
        let message =
            format!("本次取证有 {count} 个资源月份仍缺单位触达。补证清单可逐项查看查询结果。");
        let step = if proof_only { DATA } else { MIX };
        task.note(step, task.steps[step].state, &message);
        audit::record(store, "平台", "记录新云触达缺项", Some(task.id), message).await?;
    }
    let reason = match status.state.as_str() {
        "done" if proof_only => "已完成本次新云取证。",
        "needs_review" if proof_only => "新云未返回全部证据，请核对各段缺项或从数据中心补证。",
        "done" | "needs_review" => "未取得有效的平台数据快照。",
        "error_launch" => "无法打开数据平台。请检查本机 Edge。",
        "error_connection" => "官方登录页面暂时无法打开，请检查网络后重新获取数据。",
        "error_newcloud_connection" => {
            "无法打开新云平台登录页。请检查新云网络后重试，也可使用本地快照。"
        }
        "error_redbean_connection" => {
            "无法打开红豆平台登录页。请检查红豆网络后重试，也可使用本地快照。"
        }
        "error_login" if proof_only => "新云平台认证超时，可按采购清单再次取证。",
        "error_login" => match (status.newcloud, status.redbean) {
            (true, false) => "新云平台已认证。红豆平台认证超时，可重新获取数据并完成认证。",
            (false, true) => "红豆平台已认证。新云平台认证超时，可重新获取数据并完成认证。",
            (false, false) => "新云平台和红豆平台均未完成认证，可重新获取数据并完成认证。",
            (true, true) => "两平台已通过认证，认证后获取数据超时。可重新获取数据或使用本地快照。",
        },
        "error_redbean_auth" if refresh.redbean.uses_stored_account() => {
            "红豆拒绝预存账号的库存请求。请核对服务端账号权限后重试。"
        }
        "error_redbean_auth" => {
            "红豆平台拒绝本次库存请求。请重新认证并核对所选媒体和档期的查询权限。"
        }
        "error_newcloud_auth" => "新云平台未取得查询授权。请重新认证新云平台或使用本地快照。",
        "error_coverage" => "新云没有返回所选媒体的可用覆盖结果。请核对定位与媒体类型。",
        "error_inventory" => "红豆库存或规格下载失败。请核对档期与导出权限。",
        "error_import" => "平台导出未通过校验，请核对平台中的查询条件后重试。",
        "error_interrupted" => "本机获取数据的进程意外结束。",
        "error_timeout" => "获取平台数据超时。",
        _ => "获取平台数据未完成。",
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
        return Ok(PlatformCollection::Continue(None));
    }

    // An import carries this job ID only after both official sessions and the requested dates
    // are checked. Recover its committed batches even if the worker could not finish writing status.
    let committed = imported_periods(store, id).await?;
    let imported = committed.len();
    if imported == 0 {
        platform_review(store, task, &status.state, reason).await?;
        return Ok(PlatformCollection::Stopped);
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
            StepState::Done,
            format!("官方导出已导入本任务的 {imported} 段采购档期快照。"),
        );
    }
    if let Some(rolling) = &mut task.options.rolling {
        rolling.batches = committed;
    }
    task.portfolio = portfolio.get().cloned();
    if status.state.starts_with("error_") {
        task.login.phase = status.state.clone();
        task.note(DATA, StepState::Review, reason);
        audit::record(
            store,
            "平台",
            "获取数据中断",
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
        task.note(DATA, task.steps[DATA].state, &message);
        audit::record(store, "平台", "红豆库存读取", Some(task.id), message).await?;
    }
    save(store, task, None).await?;
    if let Some(strategy) = strategy {
        let periods = monthly_inventory_context(store, &task.options).await?;
        planning = Some(prepare_combination(store, combination, task, strategy, &periods).await?);
    }
    Ok(PlatformCollection::Continue(planning))
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
    focus: ComparisonFocus,
    focus_from_model: bool,
    strategy_heading: &'static str,
    priority_heading: &'static str,
    mix_applied: bool,
}

struct StrategyContext {
    scene: Value,
    decision: StrategyDecision,
    heading: &'static str,
    inherited: bool,
}

async fn prepare_strategy(
    store: &Store,
    model: Option<&Arc<Model>>,
    task: &mut Task,
) -> Result<StrategyContext, WebError> {
    let campaign_goal = task.options.campaign_goal();
    let confirmed = campaign_goal.and_then(Angle::from_confirmed_goal);
    let requested_focus = task.options.confirmed_comparison_focus();
    let goal_reason = if confirmed.is_some() {
        GoalReason::ConfirmedGoal
    } else if task.options.goal_needs_confirmation() {
        GoalReason::NeedsConfirmation
    } else {
        GoalReason::ExampleGoal
    };
    let goal_source = match goal_reason {
        GoalReason::ConfirmedGoal => "confirmed",
        GoalReason::ExampleGoal => "example",
        GoalReason::NeedsConfirmation => "unknown",
    };
    let permitted_focuses = match goal_reason {
        GoalReason::NeedsConfirmation => Vec::new(),
        GoalReason::ExampleGoal => {
            let mut permitted = Angle::Visit.permitted_focuses(&task.options);
            permitted.push(ComparisonFocus::EstimatedExposure);
            permitted
        }
        GoalReason::ConfirmedGoal => confirmed
            .expect("confirmed goal")
            .permitted_focuses(&task.options),
    };
    let schedule = task
        .options
        .rolling
        .as_ref()
        .zip(task.options.starts.as_deref())
        .zip(task.options.ends.as_deref())
        .filter(|_| task.needs_platform_login() && !task.options.proof_only);
    let (schedule_months, confirmed_schedule) = if let Some(((rolling, starts), ends)) = schedule {
        let months = if task.options.operations.is_none() && confirmed.is_some() {
            rolling.candidate_months(starts, ends)
        } else {
            Vec::new()
        };
        let fixed = if rolling.candidate_months(starts, ends).is_empty()
            || task.options.operations.is_some()
        {
            rolling
                .windows(starts, ends)
                .unwrap_or_default()
                .into_iter()
                .filter(super::schedule::Window::needs_stock)
                .map(|window| window.booking_label())
                .collect()
        } else {
            Vec::new()
        };
        (months, fixed)
    } else {
        (Vec::new(), Vec::new())
    };
    let scene = json!({
        "industry": task.options.industry.label(),
        "public_brand": task.options.industry.brand(),
        "example_objective": task.options.industry.campaign(),
        "requested_media": task.options.media.unwrap_or(Selection::Both).label(),
        "rolling_schedule": task.options.rolling.is_some(),
        "explicit_budget_ceiling": task.options.hard_cap.is_some(),
        "confirmed_goal_category": confirmed,
        "campaign_goal": campaign_goal,
        "customer_comparison_focus": requested_focus,
        "permitted_comparison_focuses": permitted_focuses,
        "core_circle_confirmed": task.options.core_circle.is_some(),
        "custom_target_circle_count": task.options.target_centers.len(),
        "assumption_source": goal_source,
        "required_bus_route_count": task.options.required_routes.len(),
        "schedule_candidate_months": schedule_months,
        "confirmed_schedule": confirmed_schedule,
    });
    if let Some(decision) = task.strategy {
        task.note(
            STRATEGY,
            if decision.angle == Angle::Unspecified {
                StepState::Review
            } else {
                StepState::Done
            },
            format!("沿用原版本已采用的策略。{}", decision.angle.copy()),
        );
        task.agent_decisions.push(json!({
            "role": STRATEGIST.id, "contract": AGENT_CONTRACT, "adopted": decision,
            "source": "inherited", "previous_task": task.previous_task,
        }));
        save(store, task, None).await?;
        return Ok(StrategyContext {
            scene,
            decision,
            heading: "原版本 · 已采用策略",
            inherited: true,
        });
    }
    let (decision, heading) = {
        let strategy: Consultation<Strategy> =
            consult(store, model, task, STRATEGY, STRATEGIST, scene.clone()).await?;
        let suggestion = strategy.output.as_ref().filter(|suggestion| {
            suggestion.reason == Some(goal_reason)
                && match goal_reason {
                    GoalReason::ConfirmedGoal => Some(suggestion.angle) == confirmed,
                    GoalReason::NeedsConfirmation => suggestion.angle == Angle::Unspecified,
                    GoalReason::ExampleGoal => matches!(
                        suggestion.angle,
                        Angle::Visit | Angle::Commute | Angle::Awareness
                    ),
                }
        });
        let angle = match goal_reason {
            GoalReason::ConfirmedGoal => confirmed.expect("confirmed campaign goal"),
            GoalReason::NeedsConfirmation => Angle::Unspecified,
            GoalReason::ExampleGoal => suggestion.map_or(Angle::Visit, |plan| plan.angle),
        };
        let model_applied = goal_reason == GoalReason::ExampleGoal && suggestion.is_some();
        let proposed_focus = suggestion.and_then(|plan| plan.comparison_focus);
        let selected_focus = proposed_focus.filter(|focus| {
            angle != Angle::Unspecified && angle.permitted_focuses(&task.options).contains(focus)
        });
        let focus_from_model = requested_focus.is_none() && selected_focus.is_some();
        let decision = StrategyDecision {
            angle,
            reason: goal_reason,
            comparison_focus: (angle != Angle::Unspecified).then(|| {
                requested_focus
                    .or(selected_focus)
                    .unwrap_or_else(|| angle.permitted_focuses(&task.options)[0])
            }),
            focus_from_model,
        };
        task.strategy = Some(decision);
        let (source, heading, status, note) = match goal_reason {
            GoalReason::ConfirmedGoal => (
                "customer",
                "客户确认 · 投放目标",
                StepState::Done,
                format!("采用已确认的客户目标。{}", angle.copy()),
            ),
            GoalReason::NeedsConfirmation => (
                "rules",
                "待确认 · 投放目标",
                StepState::Review,
                angle.copy().to_owned(),
            ),
            GoalReason::ExampleGoal if model_applied => (
                "model",
                "策略 Agent · 展示目标",
                StepState::Done,
                angle.copy().to_owned(),
            ),
            GoalReason::ExampleGoal => (
                "rules",
                "展示假设 · 投放目标",
                StepState::Fallback,
                angle.copy().to_owned(),
            ),
        };
        let phases_applied = suggestion.is_some_and(|plan| {
            task.options
                .rolling
                .as_mut()
                .is_some_and(|rolling| rolling.adopt_phases(&schedule_months, &plan.phases))
        });
        let mut adopted = json!(decision);
        if task.needs_platform_login()
            && !task.options.proof_only
            && let Some(windows) = task
                .options
                .rolling
                .as_ref()
                .zip(task.options.starts.as_deref())
                .zip(task.options.ends.as_deref())
                .and_then(|((rolling, starts), ends)| rolling.windows(starts, ends).ok())
        {
            let windows = windows
                .iter()
                .filter(|window| window.needs_stock())
                .map(super::schedule::Window::booking_label)
                .collect::<Vec<_>>();
            let basis = if phases_applied {
                "Agent 候选档期"
            } else if !confirmed_schedule.is_empty() {
                "已填报的投放档期"
            } else {
                "按所选时间范围生成的规则候选档期"
            };
            task.note(
                STRATEGY,
                status,
                format!("{basis} · {}。仅用于发起库存核验。", windows.join("；")),
            );
            adopted["collection_windows"] = json!(windows);
            adopted["schedule_basis"] = json!(basis);
        }
        strategy.record(
            task,
            STRATEGIST,
            adopted,
            source,
            phases_applied
                || suggestion.is_some_and(|plan| {
                    plan.angle == angle
                        && (plan.comparison_focus.is_none() || selected_focus.is_some())
                }),
        );
        task.note(STRATEGY, status, note);
        save(store, task, None).await?;
        (decision, heading)
    };

    Ok(StrategyContext {
        scene,
        decision,
        heading,
        inherited: false,
    })
}

async fn prepare_combination(
    store: &Store,
    combination: Option<&Arc<Model>>,
    task: &mut Task,
    strategy: StrategyContext,
    monthly_inventory: &[Value],
) -> Result<PlanningContext, WebError> {
    let StrategyContext {
        scene,
        decision,
        heading: strategy_heading,
        inherited,
    } = strategy;
    let requested_focus = task.options.confirmed_comparison_focus();
    let locked = task.options.compare_priority;
    let angle = decision.angle;
    let focus = decision.focus(&task.options);
    if let Some(priority) = locked.filter(|_| inherited) {
        task.priority = Some(priority);
        task.note(
            MIX,
            StepState::Pending,
            format!("沿用原版本核算优先级。{}。", priority.label()),
        );
        task.agent_decisions.push(json!({
            "role": PLANNER.id, "contract": AGENT_CONTRACT, "adopted": {"priority": priority},
            "source": "inherited", "previous_task": task.previous_task,
        }));
        save(store, task, None).await?;
        return Ok(PlanningContext {
            scene,
            angle,
            focus,
            strategy_heading,
            focus_from_model: requested_focus.is_none() && decision.focus_from_model,
            priority_heading: "原版本 · 核算优先级",
            mix_applied: true,
        });
    }
    let preferred = if angle == Angle::Unspecified {
        Priority::Balanced
    } else {
        focus.planning_priority()
    };
    let mut permitted = vec![locked.unwrap_or(preferred)];
    if locked.is_none() && requested_focus.is_none() && preferred != Priority::Balanced {
        permitted.push(Priority::Balanced);
    }
    let mix: Consultation<Mix> = consult(
        store,
        combination,
        task,
        MIX,
        PLANNER,
        json!({
            "scene": scene, "strategy_from_previous_step": angle, "comparison_focus": focus,
            "locked_priority": locked, "permitted_priorities": permitted,
            "monthly_inventory": monthly_inventory,
        }),
    )
    .await?;
    let proposed = mix
        .output
        .as_ref()
        .map(|plan| Priority::from(plan.priority));
    let applicable = proposed.filter(|priority| permitted.contains(priority));
    let priority = locked.or(applicable).unwrap_or(preferred);
    let adopted = locked.is_none() && applicable.is_some();
    let (mix_source, priority_heading) = if locked.is_some() {
        ("inherited", "原版本 · 核算优先级")
    } else if requested_focus.is_some() {
        ("customer", "客户确认 · 核算优先级")
    } else if adopted {
        ("model", "组合 Agent · 核算优先级")
    } else {
        ("rules", "本地规则 · 核算优先级")
    };
    mix.record(
        task,
        PLANNER,
        json!({"priority": priority, "comparison_focus": focus}),
        mix_source,
        proposed == Some(priority),
    );
    task.priority = Some(priority);
    task.note(
        MIX,
        StepState::Pending,
        format!("{}。用于接下来的 A／B 求解。", priority.label()),
    );
    save(store, task, None).await?;
    Ok(PlanningContext {
        scene,
        angle,
        focus,
        focus_from_model: requested_focus.is_none() && decision.focus_from_model,
        strategy_heading,
        priority_heading,
        mix_applied: adopted || locked.is_some() || requested_focus.is_some(),
    })
}
async fn execute(
    store: &Store,
    models: &AgentModels,
    refresh: Refresh,
    actor: &User,
    task: &mut Task,
) -> Result<(), WebError> {
    let strategy = models.get(Role::Strategy);
    let combination = models.get(Role::Combination);
    let verification = models.get(Role::Verification);
    let planning = if task.needs_platform_login() {
        let planned = if task.options.proof_only {
            None
        } else {
            Some(prepare_strategy(store, strategy, task).await?)
        };
        if !task.options.proof_only
            && let Err((_, reason)) =
                refresh.preflight(false, task.options.media.unwrap_or(Selection::Both).id())
        {
            platform_review(store, task, "error_preflight", reason).await?;
            return Ok(());
        }
        match collect_platform(store, &refresh, actor, task, planned, combination).await? {
            PlatformCollection::Stopped => return Ok(()),
            PlatformCollection::Continue(planning) => planning,
        }
    } else {
        None
    };
    if planning.is_none() {
        task.note(
            DATA,
            StepState::Running,
            "正在匹配行业、档期与已导入的赛用快照。",
        );
        save(store, task, None).await?;
    }
    let mut calendar = None;
    let mut single = None;
    if let Some(mut rolling) = task.options.rolling.take() {
        let loaded = super::schedule::Calendar::load(store, &task.options, &rolling).await?;
        let records = loaded.record(&mut rolling);
        let missing = records.iter().any(|(_, found)| !found);
        let data_state = if missing {
            StepState::Review
        } else {
            StepState::Done
        };
        for (message, _) in records {
            task.note(DATA, data_state, message);
        }
        task.note(
            DATA,
            data_state,
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
                page.notice.unwrap_or("请核对档期后重新从平台获取数据。"),
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

    let PlanningContext {
        scene,
        angle,
        focus,
        focus_from_model,
        strategy_heading,
        priority_heading,
        mix_applied,
    } = match planning {
        Some(planning) => planning,
        None => {
            let proof = task.options.proof_only;
            let strategy = prepare_strategy(store, strategy.filter(|_| !proof), task).await?;
            let periods = calendar
                .as_ref()
                .map_or_else(Vec::new, super::schedule::Calendar::planning_context);
            prepare_combination(
                store,
                combination.filter(|_| !proof),
                task,
                strategy,
                &periods,
            )
            .await?
        }
    };
    let priority = task.priority.unwrap_or_default();
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
    let policy = task.planning_policy();
    let (calendar, single, portfolio, previous, mut variants) =
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
    for variant in &mut variants {
        variant.set_route_overview(&task.route_overviews);
    }
    let platform_stock = calendar
        .as_ref()
        .is_some_and(|calendar| calendar.platform_inputs())
        || single
            .as_ref()
            .is_some_and(|(_, snapshot)| snapshot.source == super::source_label("platform"));
    task.platform_report_data = platform_stock
        && super::brief::CampaignView::new(&task.options)
            .missing()
            .is_empty()
        && variants.iter().all(|variant| {
            (variant.buses == 0 || variant.verified_route_overview())
                && variant
                    .periods
                    .iter()
                    .all(visuals::Period::has_platform_evidence)
        });
    task.route_overviews.retain(|map| {
        variants.iter().any(|variant| {
            variant
                .route_overview
                .as_ref()
                .is_some_and(|selected| selected.routes == map.routes)
        })
    });
    let [optimal, feasible, fallback, unavailable] = portfolio.engine_counts();
    task.note(MIX, StepState::Running, format!(
        "分段核算结果 · 最优 {optimal} · 限时可行 {feasible} · 规则回退 {fallback} · 无可核价资源 {unavailable}。"
    ));
    task.portfolio = Some(portfolio.clone());
    let portfolio = &portfolio;
    let operations_required = task.options.requires_operations();
    let summaries = summarize(&variants, operations_required);
    let readiness = ApprovalReadiness {
        a: summaries[0].eligible(),
        b: summaries[1].eligible(),
    };
    let checks = Checks::new(
        &variants,
        !task.options.required_routes.is_empty(),
        operations_required,
    );
    let mut missing_checks = checks.missing();
    task.route_replacement_needed = Some(missing_checks.contains(&Gate::RequiredRoutes));
    let goal_pending = angle == Angle::Unspecified;
    if goal_pending {
        missing_checks.insert(0, Gate::CustomerGoal);
    }
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
        } else if !result.proposal.complete_media || result.proposal.items.is_empty() {
            task.note(
                MIX,
                StepState::Review,
                "当前档期或预算下没有完整、可核价的媒体组合。请核对库存与刊例，或调整预算重新生成。",
            );
        } else {
            task.note(
                MIX,
                if mix_applied {
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
                if mix_applied {
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
    let requested_focus = task.options.confirmed_comparison_focus();
    let primary_metric_pending =
        requested_focus.is_some_and(|focus| missing_primary_metric(&summaries, focus));
    if primary_metric_pending {
        missing_checks.insert(0, Gate::ComparisonMetric);
    }
    let conditions_incomplete = annual_conditions_incomplete
        || single_conditions_incomplete
        || missing_checks.contains(&Gate::Operations)
        || goal_pending
        || primary_metric_pending;
    let prioritize_booking = annual_conditions_incomplete
        || single_conditions_incomplete
        || missing_checks.iter().any(|gate| gate.allows_booking());
    let monthly_evidence: Vec<_> = variants.iter().map(|variant| {
        let periods: Vec<_> = variant.periods.iter().map(|period| {
            let reach = period.exposure_metrics();
            let combination = &reach[2];
            json!({
                "starts": period.starts, "ends": period.ends,
                "placement_feasible": period.available && period.required_complete,
                "platform_inventory": period.platform_inventory,
                "operations_ready": !period.operations_required || period.operations.as_ref().is_some_and(|work| work.ready()),
                "covered_target_circles": period.circles.len(),
                "missing_unit_reach": combination.missing,
                "uses_historical_baseline": combination.baseline_months > 0,
            })
        }).collect();
        json!({"variant": variant.code, "periods": periods})
    }).collect();
    let review: Consultation<Review> = consult(store, verification, task, REPORT, REVIEWER, json!({
        "scene": scene, "strategy_from_previous_step": angle, "combination_rule": priority.label(),
        "placement_conditions_complete": !prioritize_booking,
        "goal_pending": goal_pending, "primary_metric_pending": primary_metric_pending,
        "comparison_focus": focus, "comparison_order": focus.criteria(),
        "candidates": summaries,
        "monthly_evidence": monthly_evidence,
        "checks": checks,
        "missing_checks": missing_checks,
    })).await?;
    let proposed = review.output.as_ref().and_then(|plan| {
        Some(Recommendation {
            candidate: plan.preferred_variant?,
            reason: plan.reason?,
        })
    });
    let (recommendation, recommendation_adopted) = if goal_pending {
        let required = Recommendation {
            candidate: Candidate::None,
            reason: Reason::PendingGoal,
        };
        (required, proposed == Some(required))
    } else if primary_metric_pending {
        let required = Recommendation {
            candidate: Candidate::None,
            reason: Reason::PendingPrimaryMetric,
        };
        (required, proposed == Some(required))
    } else {
        recommend(&summaries, proposed, focus)
    };
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
    let approval_quotes = ApprovalQuotes {
        a: variants[0].total.high,
        b: variants[1].total.high,
    };
    let mut guidance = vec![
        (strategy_heading, angle.copy()),
        (priority_heading, priority.label()),
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
    if !goal_pending {
        let label = if focus_from_model {
            "策略 Agent · 首要比选指标"
        } else {
            "首要比选指标"
        };
        guidance.push((label, focus.label()));
        if task.options.confirmed_comparison_focus().is_some() {
            task.note(
                REPORT,
                StepState::Running,
                format!("采用客户首要比选指标 · {}。", focus.label()),
            );
        }
        if focus_from_model {
            task.note(
                REPORT,
                StepState::Running,
                format!(
                    "采用需求策略 Agent 提议的首要比选指标 · {}。",
                    focus.label()
                ),
            );
        }
    }
    if !task.is_demo() && !readiness.complete() {
        for (label, ready) in [("A 案核验状态", readiness.a), ("B 案核验状态", readiness.b)]
        {
            let message = if ready {
                "本案已通过投放条件与证据核验。"
            } else {
                "本案尚有投放条件或证据待核，请查看本案明细。"
            };
            task.note(REPORT, StepState::Running, format!("{label} · {message}"));
            guidance.push((label, message));
        }
    }
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
    save(store, task, None).await?;
    let brand = Brand::read(store).await?;
    let html = if let Some((page, result)) = &result {
        render_export(page, result, variants, &guidance, &brand, &task.options)?
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
    if task.is_demo() {
        task.note(REPORT, StepState::Done, "合成演示 HTML 已保存，仅供演示。");
        task.state = TaskState::Demo;
        return save(store, task, Some(html)).await;
    }
    task.platform_report_data &= !report_has_open_items(&html);
    task.approval_readiness = Some(readiness);
    if conditions_incomplete || !readiness.complete() {
        task.note(
            REPORT,
            StepState::Done,
            "可交互 HTML 已生成，可以直接浏览。",
        );
        task.state = TaskState::Review;
        task.has_alternative = true;
        return save(store, task, Some(html)).await;
    }
    task.approval_quotes = Some(approval_quotes);
    task.note(
        REPORT,
        StepState::Done,
        "可交互 HTML 已生成，可以直接浏览。",
    );
    task.state = TaskState::Done;
    save(store, task, Some(html)).await
}

pub(super) fn router() -> Router<Store> {
    Router::new()
        .route("/overview", get(overview::page))
        .route("/overview/queues", get(overview::queues))
        .route("/task-log", get(listing))
        .route(
            "/media-data/snapshots/{id}/proposals",
            post(start_from_snapshot),
        )
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
        .route("/media-proposals/tasks/{id}/review-copy", get(report))
        .route(
            "/media-proposals/tasks/{id}/agent-trace",
            get(agent_trace::page),
        )
        .route(
            "/media-proposals/tasks/{id}/retry-agents",
            post(retry_agents),
        )
        .route("/media-proposals/tasks/{id}/campaign", get(campaign_sheet))
        .route(
            "/media-proposals/tasks/{id}/campaign",
            post(revise_campaign).layer(DefaultBodyLimit::max(12 * 1024)),
        )
        .route(
            "/media-proposals/tasks/{id}/confirm-goal",
            post(confirm_goal).layer(DefaultBodyLimit::max(256)),
        )
        .route(
            "/media-proposals/tasks/{id}/evidence",
            get(evidence_review::checklist),
        )
        .route(
            "/media-proposals/tasks/{id}/refresh-evidence",
            post(refresh_evidence),
        )
        .route(
            "/media-proposals/tasks/{id}/replacements",
            get(route_revision::page)
                .post(route_revision::replace)
                .layer(DefaultBodyLimit::max(4096)),
        )
        .route(
            "/media-proposals/tasks/{id}/schedule",
            get(schedule_revision::page)
                .post(schedule_revision::revise)
                .layer(DefaultBodyLimit::max(8 * 1024)),
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
