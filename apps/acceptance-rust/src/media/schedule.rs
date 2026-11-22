use std::collections::BTreeMap;

use askama::Template;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    BUS, Changes, METRO, Options, Snapshot, change_set, month_days,
    optimizer::{Policy, Portfolio, Request},
    period_months,
    plan::{Cost, Proposal, Selection},
    visuals::{Period, Strategy, Variant},
};
use crate::{
    settings::Brand,
    web::{self, WebError},
};

pub(super) const SHOWCASE_PHASES: &str = "2027-05-01 | 1 | 预热 · 新品试饮\n2027-07-01 | 3 | 主投 · 暑期试饮\n2027-10-01 | 1 | 回访 · 重点门店";

#[derive(Clone, Default, Deserialize, Serialize)]
pub(crate) struct Rolling {
    pub(super) factors: BTreeMap<String, Factor>,
    #[serde(default)]
    pub(super) batches: BTreeMap<String, Uuid>,
    #[serde(default)]
    staged: bool,
    #[serde(default)]
    snapshot_span: bool,
    #[serde(default)]
    suggest_phases: bool,
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Warmup,
    Main,
    Followup,
}

impl Phase {
    fn factor(self) -> Factor {
        let (weight, reason) = match self {
            Self::Warmup => (1, "预热 · 提前建立线下曝光"),
            Self::Main => (3, "主投 · 集中覆盖目标圈"),
            Self::Followup => (1, "补投 · 补充覆盖并留存复盘依据"),
        };
        Factor {
            weight,
            reason: reason.into(),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PhaseChoice {
    month: String,
    phase: Phase,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Factor {
    weight: u32,
    reason: String,
}

impl Default for Factor {
    fn default() -> Self {
        Self {
            weight: 1,
            reason: "常规投放".into(),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct Window {
    pub(crate) starts: String,
    pub(crate) ends: String,
    days: u32,
    factor: Factor,
}

impl Window {
    pub(crate) fn needs_stock(&self) -> bool {
        self.factor.weight > 0
    }

    pub(super) fn booking_label(&self) -> String {
        format!(
            "{} 至 {} · 权重 {}",
            self.starts, self.ends, self.factor.weight
        )
    }

    pub(super) fn planning_context(&self) -> serde_json::Value {
        let phase = if self.factor.reason.starts_with("预热") {
            Some(Phase::Warmup)
        } else if self.factor.reason.starts_with("主投") {
            Some(Phase::Main)
        } else if self.factor.reason.starts_with("回访") || self.factor.reason.starts_with("补投")
        {
            Some(Phase::Followup)
        } else {
            None
        };
        serde_json::json!({"starts": self.starts, "ends": self.ends, "days": self.days,
            "weight": self.factor.weight, "phase": phase})
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct ScheduleRevision {
    pub(super) previous_start: String,
    pub(super) previous_end: String,
    pub(super) before: Vec<String>,
    pub(super) after: Vec<String>,
    pub(super) reason: String,
    pub(super) review_operations: bool,
}

impl Rolling {
    pub(super) fn for_snapshot(starts: &str, id: Uuid) -> Self {
        Self {
            batches: BTreeMap::from([(starts.to_owned(), id)]),
            snapshot_span: true,
            ..Self::default()
        }
    }

    pub(crate) fn showcase(input: &str, starts: &str, ends: &str) -> Result<Self, &'static str> {
        let mut plan = Self::revised(None, input, starts, ends)?;
        plan.suggest_phases = input.trim().is_empty();
        Ok(plan)
    }

    pub(super) fn candidate_months(&self, starts: &str, ends: &str) -> Vec<String> {
        if self.suggest_phases {
            months_between(starts, ends)
        } else {
            Vec::new()
        }
    }

    pub(super) fn adopt_phases(&mut self, months: &[String], choices: &[PhaseChoice]) -> bool {
        if months.is_empty()
            || choices.len() > 3
            || !choices.iter().any(|choice| choice.phase == Phase::Main)
            || choices.iter().any(|choice| !months.contains(&choice.month))
            || !choices
                .windows(2)
                .all(|pair| pair[0].month < pair[1].month && pair[0].phase < pair[1].phase)
        {
            return false;
        }
        self.factors = choices
            .iter()
            .map(|choice| (format!("{}-01", choice.month), choice.phase.factor()))
            .collect();
        true
    }

    pub(super) fn revised(
        previous: Option<&Self>,
        input: &str,
        starts: &str,
        ends: &str,
    ) -> Result<Self, &'static str> {
        period_months(starts, ends).ok_or("请选择 2027 年内的有效起止日期。")?;
        let first_month = &starts[..7];
        let last_month = &ends[..7];
        let mut rolling = Self {
            staged: true,
            ..Self::default()
        };
        let mut overrides = BTreeMap::new();
        for line in input.lines().map(str::trim).filter(|line| !line.is_empty()) {
            let mut fields = line.splitn(3, '|').map(str::trim);
            let date = fields.next().unwrap_or_default();
            let weight = fields
                .next()
                .and_then(|value| value.parse::<u32>().ok())
                .filter(|weight| *weight <= 3);
            let reason = fields.next().filter(|value| {
                !value.is_empty()
                    && value.chars().count() <= 80
                    && !value.chars().any(char::is_control)
            });
            let (Some(weight), Some(reason)) = (weight, reason) else {
                return Err("调配依据按「月初日期 | 权重0–3 | 原因」填写，每行一段。");
            };
            if super::date_month(date).is_none()
                || !date.ends_with("-01")
                || overrides
                    .insert(
                        date.into(),
                        Factor {
                            weight,
                            reason: super::anonymize_food_case(reason).into_owned(),
                        },
                    )
                    .is_some()
            {
                return Err("请填写 2027 年内的月首日期，每月只填一次。");
            }
            if &date[..7] < first_month || &date[..7] > last_month {
                return Err("调配月份须在所选档期内。");
            }
        }
        if let Some(previous) = previous.filter(|plan| plan.staged) {
            rolling
                .factors
                .extend(previous.factors.iter().filter_map(|(month, factor)| {
                    let key = month.get(..7)?;
                    (first_month <= key && key <= last_month)
                        .then(|| (month.clone(), factor.clone()))
                }));
        }
        rolling.factors.extend(overrides);
        if rolling.factors.is_empty() {
            let months = months_between(starts, ends);
            let main = months.len() / 2;
            for (index, phase) in [
                (0, Phase::Warmup),
                (main, Phase::Main),
                (months.len() - 1, Phase::Followup),
            ] {
                if (phase != Phase::Main) && index == main {
                    continue;
                }
                rolling
                    .factors
                    .insert(format!("{}-01", months[index]), phase.factor());
            }
        }
        if rolling.factors.values().all(|factor| factor.weight == 0) {
            return Err("请至少安排一个有投放预算的月份。");
        }
        Ok(rolling)
    }

    pub(crate) fn windows(&self, starts: &str, ends: &str) -> Result<Vec<Window>, &'static str> {
        period_months(starts, ends).ok_or("滚动排期需要有效的 2027 年起止日期。")?;
        if self.snapshot_span {
            return Ok(vec![Window {
                starts: starts.to_owned(),
                ends: ends.to_owned(),
                days: days_from_year_start(ends) - days_from_year_start(starts) + 1,
                factor: Factor {
                    weight: 1,
                    reason: "按已导入快照的完整档期核算".into(),
                },
            }]);
        }
        let anchor = if self.staged {
            1
        } else {
            starts[8..10]
                .parse::<u32>()
                .map_err(|_| "滚动排期起始日期无效。")?
        };
        let last = days_from_year_start(ends);
        let mut windows = Vec::new();
        let mut start = starts.to_owned();
        loop {
            let month = super::date_month(&start).unwrap_or(12);
            let boundary = if month == 12 {
                366
            } else {
                let next = format!("{:02}", month + 1);
                let day = anchor.min(u32::from(month_days(&next).unwrap_or(31)));
                days_from_year_start(&format!("2027-{next}-{day:02}"))
            };
            let end = (boundary - 1).min(last);
            let days = end - days_from_year_start(&start) + 1;
            let factor_key = if self.staged {
                format!("{}-01", &start[..7])
            } else {
                start.clone()
            };
            let factor = self.factors.get(&factor_key).cloned().unwrap_or_else(|| {
                if self.staged {
                    Factor {
                        weight: 0,
                        reason: "机动观察 · 本月无采买".into(),
                    }
                } else {
                    Factor::default()
                }
            });
            windows.push(Window {
                starts: start,
                ends: date_from_day(end),
                days,
                factor,
            });
            if end == last {
                break;
            }
            start = date_from_day(boundary);
        }
        if self.factors.keys().any(|date| {
            !windows.iter().any(|window| {
                if self.staged {
                    window.starts[..7] == date[..7]
                } else {
                    &window.starts == date
                }
            })
        }) {
            return Err("调配依据必须填投放段首日。以后各段以首段起始日为锚点，短月用月末。");
        }
        Ok(windows)
    }
}

fn months_between(starts: &str, ends: &str) -> Vec<String> {
    (super::date_month(starts).unwrap_or(1)..=super::date_month(ends).unwrap_or(12))
        .map(|month| format!("2027-{month:02}"))
        .collect()
}

fn days_from_year_start(day: &str) -> u32 {
    let month = day[5..7].parse::<u32>().unwrap_or(1);
    (1..month).map(days_in).sum::<u32>() + day[8..].parse::<u32>().unwrap_or(1)
}

fn days_in(month: u32) -> u32 {
    u32::from(month_days(&format!("{month:02}")).unwrap_or(31))
}

fn date_from_day(mut day: u32) -> String {
    for month in 1..=12 {
        let length = days_in(month);
        if day <= length {
            return format!("2027-{month:02}-{day:02}");
        }
        day -= length;
    }
    unreachable!("validated dates are inside 2027")
}

struct Segment {
    window: Window,
    snapshot: Option<Snapshot>,
}

struct Placement<'a> {
    segment: &'a Segment,
    planned: u64,
    limit: u64,
    proposal: Option<Proposal>,
}

impl Placement<'_> {
    fn planned_share(&self, budget: &u32) -> String {
        let ratio = self.planned as f64 / (f64::from(*budget) * 100.0);
        let formatted = format!("{ratio:.1}");
        format!("{}%", formatted.trim_end_matches(".0"))
    }

    fn ready(&self) -> bool {
        self.segment.window.factor.weight == 0
            || self.proposal.as_ref().is_some_and(|proposal| {
                proposal.complete_media
                    && proposal.complete_required()
                    && !proposal.items.is_empty()
            })
    }
}

pub(super) struct Calendar {
    segments: Vec<Segment>,
}

pub(super) struct Plan<'a> {
    placements: Vec<Placement<'a>>,
    pub(super) total: Cost,
}

impl Plan<'_> {
    fn has_flexible_windows(&self) -> bool {
        self.placements
            .iter()
            .any(|row| row.segment.window.factor.weight == 0)
    }
}

struct BudgetComparison {
    code: &'static str,
    name: &'static str,
    previous_total: Cost,
    current_total: Cost,
    periods: Vec<PeriodComparison>,
}

struct PeriodComparison {
    starts: String,
    ends: String,
    previous_limit: u64,
    current_limit: u64,
    previous_total: Cost,
    current_total: Cost,
    current_buses: u32,
    current_exits: u32,
    changes: Option<Changes>,
}

impl BudgetComparison {
    fn new(
        current: &Variant,
        current_plan: &Plan<'_>,
        previous: &Plan<'_>,
        previous_budget: u32,
    ) -> Self {
        let periods = current_plan
            .placements
            .iter()
            .zip(&previous.placements)
            .filter(|(row, _)| row.segment.window.needs_stock())
            .map(|(row, earlier)| {
                let window = &row.segment.window;
                let zero = Cost { low: 0, high: 0 };
                let current_plan = row.proposal.as_ref();
                let previous_plan = earlier.proposal.as_ref();
                PeriodComparison {
                    starts: window.starts.clone(),
                    ends: window.ends.clone(),
                    previous_limit: earlier.limit,
                    current_limit: row.limit,
                    previous_total: previous_plan.map_or(zero, |plan| plan.total),
                    current_total: current_plan.map_or(zero, |plan| plan.total),
                    current_buses: current_plan.map_or(0, |plan| plan.quantity(BUS)),
                    current_exits: current_plan.map_or(0, |plan| plan.quantity(METRO)),
                    changes: previous_plan
                        .zip(current_plan)
                        .map(|(before, after)| change_set(before, after, previous_budget)),
                }
            })
            .collect();
        Self {
            code: current.code,
            name: current.name,
            previous_total: previous.total,
            current_total: current.total,
            periods,
        }
    }
}

impl Calendar {
    pub(super) fn planning_context(&self) -> Vec<serde_json::Value> {
        self.segments
            .iter()
            .filter(|segment| segment.window.needs_stock())
            .map(|segment| {
                let mut period = segment.window.planning_context();
                period["inventory_observed"] = serde_json::json!(segment.snapshot.is_some());
                if let Some(snapshot) = &segment.snapshot {
                    period["source"] = serde_json::json!(if snapshot.data.demo {
                        "synthetic_sample"
                    } else if snapshot.source == super::source_label("platform") {
                        "verified_platform"
                    } else {
                        "local_snapshot"
                    });
                    period["bus_in_stock"] = serde_json::json!(
                        snapshot
                            .data
                            .stock
                            .iter()
                            .any(|row| row.medium == BUS && row.free > 0)
                    );
                    period["metro_in_stock"] = serde_json::json!(
                        snapshot
                            .data
                            .stock
                            .iter()
                            .any(|row| row.medium == METRO && row.free > 0)
                    );
                    period["has_tariff"] = serde_json::json!(!snapshot.data.rates.is_empty());
                }
                period
            })
            .collect()
    }
    pub(super) fn platform_inputs(&self) -> bool {
        self.segments
            .iter()
            .filter(|segment| segment.window.needs_stock())
            .all(|segment| {
                segment
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.source == super::source_label("platform"))
            })
    }

    pub(super) fn purchase_snapshots(&self) -> Option<Vec<&Snapshot>> {
        self.segments
            .iter()
            .filter(|segment| segment.window.needs_stock())
            .map(|segment| segment.snapshot.as_ref())
            .collect()
    }

    pub(super) async fn load(
        store: &crate::store::Store,
        options: &Options,
        rolling: &Rolling,
    ) -> Result<Self, WebError> {
        let starts = options.starts.as_deref().unwrap_or_default();
        let ends = options.ends.as_deref().unwrap_or_default();
        let windows = rolling
            .windows(starts, ends)
            .map_err(|message| (StatusCode::UNPROCESSABLE_ENTITY, message))?;
        let mut segments = Vec::with_capacity(windows.len());
        for window in windows {
            let mut query = options.clone();
            query.starts = Some(window.starts.clone());
            query.ends = Some(window.ends.clone());
            query.batch = rolling.batches.get(&window.starts).copied().or_else(|| {
                (window.starts == starts && window.ends == ends)
                    .then_some(options.batch)
                    .flatten()
            });
            let snapshot = if window.needs_stock() && options.demo == Some(1) {
                let mut snapshot = super::synthetic(options.industry);
                snapshot.starts.clone_from(&window.starts);
                snapshot.ends.clone_from(&window.ends);
                let month = super::date_month(&window.starts).expect("validated schedule month");
                for observation in &mut snapshot.data.evidence.reach {
                    observation.month = month;
                }
                Some(snapshot)
            } else if !window.needs_stock()
                || ((options.source.platform() || options.lock_snapshot) && query.batch.is_none())
            {
                None
            } else {
                super::read_snapshot_matching(store.clone(), &query, true).await?
            };
            segments.push(Segment { window, snapshot });
        }
        Ok(Self { segments })
    }

    pub(super) fn record(&self, rolling: &mut Rolling) -> Vec<(String, bool)> {
        self.segments
            .iter()
            .map(|segment| {
                let window = &segment.window;
                let period = format!("{} 至 {}", window.starts, window.ends);
                if window.factor.weight == 0 {
                    return (format!("{period} · {}。", window.factor.reason), true);
                }
                let Some(snapshot) = &segment.snapshot else {
                    return (format!("{period} · 缺少精确匹配档期的库存快照。"), false);
                };
                if let Some(id) = snapshot.id.as_deref().and_then(|id| id.parse().ok()) {
                    rolling.batches.insert(window.starts.clone(), id);
                }
                let mut note = format!(
                    "{period} · {} · 库存核对 {}",
                    snapshot.source, snapshot.observed
                );
                for medium in snapshot.data.empty_inventory.keys() {
                    note.push_str(&format!(" · {}库存 API 返回 0 条", medium.label()));
                }
                (note, true)
            })
            .collect()
    }

    fn weights(&self) -> impl Iterator<Item = u64> + Clone + '_ {
        self.segments
            .iter()
            .map(|segment| u64::from(segment.window.days * segment.window.factor.weight))
    }

