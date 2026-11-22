mod brief;
mod collaboration;
mod data;
mod import;
mod operations;
pub(crate) mod optimizer;
mod plan;
pub(crate) mod platform;
pub(crate) mod schedule;
mod tasks;
mod visuals;

use std::collections::{BTreeMap, BTreeSet};

use askama::Template;
use axum::{
    Extension, Form, Router,
    extract::{DefaultBodyLimit, Multipart, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    agents::{Agents, Role},
    model::Model,
    refresh::{BrowserImport, Refresh},
    settings::Brand,
    store::Store,
    web::{WebError, database, render},
};
use plan::{
    Cost, Cover, Dataset, Inventory, Medium, Priority, Production, Proposal, Rate, Selection,
    Source,
};

const INVALID: WebError = (StatusCode::UNPROCESSABLE_ENTITY, "请核对导入参数。");
const UPLOAD_BYTES: usize = 2 * 1024 * 1024;
const BUS: Medium = Medium::Bus;
const METRO: Medium = Medium::Metro;
pub(crate) const FOOD_COVERAGE_CENTERS: &str = "上海市静安区南京西路789号|上海市静安区静安寺|上海市黄浦区人民广场|上海市黄浦区新天地|上海市徐汇区徐家汇";
pub(crate) const FOOD_COVERAGE_RADIUS: u16 = 1500;
pub(crate) const SHOWCASE_BUDGET: u32 = 200;
const SHOWCASE_BUDGET_BAND: (u64, u64) = (1_800_000, 2_500_000);
pub(crate) const SHOWCASE_START: &str = "2027-01-01";
pub(crate) const SHOWCASE_END: &str = "2027-12-31";
const SHOWCASE_DATES: &str = "2027-01-01 至 2027-12-31";
const FOOD_ONLY: WebError = (StatusCode::UNPROCESSABLE_ENTITY, "当前仅开放食品饮料行业。");
const MAX_EXPLICIT_CAP: u64 = 1_000_000_000;

pub(crate) fn anonymize_food_case(value: &str) -> std::borrow::Cow<'_, str> {
    static LEGACY_BRAND: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(concat!(
            r"(?i)(https://roastery\.",
            "\u{0073}tarbucks",
            r"\.com\.cn/?",
            ")| ?(?:",
            "\u{661f}\u{5df4}\u{514b}",
            "(?:上海",
            "\u{70d8}\u{7119}\u{5de5}\u{574a}",
            ")?|",
            "\u{0073}tarbucks",
            "|",
            "\u{70d8}\u{7119}\u{5de5}\u{574a}",
            ")",
        ))
        .expect("legacy food label pattern")
    });
    LEGACY_BRAND.replace_all(value, |parts: &regex::Captures<'_>| {
        if parts.get(1).is_some() {
            "#brand-understanding"
        } else {
            "食品饮料品牌"
        }
    })
}

#[derive(Clone, Copy, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Industry {
    Bank,
    #[default]
    Food,
    Health,
    Hospital,
    Park,
}

impl Industry {
    const ALL: [Self; 5] = [
        Self::Bank,
        Self::Food,
        Self::Health,
        Self::Hospital,
        Self::Park,
    ];
    fn id(self) -> &'static str {
        match self {
            Self::Bank => "bank",
            Self::Food => "food",
            Self::Health => "health",
            Self::Hospital => "hospital",
            Self::Park => "park",
        }
    }
    fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|item| item.id() == id)
    }
    fn from_text(value: &str) -> Option<Self> {
        let value = value.trim();
        Self::ALL
            .into_iter()
            .find(|item| item.label() == value || item.id().eq_ignore_ascii_case(value))
            .or_else(|| matches!(value, "主题公园" | "乐园（主题公园）").then_some(Self::Park))
    }
    fn label(self) -> &'static str {
        match self {
            Self::Bank => "银行保险",
            Self::Food => "食品饮料",
            Self::Health => "保健品",
            Self::Hospital => "私立医院",
            Self::Park => "乐园",
        }
    }
    fn brand(self) -> &'static str {
        match self {
            Self::Bank => "招商银行上海分行",
            Self::Food => "上海某连锁食品饮料品牌",
            Self::Health => "纽崔莱 · 安利上海旗舰体验馆",
            Self::Hospital => "上海和睦家医院",
            Self::Park => "上海迪士尼度假区",
        }
    }
    fn campaign(self) -> &'static str {
        match self {
            Self::Bank => "围绕营业网点，触达有线下服务需求的通勤人群。",
            Self::Food => "在夏季新品期向商圈通勤和周末到访人群展示试饮信息，促进到店。",
            Self::Health => "围绕体验馆，向周边消费人群介绍线下体验活动。",
            Self::Hospital => "围绕院区和门诊，帮助周边居民获取到院与服务信息。",
            Self::Park => "覆盖通往度假区的交通节点，提示周末出游与到访信息。",
        }
    }
    fn landmark(self) -> &'static str {
        match self {
            Self::Bank => "上海市浦东新区陆家嘴环路1088号 招商银行上海分行",
            Self::Food => "上海市静安区南京西路789号",
            Self::Health => "上海市黄浦区斜土路547号 安利上海旗舰体验馆",
            Self::Hospital => "上海市长宁区平塘路699号 上海和睦家医院",
            Self::Park => "上海市浦东新区川沙镇黄赵路310号 上海迪士尼乐园",
        }
    }

    fn coverage_radius(self) -> u16 {
        if matches!(self, Self::Food) {
            FOOD_COVERAGE_RADIUS
        } else {
            1000
        }
    }

    fn coverage_centers(self) -> &'static str {
        if matches!(self, Self::Food) {
            FOOD_COVERAGE_CENTERS
        } else {
            self.landmark()
        }
    }

    fn supplemental_metro_landmark(self) -> Option<&'static str> {
        match self {
            Self::Hospital => Some("上海市长宁区淞虹路377号 长宁ArtPark大融城"),
            Self::Park => Some("上海市徐汇区华山路1954号 上海交通大学徐汇校区"),
            _ => None,
        }
    }
}

#[derive(Clone, Deserialize, Default, Serialize)]
struct Options {
    #[serde(default)]
    industry: Industry,
    budget: Option<u32>,
    #[serde(default)]
    hard_cap: Option<u64>,
    media: Option<Selection>,
    starts: Option<String>,
    ends: Option<String>,
    month: Option<String>,
    compare: Option<u32>,
    demo: Option<u8>,
    batch: Option<Uuid>,
    preview: Option<u8>,
    #[serde(default)]
    source: tasks::Source,
    #[serde(default)]
    brief: Option<brief::BriefContext>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    target_centers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    core_circle: Option<String>,
    #[serde(default)]
    operations: Option<operations::Operations>,
    #[serde(default)]
    operations_required: bool,
    #[serde(default)]
    adjusted_budget: bool,
    #[serde(default)]
    fixed_showcase: bool,
    #[serde(default)]
    lock_snapshot: bool,
    #[serde(default)]
    proof_only: bool,
    #[serde(default)]
    compare_priority: Option<Priority>,
    #[serde(default)]
    required_routes: Vec<String>,
    #[serde(default)]
    rolling: Option<schedule::Rolling>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    schedule_revision: Option<schedule::ScheduleRevision>,
}

impl Options {
    fn campaign_goal(&self) -> Option<&str> {
        self.brief
            .as_ref()
            .and_then(|brief| brief::campaign_goal(&brief.campaign_goal))
    }

    fn confirmed_comparison_focus(&self) -> Option<collaboration::ComparisonFocus> {
        self.campaign_goal()?;
        self.brief.as_ref()?.comparison_focus
    }

