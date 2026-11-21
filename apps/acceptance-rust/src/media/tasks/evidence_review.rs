use std::collections::{BTreeMap, BTreeSet};

use askama::Template;
use axum::{
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use uuid::Uuid;

use crate::{
    store::Store,
    web::{WebError, render},
};

use super::{
    super::{
        BUS, load_plan, optimized_snapshot_variants, plan::bus_route_key, schedule::Calendar,
        visuals,
    },
    Task, get_task, snapshot_pins,
};

const UNAVAILABLE: WebError = (StatusCode::CONFLICT, "请先生成已锁定采购清单的提案。");

#[derive(Eq, Ord, PartialEq, PartialOrd)]
struct Gap {
    starts: String,
    ends: String,
    batch: Option<Uuid>,
    kind: &'static str,
    resource: String,
    code: String,
    needed: String,
    existing: String,
}

#[derive(Template)]
#[template(path = "media-evidence-review.html")]
struct Checklist {
    heading: &'static str,
    section: &'static str,
    task: Task,
    gaps: Vec<(Gap, String)>,
}

#[derive(Deserialize)]
pub(super) struct Format {
    download: Option<u8>,
}

pub(super) async fn checklist(
    State(store): State<Store>,
    Path(id): Path<Uuid>,
    Query(format): Query<Format>,
) -> Result<Response, WebError> {
    let task = get_task(&store, id).await?;
    if !task.can_refresh_evidence() {
        return Err(UNAVAILABLE);
    }
    let portfolio = task.portfolio.as_ref().ok_or(UNAVAILABLE)?;
    let options = &task.options;
    let variants = if let Some(rolling) = &options.rolling {
        let calendar = Calendar::load(&store, options, rolling).await?;
        calendar.variants(options, portfolio)
    } else {
        let (_, snapshot) = load_plan(store, options.clone()).await?;
        optimized_snapshot_variants(options, &snapshot.ok_or(UNAVAILABLE)?, portfolio)
    };
    let gaps = collect(&variants, &snapshot_pins(options));
    if format.download == Some(1) {
        return Ok((
            [
                (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
                (
                    header::CONTENT_DISPOSITION,
                    "attachment; filename=cityline-evidence.csv",
                ),
                (header::CACHE_CONTROL, "no-store"),
            ],
            to_csv(&gaps),
        )
            .into_response());
    }
    Ok(render(Checklist {
        heading: "提案补证清单",
        section: "tasks",
        task,
        gaps,
    }))
}

fn collect(variants: &[visuals::Variant], pins: &BTreeMap<String, Uuid>) -> Vec<(Gap, String)> {
    let mut gaps = BTreeMap::<Gap, Vec<&str>>::new();
    for variant in variants {
        for period in &variant.periods {
            let batch = pins.get(&period.starts).copied();
            let mut add = |kind, resource, code, needed, existing| {
                gaps.entry(Gap {
                    starts: period.starts.clone(),
                    ends: period.ends.clone(),
                    batch,
                    kind,
                    resource,
                    code,
                    needed,
                    existing,
                })
                .or_default()
                .push(variant.code);
            };
            if !period.available {
                let (needed, existing) = if batch.is_none() {
                    ("查询完整档期的红豆空车数及对应刊例。", "缺少同档期库存快照")
                } else if !period.required_complete {
                    (
                        "逐条复核客户指定线路的库存和刊例，确认可用替代。",
                        "已锁定库存快照",
                    )
                } else {
                    (
                        "核对所选媒体的可售量、点位规格、刊例及预算条件。",
                        "已锁定库存快照",
                    )
                };
                add(
                    "库存与组合",
                    String::new(),
                    String::new(),
                    needed.into(),
                    existing.into(),
                );
            }
            if period.screenshot_pending {
                let routes = period
                    .rows
                    .iter()
                    .filter(|row| row.media == BUS)
                    .map(|row| bus_route_key(&row.stock.name))
                    .collect::<BTreeSet<_>>();
                add(
                    "新云线路原图",
                    routes.into_iter().collect::<Vec<_>>().join("、"),
                    String::new(),
                    "在新云 AI 线路中选择这组公交线路，取得完整组合的底图截图。".into(),
                    String::new(),
                );
            }
            for row in &period.rows {
                let mut for_resource = |kind, needed, existing| {
                    add(
                        kind,
                        row.stock.name.clone(),
                        row.stock.code.clone(),
                        needed,
                        existing,
                    );
                };
                if !period.has_geometry(row) && !(row.media == BUS && period.screenshot_pending) {
                    if row.media == BUS {
                        for_resource(
                            "公交轨迹",
                            "提供本线路的新云轨迹，或本案完整线路组合的新云截图。".into(),
                            String::new(),
                        );
                    } else {
                        for_resource(
                            "地铁出口实点",
                            "提供该编号对应的具体出口坐标、坐标系和官方来源。".into(),
                            if row.coverage.iter().any(|cover| cover.media_point.is_some()) {
                                "已有新云站级落点".into()
                            } else {
                                String::new()
                            },
                        );
                    }
                }
                if !row.reach_complete() {
                    let months = row
                        .missing_reach_months
                        .iter()
                        .map(|month| format!("2027-{month:02}"))
                        .collect::<Vec<_>>()
                        .join("、");
                    let unit = if row.media == BUS {
                        "人次/辆/天"
                    } else {
                        "人次/点位/天"
                    };
                    let existing = row
                        .historical_reach
                        .as_ref()
                        .map_or_else(String::new, |reach| {
                            format!("已有历史标签人流 · {}", reach.reference)
                        });
                    for_resource(
                        "单位日触达",
                        format!(
                            "{months} 缺少 {unit}。请提供统计月份和来源，复用历史测算基准时注明口径。"
                        ),
                        existing,
                    );
                }
            }
            if period.operations_required && !period.rows.is_empty() {
                if let Some(operations) = &period.operations {
                    for (label, work) in operations.steps() {
                        let mut reasons = Vec::new();
                        if work.shortage > 0 {
                            reasons.push(format!("含天气缓冲还缺 {} 晚", work.shortage));
                        }
                        if work.outside_period {
                            reasons.push("施工窗口与投放档期冲突".into());
                        }
                        if work.overlaps {
                            reasons.push("施工窗口重叠".into());
                        }
                        if !reasons.is_empty() {
                            add(
                                "施工条件",
                                label.into(),
                                String::new(),
                                "请运营复核施工窗口与产能。".into(),
                                reasons.join(" · "),
                            );
                        }
                    }
                } else {
                    add(
                        "施工条件",
                        "上刊与下刊".into(),
                        String::new(),
                        "请运营确认夜间窗口、单晚产能和天气缓冲。".into(),
                        String::new(),
                    );
                }
            }
        }
    }
    gaps.into_iter()
        .map(|(gap, variants)| (gap, variants.join("／")))
        .collect()
}

fn to_csv(gaps: &[(Gap, String)]) -> String {
    let mut csv =
        "\u{feff}投放开始,投放结束,方案,缺项,资源,资源编号,需要的依据,已有线索,库存快照\r\n"
            .to_owned();
    for (gap, variants) in gaps {
        let batch = gap.batch.map(|id| id.to_string()).unwrap_or_default();
        let cols = [
            &gap.starts,
            &gap.ends,
            variants,
            gap.kind,
            &gap.resource,
            &gap.code,
            &gap.needed,
            &gap.existing,
            &batch,
        ];
        csv.push_str(
            &cols
                .into_iter()
                .map(safe_cell)
                .collect::<Vec<_>>()
                .join(","),
        );
        csv.push_str("\r\n");
    }
    csv
}

fn safe_cell(value: &str) -> String {
    // An external resource name must stay plain text when opened in a spreadsheet.
    let escaped = value.replace('"', "\"\"");
    let prefix = if value.trim_start().starts_with(['=', '+', '-', '@']) {
        "'"
    } else {
        ""
    };
    format!("\"{prefix}{escaped}\"")
}