    fn targets(&self, amount: u64) -> impl Iterator<Item = u64> + '_ {
        let weights = self.weights();
        let total = weights.clone().sum::<u64>().max(1);
        let mut accumulated = 0;
        weights.map(move |weight| {
            let before = amount * accumulated / total;
            accumulated += weight;
            amount * accumulated / total - before
        })
    }

    fn distribute(&self, amount: u64, bounds: &[Option<(u64, u64)>], soft: bool) -> Vec<u64> {
        let nominal = self.targets(amount).collect::<Vec<_>>();
        let bounds: Vec<_> = bounds
            .iter()
            .zip(&nominal)
            .map(|(bound, reserve)| bound.unwrap_or((*reserve, *reserve)))
            .collect();
        let total_minimum = bounds.iter().map(|(minimum, _)| minimum).sum::<u64>();
        if total_minimum > amount {
            return if soft {
                bounds.iter().map(|(minimum, _)| *minimum).collect()
            } else {
                nominal
            };
        }
        let mut result = nominal
            .iter()
            .zip(&bounds)
            .map(|(&target, &(minimum, ceiling))| target.clamp(minimum, ceiling))
            .collect::<Vec<_>>();
        let allocated = result.iter().sum::<u64>();
        if allocated > amount {
            let mut excess = allocated - amount;
            let mut slack = allocated - total_minimum;
            for (value, (minimum, _)) in result.iter_mut().zip(&bounds) {
                let adjustable = *value - minimum;
                if excess == 0 || adjustable == 0 {
                    continue;
                }
                let reduction =
                    (u128::from(excess) * u128::from(adjustable) / u128::from(slack)) as u64;
                *value -= reduction;
                excess -= reduction;
                slack -= adjustable;
            }
            return result;
        }
        let mut remaining = amount - allocated;
        while remaining > 0 {
            let eligible_weight: u64 = self
                .weights()
                .zip(&bounds)
                .zip(&result)
                .filter_map(|((weight, (_, ceiling)), current)| {
                    (current < ceiling).then_some(weight)
                })
                .sum();
            if eligible_weight == 0 {
                break;
            }
            let before = remaining;
            for ((weight, (_, ceiling)), current) in self.weights().zip(&bounds).zip(&mut result) {
                if weight == 0 || *current >= *ceiling || remaining == 0 {
                    continue;
                }
                let increment = (before * weight / eligible_weight)
                    .max(1)
                    .min(*ceiling - *current)
                    .min(remaining);
                *current += increment;
                remaining -= increment;
            }
        }
        result
    }

    pub(super) fn optimize(&self, options: &Options, budget: u32, policy: Policy) -> Portfolio {
        let mut portfolio = Portfolio::default();
        let media = options.media.unwrap_or(Selection::Both);
        let bounds: Vec<_> = self
            .segments
            .iter()
            .map(|segment| {
                segment.snapshot.as_ref().map(|snapshot| {
                    Request {
                        data: &snapshot.data,
                        target: 0,
                        target_band: None,
                        hard_cap: None,
                        media,
                        priority: policy.emphasis,
                        emphasis: policy.emphasis,
                        confirmed_focus: policy.confirmed_focus,
                        months: 1,
                        routes: &options.required_routes,
                    }
                    .spending_bounds()
                })
            })
            .collect();
        let targets = self.distribute(u64::from(budget) * 10_000, &bounds, true);
        let caps = policy
            .hard_cap
            .map(|cap| self.distribute(cap, &bounds, false));
        for (index, (segment, target)) in self.segments.iter().zip(targets).enumerate() {
            if let Some(snapshot) = &segment.snapshot {
                for strategy in Strategy::ALL {
                    portfolio.record(
                        strategy,
                        snapshot,
                        Request {
                            data: &snapshot.data,
                            target,
                            target_band: super::allocation_budget_band(budget, target),
                            hard_cap: caps.as_ref().map(|quotas| quotas[index]),
                            media,
                            priority: strategy.priority(),
                            emphasis: policy.emphasis,
                            confirmed_focus: policy.confirmed_focus,
                            months: 1,
                            routes: &options.required_routes,
                        },
                    );
                }
            }
        }
        portfolio
    }
    pub(super) fn plan(
        &self,
        options: &Options,
        budget: u32,
        portfolio: &Portfolio,
        strategy: Strategy,
    ) -> Plan<'_> {
        let placements = self
            .segments
            .iter()
            .zip(self.targets(u64::from(budget) * 10_000))
            .map(|(segment, target)| {
                let allocated = segment.snapshot.as_ref().and_then(|snapshot| {
                    portfolio.proposal(
                        strategy,
                        snapshot,
                        options.media.unwrap_or(Selection::Both),
                        1,
                        &options.required_routes,
                    )
                });
                let (limit, proposal) =
                    allocated.map_or((target, None), |(limit, proposal)| (limit, Some(proposal)));
                Placement {
                    segment,
                    planned: target,
                    limit,
                    proposal,
                }
            })
            .collect::<Vec<_>>();
        let total = placements
            .iter()
            .filter_map(|row| row.proposal.as_ref())
            .fold(Cost { low: 0, high: 0 }, |sum, plan| Cost {
                low: sum.low + plan.total.low,
                high: sum.high + plan.total.high,
            });
        Plan { placements, total }
    }
    fn build_variant(
        plan: &Plan<'_>,
        budget: u32,
        options: &Options,
        strategy: Strategy,
    ) -> Variant {
        let periods = plan
            .placements
            .iter()
            .filter(|placement| placement.segment.window.needs_stock())
            .map(|placement| {
                let window = &placement.segment.window;
                let mut period = Period::new(
                    &window.starts,
                    &window.ends,
                    window.factor.reason.clone(),
                    placement.limit,
                    placement
                        .segment
                        .snapshot
                        .as_ref()
                        .zip(placement.proposal.as_ref()),
                );
                period.operations_required = options.requires_operations();
                if let Some((rules, proposal)) = options
                    .operations
                    .as_ref()
                    .zip(placement.proposal.as_ref())
                    .filter(|(_, proposal)| !proposal.items.is_empty())
                {
                    period.operations = rules.phase(
                        &window.starts,
                        &window.ends,
                        proposal.quantity(BUS),
                        proposal.quantity(METRO),
                    );
                }
                period
            })
            .collect();
        Variant::new(strategy, budget, options.hard_cap, periods)
    }

    pub(super) fn variants(&self, options: &Options, portfolio: &Portfolio) -> Vec<Variant> {
        let budget = options.budget.unwrap_or(super::SHOWCASE_BUDGET);
        let mut variants = Strategy::ALL
            .into_iter()
            .map(|strategy| {
                let plan = self.plan(options, budget, portfolio, strategy);
                Self::build_variant(&plan, budget, options, strategy)
            })
            .collect::<Vec<_>>();
        Variant::mark_duplicates(&mut variants);
        variants
    }

    pub(super) fn selected_inventory(
        &self,
        options: &Options,
        budget: u32,
        portfolio: &Portfolio,
        strategy: Strategy,
    ) -> Option<Vec<crate::refresh::InventoryCheck>> {
        self.plan(options, budget, portfolio, strategy)
            .placements
            .into_iter()
            .filter(|placement| placement.segment.window.needs_stock())
            .map(|placement| {
                let window = &placement.segment.window;
                super::selected_inventory(
                    &window.starts,
                    &window.ends,
                    placement.proposal.as_ref()?,
                )
            })
            .collect()
    }

    pub(super) fn render(
        &self,
        options: &Options,
        variants: Vec<Variant>,
        portfolio: &Portfolio,
        prior: Option<&Portfolio>,
        brand: &Brand,
        guidance: &[(&str, &str)],
    ) -> Result<String, WebError> {
        let budget = options.budget.unwrap_or(super::SHOWCASE_BUDGET);
        let previous_budget = options
            .compare
            .filter(|amount| *amount != 0 && *amount != budget && prior.is_some());
        let mut comparisons = Vec::new();
        if let Some((previous_budget, prior)) = previous_budget.zip(prior) {
            for (strategy, variant) in Strategy::ALL.into_iter().zip(&variants) {
                let current = self.plan(options, budget, portfolio, strategy);
                let previous = self.plan(options, previous_budget, prior, strategy);
                comparisons.push(BudgetComparison::new(
                    variant,
                    &current,
                    &previous,
                    previous_budget,
                ));
            }
        }
        let plan = self.plan(options, budget, portfolio, Strategy::CommerceCoverage);
        CalendarReport {
            options,
            campaign: super::brief::CampaignView::new(options),
            brand,
            budget,
            plan: &plan,
            previous_budget,
            comparisons,
            css: web::CSS,
            guidance,
            variants,
        }
        .render()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "无法渲染滚动排期报告。"))
    }
}