    fn goal_needs_confirmation(&self) -> bool {
        self.demo != Some(1) && self.campaign_goal().is_none()
    }

    fn requires_operations(&self) -> bool {
        self.operations_required || (self.rolling.is_some() && self.demo != Some(1))
    }
}

fn budget_band(budget: u32) -> (u64, u64) {
    if budget == SHOWCASE_BUDGET {
        SHOWCASE_BUDGET_BAND
    } else {
        let target = u64::from(budget) * 10_000;
        (target - target / 5, target + target / 5)
    }
}

fn budget_band_label(budget: impl std::borrow::Borrow<u32>) -> String {
    let (low, high) = budget_band(*budget.borrow());
    format!("{}–{} 万元", low / 10_000, high / 10_000)
}

fn allocation_budget_band(budget: u32, target: u64) -> Option<(u64, u64)> {
    (budget == SHOWCASE_BUDGET).then(|| {
        let nominal = u64::from(budget) * 10_000;
        let (low, high) = budget_band(budget);
        (target * low / nominal, target * high / nominal)
    })
}

fn budget_gap_needs_confirmation(budget: u32, high_quote: u64) -> bool {
    let (low, high) = budget_band(budget);
    !(low..=high).contains(&high_quote)
}

struct Snapshot {
    id: Option<String>,
    data: Dataset,
    starts: String,
    ends: String,
    observed: String,
    imported: String,
    source: &'static str,
}

struct Changes {
    added: Vec<String>,
    removed: Vec<String>,
    kept: usize,
    previous: u64,
    previous_high: u64,
    previous_buses: u32,
    previous_exits: u32,
    lost_circles: Vec<String>,
    gained_circles: Vec<String>,
    quantities: Vec<QuantityChange>,
    explanation: Option<&'static str>,
}

struct QuantityChange {
    title: String,
    before: u32,
    after: u32,
}

struct ResultView {
    snapshot: Snapshot,
    proposal: Proposal,
    changes: Option<Changes>,
    months: u64,
}

#[derive(Template)]
#[template(path = "media.html")]
struct Page {
    heading: &'static str,
    section: &'static str,
    industry: Industry,
    media: Selection,
    budget: u32,
    budgets: [u32; 3],
    compare: u32,
    demo: bool,
    result: Option<ResultView>,
    notice: Option<&'static str>,
    model_available: bool,
    advice: Option<&'static str>,
    brief: Option<brief::BriefContext>,
    required_routes: Vec<String>,
}

#[derive(Template)]
#[template(path = "media-plan.html")]
struct PlanPage {
    heading: &'static str,
    section: &'static str,
    plan: Page,
}

fn source_label(source: &str) -> &'static str {
    match source {
        "platform" => "平台导出快照",
        "manual" => "人工导入",
        _ => "历史快照",
    }
}

impl Page {
    fn plan_url(&self, route: &str, snapshot: &Snapshot) -> String {
        let mut url = format!(
            "{route}?industry={}&budget={}&media={}&starts={}&ends={}&compare={}",
            self.industry.id(),
            self.budget,
            self.media.id(),
            snapshot.starts,
            snapshot.ends,
            self.compare
        );
        if let Some(id) = &snapshot.id {
            url.push_str("&batch=");
            url.push_str(id);
        } else if snapshot.data.demo {
            url.push_str("&demo=1");
        }
        url
    }

    fn export_url(&self, snapshot: &Snapshot) -> String {
        self.plan_url("/media-proposals/export", snapshot)
    }
}

#[derive(Template)]
#[template(path = "media-export.html")]
struct Export<'a> {
    page: &'a Page,
    result: &'a ResultView,
    css: &'static str,
    guidance: &'a [(&'static str, &'static str)],
    brand: &'a Brand,
    coverage: Vec<ReportCoverage<'a>>,
    variants: Vec<visuals::Variant>,
    full_evidence: bool,
    campaign: brief::CampaignView<'a>,
}

struct ReportCoverage<'a> {
    media: &'a str,
    circle: &'a str,
    nearest: u32,
    radius: u32,
    station_level: bool,
}

fn wan(yuan: &u64) -> String {
    format!("{:.2}", *yuan as f64 / 10_000.0)
}

fn month_days(month: &str) -> Option<u8> {
    let days = match month {
        "02" => 28,
        "04" | "06" | "09" | "11" => 30,
        "01" | "03" | "05" | "07" | "08" | "10" | "12" => 31,
        _ => return None,
    };
    Some(days)
}

fn full_month(starts: &str, ends: &str) -> bool {
    starts
        .strip_prefix("2027-")
        .and_then(|value| value.strip_suffix("-01"))
        .and_then(|month| month_days(month).map(|days| ends == format!("2027-{month}-{days}")))
        .unwrap_or(false)
}

fn date_month(date: &str) -> Option<u32> {
    if date.len() != 10 || !date.starts_with("2027-") || date.as_bytes()[7] != b'-' {
        return None;
    }
    let month = date.get(5..7)?;
    let day = date.get(8..10)?;
    if !day.bytes().all(|part| part.is_ascii_digit())
        || !(1..=month_days(month)?).contains(&day.parse::<u8>().ok()?)
    {
        return None;
    }
    month.parse().ok()
}

fn period_months(starts: &str, ends: &str) -> Option<u64> {
    let start_month = date_month(starts)?;
    let end_month = date_month(ends)?;
    (starts <= ends).then(|| u64::from(end_month - start_month + 1))
}

fn allowed_budget(value: u32) -> bool {
    matches!(value, 100 | 200 | 500)
}

async fn read_snapshot(store: Store, options: &Options) -> Result<Option<Snapshot>, WebError> {
    read_snapshot_matching(store, options, false).await
}

async fn read_snapshot_matching(
    store: Store,
    options: &Options,
    require_media: bool,
) -> Result<Option<Snapshot>, WebError> {
    let industry = options.industry.id();
    let starts = options.starts.clone();
    let ends = options.ends.clone();
    let batch = options.batch.map(|value| value.to_string());
    let media = options.media.unwrap_or(Selection::Both).mask();
    let requested = options.target_centers.clone();
    let core = options.core_circle.clone();
    database(move || {
        let db = store.connect()?;
        let required = if requested.is_empty() { core.as_slice() } else { requested.as_slice() };
        let centers = serde_json::to_string(required)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        db.query_row(
            "SELECT payload, starts, ends, observed_at, recorded_at, id, source FROM media_snapshots
             WHERE industry=?1 AND (?2 IS NULL OR starts=?2) AND (?3 IS NULL OR ends=?3)
               AND (?4 IS NULL OR id=?4) AND (NOT ?6 OR (media_mask & ?5) = ?5)
               AND NOT EXISTS (
                   SELECT 1 FROM json_each(?7) AS target WHERE NOT EXISTS (
                       SELECT 1 FROM json_each(media_snapshots.payload,'$.coverage') AS cover
                       WHERE json_extract(cover.value,'$.circle')=target.value
                         AND json_extract(cover.value,'$.radius')=?8))
             ORDER BY (media_mask & ?5) = ?5 DESC, observed_at DESC, rowid DESC LIMIT 1",
            params![industry, starts, ends, batch, media, require_media, centers, FOOD_COVERAGE_RADIUS],
            |row| {
                let payload: String = row.get(0)?;
                let mut data: Dataset = serde_json::from_str(&payload).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?;
                if !requested.is_empty() {
                    data.coverage.retain(|row| requested.contains(&row.circle)
                        && row.radius == u32::from(FOOD_COVERAGE_RADIUS));
                }
                data.confirmed_core.clone_from(&core);
                Ok(Snapshot {
                    id: Some(row.get(5)?),
                    data,
                    starts: row.get(1)?,
                    ends: row.get(2)?,
                    observed: row.get(3)?,
                    imported: row.get(4)?,
                    source: source_label(&row.get::<_, String>(6)?),
                })
            },
        )
        .optional()
    })
    .await
}

