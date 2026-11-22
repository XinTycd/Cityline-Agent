use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

use super::{BUS, METRO, plan::Priority, visuals::Variant};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Gate {
    CustomerGoal,
    ComparisonMetric,
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
            Self::CustomerGoal => (
                "客户主目标",
                "请在任务详情选择到店、认知、促销或拉新，并生成复核版本。",
            ),
            Self::ComparisonMetric => (
                "客户首要比选指标",
                "补齐两案在首要指标上的可核验证据，然后重新核算推荐结论。",
            ),
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
            status(variants.iter().all(Variant::bus_map_ready))
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
    CoreBusDays,
    EstimatedExposure,
    OnlyFeasible,
    PendingEvidence,
    EqualTradeoff,
    PendingGoal,
    PendingPrimaryMetric,
    BudgetFit,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ComparisonFocus {
    Coverage,
    CoreBusDays,
    EstimatedExposure,
}

impl ComparisonFocus {
    pub(super) fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "coverage" | "目标圈覆盖" => Some(Self::Coverage),
            "core_bus_days" | "核心圈公交车日" => Some(Self::CoreBusDays),
            "estimated_exposure" | "可核验预估人次" => Some(Self::EstimatedExposure),
            _ => None,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Coverage => "目标圈覆盖",
            Self::CoreBusDays => "核心圈公交车日",
            Self::EstimatedExposure => "可核验预估人次",
        }
    }

    pub(super) fn planning_priority(self) -> Priority {
        match self {
            Self::CoreBusDays => Priority::Nearby,
            Self::Coverage | Self::EstimatedExposure => Priority::KeyCircles,
        }
    }

    pub(super) fn criteria(self) -> [Reason; 4] {
        use Reason::{BudgetFit, CoreBusDays, EstimatedExposure, WiderCoverage};
        match self {
            Self::Coverage => [WiderCoverage, EstimatedExposure, CoreBusDays, BudgetFit],
            Self::CoreBusDays => [CoreBusDays, WiderCoverage, EstimatedExposure, BudgetFit],
            Self::EstimatedExposure => [EstimatedExposure, WiderCoverage, CoreBusDays, BudgetFit],
        }
    }
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
    core_bus_days: Relative,
    estimated_exposure: Relative,
    budget_fit: Relative,
}

impl CandidateSummary {
    pub(super) fn eligible(&self) -> bool {
        self.placement_feasible && self.evidence_complete
    }

    fn compare_by(&self, reason: Reason) -> Relative {
        match reason {
            Reason::CoreBusDays => self.core_bus_days,
            Reason::EstimatedExposure => self.estimated_exposure,
            Reason::WiderCoverage => self.coverage,
            Reason::BudgetFit => self.budget_fit,
            _ => unreachable!("only ordered comparison criteria are used"),
        }
    }
}

pub(super) fn missing_primary_metric(
    summaries: &[CandidateSummary; 2],
    focus: ComparisonFocus,
) -> bool {
    summaries.iter().all(CandidateSummary::eligible)
        && summaries
            .iter()
            .any(|option| option.compare_by(focus.criteria()[0]) == Relative::Unknown)
}

fn estimated_exposure(variant: &Variant) -> Option<u64> {
    let rows = || variant.periods.iter().flat_map(|period| &period.rows);
    (variant.available && rows().all(|row| row.reach_complete()) && rows().next().is_some())
        .then(|| rows().map(|row| row.exposures).sum())
}

fn compare(current: Option<u64>, other: Option<u64>) -> Relative {
    current
        .zip(other)
        .map_or(Relative::Unknown, |(current, other)| {
            current.cmp(&other).into()
        })
}