#[derive(Template)]
#[template(path = "media-schedule-export.html")]
struct CalendarReport<'a> {
    options: &'a Options,
    campaign: super::brief::CampaignView<'a>,
    brand: &'a Brand,
    budget: u32,
    plan: &'a Plan<'a>,
    previous_budget: Option<u32>,
    comparisons: Vec<BudgetComparison>,
    css: &'static str,
    guidance: &'a [(&'a str, &'a str)],
    variants: Vec<Variant>,
}

#[cfg(test)]
mod tests {
    use super::{Calendar, Phase, PhaseChoice, Rolling, Segment};

    #[test]
    fn date_picker_range_keeps_partial_month_boundaries() {
        let starts = "2027-06-20";
        let ends = "2027-07-08";
        let rolling = Rolling::showcase(
            "2027-06-01 | 1 | 门店试饮预热\n2027-07-01 | 3 | 主投",
            starts,
            ends,
        )
        .unwrap();
        let periods = rolling.windows(starts, ends).unwrap();
        let ranges: Vec<_> = periods
            .iter()
            .map(|period| {
                (
                    period.starts.as_str(),
                    period.ends.as_str(),
                    period.days,
                    period.factor.weight,
                )
            })
            .collect();
        assert_eq!(
            ranges,
            [
                ("2027-06-20", "2027-06-30", 11, 1),
                ("2027-07-01", "2027-07-08", 8, 3),
            ]
        );
    }

