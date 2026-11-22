use askama::Template;
use axum::{
    Extension,
    extract::{Path, State},
    response::Response,
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use uuid::Uuid;

use crate::{
    agents::Agents,
    auth::User,
    store::Store,
    web::{WebError, render},
};

use super::{
    Agent, Angle, Candidate, Check, ComparisonFocus, Gate, PLANNER, Priority, REVIEWER, Reason,
    STRATEGIST, Task, get_task,
};

struct Field {
    label: &'static str,
    value: String,
}

fn add(fields: &mut Vec<Field>, label: &'static str, value: Option<impl Into<String>>) {
    if let Some(value) = value {
        fields.push(Field {
            label,
            value: value.into(),
        });
    }
}

fn field<T: DeserializeOwned>(object: &Value, key: &str) -> Option<T> {
    serde_json::from_value(object.get(key)?.clone()).ok()
}

fn metric(reason: Reason) -> &'static str {
    match reason {
        Reason::WiderCoverage => "目标圈覆盖",
        Reason::CoreBusDays => "核心圈公交车日",
        Reason::EstimatedExposure => "可核验预估人次",
        Reason::BudgetFit => "预算贴合度",
        Reason::OnlyFeasible => "仅有一案证据齐全",
        Reason::PendingEvidence => "投放或证据待核",
        Reason::EqualTradeoff => "已核指标无显著差异",
        Reason::PendingGoal => "客户主目标待确认",
        Reason::PendingPrimaryMetric => "首要指标待核",
    }
}

fn gates(object: &Value, key: &str) -> Option<String> {
    field::<Vec<Gate>>(object, key).map(|gates| {
        if gates.is_empty() {
            "无".into()
        } else {
            gates
                .iter()
                .map(|gate| gate.guidance().0)
                .collect::<Vec<_>>()
                .join("、")
        }
    })
}

fn recommendation(object: &Value, candidate_key: &str) -> Option<String> {
    let candidate = match field::<Candidate>(object, candidate_key)? {
        Candidate::A => "方案 A",
        Candidate::B => "方案 B",
        Candidate::None => "暂缓推荐",
    };
    Some(match field::<Reason>(object, "reason") {
        Some(reason) => format!("{candidate} · {}", metric(reason)),
        None => candidate.into(),
    })
}

fn inputs(role: &Agent, input: &Value) -> Vec<Field> {
    let mut fields = Vec::new();
    let scene = input.get("scene").unwrap_or(input);
    add(
        &mut fields,
        "已确认主目标",
        field::<Angle>(scene, "confirmed_goal_category").map(Angle::label),
    );
    add(
        &mut fields,
        "客户指定指标",
        field::<ComparisonFocus>(scene, "customer_comparison_focus").map(ComparisonFocus::label),
    );
    match role.id {
        "strategy" => {
            add(
                &mut fields,
                "已填写目标圈数",
                scene["custom_target_circle_count"]
                    .as_u64()
                    .map(|count| count.to_string()),
            );
            add(
                &mut fields,
                "可建议的比选指标",
                field::<Vec<ComparisonFocus>>(scene, "permitted_comparison_focuses").map(
                    |choices| {
                        if choices.is_empty() {
                            "主目标待确认".into()
                        } else {
                            choices
                                .into_iter()
                                .map(ComparisonFocus::label)
                                .collect::<Vec<_>>()
                                .join("、")
                        }
                    },
                ),
            );
        }
        "combination" => {
            add(
                &mut fields,
                "收到的比选指标",
                field::<ComparisonFocus>(input, "comparison_focus").map(ComparisonFocus::label),
            );
            add(
                &mut fields,
                "可用核算偏好",
                field::<Vec<Priority>>(input, "permitted_priorities").map(|values| {
                    values
                        .into_iter()
                        .map(Priority::label)
                        .collect::<Vec<_>>()
                        .join("、")
                }),
            );
        }
        "verification" => {
            add(
                &mut fields,
                "比选顺序",
                field::<Vec<Reason>>(input, "comparison_order").map(|values| {
                    values
                        .into_iter()
                        .map(metric)
                        .collect::<Vec<_>>()
                        .join(" → ")
                }),
            );
            add(
                &mut fields,
                "事实和证据缺项",
                gates(input, "missing_checks"),
            );
            if let Some(candidates) = input["candidates"].as_array() {
                for candidate in candidates {
                    let label = match field::<Candidate>(candidate, "candidate") {
                        Some(Candidate::A) => "方案 A",
                        Some(Candidate::B) => "方案 B",
                        _ => continue,
                    };
                    if let Some((placement, evidence)) = candidate["placement_feasible"]
                        .as_bool()
                        .zip(candidate["evidence_complete"].as_bool())
                    {
                        add(
                            &mut fields,
                            label,
                            Some(format!(
                                "{} · {}",
                                if placement {
                                    "投放条件通过"
                                } else {
                                    "投放条件待核"
                                },
                                if evidence {
                                    "证据齐全"
                                } else {
                                    "证据待补"
                                },
                            )),
                        );
                    }
                }
            }
        }
        _ => {}
    }
    fields
}