fn change_set(previous: &Proposal, next: &Proposal, budget: u32) -> Changes {
    let keys = |proposal: &Proposal| {
        proposal
            .items
            .iter()
            .map(|item| {
                let title = format!(
                    "{} · {} {}",
                    item.stock.medium.label(),
                    item.stock.name,
                    item.stock.format
                );
                (
                    (item.stock.source.row, item.stock.source.file.clone(), title),
                    item.quantity,
                )
            })
            .collect::<BTreeMap<_, _>>()
    };
    let first = keys(previous);
    let second = keys(next);
    let previous_buses = previous.quantity(BUS);
    let previous_exits = previous.quantity(METRO);
    let next_buses = next.quantity(BUS);
    let next_exits = next.quantity(METRO);
    let explanation = match (
        next_buses.cmp(&previous_buses),
        next_exits.cmp(&previous_exits),
    ) {
        (std::cmp::Ordering::Less, std::cmp::Ordering::Greater) => {
            Some("新组合纳入更多地铁点位，并调减部分公交数量。")
        }
        (std::cmp::Ordering::Greater, std::cmp::Ordering::Less) => {
            Some("新组合增加公交数量，并减少地铁点位。")
        }
        _ => None,
    };
    Changes {
        added: second
            .keys()
            .filter(|key| !first.contains_key(*key))
            .map(|(_, _, title)| title.clone())
            .collect(),
        removed: first
            .keys()
            .filter(|key| !second.contains_key(*key))
            .map(|(_, _, title)| title.clone())
            .collect(),
        kept: first.keys().filter(|key| second.contains_key(*key)).count(),
        quantities: second
            .iter()
            .filter_map(|(key, &after)| {
                let &before = first.get(key)?;
                (before != after).then(|| QuantityChange {
                    title: key.2.clone(),
                    before,
                    after,
                })
            })
            .collect(),
        explanation,
        previous: u64::from(budget),
        previous_high: previous.total.high,
        previous_buses,
        previous_exits,
        lost_circles: previous
            .covered
            .difference(&next.covered)
            .cloned()
            .collect(),
        gained_circles: next
            .covered
            .difference(&previous.covered)
            .cloned()
            .collect(),
    }
}

fn proposal_dates(value: &str) -> Option<(String, String)> {
    let value = value.trim();
    if matches!(value, "2027全年" | "2027 全年" | "2027年全年") {
        return Some((SHOWCASE_START.into(), SHOWCASE_END.into()));
    }
    if let Some((starts, ends)) = value.split_once(['至', '~', '～']) {
        let (starts, ends) = (starts.trim(), ends.trim());
        period_months(starts, ends)?;
        return Some((starts.to_owned(), ends.to_owned()));
    }
    let days = month_days(value.strip_prefix("2027-")?)?;
    Some((format!("{value}-01"), format!("{value}-{days}")))
}

fn normalize_options(mut options: Options) -> Result<Options, WebError> {
    if options.industry != Industry::Food {
        return Err(FOOD_ONLY);
    }
    if let Some(core) = options
        .core_circle
        .take()
        .filter(|value| !value.trim().is_empty())
    {
        let selection = if options.target_centers.is_empty() {
            options
                .industry
                .coverage_centers()
                .split('|')
                .find(|name| *name == core.trim())
        } else {
            options
                .target_centers
                .iter()
                .map(String::as_str)
                .find(|name| *name == core.trim())
        };
        options.core_circle = Some(
            selection
                .ok_or((
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "核心目标圈必须是本次查询中心之一。",
                ))?
                .to_owned(),
        );
    }
    if options
        .hard_cap
        .is_some_and(|amount| !(1..=MAX_EXPLICIT_CAP).contains(&amount))
    {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, "客户预算硬上限无效。"));
    }
    if let Some(month) = options.month.take().filter(|month| !month.is_empty()) {
        if options.starts.is_some() || options.ends.is_some() {
            return Err((StatusCode::BAD_REQUEST, "请选择月份或完整档期。"));
        }
        let (starts, ends) = proposal_dates(&month).ok_or((
            StatusCode::BAD_REQUEST,
            "请输入 2027 年的月份或有效起止日期。",
        ))?;
        options.starts = Some(starts);
        options.ends = Some(ends);
    }
    let budget = options.budget.unwrap_or(SHOWCASE_BUDGET);
    let compare = options.compare.unwrap_or(0);
    if options.fixed_showcase && budget != SHOWCASE_BUDGET {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "展示提案固定按约 200 万元核算。",
        ));
    }
    if options.fixed_showcase
        && options
            .starts
            .as_deref()
            .zip(options.ends.as_deref())
            .and_then(|(starts, ends)| period_months(starts, ends))
            .is_none()
    {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "请选择有效的 2027 年起止日期。",
        ));
    }
    let valid_budget = |amount| {
        if options.adjusted_budget {
            (100..=500).contains(&amount)
        } else {
            allowed_budget(amount)
        }
    };
    if !valid_budget(budget) || (compare != 0 && !valid_budget(compare)) {
        return Err((StatusCode::BAD_REQUEST, "预算可选 100、200 或 500 万元。"));
    }
    Ok(options)
}

async fn load_plan(store: Store, options: Options) -> Result<(Page, Option<Snapshot>), WebError> {
    let options = normalize_options(options)?;
    let budget = options.budget.unwrap_or(SHOWCASE_BUDGET);
    let compare = options.compare.unwrap_or(0);
    let media = options.media.unwrap_or(Selection::Both);
    let valid = match (options.starts.as_deref(), options.ends.as_deref()) {
        (Some(a), Some(b)) => period_months(a, b).is_some(),
        (None, None) => true,
        _ => false,
    };
    let demo = options.demo == Some(1);
    let snapshot = if valid
        && demo
        && options
            .starts
            .as_deref()
            .is_none_or(|date| date == "2027-06-01")
        && options
            .ends
            .as_deref()
            .is_none_or(|date| date == "2027-06-30")
    {
        Some(synthetic(options.industry))
    } else if valid && !demo && options.batch.is_some() {
        read_snapshot(store, &options).await?
    } else {
        None
    };
    let notice = if !valid {
        Some("请选择 2027 年内的有效起止日期。")
    } else if snapshot.is_none() {
        Some("该档期没有可查看的获取数据记录，请先获取平台数据。")
    } else {
        None
    };
    Ok((
        Page {
            heading: crate::settings::DEFAULT_DESCRIPTION,
            section: "tasks",
            industry: options.industry,
            media,
            budget,
            budgets: [100, 200, 500],
            compare,
            demo,
            result: None,
            notice,
            model_available: false,
            advice: None,
            brief: options.brief,
            required_routes: options.required_routes,
        },
        snapshot,
    ))
}

fn build_plan(
    page: &Page,
    snapshot: Snapshot,
    priority: Priority,
    compare_priority: Option<Priority>,
) -> ResultView {
    let (budget, compare, media) = (page.budget, page.compare, page.media);
    let months = period_months(&snapshot.starts, &snapshot.ends).unwrap_or(1);
    let proposal = plan::assemble(
        &snapshot.data,
        u64::from(budget) * 10_000,
        media,
        priority,
        months,
        &page.required_routes,
    );
    let changes = (compare != 0 && compare != budget).then(|| {
        let previous = plan::assemble(
            &snapshot.data,
            u64::from(compare) * 10_000,
            media,
            compare_priority.unwrap_or(priority),
            months,
            &page.required_routes,
        );
        change_set(&previous, &proposal, compare)
    });
    ResultView {
        snapshot,
        proposal,
        changes,
        months,
    }
}