pub(super) fn summarize(variants: &[Variant], operations_required: bool) -> [CandidateSummary; 2] {
    let [a, b] = variants else {
        unreachable!("the allocator returns exactly A and B")
    };
    let make = |candidate, current: &Variant, other: &Variant| CandidateSummary {
        candidate,
        placement_feasible: current.available
            && (!operations_required || current.operations_ready()),
        evidence_complete: current.has_full_evidence(),
        coverage: current.circles.cmp(&other.circles).into(),
        core_bus_days: compare(current.core_bus_days(), other.core_bus_days()),
        estimated_exposure: compare(estimated_exposure(current), estimated_exposure(other)),
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
                Reason::CoreBusDays => "建议优先复核 A 案。核心圈 500 米内已核实的公交车日更多。",
                Reason::EstimatedExposure => {
                    "建议优先复核 A 案。按有出处的单位触达和投放天数估算，其累计曝光人次更高。"
                }
                Reason::BudgetFit => "建议优先复核 A 案。其高位报价更接近预算目标。",
                _ => "建议优先复核 A 案。其目标圈覆盖更广。",
            },
            Candidate::B => match self.reason {
                Reason::OnlyFeasible => {
                    "建议优先复核 B 案。当前仅 B 案满足所选档期、媒体、报价和证据条件。"
                }
                Reason::CoreBusDays => "建议优先复核 B 案。核心圈 500 米内已核实的公交车日更多。",
                Reason::EstimatedExposure => {
                    "建议优先复核 B 案。按有出处的单位触达和投放天数估算，其累计曝光人次更高。"
                }
                Reason::BudgetFit => "建议优先复核 B 案。其高位报价更接近预算目标。",
                _ => "建议优先复核 B 案。其目标圈覆盖更广。",
            },
            Candidate::None if self.reason == Reason::PendingEvidence => {
                "暂不推荐可执行方案。请先补齐两案标注的档期、采购或效果证据。"
            }
            Candidate::None if self.reason == Reason::PendingGoal => {
                "请先确认客户主目标。待核草案保留两案供核对，确认目标后可重新生成。"
            }
            Candidate::None if self.reason == Reason::PendingPrimaryMetric => {
                "客户指定的首要指标尚无法比较。请补齐该指标的证据，再复核推荐。"
            }
            Candidate::None => "两案已满足基础条件，可根据目标和阶段资源取舍，请操作员决定。",
        }
    }
}

pub(super) fn recommend(
    summaries: &[CandidateSummary; 2],
    proposed: Option<Recommendation>,
    focus: ComparisonFocus,
) -> (Recommendation, bool) {
    let feasible: Vec<_> = summaries
        .iter()
        .filter(|summary| summary.eligible())
        .collect();
    let preferred = match feasible.as_slice() {
        [] => Recommendation {
            candidate: Candidate::None,
            reason: Reason::PendingEvidence,
        },
        [only] => Recommendation {
            candidate: only.candidate,
            reason: Reason::OnlyFeasible,
        },
        _ => focus
            .criteria()
            .into_iter()
            .find_map(|reason| {
                feasible.iter().find_map(|option| {
                    let result = option.compare_by(reason);
                    (result == Relative::Higher).then_some(Recommendation {
                        candidate: option.candidate,
                        reason,
                    })
                })
            })
            .unwrap_or(Recommendation {
                candidate: Candidate::None,
                reason: Reason::EqualTradeoff,
            }),
    };
    (preferred, proposed == Some(preferred))
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
            core_bus_days: Relative::Unknown,
            estimated_exposure: Relative::Unknown,
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
            recommend(&summaries, Some(proposed), ComparisonFocus::Coverage),
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

    #[test]
    fn verified_recommendation_follows_objective_before_model_preference() {
        let summaries = [
            CandidateSummary {
                candidate: Candidate::A,
                placement_feasible: true,
                evidence_complete: true,
                coverage: Relative::Higher,
                core_bus_days: Relative::Lower,
                estimated_exposure: Relative::Higher,
                budget_fit: Relative::Higher,
            },
            CandidateSummary {
                candidate: Candidate::B,
                placement_feasible: true,
                evidence_complete: true,
                coverage: Relative::Lower,
                core_bus_days: Relative::Higher,
                estimated_exposure: Relative::Lower,
                budget_fit: Relative::Lower,
            },
        ];
        let model = Recommendation {
            candidate: Candidate::A,
            reason: Reason::WiderCoverage,
        };
        assert_eq!(
            recommend(&summaries, Some(model), ComparisonFocus::CoreBusDays),
            (
                Recommendation {
                    candidate: Candidate::B,
                    reason: Reason::CoreBusDays
                },
                false
            )
        );
        assert_eq!(
            recommend(&summaries, Some(model), ComparisonFocus::Coverage),
            (model, true)
        );
        assert_eq!(
            recommend(&summaries, Some(model), ComparisonFocus::EstimatedExposure),
            (
                Recommendation {
                    candidate: Candidate::A,
                    reason: Reason::EstimatedExposure,
                },
                false,
            )
        );
    }
}