fn outputs(role: &Agent, decision: &Value, adopted: bool) -> Vec<Field> {
    let mut fields = Vec::new();
    match role.id {
        "strategy" => {
            add(
                &mut fields,
                "投放目标",
                field::<Angle>(decision, "angle").map(Angle::label),
            );
            add(
                &mut fields,
                "首要比选指标",
                field::<ComparisonFocus>(decision, "comparison_focus").map(ComparisonFocus::label),
            );
            if adopted && decision["focus_from_model"].as_bool() == Some(true) {
                add(&mut fields, "指标来源", Some("需求策略 Agent 的建议"));
            }
        }
        "combination" => {
            add(
                &mut fields,
                "核算偏好",
                field::<Priority>(decision, "priority").map(Priority::label),
            );
            add(
                &mut fields,
                "比选指标",
                field::<ComparisonFocus>(decision, "comparison_focus").map(ComparisonFocus::label),
            );
        }
        "verification" => {
            let (result, candidate_key) = if adopted {
                (&decision["recommendation"], "candidate")
            } else {
                (decision, "preferred_variant")
            };
            add(
                &mut fields,
                "推荐结论",
                recommendation(result, candidate_key),
            );
            add(&mut fields, "复核顺序", gates(decision, "review_order"));
            add(
                &mut fields,
                "交付前复核",
                field::<Check>(decision, "check").map(|check| check.copy()),
            );
        }
        _ => {}
    }
    fields
}

struct Entry {
    title: &'static str,
    status: &'static str,
    source: &'static str,
    calls: Option<String>,
    decision_note: Option<String>,
    inputs: Vec<Field>,
    proposed: Vec<Field>,
    adopted: Vec<Field>,
    previous: Option<Uuid>,
}

impl Entry {
    fn read(task: &Task, role: Agent) -> Self {
        let saved = task
            .agent_decisions
            .iter()
            .rev()
            .find(|entry| entry["role"] == role.id);
        let record = saved.unwrap_or(&Value::Null);
        let source = match record["source"].as_str() {
            Some("customer") => "客户确认",
            Some("model") => "模型建议",
            Some("rules") => "本地规则",
            Some("inherited") => "原版本决策",
            _ => "暂无记录",
        };
        let status = match record["validation"].as_str() {
            Some("accepted") => "建议通过校验",
            Some("overridden_or_partial") => "建议已校正",
            Some("fallback") => "规则兜底",
            _ if record["source"] == "inherited" => "沿用原版本",
            _ => "暂无记录",
        };
        let calls = record["attempts"].as_u64().map(|count| {
            let millis = record["duration_ms"].as_u64().unwrap_or_default();
            format!("请求 {count} 次 · {millis} 毫秒")
        });
        Self {
            title: role.name,
            status,
            source,
            calls,
            decision_note: field(record, "fallback_reason"),
            inputs: inputs(&role, &record["input_summary"]),
            proposed: outputs(&role, &record["model_output"], false),
            adopted: outputs(&role, &record["adopted"], true),
            previous: field(record, "previous_task"),
        }
    }
}

#[derive(Template)]
#[template(path = "media-agent-trace.html")]
struct Page {
    heading: &'static str,
    section: &'static str,
    task: Task,
    roles: [Entry; 3],
    is_admin: bool,
    model_configured: bool,
}

pub(super) async fn page(
    State(store): State<Store>,
    Extension(agents): Extension<Agents>,
    Extension(user): Extension<User>,
    Path(id): Path<Uuid>,
) -> Result<Response, WebError> {
    let task = get_task(&store, id).await?;
    let roles = [STRATEGIST, PLANNER, REVIEWER].map(|role| Entry::read(&task, role));
    Ok(render(Page {
        heading: "Agent 协作记录",
        section: "tasks",
        task,
        roles,
        is_admin: user.admin,
        model_configured: agents.current().available(),
    }))
}