async fn page_data(store: Store, options: Options) -> Result<Page, WebError> {
    let compare_priority = options.compare_priority;
    let (mut page, snapshot) = load_plan(store, options).await?;
    page.result =
        snapshot.map(|snapshot| build_plan(&page, snapshot, Priority::default(), compare_priority));
    Ok(page)
}

async fn page(Query(options): Query<Options>) -> Result<Response, WebError> {
    if options.industry != Industry::Food {
        return Err(FOOD_ONLY);
    }
    if options.demo == Some(1)
        || options.batch.is_some()
        || options.budget.is_some()
        || options.media.is_some()
        || options.starts.is_some()
        || options.ends.is_some()
    {
        return Ok(Redirect::to(&legacy_plan_url(&options)?).into_response());
    }
    Ok(render(Page {
        heading: "提案任务",
        section: "tasks",
        industry: options.industry,
        media: Selection::Both,
        budget: SHOWCASE_BUDGET,
        budgets: [100, 200, 500],
        compare: 0,
        demo: false,
        result: None,
        notice: None,
        model_available: false,
        advice: None,
        brief: None,
        required_routes: Vec::new(),
    }))
}

fn legacy_plan_url(options: &Options) -> Result<String, WebError> {
    let mut url = format!(
        "/media-plans?industry={}&budget={}&media={}&compare={}",
        options.industry.id(),
        options.budget.unwrap_or(SHOWCASE_BUDGET),
        options.media.unwrap_or(Selection::Both).id(),
        options.compare.unwrap_or(0),
    );
    match (&options.starts, &options.ends) {
        (Some(starts), Some(ends)) if period_months(starts, ends).is_some() => {
            url.push_str(&format!("&starts={starts}&ends={ends}"));
        }
        (None, None) => {}
        _ => return Err((StatusCode::BAD_REQUEST, "请选择 2027 年内的有效起止日期。")),
    }
    if let Some(batch) = options.batch {
        url.push_str(&format!("&batch={batch}"));
    }
    if options.demo == Some(1) {
        url.push_str("&demo=1");
    }
    Ok(url)
}

async fn plan_page(
    State(store): State<Store>,
    Extension(agents): Extension<Agents>,
    Query(options): Query<Options>,
) -> Result<Response, WebError> {
    let mut plan = page_data(store, options).await?;
    plan.model_available = agents.current().get(Role::Strategy).is_some();
    Ok(render_plan(plan))
}

fn render_plan(plan: Page) -> Response {
    render(PlanPage {
        heading: "方案报告",
        section: "plans",
        plan,
    })
}

async fn export(
    State(store): State<Store>,
    Extension(user): Extension<crate::auth::User>,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    Query(options): Query<Options>,
) -> Result<Response, WebError> {
    let workpaper = uri.path().ends_with("/snapshot-review");
    if workpaper && !user.admin {
        return Err((StatusCode::FORBIDDEN, "库存核验副本仅供管理员查阅。"));
    }
    let preview = workpaper || options.preview == Some(1);
    let brand = Brand::read(&store).await?;
    let page = page_data(store.clone(), options.clone()).await?;
    let result = page
        .result
        .as_ref()
        .filter(|view| {
            view.proposal.complete_media
                && view.proposal.complete_required()
                && !view.proposal.items.is_empty()
        })
        .ok_or((
            StatusCode::CONFLICT,
            "当前无完整可导出的方案。请核对档期、媒体类型和报价。",
        ))?;
    if !result.snapshot.data.demo && !workpaper {
        return Err((StatusCode::CONFLICT, "正式报告由已核验的提案任务发布。"));
    }
    let variants = variants_for_snapshot(&page, &result.snapshot);
    let options = Options {
        starts: Some(result.snapshot.starts.clone()),
        ends: Some(result.snapshot.ends.clone()),
        budget: Some(page.budget),
        media: Some(page.media),
        demo: result.snapshot.data.demo.then_some(1),
        brief: page.brief.clone(),
        ..options
    };
    let html = render_export(&page, result, variants, &[], &brand, &options)?;
    let timestamp = database(move || crate::web::report_clock(&store.connect()?)).await?;
    export_response(
        crate::web::stamp_report(html, &timestamp),
        preview,
        page.industry,
        page.media,
        page.budget,
        &result.snapshot.starts,
        if result.snapshot.data.demo {
            ReportFile::Demo
        } else {
            ReportFile::Draft
        },
    )
}

fn assemble_tier(
    data: &Dataset,
    limit: u32,
    media: Selection,
    priority: Priority,
    months: u64,
    required: &[String],
) -> Proposal {
    let target = u64::from(limit) * 10_000;
    let base = plan::assemble(data, target, media, priority, months, required);
    let expanded = plan::assemble(
        data,
        target + target / 20,
        media,
        priority,
        months,
        required,
    );
    let valid = |value: &Proposal| {
        value.complete_media && value.complete_required() && !value.items.is_empty()
    };
    if valid(&expanded)
        && (!valid(&base)
            || expanded.total.high.abs_diff(target) < base.total.high.abs_diff(target))
    {
        expanded
    } else {
        base
    }
}

fn variants_for_snapshot(page: &Page, snapshot: &Snapshot) -> Vec<visuals::Variant> {
    let months = period_months(&snapshot.starts, &snapshot.ends).unwrap_or(1);
    let mut variants = visuals::Strategy::ALL
        .into_iter()
        .map(|strategy| {
            let limit = page.budget;
            let proposal = assemble_tier(
                &snapshot.data,
                limit,
                page.media,
                strategy.priority(),
                months,
                &page.required_routes,
            );
            let period = visuals::Period::new(
                &snapshot.starts,
                &snapshot.ends,
                "按客户指定档期核算。".into(),
                u64::from(limit) * 10_000,
                Some((snapshot, &proposal)),
            );
            visuals::Variant::new(strategy, limit, None, vec![period])
        })
        .collect::<Vec<_>>();
    visuals::Variant::mark_duplicates(&mut variants);
    variants
}

fn optimize_snapshot(
    options: &Options,
    snapshot: &Snapshot,
    budget: u32,
    policy: optimizer::Policy,
) -> optimizer::Portfolio {
    let mut portfolio = optimizer::Portfolio::default();
    for strategy in visuals::Strategy::ALL {
        portfolio.record(
            strategy,
            snapshot,
            optimizer::Request {
                data: &snapshot.data,
                target: u64::from(budget) * 10_000,
                target_band: allocation_budget_band(budget, u64::from(budget) * 10_000),
                hard_cap: policy.hard_cap,
                media: options.media.unwrap_or(Selection::Both),
                priority: strategy.priority(),
                emphasis: policy.emphasis,
                confirmed_focus: policy.confirmed_focus,
                months: period_months(&snapshot.starts, &snapshot.ends).unwrap_or(1),
                routes: &options.required_routes,
            },
        );
    }
    portfolio
}