    #[test]
    fn explicit_windows_are_preserved_and_automatic_choices_must_stay_in_bounds() {
        let (starts, ends) = ("2027-01-01", "2027-12-31");
        let mut fixed = Rolling::showcase("2027-09-01 | 3 | 客户确认活动", starts, ends).unwrap();
        let choice = |month: &str, phase| PhaseChoice {
            month: month.into(),
            phase,
        };
        let paid = |rolling: &Rolling| {
            rolling
                .windows(starts, ends)
                .unwrap()
                .into_iter()
                .filter(|window| window.needs_stock())
                .map(|window| window.starts)
                .collect::<Vec<_>>()
        };
        assert!(fixed.candidate_months(starts, ends).is_empty());
        assert!(!fixed.adopt_phases(&[], &[choice("2027-08", Phase::Main)]));
        assert_eq!(paid(&fixed), ["2027-09-01"]);
        let mut auto = Rolling::showcase("", starts, ends).unwrap();
        let months = auto.candidate_months(starts, ends);
        assert!(!auto.adopt_phases(&months, &[choice("2028-08", Phase::Main)]));
        assert!(!auto.adopt_phases(
            &months,
            &[
                choice("2027-08", Phase::Main),
                choice("2027-08", Phase::Followup)
            ]
        ));
        assert!(auto.adopt_phases(&months, &[choice("2027-08", Phase::Main)]));
        assert_eq!(paid(&auto), ["2027-08-01"]);
    }

