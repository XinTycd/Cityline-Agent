use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

use super::{BUS, METRO, visuals::Variant};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Gate {
    Inventory,
    RequiredRoutes,
    Quote,
    #[serde(alias = "map")]
    BusMap,
    MetroExit,
    Reach,
    Operations,
}

impl Gate {
    const ALL: [Self; 7] = [
        Self::Inventory,
        Self::RequiredRoutes,
        Self::Quote,
        Self::BusMap,
        Self::MetroExit,
        Self::Reach,
        Self::Operations,
    ];

    pub(super) fn guidance(self) -> (&'static str, &'static str) {
        match self {
            Self::Inventory => (
                "档期与库存",
                "补齐对应阶段的红豆快照，逐段复核所选媒体和可售余量。",
            ),
            Self::RequiredRoutes => (
                "必选线路",
                "按客户确认清单核对各段必选线路及对应报价，可售不足时确认替换。",
            ),
            Self::Quote => (
                "刊例与预算",
                "核对缺价、硬性上限及制作费依据，确认实际报价与目标金额的差额。",
            ),
            Self::BusMap => (
                "公交线路图",
                "按实际入选的公交组合补齐新云线路原图或缺失轨迹。",
            ),
            Self::MetroExit => ("地铁出口实点", "按红豆点位编号核对具体出口坐标与官方来源。"),
            Self::Reach => (
                "单位触达",
                "确认统计月份和每辆车或每个出口的单位日触达。历史站级人流单独展示。",
            ),
            Self::Operations => (
                "上下刊施工",
                "确认夜间窗口、单晚产能和雨停缓冲。按所选资源核对发布日前完工及档期结束后下刊。",
            ),
        }
    }

    pub(super) fn allows_booking(self) -> bool {
        matches!(
            self,
            Self::Inventory | Self::RequiredRoutes | Self::Quote | Self::Operations
        )
    }
}

#[derive(Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CheckStatus {
    Verified,
    Missing,
    NotApplicable,
}

#[derive(Serialize)]
pub(super) struct Checks {
    inventory: CheckStatus,
    required_routes: CheckStatus,
    quote: CheckStatus,
    bus_map: CheckStatus,
    metro_exit: CheckStatus,
    reach: CheckStatus,
    operations: CheckStatus,
}

impl Checks {
    pub(super) fn new(variants: &[Variant], required: bool, operations_required: bool) -> Self {
        let periods = || variants.iter().flat_map(|variant| &variant.periods);
        let inventory = status(periods().all(|period| period.observed.is_some()));
        let required_routes = if required {
            status(periods().all(|period| period.required_complete))
        } else {
            CheckStatus::NotApplicable
        };
        let quote = status(periods().all(|period| period.available));
        let bus_map = if periods().any(|period| period.rows.iter().any(|row| row.media == BUS)) {
            status(periods().all(|period| period.bus_map_ready()))
        } else {
            CheckStatus::NotApplicable
        };
        let metro_exit = if periods().any(|period| period.rows.iter().any(|row| row.media == METRO))
        {
            status(periods().all(|period| period.metro_map_ready()))
        } else {
            CheckStatus::NotApplicable
        };
        let reach = status(periods().all(|period| {
            !period.rows.is_empty() && period.rows.iter().all(super::visuals::Line::reach_complete)
        }));
        Self {
            inventory,
            required_routes,
            quote,
            bus_map,
            metro_exit,
            reach,
            operations: if operations_required {
                status(variants.iter().all(Variant::operations_ready))
            } else {
                CheckStatus::NotApplicable
            },
        }
    }

    pub(super) fn missing(&self) -> Vec<Gate> {
        let values = [
            self.inventory,
            self.required_routes,
            self.quote,
            self.bus_map,
            self.metro_exit,
            self.reach,
            self.operations,
        ];
        Gate::ALL
            .into_iter()
            .zip(values)
            .filter_map(|(gate, value)| (value == CheckStatus::Missing).then_some(gate))
            .collect()
    }
}