fn optimized_snapshot_variants(
    options: &Options,
    snapshot: &Snapshot,
    portfolio: &optimizer::Portfolio,
) -> Vec<visuals::Variant> {
    let budget = options.budget.unwrap_or(SHOWCASE_BUDGET);
    let months = period_months(&snapshot.starts, &snapshot.ends).unwrap_or(1);
    let selection = options.media.unwrap_or(Selection::Both);
    let mut variants = visuals::Strategy::ALL
        .into_iter()
        .map(|strategy| {
            let proposal = portfolio.proposal(
                strategy,
                snapshot,
                selection,
                months,
                &options.required_routes,
            );
            let period = visuals::Period::new(
                &snapshot.starts,
                &snapshot.ends,
                "按客户指定档期核算。".into(),
                u64::from(budget) * 10_000,
                proposal.as_ref().map(|(_, proposal)| (snapshot, proposal)),
            );
            visuals::Variant::new(strategy, budget, options.hard_cap, vec![period])
        })
        .collect::<Vec<_>>();
    visuals::Variant::mark_duplicates(&mut variants);
    variants
}

fn selected_inventory(
    starts: &str,
    ends: &str,
    proposal: &plan::Proposal,
) -> Option<crate::refresh::InventoryCheck> {
    if !proposal.complete_media || !proposal.complete_required() || proposal.items.is_empty() {
        return None;
    }
    Some(crate::refresh::InventoryCheck {
        starts: starts.to_owned(),
        ends: ends.to_owned(),
        demands: proposal
            .items
            .iter()
            .map(|item| {
                let stock = &item.stock;
                crate::refresh::StockDemand {
                    medium: match stock.medium {
                        BUS => "bus",
                        METRO => "metro",
                    },
                    name: stock.name.clone(),
                    code: stock.code.clone(),
                    operator: stock.operator.clone(),
                    grade: stock.grade.clone(),
                    format: stock.format.clone(),
                    quantity: item.quantity,
                }
            })
            .collect(),
    })
}

fn render_export(
    page: &Page,
    result: &ResultView,
    variants: Vec<visuals::Variant>,
    guidance: &[(&'static str, &'static str)],
    brand: &Brand,
    options: &Options,
) -> Result<String, WebError> {
    let coverage = result
        .proposal
        .items
        .iter()
        .filter_map(|item| {
            let (edge, nearest) = item
                .coverage
                .iter()
                .filter_map(|cover| cover.distance.map(|distance| (cover, distance)))
                .min_by_key(|(_, distance)| *distance)?;
            Some(ReportCoverage {
                media: item.stock.name.as_str(),
                circle: edge.circle.as_str(),
                nearest,
                radius: edge.radius,
                station_level: edge.station.is_some(),
            })
        })
        .take(12)
        .collect();
    let full_evidence = variants.iter().all(visuals::Variant::has_full_evidence);
    Export {
        page,
        result,
        css: crate::web::CSS,
        guidance,
        brand,
        coverage,
        variants,
        full_evidence,
        campaign: brief::CampaignView::new(options),
    }
    .render()
    .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "无法渲染方案 HTML。"))
}

enum ReportFile {
    Demo,
    Draft,
    Confirmed,
}

fn export_response(
    html: String,
    preview: bool,
    industry: Industry,
    media: Selection,
    budget: u32,
    starts: &str,
    file: ReportFile,
) -> Result<Response, WebError> {
    let mut response = crate::web::report_response(html);
    if !preview {
        let status = match file {
            ReportFile::Demo => "",
            ReportFile::Draft => "draft-",
            ReportFile::Confirmed => "confirmed-",
        };
        let name = format!(
            "attachment; filename=cityline-{status}{}-{}-{}w-{}.html",
            industry.id(),
            media.id(),
            budget,
            starts,
        );
        response.headers_mut().insert(
            header::CONTENT_DISPOSITION,
            header::HeaderValue::from_str(&name).map_err(|_| INVALID)?,
        );
    }
    Ok(response)
}

struct Upload {
    file: String,
    bytes: Vec<u8>,
}

type Uploads = BTreeMap<String, Vec<Upload>>;

async fn receive(mut body: Multipart) -> Result<(BTreeMap<String, String>, Uploads), WebError> {
    let mut text = BTreeMap::new();
    let mut files = Uploads::new();
    while let Some(mut field) = body.next_field().await.map_err(|_| INVALID)? {
        let name = field.name().ok_or(INVALID)?.to_owned();
        if matches!(
            name.as_str(),
            "bus_stock"
                | "metro_stock"
                | "bus_coverage"
                | "metro_coverage"
                | "combined_coverage"
                | "metro_specs"
                | "reach"
                | "route_geometry"
                | "official_maps"
                | "rates"
        ) {
            let Some(file) = field
                .file_name()
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
            else {
                continue;
            };
            if file.chars().count() > 120
                || file.contains(['/', '\\'])
                || file.chars().any(char::is_control)
            {
                return Err(INVALID);
            }
            let file = anonymize_food_case(&file).into_owned();
            let limit = if name == "bus_stock" { 5 } else { 1 };
            let uploads = files.entry(name).or_default();
            if uploads.len() >= limit || uploads.iter().any(|upload| upload.file == file) {
                return Err((
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "请检查文件数量及重复文件名。",
                ));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = field.chunk().await.map_err(|_| INVALID)? {
                if bytes.len() + chunk.len() > UPLOAD_BYTES {
                    return Err((StatusCode::PAYLOAD_TOO_LARGE, "单份表格不能超过 2 MiB。"));
                }
                bytes.extend_from_slice(&chunk);
            }
            uploads.push(Upload { file, bytes });
        } else if matches!(
            name.as_str(),
            "industry"
                | "starts"
                | "ends"
                | "observed_at"
                | "circle"
                | "radius"
                | "attested"
                | "metro_station"
                | "cover_center"
                | "supplemental_metro_center"
                | "required_routes"
                | "batch"
        ) {
            let raw = field.text().await.map_err(|_| INVALID)?;
            if raw.len() > 300
                || raw.chars().any(char::is_control)
                || text.insert(name, raw.trim().to_owned()).is_some()
            {
                return Err(INVALID);
            }
        } else {
            return Err(INVALID);
        }
    }
    Ok((text, files))
}

struct ImportScope<'a> {
    circle: &'a str,
    radius: u32,
    metro_station: &'a str,
    cover_center: &'a str,
    supplemental_metro_center: &'a str,
    required_routes: &'a [String],
    official_inventory: bool,
}