    #[test]
    fn short_range_outside_example_phases_still_has_a_paid_stage() {
        let starts = "2027-02-04";
        let ends = "2027-02-13";
        let rolling = Rolling::showcase("", starts, ends).unwrap();
        let periods = rolling.windows(starts, ends).unwrap();
        assert_eq!(periods.len(), 1);
        assert!(periods[0].needs_stock());
        assert!(Rolling::showcase("2027-05-01 | 1 | 试饮", starts, ends).is_err());
    }

    #[test]
    fn funding_is_moved_to_required_stages_and_missing_stage_keeps_its_share() {
        let rolling = Rolling::showcase("", "2027-01-01", "2027-12-31").unwrap();
        let calendar = Calendar {
            segments: rolling
                .windows("2027-01-01", "2027-12-31")
                .unwrap()
                .into_iter()
                .filter(|window| window.needs_stock())
                .map(|window| Segment {
                    window,
                    snapshot: None,
                })
                .collect(),
        };
        let bounds = [Some((35, 120)), Some((5, 120)), Some((35, 120))];
        let allocated = calendar.distribute(100, &bounds, true);
        assert_eq!(allocated.iter().sum::<u64>(), 100);
        assert!(allocated[0] >= 35 && allocated[2] >= 35);
        assert_eq!(calendar.distribute(50, &bounds, true), vec![35, 5, 35]);
        assert_eq!(
            calendar.distribute(50, &bounds, false).iter().sum::<u64>(),
            50
        );
        let nominal = calendar.targets(100).collect::<Vec<_>>();
        let partial = calendar.distribute(100, &[bounds[0], None, bounds[2]], true);
        assert_eq!(partial[1], nominal[1]);
    }

    #[test]
    fn explicit_phase_weights_keep_original_budget_when_feasible() {
        let (starts, ends) = ("2027-01-01", "2027-12-31");
        let rolling = Rolling::showcase(
            "2027-05-01 | 1 | 预热\n2027-07-01 | 3 | 主投\n2027-10-01 | 1 | 回访",
            starts,
            ends,
        )
        .unwrap();
        let segments = rolling
            .windows(starts, ends)
            .unwrap()
            .into_iter()
            .filter(|window| window.needs_stock())
            .map(|window| Segment {
                window,
                snapshot: None,
            })
            .collect();
        let calendar = Calendar { segments };
        let bounds = [
            Some((320_000, 700_000)),
            Some((310_000, 1_500_000)),
            Some((330_000, 650_000)),
        ];
        assert_eq!(
            calendar.distribute(2_000_000, &bounds, true),
            [400_000, 1_200_000, 400_000]
        );
    }
}