fn status(verified: bool) -> CheckStatus {
    if verified {
        CheckStatus::Verified
    } else {
        CheckStatus::Missing
    }
}

pub(super) fn review_queue(missing: &[Gate], suggested: &[Gate]) -> (Vec<Gate>, bool) {
    let valid = !suggested.is_empty()
        && suggested
            .iter()
            .enumerate()
            .all(|(index, gate)| missing.contains(gate) && !suggested[..index].contains(gate));
    if !valid {
        return (missing.to_vec(), false);
    }
    let mut order = suggested.to_vec();
    order.extend(
        missing
            .iter()
            .copied()
            .filter(|gate| !suggested.contains(gate)),
    );
    (order, true)
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) enum Candidate {
    A,
    B,
    #[serde(rename = "none")]
    None,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Reason {
    WiderCoverage,
    CoreFrequency,
    OnlyFeasible,
    PendingEvidence,
    EqualTradeoff,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Relative {
    Higher,
    Equal,
    Lower,
    Unknown,
}

impl From<Ordering> for Relative {
    fn from(order: Ordering) -> Self {
        match order {
            Ordering::Greater => Self::Higher,
            Ordering::Equal => Self::Equal,
            Ordering::Less => Self::Lower,
        }
    }
}

#[derive(Serialize)]
pub(super) struct CandidateSummary {
    candidate: Candidate,
    placement_feasible: bool,
    evidence_complete: bool,
    coverage: Relative,
    core_bus_frequency: Relative,
    budget_fit: Relative,
}

impl CandidateSummary {
    fn eligible(&self) -> bool {
        self.placement_feasible && self.evidence_complete
    }
}

pub(super) fn summarize(variants: &[Variant], operations_required: bool) -> [CandidateSummary; 2] {
    let [a, b] = variants else {
        unreachable!("the allocator returns exactly A and B")
    };
    let core_ratio = |variant: &Variant| {
        let mut units = 0_u64;
        let mut near = 0_u64;
        for period in &variant.periods {
            for row in period.rows.iter().filter(|row| row.media == BUS) {
                let distance = row
                    .coverage
                    .iter()
                    .filter(|edge| Some(edge.circle.as_str()) == period.primary_circle.as_deref())
                    .filter_map(|edge| edge.distance)
                    .min();
                units += u64::from(row.quantity);
                if distance.is_some_and(|metres| metres <= 500) {
                    near += u64::from(row.quantity);
                }
            }
        }
        (units > 0).then(|| near * 10000 / units)
    };
    let make = |candidate, current: &Variant, other: &Variant| CandidateSummary {
        candidate,
        placement_feasible: current.available
            && (!operations_required || current.operations_ready()),
        evidence_complete: current.has_full_evidence(),
        coverage: current.circles.cmp(&other.circles).into(),
        core_bus_frequency: core_ratio(current)
            .zip(core_ratio(other))
            .map_or(Relative::Unknown, |(current, other)| {
                current.cmp(&other).into()
            }),
        budget_fit: other
            .target_difference
            .cmp(&current.target_difference)
            .into(),
    };
    [make(Candidate::A, a, b), make(Candidate::B, b, a)]
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(super) struct Recommendation {
    pub(super) candidate: Candidate,
    pub(super) reason: Reason,
}

impl Recommendation {
    pub(super) fn guidance(self) -> &'static str {
        match self.candidate {
            Candidate::A => match self.reason {
                Reason::OnlyFeasible => {
                    "建议优先复核 A 案。当前仅 A 案满足所选档期、媒体、报价和证据条件。"
                }
                Reason::CoreFrequency => "建议优先复核 A 案。其门店近场公交投放占比更高。",
                _ => "建议优先复核 A 案。其目标圈覆盖更广。",
            },
            Candidate::B => match self.reason {
                Reason::OnlyFeasible => {
                    "建议优先复核 B 案。当前仅 B 案满足所选档期、媒体、报价和证据条件。"
                }
                Reason::CoreFrequency => "建议优先复核 B 案。其门店近场公交投放占比更高。",
                _ => "建议优先复核 B 案。其目标圈覆盖更广。",
            },
            Candidate::None if self.reason == Reason::PendingEvidence => {
                "暂不推荐可执行方案。请先补齐两案标注的档期、采购或效果证据。"
            }
            Candidate::None => "两案已满足基础条件，可根据目标和阶段资源取舍，请操作员决定。",
        }
    }
}

pub(super) fn recommend(
    summaries: &[CandidateSummary; 2],
    proposed: Option<Recommendation>,
    prioritize_nearby: bool,
) -> (Recommendation, bool) {
    let feasible: Vec<_> = summaries
        .iter()
        .filter(|summary| summary.eligible())
        .collect();
    let qualified = |item: Recommendation| match item.candidate {
        Candidate::None => match item.reason {
            Reason::PendingEvidence => feasible.is_empty(),
            Reason::EqualTradeoff => {
                feasible.len() == 2
                    && feasible.iter().all(|option| {
                        option.coverage == Relative::Equal
                            && matches!(
                                option.core_bus_frequency,
                                Relative::Equal | Relative::Unknown
                            )
                    })
            }
            _ => false,
        },
        candidate => feasible.iter().any(|option| {
            option.candidate == candidate
                && match item.reason {
                    Reason::OnlyFeasible => feasible.len() == 1,
                    Reason::WiderCoverage => option.coverage == Relative::Higher,
                    Reason::CoreFrequency => option.core_bus_frequency == Relative::Higher,
                    _ => false,
                }
        }),
    };
    if let Some(item) = proposed.filter(|item| qualified(*item)) {
        return (item, true);
    }
    let preferred = match feasible.as_slice() {
        [] => Recommendation {
            candidate: Candidate::None,
            reason: Reason::PendingEvidence,
        },
        [only] => Recommendation {
            candidate: only.candidate,
            reason: Reason::OnlyFeasible,
        },
        _ => {
            let criteria = if prioritize_nearby {
                [Reason::CoreFrequency, Reason::WiderCoverage]
            } else {
                [Reason::WiderCoverage, Reason::CoreFrequency]
            };
            criteria
                .into_iter()
                .find_map(|reason| {
                    feasible.iter().find_map(|option| {
                        let item = Recommendation {
                            candidate: option.candidate,
                            reason,
                        };
                        qualified(item).then_some(item)
                    })
                })
                .unwrap_or(Recommendation {
                    candidate: Candidate::None,
                    reason: Reason::EqualTradeoff,
                })
        }
    };
    (preferred, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_cannot_recommend_candidate_with_unverified_evidence() {
        let candidate = |candidate, evidence_complete, coverage| CandidateSummary {
            candidate,
            placement_feasible: true,
            evidence_complete,
            coverage,
            core_bus_frequency: Relative::Unknown,
            budget_fit: Relative::Equal,
        };
        let summaries = [
            candidate(Candidate::A, false, Relative::Higher),
            candidate(Candidate::B, true, Relative::Lower),
        ];
        let proposed = Recommendation {
            candidate: Candidate::A,
            reason: Reason::WiderCoverage,
        };
        assert_eq!(
            recommend(&summaries, Some(proposed), false),
            (
                Recommendation {
                    candidate: Candidate::B,
                    reason: Reason::OnlyFeasible
                },
                false,
            )
        );
    }

    #[test]
    fn model_can_sort_existing_gaps_but_cannot_remove_them() {
        let missing = [Gate::Inventory, Gate::Reach];
        assert_eq!(
            review_queue(&missing, &[Gate::Reach]),
            (vec![Gate::Reach, Gate::Inventory], true)
        );
        assert_eq!(
            review_queue(&missing, &[Gate::Reach, Gate::Reach]),
            (missing.to_vec(), false)
        );
        assert_eq!(
            review_queue(&missing, &[Gate::BusMap]),
            (missing.to_vec(), false)
        );
    }
}
