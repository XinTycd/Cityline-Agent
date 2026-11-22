use std::collections::BTreeSet;

use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::web::WebError;

#[derive(Serialize)]
pub(crate) struct InventoryCheck {
    pub(crate) starts: String,
    pub(crate) ends: String,
    pub(crate) demands: Vec<StockDemand>,
}

#[derive(Serialize)]
pub(crate) struct StockDemand {
    pub(crate) medium: &'static str,
    pub(crate) name: String,
    pub(crate) code: String,
    pub(crate) operator: String,
    pub(crate) grade: String,
    pub(crate) format: String,
    pub(crate) quantity: u32,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Insufficient,
    MissingOrChanged,
    Ambiguous,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Finding {
    period: usize,
    demands: Vec<usize>,
    kind: Kind,
    available: Option<u32>,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct InventoryIssue {
    pub(crate) starts: String,
    pub(crate) ends: String,
    medium: String,
    pub(crate) name: String,
    pub(crate) code: String,
    pub(crate) specification: String,
    pub(crate) required: u64,
    pub(crate) available: Option<u32>,
    kind: Kind,
}

impl InventoryIssue {
    pub(crate) fn unit(&self) -> &'static str {
        if self.medium == "bus" { "辆" } else { "处" }
    }

    pub(crate) fn reason(&self) -> &'static str {
        match self.kind {
            Kind::Insufficient => "可售余量不足",
            Kind::MissingOrChanged => "资源缺失或标识变更",
            Kind::Ambiguous => "红豆返回多个匹配资源",
        }
    }
}

pub(crate) struct InventoryFailure {
    pub(crate) error: WebError,
    pub(crate) issues: Vec<InventoryIssue>,
}

impl From<WebError> for InventoryFailure {
    fn from(error: WebError) -> Self {
        Self {
            error,
            issues: Vec::new(),
        }
    }
}

impl InventoryFailure {
    pub(super) fn from_findings(output: &[u8], checks: &[InventoryCheck]) -> Option<Self> {
        let findings = serde_json::from_slice::<Vec<Finding>>(output).ok()?;
        if findings.is_empty() {
            return None;
        }
        let mut seen = BTreeSet::new();
        let mut issues = Vec::new();
        for finding in findings {
            let period = checks.get(finding.period)?;
            let first = period.demands.get(*finding.demands.first()?)?;
            let mut required = 0;
            for &index in &finding.demands {
                let demand = period.demands.get(index)?;
                if demand.medium != first.medium || !seen.insert((finding.period, index)) {
                    return None;
                }
                required += u64::from(demand.quantity);
            }
            match (finding.kind, finding.available) {
                (Kind::Insufficient, Some(available)) if u64::from(available) < required => {}
                (Kind::MissingOrChanged | Kind::Ambiguous, None) if finding.demands.len() == 1 => {}
                _ => return None,
            }
            issues.push(InventoryIssue {
                starts: period.starts.clone(),
                ends: period.ends.clone(),
                medium: first.medium.to_owned(),
                name: first.name.clone(),
                code: first.code.clone(),
                specification: [&first.operator, &first.grade, &first.format]
                    .into_iter()
                    .filter(|value| !value.is_empty())
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(" · "),
                required,
                available: finding.available,
                kind: finding.kind,
            });
        }
        Some(Self {
            error: (
                StatusCode::CONFLICT,
                "所选方案存在余量不足或标识变更的资源。请重新获取数据，生成并审核新版本。",
            ),
            issues,
        })
    }
}
