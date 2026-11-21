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

#[derive(Clone, Default, Deserialize, Serialize)]
pub(crate) struct Rolling {
    pub(super) factors: BTreeMap<String, Factor>,
    #[serde(default)]
    pub(super) batches: BTreeMap<String, Uuid>,
    #[serde(default)]
    staged: bool,
}

#[derive(Clone, Deserialize, Serialize)]
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

#[derive(Clone)]
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
}

impl Rolling {
    pub(crate) fn showcase(input: &str, starts: &str, ends: &str) -> Result<Self, &'static str> {
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
                            reason: reason.into(),
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
        for (month, weight, description) in [
            (5, 1, "预热 · 介绍新品和门店试饮信息"),
            (7, 3, "主投 · 暑期新品试饮到店"),
            (10, 1, "回访 · 复购提醒与试饮活动复盘"),
        ] {
            let date = format!("2027-{month:02}-01");
            if first_month <= &date[..7] && &date[..7] <= last_month {
                rolling.factors.insert(
                    date,
                    Factor {
                        weight,
                        reason: description.into(),
                    },
                );
            }
        }
        if rolling.factors.is_empty() {
            for month in
                super::date_month(starts).unwrap_or(1)..=super::date_month(ends).unwrap_or(12)
            {
                rolling
                    .factors
                    .insert(format!("2027-{month:02}-01"), Factor::default());
            }
        }
        rolling.factors.extend(overrides);
        if rolling.factors.values().all(|factor| factor.weight == 0) {
            return Err("请至少安排一个有投放预算的月份。");
        }
        Ok(rolling)
    }

    pub(crate) fn windows(&self, starts: &str, ends: &str) -> Result<Vec<Window>, &'static str> {
        period_months(starts, ends).ok_or("滚动排期需要有效的 2027 年起止日期。")?;
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
    limit: u64,
    proposal: Option<Proposal>,
}

impl Placement<'_> {
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
    pub(super) complete: bool,
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
            let snapshot = if !window.needs_stock()
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
        let mut result: Vec<_> = bounds.iter().map(|(minimum, _)| *minimum).collect();
        let reserved: u64 = result.iter().sum();
        if reserved > amount {
            return if soft { result } else { nominal };
        }
        let mut remaining = amount - reserved;
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
                        hard_cap: None,
                        media,
                        priority: policy.emphasis,
                        emphasis: policy.emphasis,
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
                            hard_cap: caps.as_ref().map(|quotas| quotas[index]),
                            media,
                            priority: strategy.priority(),
                            emphasis: policy.emphasis,
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
        let complete = placements.iter().all(Placement::ready);
        Plan {
            placements,
            total,
            complete,
        }
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
                    format!(
                        "{} · 调配权重 {}",
                        window.factor.reason, window.factor.weight
                    ),
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

    pub(super) fn render(
        &self,
        options: &Options,
        variants: Vec<Variant>,
        portfolio: &Portfolio,
        prior: Option<&Portfolio>,
        brand: &Brand,
        guidance: &[(&str, &str)],
    ) -> Result<(String, bool), WebError> {
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
        let full_evidence = variants.iter().all(Variant::has_full_evidence);
        let operations_incomplete =
            options.requires_operations() && !variants.iter().all(Variant::operations_ready);
        let plan = self.plan(options, budget, portfolio, Strategy::CommerceCoverage);
        CalendarReport {
            options,
            campaign: super::brief::CampaignView::new(options),
            brand,
            budget,
            plan: &plan,
            previous_budget,
            comparisons,
            full_evidence,
            operations_incomplete,
            css: web::CSS,
            guidance,
            variants,
        }
        .render()
        .map(|html| (html, full_evidence))
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
    full_evidence: bool,
    operations_incomplete: bool,
    css: &'static str,
    guidance: &'a [(&'a str, &'a str)],
    variants: Vec<Variant>,
}

#[cfg(test)]
mod tests {
    use super::{Calendar, Rolling, Segment};

    #[test]
    fn date_picker_range_keeps_partial_month_boundaries() {
        let starts = "2027-06-20";
        let ends = "2027-07-08";
        let rolling = Rolling::showcase("2027-06-01 | 1 | 门店试饮预热", starts, ends).unwrap();
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
}