fn make_dataset(mut files: Uploads, scope: ImportScope<'_>) -> Result<Dataset, &'static str> {
    let ImportScope {
        circle,
        radius,
        metro_station,
        cover_center,
        supplemental_metro_center,
        required_routes,
        official_inventory,
    } = scope;
    let mut dataset = Dataset {
        confirmed_core: None,
        stock: Vec::new(),
        empty_inventory: BTreeMap::new(),
        coverage: Vec::new(),
        rates: Vec::new(),
        demo: false,
        supplemental_metro_center: None,
        evidence: visuals::Evidence::default(),
    };
    let combined = files
        .remove("combined_coverage")
        .and_then(|mut items| items.pop());
    let has_combined = combined.is_some();
    if !supplemental_metro_center.is_empty() {
        if !has_combined || !files.contains_key("metro_stock") {
            return Err("地铁补充圈需同时提供新云合并覆盖表和红豆地铁库存。");
        }
        dataset.supplemental_metro_center = Some(supplemental_metro_center.into());
    }
    if let Some(upload) = combined {
        if files.contains_key("bus_coverage") || files.contains_key("metro_coverage") {
            return Err("新云合并覆盖表与独立覆盖表请选择一种上传。");
        }
        dataset.coverage = import::combined_coverage(
            &import::Table::read(upload.file, &upload.bytes)?,
            cover_center,
            supplemental_metro_center,
        )?;
    }
    let mut specs = BTreeMap::new();
    if let Some(upload) = files
        .remove("metro_specs")
        .and_then(|mut items| items.pop())
    {
        if !files.contains_key("metro_stock") {
            return Err("规格表须与同批地铁库存表一起导入。");
        }
        specs = import::metro_specs(&import::Table::read(upload.file, &upload.bytes)?)?;
    }
    let empty_specs = BTreeMap::new();
    for (medium, stock, coverage) in [
        (BUS, "bus_stock", "bus_coverage"),
        (METRO, "metro_stock", "metro_coverage"),
    ] {
        let cover_file = files.remove(coverage).and_then(|mut items| items.pop());
        match (files.remove(stock), cover_file) {
            (Some(stocks), cover_file)
                if has_combined
                    || cover_file.is_some()
                    || (medium == BUS && !required_routes.is_empty()) =>
            {
                let station = if medium == METRO { metro_station } else { "" };
                let catalog = if medium == METRO {
                    &specs
                } else {
                    &empty_specs
                };
                for stock in stocks {
                    let api_file = match medium {
                        BUS => "红豆库存API-公交.csv",
                        METRO => "红豆库存API-地铁.csv",
                    };
                    let allow_empty = official_inventory && stock.file == api_file;
                    let table = import::Table::read(stock.file, &stock.bytes)?;
                    let rows = import::stock(&table, medium, station, catalog, allow_empty)?;
                    if rows.is_empty()
                        && dataset
                            .empty_inventory
                            .insert(
                                medium,
                                Source {
                                    file: api_file.into(),
                                    row: 1,
                                },
                            )
                            .is_some()
                    {
                        return Err("同一档期的媒体库存包含重复的 API 空结果。");
                    }
                    dataset.stock.extend(rows);
                }
                if let Some(coverage) = cover_file {
                    dataset.coverage.extend(import::coverage(
                        &import::Table::read(coverage.file, &coverage.bytes)?,
                        medium,
                        circle,
                        radius,
                    )?);
                }
            }
            (None, None) => {}
            _ => return Err("同一媒体需要同时导入库存表和覆盖表。"),
        }
    }
    if dataset.stock.is_empty() && dataset.empty_inventory.is_empty() {
        return Err("请至少导入公交或地铁其中一组数据。");
    }
    let supplied_media: BTreeSet<_> = dataset.stock.iter().map(|row| row.medium).collect();
    if dataset
        .empty_inventory
        .keys()
        .any(|medium| supplied_media.contains(medium))
    {
        return Err("同一档期的媒体库存同时包含记录和 API 空结果。");
    }
    dataset
        .coverage
        .retain(|area| supplied_media.contains(&area.medium));
    let required_keys = required_routes
        .iter()
        .map(|route| plan::bus_route_key(route))
        .collect::<BTreeSet<_>>();
    let required_stock = |row: &Inventory| {
        row.medium == BUS && required_keys.contains(&plan::bus_route_key(&row.name))
    };
    if dataset.coverage.is_empty()
        && dataset.empty_inventory.is_empty()
        && !dataset.stock.iter().any(required_stock)
    {
        return Err("所选中心点的覆盖媒体与上传库存类型不一致。");
    }
    dataset
        .stock
        .retain(|row| plan::has_coverage(row, &dataset.coverage) || required_stock(row));
    let mut metro_points = BTreeSet::new();
    let mut metro_codes = BTreeSet::new();
    if dataset.stock.iter().any(|row| {
        row.medium == METRO
            && (!metro_points.insert(plan::key(&row.name))
                || (!row.code.is_empty() && !metro_codes.insert(plan::key(&row.code))))
    }) {
        return Err("地铁库存包含重复出口，请按点位编号核对导出表。");
    }
    let rates = files
        .remove("rates")
        .and_then(|mut uploads| uploads.pop())
        .ok_or("请上传脱敏刊例价表。")?;
    dataset.rates = import::rates(&import::Table::read(rates.file, &rates.bytes)?)?;
    dataset
        .evidence
        .shapes
        .extend(dataset.stock.iter().filter_map(|row| {
            if row.medium != METRO {
                return None;
            }
            let shape = specs
                .get(&plan::key(&row.code))?
                .exit_shape(row, official_inventory)?;
            (!shape.repeats_stock_station(row, &dataset.coverage)).then_some(shape)
        }));
    update_evidence(&mut dataset, files)?;
    Ok(dataset)
}

#[cfg(test)]
mod import_tests {
    use super::{ImportScope, Upload, make_dataset};

    #[test]
    fn redbean_spec_coordinates_require_matching_outlet_and_declared_crs() {
        let files = || {
            [
                ("metro_stock", "地铁编号,地铁站名,出入口号,级别,空车数\nS01,测试站,A,A+,1\nS02,测试站,B,A+,1\nS03,测试站,C,A+,1\nS04,测试站,D,A+,1"),
                ("metro_specs", "地铁编号,地铁站名,出入口号,级别,面积,粘贴形式,坐标,坐标系\nS01,测试站,A,A+,9,灯箱,\"121.455,31.227\",BD-09\nS02,测试站,B,A+,9,灯箱,\"121.455,31.227\",\nS03,测试站,C,A+,9,灯箱,\"121.450,31.220\",BD-09\nS04,测试站,D,A+,9,灯箱,\"121.460,31.230\",GCJ-02\nS99,测试站,E,A+,9,灯箱,\"121.470,31.230\",BD-09"),
                ("combined_coverage", "明细分析\n中心点名称,媒体类型,媒体名称,覆盖半径,最近距离,最近站点,站点经度,站点纬度\n测试圈,地铁出入口,地铁1号线,500米,100米,测试站,121.450,31.220"),
                ("rates", "媒体类型,匹配对象,发布费,计价数量,计价周期,制作费\n地铁出入口,A+,约15–25万,1,1月,100元/㎡"),
            ]
            .into_iter()
            .map(|(field, csv)| (field.into(), vec![Upload { file: format!("{field}.csv"), bytes: csv.as_bytes().to_vec() }]))
            .collect()
        };
        for official_inventory in [false, true] {
            let data = make_dataset(
                files(),
                ImportScope {
                    circle: "",
                    radius: 0,
                    metro_station: "",
                    cover_center: "测试圈",
                    supplemental_metro_center: "",
                    required_routes: &[],
                    official_inventory,
                },
            )
            .unwrap();
            assert_eq!(data.evidence.shapes.len(), 1);
            assert_eq!(data.evidence.shapes[0].resource, "S01");
            assert_eq!(
                data.evidence.shapes[0].platform_verified,
                official_inventory
            );
        }
    }
}

fn read_evidence(mut files: Uploads) -> Result<visuals::Evidence, &'static str> {
    let mut additions = visuals::Evidence::default();
    if let Some(file) = files.remove("reach").and_then(|mut files| files.pop()) {
        additions.reach = import::reach(&import::Table::read(file.file, &file.bytes)?)?;
    }
    if let Some(file) = files
        .remove("route_geometry")
        .and_then(|mut files| files.pop())
    {
        let name = file.file.to_ascii_lowercase();
        additions.shapes = if [".csv", ".xlsx", ".xls"]
            .iter()
            .any(|ext| name.ends_with(ext))
        {
            import::metro_exit_points(&import::Table::read(file.file, &file.bytes)?)?
        } else {
            visuals::read_shapes(file.file, &file.bytes)?
        };
    }
    if let Some(file) = files
        .remove("official_maps")
        .and_then(|mut files| files.pop())
    {
        additions.maps = visuals::read_maps(&file.bytes)?;
    }
    Ok(additions)
}

fn update_evidence(dataset: &mut Dataset, files: Uploads) -> Result<(), &'static str> {
    let mut additions = read_evidence(files)?;
    for map in &mut additions.maps {
        map.basemap_verified = false;
    }
    additions.check_exit_points(&dataset.stock, &dataset.coverage)?;
    dataset.evidence.supplement(additions, &dataset.stock)
}

#[derive(Deserialize)]
struct SupplementContext {
    task: Option<Uuid>,
}

async fn supplement(
    State(store): State<Store>,
    Extension(refresh): Extension<Refresh>,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
    Query(context): Query<SupplementContext>,
    body: Multipart,
) -> Result<Redirect, WebError> {
    if let Some(task) = context.task {
        tasks::check_supplement_context(&store, task, id).await?;
    }
    let (fields, files) = receive(body).await?;
    if fields.len() != 1
        || fields.get("attested").is_none_or(|value| value != "yes")
        || files.is_empty()
        || files
            .keys()
            .any(|key| !matches!(key.as_str(), "reach" | "route_geometry" | "official_maps"))
    {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "请上传新云证据文件并确认原始来源。",
        ));
    }
    let added = database(move || {
        let mut db = store.connect()?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let id = id.to_string();
        let base: Option<(String, String, String, Option<String>)> = tx.query_row(
            "SELECT latest.id, latest.payload, root.id, root.refresh_id
             FROM media_snapshots AS requested
             JOIN media_snapshots AS root ON root.id=coalesce(requested.evidence_root,requested.id)
             JOIN media_snapshots AS latest ON latest.id=coalesce((
                 SELECT id FROM media_snapshots WHERE evidence_root=root.id ORDER BY rowid DESC LIMIT 1
             ),root.id)
             WHERE requested.id=?1 AND root.industry='food'",
            [&id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ).optional()?;
        let Some((base, previous, root, refresh_id)) = base else {
            return Ok(Err((StatusCode::NOT_FOUND, "未找到数据快照。")));
        };
        if refresh_id.as_deref().and_then(|id| id.parse().ok()).is_some_and(|id| refresh.is_active(id)) {
            return Ok(Err((StatusCode::CONFLICT, "此快照正在接收平台数据，请在获取数据完成后补证。")));
        }
        let mut dataset: Dataset = serde_json::from_str(&previous).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
        })?;
        if let Err(error) = update_evidence(&mut dataset, files) {
            return Ok(Err((StatusCode::UNPROCESSABLE_ENTITY, error)));
        }
        let payload = serde_json::to_string(&dataset).map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        let saved = if payload != previous {
            let next = Uuid::new_v4().to_string();
            tx.execute(
                "INSERT INTO media_snapshots (id,industry,starts,ends,observed_at,payload,source,media_mask,evidence_root)
                 SELECT ?1,industry,starts,ends,observed_at,?2,source,media_mask,?3
                 FROM media_snapshots WHERE id=?4",
                params![next, payload, root, base],
            )?;
            crate::audit::write(&tx, "平台", "补充线路和触达证据", context.task,
                &format!("原快照 {base} · 新快照 {next} · 操作入口 {id}"))?;
            next
        } else {
            base
        };
        tx.commit()?;
        Ok(Ok(saved))
    }).await?;
    let saved = added?;
    if let Some(task) = context.task {
        return Ok(Redirect::to(&format!(
            "/media-data?batch={saved}&task={task}"
        )));
    }
    Ok(Redirect::to(&format!("/media-data?batch={saved}")))
}

#[derive(Deserialize)]
struct ImportView {
    view: Option<String>,
}

async fn upload(
    State(store): State<Store>,
    worker: Option<Extension<BrowserImport>>,
    Query(view): Query<ImportView>,
    body: Multipart,
) -> Result<Redirect, WebError> {
    let (fields, files) = receive(body).await?;
    let get = |name: &str| fields.get(name).map(String::as_str).unwrap_or_default();
    let industry = Industry::parse(get("industry"))
        .filter(|industry| *industry == Industry::Food)
        .ok_or(FOOD_ONLY)?
        .id();
    let starts = get("starts").to_owned();
    let ends = get("ends").to_owned();
    let observed = get("observed_at").to_owned();
    let circle = get("circle").to_owned();
    let metro_station = get("metro_station").to_owned();
    let cover_center = get("cover_center").to_owned();
    let supplemental_metro_center = get("supplemental_metro_center").to_owned();
    let required_routes = plan::parse_required_routes(get("required_routes"))
        .map_err(|message| (StatusCode::UNPROCESSABLE_ENTITY, message))?;
    let worker = worker.map(|Extension(worker)| worker);
    let refresh_id = worker.as_ref().map(|worker| worker.id.to_string());
    let source = if refresh_id.is_some() {
        "platform"
    } else {
        "manual"
    };
    const REQUEST_MISMATCH: WebError = (
        StatusCode::UNPROCESSABLE_ENTITY,
        "平台导出与本次获取数据的条件不一致。",
    );
    if let Some(worker) = &worker {
        let request = &worker.request;
        let center = plan::key(&cover_center);
        let supplemental = request
            .supplemental_center
            .filter(|_| request.media != "bus")
            .unwrap_or_default();
        let matches_period = request
            .periods
            .iter()
            .any(|period| period.starts == starts && period.ends == ends);
        if request.industry != industry
            || !matches_period
            || plan::key(&request.center) != center
            || plan::key(supplemental) != plan::key(&supplemental_metro_center)
            || request.required_routes != required_routes
        {
            return Err(REQUEST_MISMATCH);
        }
    }
    let radius = match get("radius") {
        "" => 0,
        value => value.parse::<u32>().map_err(|_| INVALID)?,
    };
    let valid_timestamp = observed.len() == 16
        && observed.bytes().enumerate().all(|(i, ch)| match i {
            4 | 7 => ch == b'-',
            10 => ch == b'T',
            13 => ch == b':',
            _ => ch.is_ascii_digit(),
        });
    if period_months(&starts, &ends).is_none()
        || !valid_timestamp
        || get("attested") != "yes"
        || radius > 5000
        || metro_station.chars().count() > 60
        || cover_center.chars().count() > 260
        || supplemental_metro_center.chars().count() > 100
    {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "请填入 2027 年内的有效起止日期、核对时间及覆盖半径，并确认库存查询日期。",
        ));
    }
    let official_inventory = worker.is_some();
    let parsed = tokio::task::spawn_blocking(move || {
        make_dataset(
            files,
            ImportScope {
                circle: &circle,
                radius,
                metro_station: &metro_station,
                cover_center: &cover_center,
                supplemental_metro_center: &supplemental_metro_center,
                required_routes: &required_routes,
                official_inventory,
            },
        )
    })
    .await
    .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "导入处理中断。"))?;
    let dataset = parsed.map_err(|error| (StatusCode::UNPROCESSABLE_ENTITY, error))?;
    let has_bus = dataset.empty_inventory.contains_key(&BUS)
        || dataset.stock.iter().any(|row| row.medium == BUS)
        || dataset.coverage.iter().any(|row| row.medium == BUS);
    let has_metro = dataset.empty_inventory.contains_key(&METRO)
        || dataset.stock.iter().any(|row| row.medium == METRO)
        || dataset.coverage.iter().any(|row| row.medium == METRO);
    let selected = if !has_metro {
        Selection::Bus
    } else if !has_bus {
        Selection::Metro
    } else {
        Selection::Both
    };
    if worker
        .as_ref()
        .is_some_and(|worker| worker.request.media != selected.id())
    {
        return Err(REQUEST_MISMATCH);
    }
    let payload = serde_json::to_string(&dataset)
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "导入结果无法保存。"))?;
    let (id, starts, ends) = database(move || {
        let mut db = store.connect()?;
        let tx = db.transaction()?;
        let proposed = Uuid::new_v4().to_string();
        let written = tx.execute(
            "INSERT INTO media_snapshots (id,industry,starts,ends,observed_at,payload,source,refresh_id,media_mask)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT(refresh_id, starts, ends) DO NOTHING",
            params![proposed, industry, starts, ends, observed, anonymize_food_case(&payload).as_ref(), source, refresh_id, selected.mask()],
        )?;
        let id: String = if written == 1 {
            let batch = refresh_id.as_deref().map(|id| format!(" · 获取数据批次 {id}")).unwrap_or_default();
            crate::audit::write(&tx, "平台", "导入快照", None, &format!("{} · {industry} · {starts} 至 {ends} · 快照 {proposed}{batch}", source_label(source)))?;
            proposed
        } else {
            tx.query_row("SELECT id FROM media_snapshots WHERE refresh_id=?1 AND starts=?2 AND ends=?3", params![refresh_id, starts, ends], |row| row.get(0))?
        };
        tx.commit()?;
        Ok((id, starts, ends))
    }).await?;
    if worker.is_none() && view.view.as_deref() == Some("data") {
        return Ok(Redirect::to(&format!("/media-data?batch={id}")));
    }
    let url = format!(
        "/media-proposals?industry={industry}&media={}&starts={starts}&ends={ends}&batch={id}",
        selected.id()
    );
    Ok(Redirect::to(&url))
}

fn synthetic(industry: Industry) -> Snapshot {
    let source = |file: &str, row| Source {
        file: file.into(),
        row,
    };
    let mut stock = Vec::new();
    let mut coverage = Vec::new();
    let mut exposure = Vec::new();
    let core = format!("{} · 试饮核心商圈", industry.brand());
    for index in 1..=32 {
        let medium = if index % 2 == 0 { BUS } else { METRO };
        let near_core = matches!(index % 8, 1 | 2);
        let name = format!("示例{} {:02}", medium.label(), index);
        let format = if medium == BUS { "大三侧" } else { "灯箱" };
        stock.push(Inventory {
            medium,
            name: name.clone(),
            station: None,
            station_attested: false,
            code: String::new(),
            metro_spec: None,
            operator: String::new(),
            grade: "A+".into(),
            format: format.into(),
            free: if near_core && medium == BUS {
                3
            } else {
                u32::from(index != 5)
            },
            area: Some(10),
            source: source("合成库存表", index + 1),
        });
        exposure.push(visuals::Reach {
            medium,
            resource: name.clone(),
            month: 6,
            daily: 3_000 + index as u32 * 113,
            reference: "合成样例单位日触达".into(),
            source: source("合成触达表", index + 1),
            baseline: false,
            unit_verified: true,
            platform_verified: false,
        });
        coverage.push(Cover {
            center_point: None,
            media_point: None,
            medium,
            name,
            station: None,
            metro_line: None,
            operator: String::new(),
            format: format.into(),
            radius: 500,
            distance: Some(if near_core {
                100 + index as u32 * 4
            } else {
                250 + index as u32 * 5
            }),
            circle: if near_core {
                core.clone()
            } else {
                format!("{} · 示例圈 {}", industry.brand(), index % 6 + 1)
            },
            source: source("合成覆盖表", index + 1),
        });
    }
    Snapshot {
        id: None,
        data: Dataset {
            confirmed_core: Some(core),
            evidence: visuals::Evidence {
                reach: exposure,
                ..Default::default()
            },
            stock,
            empty_inventory: BTreeMap::new(),
            coverage,
            demo: true,
            supplemental_metro_center: None,
            rates: vec![
                Rate {
                    medium: BUS,
                    target: "A+".into(),
                    fee: Cost {
                        low: 30_000,
                        high: 50_000,
                    },
                    production: Production::Fixed(Cost {
                        low: 5000,
                        high: 10_000,
                    }),
                    unit: 1,
                    monthly: true,
                    source: source("合成刊例表", 2),
                },
                Rate {
                    medium: METRO,
                    target: "A+".into(),
                    fee: Cost {
                        low: 150_000,
                        high: 250_000,
                    },
                    production: Production::ByArea(100),
                    unit: 1,
                    monthly: true,
                    source: source("合成刊例表", 3),
                },
            ],
        },
        starts: "2027-06-01".into(),
        ends: "2027-06-30".into(),
        observed: "合成样例".into(),
        imported: "合成样例".into(),
        source: "合成样例",
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Advice {
    angle: String,
}

async fn strategy(
    State(store): State<Store>,
    Extension(agents): Extension<Agents>,
    Form(options): Form<Options>,
) -> Result<Response, WebError> {
    let models = agents.current();
    let model = models
        .get(Role::Strategy)
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "策略模型尚未配置。"))?;
    let mut page = page_data(store.clone(), options).await?;
    page.model_available = true;
    let Some(_slot) = model.permit() else {
        page.notice = Some("提案 Agent 正忙。已保留当前方案，可稍后重试。");
        return Ok(render_plan(page));
    };
    let prompt = Model::system_prompt(
        "户外媒体提案策略 Agent",
        "为给定行业示例选择一个适合本案的提案重点。只返回 {\"angle\":\"visit|commute|awareness\"}，分别代表到访引导、通勤覆盖、品牌认知。不要生成价格、库存或人流。",
    );
    let output: Option<Advice> = model
        .complete_structured(
            &prompt,
            &serde_json::json!({
                "industry": page.industry.label(),
                "example_objective": page.industry.campaign(),
                "requested_media": page.media.id(),
            }),
        )
        .await
        .ok();
    page.advice = match output.as_ref().map(|advice| advice.angle.as_str()) {
        Some("visit") => Some("AI 建议以到访引导为重点，可优先核对入口和步行到达路径。"),
        Some("commute") => Some("AI 建议以通勤覆盖为重点，可优先核对路线途经的主要换乘点。"),
        Some("awareness") => Some("AI 建议以品牌认知为重点，可从目标圈的总体覆盖解释投放组合。"),
        _ => {
            page.notice = Some("提案 Agent 暂未返回可用策略。已保留当前方案。");
            None
        }
    };
    crate::audit::record(
        &store,
        "Agent",
        "提案重点建议",
        None,
        if page.advice.is_some() {
            "模型返回结构化策略。"
        } else {
            "模型未返回可用结果。"
        }
        .into(),
    )
    .await?;
    Ok(render_plan(page))
}

pub(crate) fn router(store: Store, agents: Agents, refresh: crate::refresh::Refresh) -> Router {
    Router::new()
        .route("/media-proposals", get(page))
        .merge(tasks::router())
        .merge(data::router())
        .merge(brief::router())
        .merge(platform::router())
        .route("/media-plans", get(plan_page))
        .route("/media-proposals/export", get(export))
        .route("/media-proposals/snapshot-review", get(export))
        .route("/media-proposals/strategy", post(strategy))
        .route(
            "/api/media-proposals/snapshots/{id}/evidence",
            post(supplement).layer(DefaultBodyLimit::max(4 * UPLOAD_BYTES + 64 * 1024)),
        )
        .route(
            "/api/media-proposals/import",
            post(upload).layer(DefaultBodyLimit::max(12 * UPLOAD_BYTES + 64 * 1024)),
        )
        .layer(Extension(agents))
        .layer(Extension(refresh))
        .with_state(store)
}
